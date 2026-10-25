package cli

import (
	"fmt"
	"os"
	"regexp"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"golang.org/x/term"
)

var sendCmd = &cobra.Command{
	Use:   "send <name>[@host] <message>",
	Short: "Send a message to an agent: typed in now, or held until its turn ends",
	Long: `Send delivers a message to an agent, and says what it did with it and what
status it found the agent in.

An agent at its prompt gets the message typed into its terminal. A Claude
agent that is busy (processing, or waiting on a prompt) holds it instead:
when its turn ends, its Stop hook hands over every message held for it,
oldest first, and Claude goes on with them as its next step (its screen shows
them as "Stop hook feedback"). If the turn ends without that hook (Esc, a
deny, a failed turn), the held messages are typed in once it has sat at its
prompt for 10s, or ahead of the next message sent to it. "agent status
<name> -v" shows how many are held.

--now types the message in even while the agent is busy (Claude takes it at
its next tool boundary), as send always does for other backends. Typing is
refused while the agent is at a dialog (a permission or workspace trust
prompt), where the Enter would answer the dialog instead; answer those with
approve/deny. Send is also refused once the agent has exited, since the
message would run in the shell left behind. --force types the message in
without these checks, for a local agent, and only works for a person at a
terminal. An agent on another host is addressed as name@host (needs a
running bus); its host holds or types the message the same way.

A message sent by an agent (AGENT_NAME set) starts with "[from name@host]",
so the recipient knows where to reply. One sent without AGENT_NAME starts
with "[from a person on host]", or "[from a Claude session on host]" from an
AI CLI a person started (AI_AGENT or CLAUDECODE=1 set): neither has an agent
record to reply to. A slash command like "/compact" is left unmarked, and
never held: a hook can't run it, so it is typed in now.

"Sent" means the message was typed into the agent's terminal, not that the
agent has read it: right after a send, the agent still looks idle until it
picks the message up. To wait for the turn it starts (or, for a held
message, until the agent is done with it too), use
"agent wait <name> --for idle,waiting --next".

A long message typed into Claude's terminal can arrive as a pasted block,
which Claude may treat as data rather than as instructions: keep instructions
short, or put them in a file and send its path. A held message arrives
whole, however long.`,
	Example: `  agent send researcher "analyze the codebase structure"
  agent send builder "run the tests" && agent wait builder --for idle,waiting --next
  agent send builder --now "stop: the tests are already fixed"
  agent send builder@hub "run the tests"`,
	Args: cobra.MinimumNArgs(2),
	RunE: runSend,
}

var sendForce, sendNow bool

func init() {
	sendCmd.Flags().BoolVar(&sendNow, "now", false, "Type it in now even if the agent is busy, instead of holding it until its turn ends")
	sendCmd.Flags().BoolVar(&sendForce, "force", false, "Send even if the agent is at a dialog or has exited (local agents, people only)")
}

func runSend(cmd *cobra.Command, args []string) error {
	name := args[0]
	message := strings.Join(args[1:], " ")

	ref, remote, err := parseTarget(name)
	if err != nil {
		return err
	}
	message = fromAgent(message)
	if sendForce && (os.Getenv("AGENT_NAME") != "" || !stdinIsTerminal()) {
		// Agents may send without a prompt; --force would let one press Enter
		// on another agent's permission dialog, which is a person's call. An
		// agent's commands never have a terminal on stdin, even if it clears
		// AGENT_NAME.
		return fmt.Errorf("--force needs a person at a terminal; agents answer dialogs through a person (agent approve/deny)")
	}
	switch {
	case remote && sendForce:
		return fmt.Errorf("--force only applies to local agents")
	case remote:
		var resp *natsbus.SendResponse
		err = callRemote(ref.Host, func(c *natsbus.RPCClient) (err error) {
			resp, err = c.SendMessage(getContext(), ref, natsbus.SendRequest{Message: message, Now: sendNow})
			return err
		})
		if err != nil {
			return err
		}
		if resp == nil { // a host that predates holding: it typed the message
			fmt.Printf("Sent to %s.\n", name)
			return nil
		}
		printSent(name, service.SendResult{Status: service.AgentStatus(resp.Status), Detail: resp.Detail,
			Held: resp.Held, HeldCount: resp.HeldCount, Typed: resp.Typed})
	case sendForce:
		if err := agentService.SendUnchecked(getContext(), ref.Name, message); err != nil {
			return err
		}
		fmt.Printf("Sent to %s (unchecked: --force).\n", name)
	default:
		res, err := agentService.Send(getContext(), ref.Name, message, service.SendOptions{Now: sendNow, From: senderID()})
		if err != nil {
			return err
		}
		printSent(name, res)
	}
	return nil
}

// printSent says what send did with a message, and what it found the agent
// doing.
func printSent(name string, res service.SendResult) {
	found := "it was " + string(res.Status)
	if res.Status == service.AgentStatusUnknown {
		found = "its status is unknown"
	}
	switch {
	case res.Held:
		behind := ""
		if res.HeldCount > 1 {
			behind = ", behind " + plural(res.HeldCount-1, "other message")
		}
		if res.Status == service.AgentStatusWaiting && res.Detail != "" {
			found += " on " + service.OneLine(res.Detail)
		}
		fmt.Printf("Held for %s until its turn ends%s (%s).\n", name, behind, found)
		if res.Status == service.AgentStatusWaiting {
			hint("its turn goes on once the prompt is answered: agent status %s -v shows it", name)
		}
	case res.Typed > 1:
		fmt.Printf("Sent to %s, after %s held for it (%s).\n", name, plural(res.Typed-1, "message"), found)
	default:
		fmt.Printf("Sent to %s (%s).\n", name, found)
	}
}

// senderID is who a message is from, as held messages record it: the
// sending agent, or "user".
func senderID() string {
	if name := os.Getenv("AGENT_NAME"); name != "" {
		return name
	}
	return "user"
}

// fromAgent marks a message with who sent it: AGENT_NAME, or a person.
func fromAgent(message string) string {
	return markSender(os.Getenv("AGENT_NAME"), message)
}

// markSender prefixes message with its sender's address on this host, so
// the recipient knows who to reply to. A sender without an agent record
// (sender "") has no address to reply to with agent send, so the tag only
// says what it is: "[from a person on host]", or "[from a Claude session on
// host]" for an AI CLI a person started (see unrecordedSender). Their slash
// commands ("/compact") stay unmarked, since a prefix would turn them into
// plain messages.
func markSender(sender, message string) string {
	if sender == "" {
		if strings.HasPrefix(message, "/") {
			return message
		}
		return fmt.Sprintf("[from %s on %s] %s", unrecordedSender(), cfg.Host, message)
	}
	return fmt.Sprintf("[from %s@%s] %s", sender, cfg.Host, message)
}

// aiToolName matches a usable tool name: the part of AI_AGENT before its
// first "_" (e.g. "claude-code" in "claude-code_2-1-310_agent").
var aiToolName = regexp.MustCompile(`^[A-Za-z][A-Za-z0-9-]{0,23}$`)

// unrecordedSender describes a sender without AGENT_NAME: an AI CLI if its
// environment says so (AI_AGENT names the tool; Claude Code also sets
// CLAUDECODE=1), else a person.
func unrecordedSender() string {
	if v := os.Getenv("AI_AGENT"); v != "" {
		tool, _, _ := strings.Cut(v, "_")
		switch {
		case strings.HasPrefix(strings.ToLower(tool), "claude"):
			return "a Claude session"
		case aiToolName.MatchString(tool):
			return "a " + tool + " session"
		}
		return "an AI session"
	}
	if os.Getenv("CLAUDECODE") == "1" {
		return "a Claude session"
	}
	return "a person"
}

// stdinIsTerminal is a real isatty check: /dev/null, which agents' commands
// often have on stdin, is also a character device.
func stdinIsTerminal() bool {
	return term.IsTerminal(int(os.Stdin.Fd()))
}

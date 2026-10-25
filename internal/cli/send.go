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
	Short: "Type a message into an agent now",
	Long: `Send types a message into an agent's terminal immediately, and says what
status it found the agent in.

The message is typed in even while the agent is working (Claude and agy queue
it). It is refused while the agent is at a dialog (a permission or workspace
trust prompt), where the Enter would answer the dialog instead; answer those
with approve/deny. It is also refused once the agent has exited, since the
message would run in the shell left behind. --force overrides these checks
for a local agent, and only works for a person at a terminal. Use 'queue' to
wait until the agent is idle. An agent on another host is addressed as
name@host (needs a running bus).

A message sent by an agent (AGENT_NAME set) starts with "[from name@host]",
so the recipient knows where to reply. One sent without AGENT_NAME starts
with "[from a person on host]", or "[from a Claude session on host]" from an
AI CLI a person started (AI_AGENT or CLAUDECODE=1 set): neither has an agent
record to reply to. A slash command like "/compact" is left unmarked.

"Sent" means the message was typed into the agent's terminal, not that the
agent has read it: right after a send, the agent still looks idle until it
picks the message up. To wait for the turn it starts, use
"agent wait <name> --for idle,waiting --next".

A long message typed into Claude's terminal can arrive as a pasted block,
which Claude may treat as data rather than as instructions: keep instructions
short, or put them in a file and send its path.`,
	Example: `  agent send researcher "analyze the codebase structure"
  agent send builder "run the tests" && agent wait builder --for idle,waiting --next
  agent send builder@hub "run the tests"`,
	Args: cobra.MinimumNArgs(2),
	RunE: runSend,
}

var sendForce bool

func init() {
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
	var found string // the status send found the agent in, if it knows
	switch {
	case remote && sendForce:
		return fmt.Errorf("--force only applies to local agents")
	case remote:
		err = callRemote(ref.Host, func(c *natsbus.RPCClient) error {
			return c.Send(getContext(), ref, message)
		})
	case sendForce:
		err = agentService.SendUnchecked(getContext(), ref.Name, message)
		found = "unchecked: --force"
	default:
		var status service.AgentStatus
		status, err = agentService.Send(getContext(), ref.Name, message)
		found = "it was " + string(status)
		if status == service.AgentStatusUnknown {
			found = "its status is unknown"
		}
	}
	if err != nil {
		return err
	}

	if found != "" {
		fmt.Printf("Sent to %s (%s).\n", name, found)
	} else {
		fmt.Printf("Sent to %s.\n", name)
	}
	return nil
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

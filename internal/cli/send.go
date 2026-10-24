package cli

import (
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"golang.org/x/term"
)

var sendCmd = &cobra.Command{
	Use:   "send <name>[@host] <message>",
	Short: "Send a message to an agent immediately",
	Long: `Send delivers a message to an agent's terminal immediately.

The message is typed in even while the agent is working (Claude and agy queue
it). It is refused while the agent is at a dialog (a permission or workspace
trust prompt), where the Enter would answer the dialog instead; answer those
with approve/deny. --force overrides that check for a local agent. Use 'queue'
to wait until the agent is idle. An agent on another host is addressed as
name@host (needs a running bus).

Examples:
  agent send researcher "analyze the codebase structure"
  agent send builder "please create the user model"
  agent send builder@hub "run the tests"`,
	Args: cobra.MinimumNArgs(2),
	RunE: runSend,
}

var sendForce bool

func init() {
	sendCmd.Flags().BoolVar(&sendForce, "force", false, "Send even if the agent is at a dialog (local agents only)")
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
		err = callRemote(ref.Host, func(c *natsbus.RPCClient) error {
			return c.Send(getContext(), ref, message)
		})
	case sendForce:
		err = agentService.SendUnchecked(getContext(), ref.Name, message)
	default:
		err = agentService.Send(getContext(), ref.Name, message)
	}
	if err != nil {
		return err
	}

	fmt.Printf("Sent message to %q\n", name)
	return nil
}

// fromAgent marks a message sent by an agent with its address, so the
// recipient knows who to reply to. Messages typed by a user are unmarked.
func fromAgent(message string) string {
	from := os.Getenv("AGENT_NAME")
	if from == "" {
		return message
	}
	return fmt.Sprintf("[from %s@%s] %s", from, cfg.Host, message)
}

// stdinIsTerminal is a real isatty check: /dev/null, which agents' commands
// often have on stdin, is also a character device.
func stdinIsTerminal() bool {
	return term.IsTerminal(int(os.Stdin.Fd()))
}

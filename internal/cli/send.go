package cli

import (
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
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
	if remote {
		if sendForce {
			return fmt.Errorf("--force only applies to local agents")
		}
		client, closeFn, err := dialRemote(ref.Host)
		if err != nil {
			return err
		}
		defer closeFn()
		if err := client.Send(getContext(), ref, message); err != nil {
			return err
		}
	} else if sendForce {
		if err := agentService.SendUnchecked(getContext(), ref.Name, message); err != nil {
			return err
		}
	} else if err := agentService.Send(getContext(), ref.Name, message); err != nil {
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

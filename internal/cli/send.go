package cli

import (
	"fmt"
	"strings"

	"github.com/spf13/cobra"
)

var sendCmd = &cobra.Command{
	Use:   "send <name>[@host] <message>",
	Short: "Send a message to an agent immediately",
	Long: `Send delivers a message to an agent's terminal immediately.

The message is sent regardless of the agent's current state. Use 'queue'
if you want to wait until the agent is idle. An agent on another host is
addressed as name@host (needs a running bus).

Examples:
  agent send researcher "analyze the codebase structure"
  agent send builder "please create the user model"
  agent send builder@hub "run the tests"`,
	Args: cobra.MinimumNArgs(2),
	RunE: runSend,
}

func runSend(cmd *cobra.Command, args []string) error {
	name := args[0]
	message := strings.Join(args[1:], " ")

	ref, remote, err := parseTarget(name)
	if err != nil {
		return err
	}
	if remote {
		client, closeFn, err := dialRemote(ref.Host)
		if err != nil {
			return err
		}
		defer closeFn()
		if err := client.Send(getContext(), ref, message); err != nil {
			return err
		}
	} else if err := agentService.Send(getContext(), ref.Name, message); err != nil {
		return err
	}

	fmt.Printf("Sent message to %q\n", name)
	return nil
}

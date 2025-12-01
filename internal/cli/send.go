package cli

import (
	"fmt"
	"strings"

	"github.com/spf13/cobra"
)

var sendCmd = &cobra.Command{
	Use:   "send <name> <message>",
	Short: "Send a message to an agent immediately",
	Long: `Send delivers a message to an agent's terminal immediately.

The message is sent regardless of the agent's current state. Use 'queue'
if you want to wait until the agent is idle.

Examples:
  agent send researcher "analyze the codebase structure"
  agent send builder "please create the user model"`,
	Args: cobra.MinimumNArgs(2),
	RunE: runSend,
}

func runSend(cmd *cobra.Command, args []string) error {
	name := args[0]
	message := strings.Join(args[1:], " ")

	if err := agentService.Send(getContext(), name, message); err != nil {
		return err
	}

	fmt.Printf("Sent message to %q\n", name)
	return nil
}

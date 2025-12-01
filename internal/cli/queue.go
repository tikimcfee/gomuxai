package cli

import (
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
)

var queueCmd = &cobra.Command{
	Use:   "queue <name> <message>",
	Short: "Queue a message for delivery when agent is idle",
	Long: `Queue adds a message to the inbox for delivery when the agent becomes idle.

The watcher daemon monitors agents and delivers queued messages automatically
when it detects the agent is ready (idle prompt visible).

Use --from to specify the sender (defaults to AGENT_NAME env var or "user").

Examples:
  agent queue researcher "when you're free, check the test results"
  agent queue builder --from coordinator "priority: fix the auth bug"`,
	Args: cobra.MinimumNArgs(2),
	RunE: runQueue,
}

var queueFrom string

func init() {
	queueCmd.Flags().StringVar(&queueFrom, "from", "", "Sender ID (defaults to AGENT_NAME or 'user')")
}

func runQueue(cmd *cobra.Command, args []string) error {
	name := args[0]
	message := strings.Join(args[1:], " ")

	// Determine sender
	sender := queueFrom
	if sender == "" {
		sender = os.Getenv("AGENT_NAME")
	}
	if sender == "" {
		sender = "user"
	}

	if err := agentService.Queue(getContext(), sender, name, message); err != nil {
		return err
	}

	fmt.Printf("Queued message from %q to %q\n", sender, name)
	return nil
}

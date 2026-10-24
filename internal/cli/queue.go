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
	Long: `Queue adds a message to the inbox for delivery when the agent is ready.

The watcher ("agent watch") delivers queued messages when the agent's status
is idle, or error (a failed turn, back at its prompt). It holds them while the
agent is processing or waiting, and fails them if the agent has exited.
Without a running watcher, queued messages are never delivered.

The agent must be on this host: name@<this host> works, while an agent on
another host is refused, since queueing isn't served over the bus (use
"agent send name@host").

Use --from to specify the sender (defaults to AGENT_NAME env var or "user").
A message from anyone but "user" is marked with the sender's address, as
"agent send" marks messages from agents.

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
	ref, remote, err := parseTarget(args[0])
	if err != nil {
		return err
	}
	if remote {
		return fmt.Errorf("queued messages are kept per host, and %s is on %q: queueing isn't served over the bus, so use agent send %s instead", ref, ref.Host, ref)
	}
	name := ref.Name
	message := strings.Join(args[1:], " ")

	// Determine sender
	sender := queueFrom
	if sender == "" {
		sender = os.Getenv("AGENT_NAME")
	}
	if sender == "" {
		sender = "user"
	}

	if sender != "user" {
		message = markSender(sender, message)
	}
	if err := agentService.Queue(getContext(), sender, name, message); err != nil {
		return err
	}

	fmt.Printf("Queued message from %q to %q\n", sender, name)
	return nil
}

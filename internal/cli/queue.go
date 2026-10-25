package cli

import (
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var queueCmd = &cobra.Command{
	Use:   "queue <name> <message>",
	Short: "Queue a message, delivered by the watcher when the agent is idle",
	Long: `Queue adds a message to the agent's inbox, for delivery when it is ready.

The watcher ("agent watch") delivers queued messages when the agent's status
is idle, or error (a failed turn, back at its prompt). It holds them while the
agent is processing or waiting, and fails them if the agent has exited.
Without a running watcher, queued messages are never delivered: queue says so
when it finds none.

The agent must be on this host: name@<this host> works, while an agent on
another host is refused, since queueing isn't served over the bus (use
"agent send name@host").

Use --from to specify the sender (defaults to AGENT_NAME env var or "user").
Messages are marked with the sender as "agent send" marks them: an agent's
address, or "[from a person on host]" for "user".`,
	Example: `  agent queue researcher "when you're free, check the test results"
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

	if sender == "user" {
		message = markSender("", message)
	} else {
		message = markSender(sender, message)
	}
	ctx := getContext()
	if err := agentService.Queue(ctx, sender, name, message); err != nil {
		return err
	}

	line := fmt.Sprintf("Queued for %s from %s", name, sender)
	if pending, err := store.GetPendingMessages(ctx, name, 100); err == nil && len(pending) > 1 {
		line += fmt.Sprintf(", behind %s", plural(len(pending)-1, "other message"))
	}
	if info, err := agentService.StatusInfo(ctx, name); err == nil {
		line += fmt.Sprintf(" (it is %s)", info.Status)
		if info.Status == service.AgentStatusExited {
			line += "; it has exited, so the watcher will fail the message"
		}
	}
	fmt.Println(line + ".")
	if running, checked := watcherRunning(); checked && !running {
		hint("no watcher is running, so it won't be delivered until one starts: agent watch (or agent send %s now)", name)
	}
	return nil
}

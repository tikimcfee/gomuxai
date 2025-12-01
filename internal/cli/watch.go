package cli

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/watcher"
)

var watchCmd = &cobra.Command{
	Use:   "watch",
	Short: "Start the message delivery watcher",
	Long: `Watch starts a daemon that monitors agents and delivers queued messages.

The watcher periodically checks each agent's status and delivers pending
messages when agents become idle.

Examples:
  agent watch
  agent watch --interval 5s`,
	Args: cobra.NoArgs,
	RunE: runWatch,
}

var watchInterval time.Duration

func init() {
	watchCmd.Flags().DurationVar(&watchInterval, "interval", 2*time.Second, "Check interval")
}

func runWatch(cmd *cobra.Command, args []string) error {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	w := watcher.NewWatcher(agentService, store, watchInterval)
	w.Start(ctx)

	fmt.Printf("Watcher started (interval: %s)\n", watchInterval)
	fmt.Println("Press Ctrl+C to stop")

	// Wait for interrupt
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	<-sigCh

	fmt.Println("\nStopping watcher...")
	w.Stop()
	fmt.Println("Watcher stopped")

	return nil
}

package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/watcher"
)

var watchCmd = &cobra.Command{
	Use:   "watch",
	Short: "Deliver queued messages as agents become idle (runs until Ctrl-C)",
	Long: `Watch runs in the foreground, delivering queued messages ("agent queue"),
until interrupted.

Every --interval it checks each agent that has a queued message, and delivers
the oldest one when the agent is idle, or back at its prompt after a failed
turn (error). It holds messages while an agent is processing or waiting, and
fails them if the agent has exited. Deliveries and failures are logged on
stderr.

Several watchers can run at once (each delivery is checked against the
agent's status), but one per host is enough. Each running watcher holds
.gomuxai/watch.lock, which is how queue knows whether one is running.`,
	Example: `  agent watch
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

	others, checked := watcherRunning()
	release, err := holdWatchLock()
	if err != nil {
		return err
	}
	defer release()

	w := watcher.NewWatcher(agentService, store, watchInterval)
	w.Start(ctx)

	agents, _ := agentService.List(ctx)
	pending := 0
	for _, a := range agents {
		if msgs, err := store.GetPendingMessages(ctx, a.Name, 100); err == nil {
			pending += len(msgs)
		}
	}
	fmt.Printf("Watching %s on host %s every %s; %s queued. Ctrl-C stops.\n",
		plural(len(agents), "agent"), cfg.Host, watchInterval, plural(pending, "message"))
	if checked && others {
		hint("another watcher is already running for this data dir; one is enough")
	}

	// Wait for interrupt
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	<-sigCh

	w.Stop()
	fmt.Println("\nWatcher stopped")

	return nil
}

// watchLockPath is the file each running watcher holds a shared lock on.
func watchLockPath() string {
	return filepath.Join(filepath.Dir(cfg.DatabasePath), "watch.lock")
}

// holdWatchLock takes a shared lock on the watch lock file, held until
// release (or until the process exits, however it exits).
func holdWatchLock() (release func(), err error) {
	f, err := os.OpenFile(watchLockPath(), os.O_RDWR|os.O_CREATE, 0600)
	if err != nil {
		return nil, fmt.Errorf("opening watch lock: %w", err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_SH); err != nil {
		f.Close()
		return nil, fmt.Errorf("locking %s: %w", watchLockPath(), err)
	}
	return func() { f.Close() }, nil
}

// watcherRunning reports whether a watcher holds the watch lock. checked is
// false if it couldn't tell. Two checks at the same instant can see each
// other as a watcher, which errs toward "running", the side that prints no
// hint.
func watcherRunning() (running, checked bool) {
	f, err := os.OpenFile(watchLockPath(), os.O_RDWR|os.O_CREATE, 0600)
	if err != nil {
		return false, false
	}
	defer f.Close()
	err = syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
	if err == nil {
		syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
		return false, true
	}
	if errors.Is(err, syscall.EWOULDBLOCK) {
		return true, true
	}
	return false, false
}

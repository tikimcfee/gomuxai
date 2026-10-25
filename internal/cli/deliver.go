package cli

import (
	"errors"
	"fmt"
	"os"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var deliverCmd = &cobra.Command{
	Use:   "deliver <name>",
	Short: "Type an agent's held messages in once it's at its prompt (send starts this)",
	Long: `Deliver types the messages held for an agent (see "agent send") into its
terminal, oldest first, as one prompt, if the agent is at its prompt.

Held messages normally reach the agent from its Stop hook when its turn
ends. A turn that ends without that hook (Esc, a deny, a failed turn) leaves
them held, so send starts "agent deliver --wait <name>" in the background
when it holds a message. That checks every 2s and types the held messages
once the agent has sat at its prompt for 10s (time for a person who pressed
Esc to type first). It exits once nothing is held, or when the agent has
exited or is gone. One runs per agent at a time; it logs to
.gomuxai/agents/<name>/deliver.log.

Without --wait, deliver checks once: it types the held messages now if the
agent is at its prompt, and otherwise says why not. To type into a busy
agent, use "agent send <name> --now".`,
	Example: `  agent deliver helper
  agent deliver --wait helper`,
	Args: cobra.ExactArgs(1),
	RunE: runDeliver,
}

var deliverWait bool

// How often a waiting deliverer checks, and how long the agent must sit at
// its prompt before it types.
const (
	deliverInterval = 2 * time.Second
	deliverGrace    = 10 * time.Second
)

func init() {
	deliverCmd.Flags().BoolVar(&deliverWait, "wait", false, "Keep checking until nothing is held (what send starts)")
}

func runDeliver(cmd *cobra.Command, args []string) error {
	ref, remote, err := parseTarget(args[0])
	if err != nil {
		return err
	}
	if remote {
		return fmt.Errorf("messages are held per host, and %s is on %q: deliver runs on that host (agent send %s holds or types there)", ref, ref.Host, ref)
	}
	name := ref.Name
	ctx := getContext()
	if !deliverWait {
		n, err := agentService.DeliverHeld(ctx, name, service.DeliverOptions{})
		switch {
		case errors.Is(err, service.ErrNothingHeld):
			fmt.Printf("Nothing is held for %s.\n", name)
			return nil
		case err != nil:
			return err
		case n > 0:
			fmt.Printf("Typed %s held for %s.\n", plural(n, "message"), name)
			return nil
		}
		status := service.AgentStatusUnknown
		if info, err := agentService.StatusInfo(ctx, name); err == nil {
			status = info.Status
		}
		fmt.Printf("Still holding %s for %s (it is %s): the end of its turn delivers them.\n",
			plural(agentService.HeldCount(ctx, name), "message"), name, status)
		return nil
	}

	for {
		release, ok, err := tryDelivererLock(name)
		if err != nil {
			return err
		}
		if !ok {
			return nil // another deliverer has it (send starts one per held message)
		}
		n, err := agentService.DeliverHeld(ctx, name, service.DeliverOptions{Wait: true, Interval: deliverInterval, Grace: deliverGrace})
		release()
		if n > 0 {
			fmt.Printf("%s: typed %s held for %s.\n", logStamp(), plural(n, "message"), name)
		}
		if err != nil && !errors.Is(err, service.ErrNothingHeld) {
			return err
		}
		// A deliverer started while this one held the lock exited at once:
		// look again, now that the lock is free, for what it was started for.
		if agentService.HeldCount(ctx, name) == 0 {
			return nil
		}
	}
}

// logStamp is the time, for deliver's log lines.
func logStamp() string {
	return time.Now().Format(time.RFC3339)
}

// tryDelivererLock takes the agent's deliverer lock without blocking. ok is
// false if another deliverer holds it.
func tryDelivererLock(name string) (release func(), ok bool, err error) {
	path := agentService.DelivererLockPath(name)
	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0600)
	if err != nil {
		return nil, false, fmt.Errorf("opening deliverer lock: %w", err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		f.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) {
			return nil, false, nil
		}
		return nil, false, fmt.Errorf("locking %s: %w", path, err)
	}
	return func() { f.Close() }, true, nil
}

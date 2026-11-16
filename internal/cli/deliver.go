package cli

import (
	"errors"
	"fmt"
	"os"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var deliverCmd = &cobra.Command{
	Use:   "deliver <name>",
	Short: "Type an agent's held messages in once it's at its prompt (send starts this)",
	Long: `Deliver types the messages held for an agent (see "agent send") into its
terminal, oldest first, as one prompt, if the agent is at its prompt.

Held messages normally reach the agent from a hook: Claude's Stop when its
turn ends, agy's PostInvocation when its current step ends. A turn that
ends without that hook (Esc, Ctrl+C, a deny, a failed turn) leaves
them held, so send starts "agent deliver --wait <name>" in the background
when it holds a message. That checks every 2s and types the held messages
once the agent has sat at its prompt, with nothing typed in its input line,
for 10s (time for a person who pressed Esc to type first; it never types
into their half-written input). It exits once nothing is held, or when the
agent's session has ended or the agent is gone. One runs per agent at a time; it logs to
.gomuxai/agents/<name>/deliver.log.

Without --wait, deliver checks once: it types the held messages now if the
agent is at its prompt with an empty input line, and otherwise says why not. To type into a busy
agent, use "agent send <name> --now".

An agent on another host (name@host) goes over the bus, as send does: that
host checks once and types them, if its bus serves the respond op (the
default; it also answers approve and deny). --wait runs only on the
agent's host.`,
	Example: `  agent deliver helper
  agent deliver helper@hub
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
		if deliverWait {
			return fmt.Errorf("deliver --wait runs on the agent's host, where send starts it when it holds a message: agent deliver %s (without --wait) types %s's held messages now, over the bus", ref, ref.Name)
		}
		var resp *natsbus.DeliverResponse
		err := callRemote(ref.Host, func(c *natsbus.RPCClient) error {
			var err error
			resp, err = c.Deliver(getContext(), ref)
			return err
		})
		if err != nil {
			if strings.Contains(err.Error(), "invalid permission response") {
				return fmt.Errorf("%s's agent binary predates deliver over the bus (%w): run agent deliver %s on that host", ref.Host, err, ref.Name)
			}
			return err
		}
		printDelivered(ref.String(), service.DeliverResult{Typed: resp.Typed, Held: resp.Held, Status: service.AgentStatus(resp.Status)})
		return nil
	}
	name := ref.Name
	ctx := getContext()
	if !deliverWait {
		res, err := agentService.DeliverNow(ctx, name)
		switch {
		case errors.Is(err, service.ErrNothingHeld):
		case err != nil:
			return err
		}
		printDelivered(name, res)
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

// printDelivered says what agent deliver did for name.
func printDelivered(name string, res service.DeliverResult) {
	switch {
	case res.Typed > 0:
		fmt.Printf("Typed %s held for %s.\n", plural(res.Typed, "message"), name)
	case res.Held == 0:
		fmt.Printf("Nothing is held for %s.\n", name)
	default:
		fmt.Printf("Still holding %s for %s (it is %s): the end of its turn delivers them.\n", plural(res.Held, "message"), name, res.Status)
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

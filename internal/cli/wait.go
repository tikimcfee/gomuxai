package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// Default status re-check intervals for wait. With a bus, events carry the
// transitions and the check is only a backstop; without one, it is the only
// source. A check can run the backend CLI (claude agents --json), so keep it slow.
const (
	waitBackstopInterval = 5 * time.Second
	waitPollInterval     = 2 * time.Second
)

// waitTimeoutExitCode tells a timeout apart from an error (exit 1) in scripts.
const waitTimeoutExitCode = 2

var (
	waitFor      string
	waitTimeout  time.Duration
	waitInterval time.Duration
	waitNext     bool
)

var waitCmd = &cobra.Command{
	Use:   "wait <name>[@host] --for <status[,status...]>",
	Short: "Wait until an agent reaches a status",
	Long: `Wait blocks until an agent's status is one of the given statuses, prints
it, and exits 0. Statuses: idle, processing, waiting, error, exited, unknown.

With --timeout, wait gives up after that long and exits 2. It exits 1 on
errors, including the agent exiting while you wait for another status.

With an agent bus running, wait reacts to status events as soon as they are
reported, and re-checks the status every 5s as a backstop for changes no hook
reports (e.g. an interrupted permission prompt going back to idle). Without a
bus it checks every 2s. --interval overrides either.

A status reached after wait starts counts even if the agent has moved on by
the time wait returns.

An agent on another host is addressed as name@host (needs a running bus);
its host's own status check confirms events, as for a local agent.

Right after "agent send", the agent still looks idle from its previous turn
until it picks the message up, so "--for idle" would return at once. Use
--next: it ignores the given statuses until the agent has been seen in another
one (e.g. processing), so it waits for the turn the message starts. Run it
right after the send; a turn that finishes before wait starts is missed.

Examples:
  agent send helper "review auth.go" && agent wait helper --for idle --next
  agent wait helper --for idle
  agent wait helper --for idle,waiting --timeout 10m
  agent wait helper --for exited
  agent wait helper@hub --for idle --timeout 10m`,
	Args: cobra.ExactArgs(1),
	RunE: runWait,
}

func init() {
	waitCmd.Flags().StringVar(&waitFor, "for", "", "Status(es) to wait for, comma-separated (required)")
	waitCmd.Flags().DurationVar(&waitTimeout, "timeout", 0, "Give up after this long and exit 2 (default: wait forever)")
	waitCmd.Flags().DurationVar(&waitInterval, "interval", 0, "Status re-check interval (default: 5s with a bus, 2s without)")
	waitCmd.Flags().BoolVar(&waitNext, "next", false, "Wait for the next arrival: ignore the statuses until the agent has left them")
	waitCmd.MarkFlagRequired("for")
}

func runWait(cmd *cobra.Command, args []string) error {
	ref, err := natsbus.ParseAgentRef(args[0])
	if err != nil {
		return err
	}
	name, host := ref.Name, cfg.Host
	remote := isRemoteHost(ref.Host)
	if remote {
		host = ref.Host
	}

	targets, err := parseWaitStatuses(waitFor)
	if err != nil {
		return err
	}
	// An exited agent never reaches anything else.
	var fail []string
	if !slices.Contains(targets, string(service.AgentStatusExited)) {
		fail = []string{string(service.AgentStatusExited)}
	}

	nc, err := busConnect()
	if err != nil && remote {
		return fmt.Errorf("waiting on %s needs a running agent bus: %w", ref, err)
	}
	if err != nil {
		if !errors.Is(err, natsbus.ErrNoBus) {
			fmt.Fprintf(os.Stderr, "agent wait: can't use the bus (%v); polling instead\n", err)
		}
		nc = nil
	} else {
		defer nc.Close()
	}

	interval := waitInterval
	if interval <= 0 {
		interval = waitPollInterval
		if nc != nil {
			interval = waitBackstopInterval
		}
	}

	ctx := getContext()
	if waitTimeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, waitTimeout)
		defer cancel()
	}

	current := func(ctx context.Context) (natsbus.Observation, error) {
		info, err := agentService.StatusInfo(ctx, name)
		if err != nil {
			return natsbus.Observation{}, err
		}
		return natsbus.Observation{
			Status:         string(info.Status),
			ReportedStatus: string(info.ReportedStatus),
			ReportedAt:     info.ReportedAt,
		}, nil
	}
	if remote {
		// The remote host's own status check confirms events, as locally.
		client := natsbus.NewRPCClient(nc, 0)
		current = func(ctx context.Context) (natsbus.Observation, error) {
			resp, err := client.Status(ctx, ref)
			if err != nil {
				return natsbus.Observation{}, err
			}
			return natsbus.Observation{
				Status:         resp.Status,
				ReportedStatus: resp.ReportedStatus,
				ReportedAt:     resp.ReportedAt,
			}, nil
		}
	}

	status, err := natsbus.Wait(ctx, nc, natsbus.WaitOptions{
		Host:     host,
		Agent:    name,
		For:      targets,
		Fail:     fail,
		Next:     waitNext,
		Current:  current,
		Interval: interval,
	})
	if errors.Is(err, context.DeadlineExceeded) {
		last := status
		if last == "" {
			last = "unknown"
		}
		fmt.Fprintf(os.Stderr, "Error: timed out after %s waiting for %s to be %s (last status: %s)\n",
			waitTimeout, ref, strings.Join(targets, " or "), last)
		if nc != nil {
			nc.Close()
		}
		os.Exit(waitTimeoutExitCode)
	}
	if err != nil {
		return err
	}
	fmt.Fprintf(cmd.OutOrStdout(), "%s: %s\n", ref, status)
	return nil
}

// parseWaitStatuses validates a comma-separated --for list.
func parseWaitStatuses(s string) ([]string, error) {
	var statuses []string
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		st, err := service.ParseAgentStatus(part)
		if err != nil {
			return nil, err
		}
		statuses = append(statuses, string(st))
	}
	if len(statuses) == 0 {
		return nil, fmt.Errorf("--for needs at least one status")
	}
	return statuses, nil
}

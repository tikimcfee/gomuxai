package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"slices"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Default status re-check intervals for wait. With a bus, events carry the
// transitions and the check is only a backstop; without one, it is the only
// source. A check can run the backend CLI (claude agents --json), so keep it slow.
const (
	waitBackstopInterval = 5 * time.Second
	waitPollInterval     = 2 * time.Second
)

// waitRelistInterval is how often wait --follow with no names looks for
// agents spawned since it started. A variable for tests.
var waitRelistInterval = 10 * time.Second

// waitTimeoutExitCode tells a timeout apart from an error (exit 1) in scripts.
const waitTimeoutExitCode = 2

var (
	waitFor      string
	waitTimeout  time.Duration
	waitInterval time.Duration
	waitNext     bool
	waitAny      bool
	waitFollow   bool
)

var waitCmd = &cobra.Command{
	Use:   "wait [name[@host]...] --for <status[,status...]>",
	Short: "Block until agents reach a status (exit 2 on timeout)",
	Long: `Wait blocks until an agent's status is one of the given statuses, prints
it, and exits 0. Statuses: idle, processing, waiting, error, exited, unknown.

With --timeout, wait gives up after that long and exits 2. It exits 1 on
errors, including the agent exiting while you wait for another status.

Several agents: wait watches them all at once and prints "name: status" for
each as it gets there, so one that finishes early isn't missed. It returns
when every one has (or, with --any, at the first). An agent that exits or
can't be checked is reported on stderr and the others are still waited on;
wait then exits 1. A waiting or error line says what the agent is asking or
why, e.g. "helper: waiting (permission: Bash: rm -rf build)".

No names: every agent on this host but yourself, with --any or --follow.
"agent wait --any --for waiting" blocks until any of them needs a person and
says which one and what it's asking.

--follow keeps going: it prints a line each time one of the agents arrives
at a status (each arrival once; an agent must leave and come back to be
printed again), until Ctrl-C or --timeout. With no names it also picks up
agents spawned while it runs. For a person in a spare terminal, or a script
that reads lines.

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
--next: it waits for the turn the last send started. It ignores the given
statuses until the agent has been seen in another one (e.g. processing), or
until that turn has reported one of them, so a turn that ended before wait
started still counts. Waiting again with --next and no new send returns that
same turn's status at once; to see each new arrival, use --follow.

An agent blocked on a permission prompt stays "waiting" until someone
answers, so a coordinator usually waits with --for idle,waiting and checks
which it got. Without "waiting" in --for, wait says on stderr when it sees
the agent waiting, and keeps waiting.

For a Claude agent, its reply is then in "agent state get <name> _result".`,
	Example: `  agent send helper "review auth.go" && agent wait helper --for idle,waiting --next --timeout 30m
  agent wait lead-api lead-db --for idle,waiting --next --timeout 30m
  agent wait lead-api lead-db --any --for idle,waiting --next
  agent wait --any --for waiting          # until any agent here needs a person
  agent wait --follow --for waiting       # each prompt, as it comes, until Ctrl-C
  agent wait helper@hub --for idle,waiting --timeout 10m`,
	RunE: runWait,
}

func init() {
	waitCmd.Flags().StringVar(&waitFor, "for", "", "Status(es) to wait for, comma-separated (required)")
	waitCmd.Flags().DurationVar(&waitTimeout, "timeout", 0, "Give up after this long and exit 2 (default: wait forever)")
	waitCmd.Flags().DurationVar(&waitInterval, "interval", 0, "Status re-check interval (default: 5s with a bus, 2s without)")
	waitCmd.Flags().BoolVar(&waitNext, "next", false, "Wait for the turn the last send started, not a status the agent is already in")
	waitCmd.Flags().BoolVar(&waitAny, "any", false, "With several agents, return when the first one gets there")
	waitCmd.Flags().BoolVar(&waitFollow, "follow", false, "Print every arrival until Ctrl-C or --timeout")
	waitCmd.MarkFlagRequired("for")
}

func runWait(cmd *cobra.Command, args []string) error {
	statuses, err := parseWaitStatuses(waitFor)
	if err != nil {
		return err
	}
	refs, err := parseWaitRefs(args)
	if err != nil {
		return err
	}
	everyAgent := len(refs) == 0
	if everyAgent && !waitAny && !waitFollow {
		return usageError(cmd, fmt.Errorf("name the agents to wait on, or add --any or --follow to watch every agent on this host"))
	}

	ctx := getContext()
	var remote *natsbus.AgentRef // the first agent on another host, if any
	for _, ref := range refs {
		if isRemoteHost(ref.Host) {
			if remote == nil {
				remote = &ref
			}
			continue
		}
		// A typo would otherwise wait out the whole timeout on the others.
		if _, err := agentService.Agent(ctx, ref.Name); err != nil {
			return err
		}
	}

	nc, err := busConnect()
	if err != nil && remote != nil {
		return fmt.Errorf("waiting on %s needs a running agent bus: %w", remote, err)
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
	w := &waiter{nc: nc, statuses: statuses, interval: interval}
	// An exited agent never reaches anything else.
	if !slices.Contains(statuses, string(service.AgentStatusExited)) {
		w.fail = []string{string(service.AgentStatusExited)}
	}

	if waitTimeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, waitTimeout)
		defer cancel()
	}

	switch {
	case waitFollow:
		return w.follow(ctx, cmd, refs)
	case len(refs) == 1:
		return w.one(ctx, cmd, refs[0])
	}
	if everyAgent {
		if refs, err = otherAgents(ctx); err != nil {
			return err
		}
		if len(refs) == 0 {
			return fmt.Errorf("no other agents on host %s to wait on: agent list shows them", cfg.Host)
		}
	}
	return w.several(ctx, cmd, refs, everyAgent)
}

// parseWaitRefs parses wait's agent names, dropping repeats.
func parseWaitRefs(args []string) ([]natsbus.AgentRef, error) {
	var refs []natsbus.AgentRef
	for _, arg := range args {
		ref, err := natsbus.ParseAgentRef(arg)
		if err != nil {
			return nil, err
		}
		if !slices.Contains(refs, ref) {
			refs = append(refs, ref)
		}
	}
	return refs, nil
}

// otherAgents returns every agent on this host except the caller.
func otherAgents(ctx context.Context) ([]natsbus.AgentRef, error) {
	agents, err := agentService.List(ctx)
	if err != nil {
		return nil, err
	}
	self := os.Getenv("AGENT_NAME")
	var refs []natsbus.AgentRef
	for _, a := range agents {
		if a.Name != self {
			refs = append(refs, natsbus.AgentRef{Name: a.Name})
		}
	}
	return refs, nil
}

// waiter holds what every agent's wait shares.
type waiter struct {
	nc       *nats.Conn // nil without a bus
	statuses []string
	fail     []string
	interval time.Duration

	mu sync.Mutex // serializes output and hints from several agents' waits
}

// wait waits for one agent; see natsbus.WaitOptions for next and after.
func (w *waiter) wait(ctx context.Context, ref natsbus.AgentRef, next bool, after time.Time) (natsbus.Observation, error) {
	host := cfg.Host
	if isRemoteHost(ref.Host) {
		host = ref.Host
	}
	return natsbus.WaitObservation(ctx, w.nc, natsbus.WaitOptions{
		Host:     host,
		Agent:    ref.Name,
		For:      w.statuses,
		Fail:     w.fail,
		Next:     next,
		After:    after,
		Current:  w.current(ref),
		Interval: w.interval,
	})
}

// current returns the status check for ref: the service locally, the remote
// host's own status check otherwise.
func (w *waiter) current(ref natsbus.AgentRef) func(context.Context) (natsbus.Observation, error) {
	check := func(ctx context.Context) (natsbus.Observation, error) {
		info, err := agentService.StatusInfo(ctx, ref.Name)
		if err != nil {
			return natsbus.Observation{}, err
		}
		return natsbus.Observation{
			Status:         string(info.Status),
			Detail:         info.Detail,
			ReportedStatus: string(info.ReportedStatus),
			ReportedAt:     info.ReportedAt,
			SentAt:         info.SentAt,
			TurnAt:         info.TurnAt,
		}, nil
	}
	if isRemoteHost(ref.Host) {
		client := natsbus.NewRPCClient(w.nc, 0)
		check = func(ctx context.Context) (natsbus.Observation, error) {
			resp, err := client.Status(ctx, ref)
			if err != nil {
				return natsbus.Observation{}, err
			}
			return natsbus.Observation{
				Status:         resp.Status,
				Detail:         resp.Detail,
				ReportedStatus: resp.ReportedStatus,
				ReportedAt:     resp.ReportedAt,
				SentAt:         resp.SentAt,
				TurnAt:         resp.TurnAt,
			}, nil
		}
	}
	if slices.Contains(w.statuses, string(service.AgentStatusWaiting)) {
		return check
	}
	// Blocked on a person, the agent won't reach the statuses until someone
	// answers: say so once, from the status check that saw it.
	return func(ctx context.Context) (natsbus.Observation, error) {
		obs, err := check(ctx)
		if err == nil && obs.Status == string(service.AgentStatusWaiting) {
			w.mu.Lock()
			hint("%s is waiting on a person (a permission prompt or question), so it won't be %s until someone answers: agent status %s -v shows what it's asking (--for %s,waiting returns on prompts)",
				ref, strings.Join(w.statuses, " or "), ref, strings.Join(w.statuses, ","))
			w.mu.Unlock()
		}
		return obs, err
	}
}

// one waits for a single named agent. Its output and errors are wait's
// original ones, which scripts rely on.
func (w *waiter) one(ctx context.Context, cmd *cobra.Command, ref natsbus.AgentRef) error {
	obs, err := w.wait(ctx, ref, waitNext, time.Time{})
	if errors.Is(err, context.DeadlineExceeded) {
		return &exitError{code: waitTimeoutExitCode, err: fmt.Errorf("timed out after %s waiting for %s to be %s (last status: %s)",
			waitTimeout, ref, strings.Join(w.statuses, " or "), orUnknown(obs.Status))}
	}
	if err != nil {
		return err
	}
	fmt.Fprintf(cmd.OutOrStdout(), "%s: %s\n", ref, obs.Status)
	if obs.Status == string(service.AgentStatusWaiting) && obs.Detail != "" {
		hint("%s is asking: %s; agent status %s -v shows all of it, then agent approve %s or agent deny %s",
			ref, oneLineDetail(obs.Detail), ref, ref, ref)
	}
	return nil
}

// waitOutcome is how one agent's wait ended.
type waitOutcome struct {
	ref natsbus.AgentRef
	obs natsbus.Observation
	err error
}

// several waits for refs at once, until all of them (or, with --any, the
// first) get there. everyAgent says refs weren't named: an agent that has
// exited is then skipped, not reported.
func (w *waiter) several(ctx context.Context, cmd *cobra.Command, refs []natsbus.AgentRef, everyAgent bool) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	outcomes := make(chan waitOutcome, len(refs))
	for _, ref := range refs {
		go func() {
			obs, err := w.wait(ctx, ref, waitNext, time.Time{})
			outcomes <- waitOutcome{ref, obs, err}
		}()
	}

	out := cmd.OutOrStdout()
	var failed, timedOut []string
	reached := 0
	for range refs {
		o := <-outcomes
		switch {
		case o.err == nil:
			reached++
			fmt.Fprintln(out, arrivalLine(o.ref, o.obs))
			if waitAny {
				return nil
			}
		case errors.Is(o.err, context.DeadlineExceeded):
			timedOut = append(timedOut, fmt.Sprintf("%s %s", o.ref, orUnknown(o.obs.Status)))
		case everyAgent && o.obs.Status == string(service.AgentStatusExited):
			// Not one you asked about, and it will never need anything.
		default:
			fmt.Fprintf(hintOut, "agent wait: %v\n", o.err)
			failed = append(failed, fmt.Sprintf("%s (%s)", o.ref, orUnknown(o.obs.Status)))
		}
	}

	want := strings.Join(w.statuses, " or ")
	if len(failed) > 0 {
		left := failed
		if len(timedOut) > 0 {
			left = append(left, "and timed out on "+strings.Join(timedOut, ", "))
		}
		return fmt.Errorf("%d of %s reached %s; not %s", reached, plural(len(refs), "agent"), want, strings.Join(left, ", "))
	}
	if len(timedOut) > 0 {
		which := "for"
		if waitAny {
			which = "for any of"
		}
		return &exitError{code: waitTimeoutExitCode, err: fmt.Errorf("timed out after %s waiting %s %s to be %s (last status: %s)",
			waitTimeout, which, plural(len(timedOut), "agent"), want, strings.Join(timedOut, ", "))}
	}
	if reached == 0 {
		return fmt.Errorf("none of the %s can become %s: they have exited", plural(len(refs), "agent"), want)
	}
	return nil
}

// follow prints every arrival of refs (every agent on this host, if none)
// until interrupted or ctx ends.
func (w *waiter) follow(ctx context.Context, cmd *cobra.Command, refs []natsbus.AgentRef) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	interrupted := make(chan os.Signal, 1)
	signal.Notify(interrupted, os.Interrupt, syscall.SIGTERM)
	defer signal.Stop(interrupted)

	everyAgent := len(refs) == 0
	out := cmd.OutOrStdout()
	var wg sync.WaitGroup
	defer wg.Wait()
	watching := map[natsbus.AgentRef]bool{}
	ended := make(chan natsbus.AgentRef, 16)
	// watch follows one agent until its wait fails or ctx ends.
	watch := func(ref natsbus.AgentRef) {
		defer wg.Done()
		next, after := waitNext, time.Time{}
		for {
			obs, err := w.wait(ctx, ref, next, after)
			if ctx.Err() != nil {
				return
			}
			w.mu.Lock()
			switch {
			case err == nil:
				fmt.Fprintln(out, arrivalLine(ref, obs))
			case everyAgent && obs.Status == string(service.AgentStatusExited):
			case everyAgent && errors.As(err, new(*service.AgentNotFoundError)):
			default:
				fmt.Fprintf(hintOut, "agent wait: %v; no longer watching %s\n", err, ref)
			}
			w.mu.Unlock()
			if err != nil {
				ended <- ref
				return
			}
			// Each arrival once: the next one must be newer.
			next, after = true, obs.ReportedAt
			if after.IsZero() {
				after = time.Now()
			}
		}
	}
	start := func(refs []natsbus.AgentRef) {
		for _, ref := range refs {
			if !watching[ref] {
				watching[ref] = true
				wg.Add(1)
				go watch(ref)
			}
		}
	}

	var relist <-chan time.Time // stays nil for named agents
	if everyAgent {
		found, err := otherAgents(ctx)
		if err != nil {
			return err
		}
		refs = found
		ticker := time.NewTicker(waitRelistInterval)
		defer ticker.Stop()
		relist = ticker.C
	}
	start(refs)
	who := strings.Join(refStrings(refs), ", ")
	if everyAgent {
		who = fmt.Sprintf("every agent on host %s (%s now)", cfg.Host, plural(len(refs), "agent"))
	}
	notice("Waiting for %s to be %s, each time. Ctrl-C stops.", who, strings.Join(w.statuses, " or "))

	live := len(refs)
	for {
		select {
		case <-interrupted:
			return nil
		case <-ctx.Done():
			if errors.Is(ctx.Err(), context.DeadlineExceeded) {
				return &exitError{code: waitTimeoutExitCode, err: fmt.Errorf("stopped following after %s (--timeout)", waitTimeout)}
			}
			return ctx.Err()
		case <-relist:
			found, err := otherAgents(ctx)
			if err != nil {
				continue // the next relist may work; the agents already watched still are
			}
			for _, ref := range found {
				if !watching[ref] {
					live++
				}
			}
			start(found)
		case ref := <-ended:
			// A respawn under the same name is picked up by the next relist.
			delete(watching, ref)
			live--
			if live == 0 && !everyAgent {
				return fmt.Errorf("no agents left to follow")
			}
		}
	}
}

// arrivalLine renders one agent's arrival for several agents' waits:
// "name: status", plus what it's asking when waiting, or why when error.
func arrivalLine(ref natsbus.AgentRef, obs natsbus.Observation) string {
	line := fmt.Sprintf("%s: %s", ref, obs.Status)
	switch service.AgentStatus(obs.Status) {
	case service.AgentStatusWaiting, service.AgentStatusError:
		if obs.Detail != "" {
			line += " (" + oneLineDetail(obs.Detail) + ")"
		}
	}
	return line
}

// oneLineDetail shortens a status detail to one line that can't write
// escape sequences to the terminal.
func oneLineDetail(detail string) string {
	return service.OneLine(util.SanitizeTerminal(detail))
}

func refStrings(refs []natsbus.AgentRef) []string {
	s := make([]string, len(refs))
	for i, ref := range refs {
		s[i] = ref.String()
	}
	return s
}

func orUnknown(status string) string {
	if status == "" {
		return "unknown"
	}
	return status
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

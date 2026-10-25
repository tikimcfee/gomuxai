package cli

import (
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// orientMaxAgents caps how many agents the orientation names.
const orientMaxAgents = 8

// orientBusTimeout bounds the wait for the bus's status answer: the
// orientation must stay fast.
const orientBusTimeout = 500 * time.Millisecond

// runOrient prints a short orientation for whoever runs "agent" alone.
func runOrient(cmd *cobra.Command, args []string) error {
	return orient(os.Stdout, os.Getenv("AGENT_NAME"))
}

// orient writes the orientation for me (an agent's name, or "" for a
// person): who you are, the agents that matter to you, the bus, your queued
// messages, and where to go next. Statuses are resolved as by agent status;
// claude agents share one backend call.
func orient(w io.Writer, me string) error {
	ctx := getContext()
	agents, err := agentService.List(ctx)
	if err != nil {
		return err
	}
	t := newTeams(agents)

	// Who you are.
	switch self := t.agents[me]; {
	case me == "":
		fmt.Fprintf(w, "You are a person at a terminal, on host %s.\n", cfg.Host)
	case self == nil:
		fmt.Fprintf(w, "You are agent %s on host %s, with no agent record here (so agent state set %s fails).\n", me, cfg.Host, me)
	case self.SpawnedBy != "":
		fmt.Fprintf(w, "You are agent %s on host %s, a helper of %s.\n", me, cfg.Host, self.SpawnedBy)
	default:
		fmt.Fprintf(w, "You are agent %s on host %s, spawned by a person.\n", me, cfg.Host)
	}

	// The agents that matter to you: your helpers, or everyone, in the
	// order agent list shows them.
	infos := make([]natsbus.AgentInfo, len(agents))
	for i, a := range agents {
		infos[i] = natsbus.AgentInfo{Name: a.Name, SpawnedBy: a.SpawnedBy}
	}
	var shown []*storage.Agent
	for _, a := range teamOrder(infos) {
		shown = append(shown, t.agents[a.Name])
	}
	if me != "" {
		shown = nil
		for _, name := range t.helpers(me) {
			shown = append(shown, t.agents[name])
		}
	}
	switch {
	case len(agents) == 0:
		fmt.Fprintln(w, "Agents here: none yet.")
	case me != "" && len(shown) == 0:
		fmt.Fprintf(w, "Your helpers: none (%s here: agent list).\n", plural(len(agents), "agent"))
	default:
		label := fmt.Sprintf("Agents here (%d)", len(agents))
		if me != "" {
			label = fmt.Sprintf("Your helpers (%d of %s here)", len(shown), plural(len(agents), "agent"))
		}
		fmt.Fprintf(w, "%s: %s\n", label, agentStatuses(shown))
	}

	fmt.Fprintf(w, "Bus: %s\n", busSummary())

	if me != "" && t.agents[me] != nil {
		if pending, err := store.GetPendingMessages(ctx, me, 100); err == nil && len(pending) > 0 {
			line := fmt.Sprintf("Queued for you: %s, delivered when you're idle", plural(len(pending), "message"))
			if agentService.HoldsMessages(ctx, me) {
				line = fmt.Sprintf("Held for you: %s, delivered when this turn ends", plural(len(pending), "message"))
			} else if running, ok := watcherRunning(); ok && !running {
				line += ", but no watcher is running to deliver (agent watch)"
			}
			fmt.Fprintln(w, line+".")
		}
	}

	if len(agents) == 0 {
		fmt.Fprintln(w, `Start with: agent spawn <name> --backend claude, then agent send <name> "<task>". All commands: agent help`)
	} else {
		fmt.Fprintln(w, `Next: agent status <name> -v (live status), agent send <name> "<message>", agent wait <name> --for idle,waiting --next. All commands: agent help`)
	}
	return nil
}

// agentStatuses lists agents with their status, e.g. "helper idle,
// builder processing (1 held)", up to orientMaxAgents of them.
func agentStatuses(agents []*storage.Agent) string {
	var parts []string
	for i, a := range agents {
		if i == orientMaxAgents {
			parts = append(parts, fmt.Sprintf("and %d more (agent list)", len(agents)-i))
			break
		}
		status, held := service.AgentStatusUnknown, ""
		if info, err := agentService.StatusInfo(getContext(), a.Name); err == nil {
			status = info.Status
			if info.Held > 0 {
				held = fmt.Sprintf(" (%d held)", info.Held)
			}
		}
		parts = append(parts, fmt.Sprintf("%s %s%s", a.Name, status, held))
	}
	return strings.Join(parts, ", ")
}

// busSummary describes this host's bus in a few words.
func busSummary() string {
	info, err := queryBus(orientBusTimeout)
	switch {
	case errors.Is(err, natsbus.ErrNoBus) && !busSetUp():
		return "not set up (only needed to reach other hosts: agent bus --help)"
	case errors.Is(err, natsbus.ErrNoBus):
		return "not running (agent bus start)"
	case err != nil:
		return fmt.Sprintf("can't tell (%v)", err)
	}
	s := fmt.Sprintf("running, %d of %s linked", info.LinkedPeers, plural(len(info.Trusted), "trusted peer"))
	if len(info.Trusted) > 0 {
		s += " (" + strings.Join(info.Trusted, ", ") + ")"
	}
	if info.LeafAddr != "" {
		s += "; this host is the hub"
	}
	return s
}

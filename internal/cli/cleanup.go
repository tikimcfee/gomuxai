package cli

import (
	"fmt"
	"os"
	"slices"
	"sort"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var cleanupCmd = &cobra.Command{
	Use:   "cleanup <name> [<name>...]",
	Short: "Clean up agents by name",
	Long: `Cleanup removes the named agents: it asks each idle one to exit, kills its
tmux session, and deletes its record, queued messages, state, and files. Its
log file stays, for agent logs. It prints one line per name, and exits
non-zero if any of them failed.

Inside an agent (AGENT_NAME set), you can clean up only your own helpers
and their helpers, as "agent list" shows under PARENT, and not yourself:
your session would be killed partway through the cleanup. Name a helper's
own helpers along with it, or it is refused, since you couldn't reach them
once it's gone. A person can clean up any agent. Cleanup never removes an
agent you didn't name: it says which helpers remain instead.

Examples:
  agent cleanup researcher
  agent cleanup lead-build lead-build-test`,
	Args: cobra.MinimumNArgs(1),
	RunE: runCleanup,
}

func runCleanup(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	list, err := agentService.List(ctx)
	if err != nil {
		return err
	}
	t := newTeams(list)
	caller := os.Getenv("AGENT_NAME")

	var names []string
	seen := make(map[string]bool)
	for _, name := range args {
		if !seen[name] {
			seen[name] = true
			names = append(names, name)
		}
	}
	// Helpers before their parents: a parent's check needs its helpers'
	// outcomes, and its note then names only the helpers that remain.
	sort.SliceStable(names, func(i, j int) bool { return t.depth(names[i]) > t.depth(names[j]) })

	// Decide every name before removing any, so each is judged by the
	// recorded parents as they were.
	refusals := make(map[string]string)
	cleared := make(map[string]bool)
	for _, name := range names {
		if t.agents[name] == nil {
			continue
		}
		if reason := t.refusal(caller, name, cleared); reason != "" {
			refusals[name] = reason
		} else {
			cleared[name] = true
		}
	}

	failed := 0
	for _, name := range names {
		if t.agents[name] == nil {
			fmt.Printf("failed %q: no such agent\n", name)
			failed++
			continue
		}
		if !cleared[name] {
			fmt.Printf("refused %q: %s\n", name, refusals[name])
			failed++
			continue
		}
		agent := t.agents[name]
		killed, _ := tmuxClient.SessionExists(agent.SessionName)
		err := agentService.Cleanup(ctx, name)
		if _, partial := err.(*service.CleanupError); err != nil && !partial {
			fmt.Printf("failed %q: %v\n", name, err)
			failed++
			continue
		}
		delete(t.agents, name)

		did := "removed its record, state, logs, queued messages, and files"
		if killed {
			did = fmt.Sprintf("killed session %s; %s", agent.SessionName, did)
		}
		fmt.Printf("cleaned up %q: %s\n", name, did)
		if err != nil {
			fmt.Printf("  but: %v\n", err)
			failed++
		}
		if left := t.helpers(name); len(left) > 0 {
			fmt.Printf("  note: its helpers %s remain (agent cleanup %s)\n",
				strings.Join(left, ", "), strings.Join(left, " "))
		}
	}

	if failed > 0 {
		return fmt.Errorf("%d of %d cleanups failed", failed, len(names))
	}
	return nil
}

// teams indexes agents by name, with each agent's recorded parent.
type teams struct {
	agents map[string]*storage.Agent
	order  []string // names as listed
}

func newTeams(list []*storage.Agent) *teams {
	t := &teams{agents: make(map[string]*storage.Agent, len(list))}
	for _, a := range list {
		t.agents[a.Name] = a
		t.order = append(t.order, a.Name)
	}
	return t
}

// helpers returns the agents that name spawned and are still present.
func (t *teams) helpers(name string) []string {
	var out []string
	for _, n := range t.order {
		if a := t.agents[n]; a != nil && a.SpawnedBy == name {
			out = append(out, n)
		}
	}
	return out
}

// ancestors returns name's recorded parent, that agent's parent, and so on,
// up to a person or an agent that isn't listed (a coordinator that wasn't
// spawned through agent, or one that's been cleaned up). A parent's name can
// be reused after cleanup, so recorded parents can loop: each name appears
// once.
func (t *teams) ancestors(name string) []string {
	var out []string
	seen := map[string]bool{name: true}
	for a := t.agents[name]; a != nil && a.SpawnedBy != "" && !seen[a.SpawnedBy]; a = t.agents[a.SpawnedBy] {
		seen[a.SpawnedBy] = true
		out = append(out, a.SpawnedBy)
	}
	return out
}

func (t *teams) depth(name string) int {
	return len(t.ancestors(name))
}

// refusal says why caller ("" for a person) may not clean up name, a
// listed agent, or returns "" if it may. cleared holds the names already
// cleared to be cleaned up alongside it.
func (t *teams) refusal(caller, name string, cleared map[string]bool) string {
	if caller == "" {
		return ""
	}
	by := "a person"
	if parent := t.agents[name].SpawnedBy; parent != "" {
		by = fmt.Sprintf("%q", parent)
	}
	if name == caller {
		return fmt.Sprintf("an agent can't clean itself up, since its session would be killed partway through; ask %s to", by)
	}
	if !slices.Contains(t.ancestors(name), caller) {
		return fmt.Sprintf("it was spawned by %s, not by you (%q) or your helpers", by, caller)
	}
	// Once it's gone, nothing records that its helpers are yours.
	var left []string
	for _, h := range t.helpers(name) {
		if !cleared[h] {
			left = append(left, h)
		}
	}
	if len(left) > 0 {
		return fmt.Sprintf("its helpers %s would be left behind, and only a person could clean them up then; clean them up with it (agent cleanup %s %s)",
			strings.Join(left, ", "), name, strings.Join(left, " "))
	}
	return ""
}

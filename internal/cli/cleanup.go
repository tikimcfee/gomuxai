package cli

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var (
	cleanupEnded  bool
	cleanupDryRun bool
)

var cleanupCmd = &cobra.Command{
	Use:   "cleanup <name> [<name>...] | --ended [--dry-run]",
	Short: "Stop agents and delete their records",
	Long: `Cleanup removes the named agents: for a spawned agent it asks an idle CLI to
exit and kills its tmux session; for an adopted session (agent adopt) it stops
log piping and leaves the pre-existing tmux session running. In both cases it
deletes the agent's record, queued messages, state, and files while keeping its
log file and list of transcripts, for agent logs. It prints one line per name ("cleaned up",
"refused", or "failed"), and exits 1 if any of them wasn't cleaned up.

Inside an agent (AGENT_NAME set), you can clean up only your own helpers
and their helpers, as "agent list" shows under PARENT, and not yourself:
your session would be killed partway through the cleanup. Name a helper's
own helpers along with it, or it is refused, since you couldn't reach them
once it's gone. A person can clean up any agent. Given names, cleanup never
removes an agent you didn't name: it says which helpers remain instead.

--ended picks the agents instead of names: every agent on this host whose
status is ended, as agent status resolves it (not just the last report), that
you may clean up (inside an agent: your helpers and theirs). An ended agent's
helpers that haven't ended stay; for a person cleanup says so, and inside an
agent the ended one is skipped with them, since only a person could reach
them once it's gone ("skipped"). It prints one line per agent, as for names,
then a total, and exits 1 if any it picked wasn't cleaned up. --dry-run lists what it
would clean up, why each is ended, what happens to its tmux session, and how
many held messages would be deleted with it, and changes nothing.`,
	Example: `  agent cleanup researcher
  agent cleanup lead-build lead-build-test
  agent cleanup --ended --dry-run     # what --ended would clean up, and why
  agent cleanup --ended`,
	Args: func(cmd *cobra.Command, args []string) error {
		if cleanupEnded {
			if len(args) > 0 {
				return errors.New("--ended picks the agents itself: name agents, or pass --ended, not both")
			}
			return nil
		}
		if cleanupDryRun {
			return errors.New("--dry-run goes with --ended: agent cleanup --ended --dry-run")
		}
		return cobra.MinimumNArgs(1)(cmd, args)
	},
	RunE: runCleanup,
}

func init() {
	cleanupCmd.Flags().BoolVar(&cleanupEnded, "ended", false, "Clean up every agent here whose status is ended (yours only, inside an agent)")
	cleanupCmd.Flags().BoolVar(&cleanupDryRun, "dry-run", false, "With --ended: list what it would clean up and why, changing nothing")
}

func runCleanup(cmd *cobra.Command, args []string) error {
	if cleanupEnded {
		return runCleanupEnded(cleanupDryRun)
	}
	ctx := getContext()
	list, err := agentService.List(ctx)
	if err != nil {
		return err
	}
	t := newTeams(list)
	caller := nonInteractiveCallerAgent()

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
			_, err := agentService.Agent(ctx, name)
			fmt.Printf("failed %q: %v\n", name, err)
			failed++
			continue
		}
		if !cleared[name] {
			fmt.Printf("refused %q: %s\n", name, refusals[name])
			failed++
			continue
		}
		if !cleanupOne(ctx, t, name) {
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

// cleanupOne cleans up name, a listed agent, prints its line, and drops it
// from t. It reports whether the cleanup went through in full.
func cleanupOne(ctx context.Context, t *teams, name string) bool {
	agent := t.agents[name]
	killed, _ := tmuxClient.SessionExists(agent.SessionName)
	err := agentService.Cleanup(ctx, name)
	if _, partial := err.(*service.CleanupError); err != nil && !partial {
		fmt.Printf("failed %q: %v\n", name, err)
		return false
	}
	delete(t.agents, name)

	did := fmt.Sprintf("removed its record, state, queued messages, and files; its log stays (agent logs %s)", name)
	if killed {
		if agent.Adopted {
			did = fmt.Sprintf("left session %s running; %s", agent.SessionName, did)
		} else {
			did = fmt.Sprintf("killed session %s; %s", agent.SessionName, did)
		}
	}
	fmt.Printf("cleaned up %q: %s\n", name, did)
	if err != nil {
		fmt.Printf("  but: %v\n", err)
		return false
	}
	return true
}

// runCleanupEnded is cleanup --ended: it cleans up (or, dry, lists) every
// agent here whose resolved status is ended and that the caller may clean
// up.
func runCleanupEnded(dryRun bool) error {
	ctx := getContext()
	list, err := agentService.List(ctx)
	if err != nil {
		return err
	}
	t := newTeams(list)
	caller := nonInteractiveCallerAgent()

	// Every status first, so each is judged as things stood before any
	// cleanup, as named cleanup decides every name before removing any.
	ended := make(map[string]*service.StatusInfo)
	var picked []string
	others, skipped := 0, 0
	for _, a := range list {
		info, err := agentService.StatusInfo(ctx, a.Name)
		if err != nil {
			fmt.Printf("skipped %q: its status couldn't be read: %v\n", a.Name, err)
			skipped++
			continue
		}
		if info.Status != service.AgentStatusEnded {
			continue
		}
		ended[a.Name] = info
		if caller != "" && a.Name != caller && !slices.Contains(t.ancestors(a.Name), caller) {
			others++
			continue
		}
		picked = append(picked, a.Name)
	}
	// Helpers before their parents, as for names: a parent's check needs
	// its helpers' outcomes. Then by name.
	sort.Slice(picked, func(i, j int) bool {
		if di, dj := t.depth(picked[i]), t.depth(picked[j]); di != dj {
			return di > dj
		}
		return picked[i] < picked[j]
	})

	var names []string
	cleared := make(map[string]bool)
	for _, name := range picked {
		if name == caller {
			fmt.Printf("skipped %q (ended): that's you, and an agent can't clean itself up\n", name)
			skipped++
			continue
		}
		// Inside an agent, nothing would record that the helpers left
		// behind are yours.
		if left := helpersLeft(t, name, cleared); caller != "" && len(left) > 0 {
			fmt.Printf("skipped %q (ended): its helpers %s haven't ended, and only a person could clean them up once it's gone\n",
				name, strings.Join(left, ", "))
			skipped++
			continue
		}
		cleared[name] = true
		names = append(names, name)
	}

	failed := 0
	for _, name := range names {
		if dryRun {
			fmt.Printf("would clean up %q: %s\n", name, endedWhy(ended[name]))
			fmt.Printf("  %s; %s\n", sessionFate(t.agents[name]), heldWords(ended[name].Held))
		} else if !cleanupOne(ctx, t, name) {
			failed++
		}
		if left := helpersLeft(t, name, cleared); len(left) > 0 {
			fmt.Printf("  note: its helpers %s haven't ended, so they stay (agent cleanup %s)\n",
				strings.Join(left, ", "), strings.Join(left, " "))
		}
	}

	var total string
	switch {
	case len(names) == 0 && caller == "":
		total = "No ended agents to clean up here."
	case len(names) == 0:
		total = "None of your helpers has ended to clean up."
	case dryRun:
		total = fmt.Sprintf("Would clean up %s; nothing changed (dry run).", plural(len(names), "ended agent"))
	case failed > 0:
		total = fmt.Sprintf("Cleaned up %d of %s.", len(names)-failed, plural(len(names), "ended agent"))
	default:
		total = fmt.Sprintf("Cleaned up %s.", plural(len(names), "ended agent"))
	}
	if skipped > 0 {
		total += fmt.Sprintf(" Skipped %d.", skipped)
	}
	if others > 0 {
		total += fmt.Sprintf(" Left %s here that others spawned, not you (%q) or your helpers.",
			plural(others, "more ended agent"), caller)
	}
	fmt.Println(total)

	if failed > 0 {
		return fmt.Errorf("%d of %d cleanups failed", failed, len(names))
	}
	return nil
}

// helpersLeft returns name's listed helpers that aren't in cleared.
func helpersLeft(t *teams, name string, cleared map[string]bool) []string {
	var left []string
	for _, h := range t.helpers(name) {
		if !cleared[h] {
			left = append(left, h)
		}
	}
	return left
}

// endedWhy says how an ended agent ended: "ended (quit) 8h34m ago:
// prompt_input_exit".
func endedWhy(info *service.StatusInfo) string {
	why := statusWords(info)
	if !info.UpdatedAt.IsZero() {
		why += fmt.Sprintf(" %s ago", time.Since(info.UpdatedAt).Round(time.Second))
	}
	if info.Detail != "" {
		why += ": " + strings.Join(strings.Fields(info.Detail), " ")
	}
	return why
}

// sessionFate says what cleanup would do to agent's tmux session.
func sessionFate(agent *storage.Agent) string {
	exists, err := tmuxClient.SessionExists(agent.SessionName)
	switch {
	case err != nil:
		return fmt.Sprintf("its session %s couldn't be checked: %v", agent.SessionName, err)
	case !exists:
		return fmt.Sprintf("its session %s is already gone", agent.SessionName)
	case agent.Adopted:
		return fmt.Sprintf("adopted: its session %s stays running, and only the record goes", agent.SessionName)
	default:
		return fmt.Sprintf("kills its session %s", agent.SessionName)
	}
}

// heldWords says how many held messages cleanup would delete.
func heldWords(n int) string {
	if n == 0 {
		return "no held messages"
	}
	return fmt.Sprintf("deletes %s held for it", plural(n, "message"))
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

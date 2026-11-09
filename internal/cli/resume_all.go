package cli

import (
	"context"
	"fmt"
	"slices"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// How resume --all runs resumes side by side: at most resumeAllParallel
// at once, each launch at least resumeAllSpacing after the one before.
// Each resume spends most of its time waiting for its CLI to start
// (startup_delay), so a few at once save most of that wait, while spacing
// the launches keeps CLIs from starting in the same instant (a CLI writes
// its own shared config, such as Claude Code's ~/.claude.json, as it
// starts). Variables for tests.
var (
	resumeAllParallel = 4
	resumeAllSpacing  = 500 * time.Millisecond
)

// resumeOne resumes one agent, as agent resume does; a variable for tests.
var resumeOne = resumeAgent

// resumeEntry is one ended agent resume --all looked at.
type resumeEntry struct {
	name  string
	depth int
	// skip says why it is left as it is; "" to resume it.
	skip string
	line string // what was done, once done
	fail bool
	done chan struct{}
}

// runResumeAll is resume --all: it resumes every agent here (inside an
// agent, every helper of the caller's, and theirs) that a reboot or a
// killed tmux server ended, parents before their helpers, and lists the
// ended agents it leaves and why. dryRun changes nothing.
func runResumeAll(ctx context.Context, dryRun bool, caller string, skipPermissions, trustWorkspace bool) error {
	list, err := agentService.List(ctx)
	if err != nil {
		return err
	}
	t := newTeams(list)

	// Every status first, so each is judged as things stood before any
	// resume.
	var entries []*resumeEntry
	others, running := 0, 0
	for _, a := range list {
		if a.Name == caller {
			continue
		}
		if caller != "" && !slices.Contains(t.ancestors(a.Name), caller) {
			others++
			continue
		}
		e := &resumeEntry{name: a.Name, depth: t.depth(a.Name), done: make(chan struct{})}
		info, err := agentService.StatusInfo(ctx, a.Name)
		switch {
		case err != nil:
			e.skip = fmt.Sprintf("its status couldn't be read: %v", err)
		case info.Status != service.AgentStatusEnded:
			running++
			continue
		case info.EndReason != backend.EndGone:
			e.skip = fmt.Sprintf("%s, on purpose, not by a reboot: agent resume %s resumes it", statusWords(info), a.Name)
		default:
			if refusal := agentService.ResumeRefusal(a, info.Status, agentService.LastSessionID(ctx, a.Name)); refusal != nil {
				e.skip = refusal.Error()
			}
		}
		entries = append(entries, e)
	}
	// Parents before their helpers, so a helper comes back to a running
	// parent. Then by name: the order lines print in.
	sort.Slice(entries, func(i, j int) bool {
		if entries[i].depth != entries[j].depth {
			return entries[i].depth < entries[j].depth
		}
		return entries[i].name < entries[j].name
	})

	var picked []*resumeEntry
	for _, e := range entries {
		if e.skip != "" {
			e.line = fmt.Sprintf("skipped %s: %s", e.name, e.skip)
			close(e.done)
			continue
		}
		picked = append(picked, e)
	}
	go runResumeWaves(ctx, picked, dryRun, caller, skipPermissions, trustWorkspace)

	failed := 0
	for _, e := range entries {
		<-e.done
		fmt.Println(e.line)
		if e.fail {
			failed++
		}
	}
	fmt.Println(resumeAllTotal(len(picked), len(entries)-len(picked), failed, others, running, dryRun, caller))
	if failed > 0 && !dryRun {
		return fmt.Errorf("%d of %d resumes failed", failed, len(picked))
	}
	return nil
}

// runResumeWaves resumes picked (sorted by depth), one depth at a time,
// a few at once within it, closing each entry's done once it has its line.
func runResumeWaves(ctx context.Context, picked []*resumeEntry, dryRun bool, caller string, skipPermissions, trustWorkspace bool) {
	parallel := max(resumeAllParallel, 1)
	for start := 0; start < len(picked); {
		end := start
		for end < len(picked) && picked[end].depth == picked[start].depth {
			end++
		}
		sem := make(chan struct{}, parallel)
		var wg sync.WaitGroup
		var next time.Time
		for _, e := range picked[start:end] {
			sem <- struct{}{}
			if wait := time.Until(next); wait > 0 && !dryRun {
				time.Sleep(wait)
			}
			next = time.Now().Add(resumeAllSpacing)
			wg.Add(1)
			go func() {
				defer func() { <-sem; wg.Done(); close(e.done) }()
				if dryRun {
					e.line, e.fail = dryResumeLine(ctx, e.name)
					return
				}
				resp, err := resumeOne(ctx, e.name, caller, skipPermissions, trustWorkspace)
				if err != nil {
					e.line, e.fail = fmt.Sprintf("failed %s: %v", e.name, err), true
					return
				}
				e.line = resumedLine(e.name, resp)
			}()
		}
		wg.Wait() // a depth's resumes finish before their helpers start
		start = end
	}
}

// dryResumeLine says what resume --all would do with name, checking what
// resume checks before it starts anything. fail is true if it would fail.
func dryResumeLine(ctx context.Context, name string) (line string, fail bool) {
	agent, err := agentService.Agent(ctx, name)
	if err != nil {
		return fmt.Sprintf("would fail %s: %v", name, err), true
	}
	s, err := resumeSession(ctx, agent)
	if err != nil {
		return fmt.Sprintf("would fail %s: %v", name, err), true
	}
	return fmt.Sprintf("would resume %s (%s) on session %s, in %s", name, agent.Backend, s.SessionID, s.Cwd), false
}

// resumedLine is resume --all's line for an agent it resumed.
func resumedLine(name string, resp *natsbus.ResumeResponse) string {
	line := fmt.Sprintf("resumed %s (%s) on session %s, in %s", name, resp.Backend, resp.SessionID, resp.Cwd)
	if resp.Status != "" {
		status := resp.Status
		if resp.EndReason != "" {
			status += " (" + resp.EndReason + ")"
		}
		line += ": " + status
	}
	if resp.Held > 0 {
		line += fmt.Sprintf("; %s held for it go in at its prompt", plural(resp.Held, "message"))
	}
	return line
}

// resumeAllTotal is resume --all's last line.
func resumeAllTotal(picked, skipped, failed, others, running int, dryRun bool, caller string) string {
	var parts []string
	switch {
	case picked == 0 && caller == "":
		parts = append(parts, "No agents here were ended by a reboot or a killed tmux server.")
	case picked == 0:
		parts = append(parts, "None of your helpers was ended by a reboot or a killed tmux server.")
	case dryRun && failed > 0:
		parts = append(parts, fmt.Sprintf("Would resume %d of %s; %d would fail; nothing changed (dry run).", picked-failed, plural(picked, "agent"), failed))
	case dryRun:
		parts = append(parts, fmt.Sprintf("Would resume %s; nothing changed (dry run).", plural(picked, "agent")))
	case failed > 0:
		parts = append(parts, fmt.Sprintf("Resumed %d of %s; %d failed.", picked-failed, plural(picked, "agent"), failed))
	default:
		parts = append(parts, fmt.Sprintf("Resumed %s.", plural(picked, "agent")))
	}
	if skipped > 0 {
		parts = append(parts, fmt.Sprintf("Skipped %d.", skipped))
	}
	if running > 0 {
		parts = append(parts, fmt.Sprintf("%s still running.", plural(running, "agent")))
	}
	if others > 0 {
		parts = append(parts, fmt.Sprintf("Left %s here that others spawned, not you (%q) or your helpers.", plural(others, "more agent"), caller))
	}
	return strings.Join(parts, " ")
}

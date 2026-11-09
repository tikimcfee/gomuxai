package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// goneAgent records an agent whose tmux session is gone, as a reboot
// leaves it: its last report is report (idle if empty), with transcript
// (or, without one, session id sessionID) as its session.
func goneAgent(t *testing.T, name, parent, backendName, transcript, sessionID string, report service.StatusReport) {
	t.Helper()
	ctx := context.Background()
	if err := store.CreateAgent(ctx, &storage.Agent{Name: name, SessionName: "rpc-cli-" + name, Backend: backendName, SpawnedBy: parent, CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if report.Status == "" {
		report.Status = service.AgentStatusIdle
	}
	report.Transcript, report.SessionID = transcript, sessionID
	if err := agentService.Report(ctx, name, report); err != nil {
		t.Fatal(err)
	}
}

// After a reboot, resume --all brings back the agents it ended, parents
// first, and leaves the ones ended on purpose or that can't be resumed,
// saying why; --dry-run says the same and starts nothing.
func TestResumeAll(t *testing.T) {
	home, dir, _ := setupContinue(t, nil)
	var mu sync.Mutex
	var resumed []string
	backends.RegisterFactory("claude", func(c backend.ProviderConfig) backend.Provider {
		if c.Resume != "" {
			mu.Lock()
			resumed = append(resumed, c.Resume)
			mu.Unlock()
		}
		return fakeClaude{namedagents.NewClaudeBackendWithConfig(namedagents.ClaudeConfig{Resume: c.Resume})}
	})
	oldSpacing := resumeAllSpacing
	resumeAllSpacing = 0
	t.Cleanup(func() { resumeAllSpacing = oldSpacing })

	leadID, helperID, quitID := "11111111-0000-0000-0000-000000000001", "11111111-0000-0000-0000-000000000002", "11111111-0000-0000-0000-000000000003"
	goneAgent(t, "lead", "", "claude", claudeTranscript(t, home, leadID, dir, "", time.Hour), "", service.StatusReport{})
	goneAgent(t, "lead-h", "lead", "claude", claudeTranscript(t, home, helperID, dir, "", time.Hour), "", service.StatusReport{})
	goneAgent(t, "quitter", "", "claude", claudeTranscript(t, home, quitID, dir, "", time.Hour), "",
		service.StatusReport{Status: service.AgentStatusEnded, EndReason: backend.EndQuit})
	goneAgent(t, "aid", "", "aider", "", "", service.StatusReport{})
	goneAgent(t, "blank", "", "claude", "", "", service.StatusReport{})

	skips := []string{
		"skipped aid: agent aid runs aider: resume continues claude and agy sessions",
		"skipped blank: no session to resume agent blank on: its CLI never named one (agent logs blank --terminal shows what it did)",
		"skipped quitter: ended (quit), on purpose, not by a reboot: agent resume quitter resumes it",
	}
	out, err := stdout(t, func() error { return runResumeAll(context.Background(), true, "", false, false) })
	if err != nil {
		t.Fatal(err)
	}
	want := strings.Join([]string{skips[0], skips[1],
		"would resume lead (claude) on session " + leadID + ", in " + dir,
		skips[2],
		"would resume lead-h (claude) on session " + helperID + ", in " + dir,
		"Would resume 2 agents; nothing changed (dry run). Skipped 3. 1 agent still running.",
	}, "\n") + "\n"
	if out != want {
		t.Errorf("--dry-run printed:\n%s\nwant:\n%s", out, want)
	}
	for _, name := range []string{"lead", "lead-h"} {
		if exists, _ := tmuxClient.SessionExists("rpc-cli-" + name); exists {
			t.Errorf("--dry-run started %s", name)
		}
	}

	out, err = stdout(t, func() error { return runResumeAll(context.Background(), false, "", false, false) })
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSuffix(out, "\n"), "\n")
	if len(lines) != 6 || lines[0] != skips[0] || lines[1] != skips[1] || lines[3] != skips[2] ||
		!strings.HasPrefix(lines[2], "resumed lead (claude) on session "+leadID+", in "+dir) ||
		!strings.HasPrefix(lines[4], "resumed lead-h (claude) on session "+helperID+", in "+dir) ||
		lines[5] != "Resumed 2 agents. Skipped 3. 1 agent still running." {
		t.Errorf("resume --all printed:\n%s", out)
	}
	if !slices.Equal(resumed, []string{leadID, helperID}) {
		t.Errorf("resumed sessions %v, want lead's then its helper's", resumed)
	}
	for _, name := range []string{"lead", "lead-h"} {
		if exists, _ := tmuxClient.SessionExists("rpc-cli-" + name); !exists {
			t.Errorf("%s has no tmux session after resume --all", name)
		}
	}
	if exists, _ := tmuxClient.SessionExists("rpc-cli-quitter"); exists {
		t.Error("resume --all resumed quitter, which ended on purpose")
	}

	// Nothing left that a reboot ended.
	out, err = stdout(t, func() error { return runResumeAll(context.Background(), false, "", false, false) })
	if err != nil || !strings.HasSuffix(out, "No agents here were ended by a reboot or a killed tmux server. Skipped 3. 3 agents still running.\n") {
		t.Errorf("second resume --all: %v\n%s", err, out)
	}
}

// resume --all brings back one depth of the tree at a time, a few at once
// within it, prints its lines in a stable order whatever order they finish
// in, and fails if any resume did. Inside an agent it covers only that
// agent's helpers.
func TestResumeAll_OrderAndParallel(t *testing.T) {
	setupCLI(t)
	oldOne, oldParallel, oldSpacing := resumeOne, resumeAllParallel, resumeAllSpacing
	t.Cleanup(func() { resumeOne, resumeAllParallel, resumeAllSpacing = oldOne, oldParallel, oldSpacing })
	resumeAllParallel, resumeAllSpacing = 2, 0

	tree := map[string]string{"p1": "", "p2": "", "p3": "", "p4": "", "h1": "p1", "h2": "p1", "hh": "h1", "q-h": "p3"}
	for name, parent := range tree {
		goneAgent(t, name, parent, "claude", "", "sess-"+name, service.StatusReport{})
	}

	var mu sync.Mutex
	var events []string
	var callers []string
	active, peak := 0, 0
	resumeOne = func(ctx context.Context, name, caller string, skip, trust bool) (*natsbus.ResumeResponse, error) {
		mu.Lock()
		events = append(events, "start "+name)
		callers = append(callers, caller)
		active++
		peak = max(peak, active)
		mu.Unlock()
		// Stands in for the CLI's startup, so resumes overlap.
		time.Sleep(20 * time.Millisecond)
		mu.Lock()
		events = append(events, "end "+name)
		active--
		mu.Unlock()
		if name == "p3" {
			return nil, errors.New("boom")
		}
		return &natsbus.ResumeResponse{Name: name, Backend: "claude", SessionID: "sess-" + name, Cwd: "/w", Status: "idle"}, nil
	}

	out, err := stdout(t, func() error { return runResumeAll(context.Background(), false, "", false, false) })
	if err == nil || err.Error() != "1 of 8 resumes failed" {
		t.Errorf("err = %v, want 1 of 8 failed", err)
	}
	want := "resumed p1 (claude) on session sess-p1, in /w: idle\n" +
		"resumed p2 (claude) on session sess-p2, in /w: idle\n" +
		"failed p3: boom\n" +
		"resumed p4 (claude) on session sess-p4, in /w: idle\n" +
		"resumed h1 (claude) on session sess-h1, in /w: idle\n" +
		"resumed h2 (claude) on session sess-h2, in /w: idle\n" +
		"resumed q-h (claude) on session sess-q-h, in /w: idle\n" +
		"resumed hh (claude) on session sess-hh, in /w: idle\n" +
		"Resumed 7 of 8 agents; 1 failed. 1 agent still running.\n"
	if out != want {
		t.Errorf("printed:\n%s\nwant:\n%s", out, want)
	}
	if peak != 2 {
		t.Errorf("at most %d resumed at once, want 2 (the limit)", peak)
	}
	at := func(event string) int { return slices.Index(events, event) }
	for name, parent := range tree {
		if parent != "" && at("start "+name) < at("end "+parent) {
			t.Errorf("%s started before its parent %s was back: %v", name, parent, events)
		}
	}
	// Each depth finishes before the next starts.
	for _, p := range []string{"p1", "p2", "p3", "p4"} {
		for _, h := range []string{"h1", "h2", "q-h"} {
			if at("start "+h) < at("end "+p) {
				t.Errorf("%s started before depth 0's %s was done: %v", h, p, events)
			}
		}
	}

	// Inside agent p1: its helpers and theirs only, resumed as p1.
	// (The stand-in started nothing, so every one is still ended (gone).)
	events, callers = nil, nil
	out, err = stdout(t, func() error { return runResumeAll(context.Background(), false, "p1", false, false) })
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Count(out, "resumed "); got != 3 || !strings.Contains(out, "resumed hh ") ||
		!strings.HasSuffix(out, "Resumed 3 agents. Left 5 more agents here that others spawned, not you (\"p1\") or your helpers.\n") {
		t.Errorf("resume --all as p1:\n%s", out)
	}
	if slices.ContainsFunc(callers, func(c string) bool { return c != "p1" }) {
		t.Errorf("resumed as %v, want p1 each time", callers)
	}
}

func TestResumeArgs(t *testing.T) {
	t.Cleanup(func() { resumeAll, resumeDryRun, resumeAttach = false, false, false })
	for _, tc := range []struct {
		all, dry, attach bool
		args             []string
		want             string
	}{
		{all: true, args: []string{"lead"}, want: "--all picks the agents itself"},
		{all: true, args: []string{"lead@hub"}, want: "run agent resume --all on hub to resume its own, or agent resume lead@hub for that one"},
		{all: true, attach: true, want: "--attach opens one agent's session, and --all resumes several"},
		{dry: true, args: []string{"lead"}, want: "--dry-run goes with --all"},
		{args: nil, want: "accepts 1 arg"},
		{all: true, dry: true},
		{args: []string{"lead"}},
	} {
		resumeAll, resumeDryRun, resumeAttach = tc.all, tc.dry, tc.attach
		err := resumeArgs(resumeCmd, tc.args)
		if got := fmt.Sprint(err); (tc.want == "" && err != nil) || (tc.want != "" && !strings.Contains(got, tc.want)) {
			t.Errorf("all=%v dry=%v attach=%v %v: %v, want %q", tc.all, tc.dry, tc.attach, tc.args, err, tc.want)
		}
	}
}

// An agy conversation whose workspace agy hasn't recorded yet (its
// prompts' history lines carry no conversation id) resumes in the
// directory the agent's record says its session ran in.
func TestResumeSession_AgyWithoutWorkspace(t *testing.T) {
	home, dir, _ := setupContinue(t, nil)
	conv := "320ae9a3-0000-0000-0000-000000000001"
	path := filepath.Join(home, ".gemini", "antigravity-cli", "brain", conv, ".system_generated", "logs", "transcript_full.jsonl")
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	// No presence lock for it: agy doesn't hold it open.
	if err := os.MkdirAll(filepath.Join(home, ".gemini", "antigravity-cli", "presence"), 0o755); err != nil {
		t.Fatal(err)
	}
	writeTestFile(t, path, `{"type":"USER_INPUT","content":"hi"}`+"\n")
	writeTestFile(t, filepath.Join(home, ".gemini", "antigravity-cli", "history.jsonl"),
		`{"display":"hi","timestamp":1,"workspace":"`+dir+`"}`+"\n")
	ctx := context.Background()
	if err := store.CreateAgent(ctx, &storage.Agent{Name: "ag", SessionName: "rpc-cli-ag", Backend: "agy", Cwd: dir, CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if err := agentService.Report(ctx, "ag", service.StatusReport{Status: service.AgentStatusIdle, Transcript: path, SessionID: conv}); err != nil {
		t.Fatal(err)
	}
	agent, err := agentService.Agent(ctx, "ag")
	if err != nil {
		t.Fatal(err)
	}
	s, err := resumeSession(ctx, agent)
	if err != nil || s.SessionID != conv || s.Cwd != dir {
		t.Errorf("resumeSession = %s in %q, %v; want %s in %s", s.SessionID, s.Cwd, err, conv, dir)
	}

	agent.Cwd = ""
	if _, err := resumeSession(ctx, agent); err == nil || !strings.Contains(err.Error(), "recorded no working directory") {
		t.Errorf("with no cwd anywhere: %v, want the refusal", err)
	}
}

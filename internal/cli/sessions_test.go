package cli

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// setupSessions is setupCLI with a temp home holding two synthetic Claude
// sessions, one of them the worker agent's transcript, and claude agents
// answering for both.
func setupSessions(t *testing.T) (home string) {
	t.Helper()
	setupCLI(t)
	forbidBus(t)
	if err := os.MkdirAll(cfg.LogDir, 0o755); err != nil {
		t.Fatal(err)
	}
	home = t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("CLAUDE_CONFIG_DIR", "")
	projects := filepath.Join(home, ".claude", "projects", "-home-user-shop")
	if err := os.MkdirAll(projects, 0o755); err != nil {
		t.Fatal(err)
	}
	writeTestFile(t, filepath.Join(projects, "sess-l.jsonl"),
		`{"type":"user","sessionId":"sess-l","cwd":"/home/user/shop","message":{"role":"user","content":"hi"}}`+"\n"+
			`{"type":"ai-title","aiTitle":"A person's own session","sessionId":"sess-l"}`+"\n")
	worker := filepath.Join(projects, "sess-w.jsonl")
	writeTestFile(t, worker,
		`{"type":"user","sessionId":"sess-w","cwd":"/home/user/shop","message":{"role":"user","content":"go"}}`+"\n"+
			`{"type":"custom-title","customTitle":"Worker's task","sessionId":"sess-w"}`+"\n")
	old := time.Now().Add(-time.Hour)
	if err := os.Chtimes(filepath.Join(projects, "sess-l.jsonl"), old, old); err != nil {
		t.Fatal(err)
	}
	hookTranscript(t, worker)

	oldClaude := claudeSessions
	claudeSessions = func(context.Context) (map[string]types.Status, error) {
		return map[string]types.Status{"sess-w": types.StatusWaiting, "sess-l": types.StatusIdle}, nil
	}
	reset := func() { sessionsJSON, sessionsHarness, sessionsWorkgroup, sessionsLocal = false, "", false, false }
	reset()
	t.Cleanup(func() { claudeSessions = oldClaude; reset() })
	return home
}

func runSessionsJSON(t *testing.T) []discovery.Session {
	t.Helper()
	sessionsJSON = true
	defer func() { sessionsJSON = false }()
	out, err := stdout(t, func() error { return runSessions(sessionsCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	var got []discovery.Session
	if err := json.Unmarshal([]byte(out), &got); err != nil {
		t.Fatalf("decoding %q: %v", out, err)
	}
	return got
}

func TestSessions(t *testing.T) {
	setupSessions(t)
	got := runSessionsJSON(t)
	if len(got) != 2 {
		t.Fatalf("sessions = %+v, want the worker's and the person's", got)
	}
	w, l := got[0], got[1]
	if w.SessionID != "sess-w" || w.Workgroup == nil || w.Workgroup.Name != "worker" || w.Workgroup.Store != filepath.Dir(cfg.DatabasePath) ||
		w.Workgroup.Host != "local" || w.Status != "waiting" || w.Title != "Worker's task" || w.Project != "shop" {
		t.Errorf("worker's session = %+v %+v", w, w.Workgroup)
	}
	if l.SessionID != "sess-l" || l.Workgroup != nil || l.Status != "idle" || l.Title != "A person's own session" {
		t.Errorf("person's session = %+v", l)
	}

	sessionsLocal = true
	if got := runSessionsJSON(t); len(got) != 1 || got[0].SessionID != "sess-l" {
		t.Errorf("--local = %+v, want sess-l only", got)
	}
	sessionsLocal, sessionsWorkgroup = false, true
	if got := runSessionsJSON(t); len(got) != 1 || got[0].SessionID != "sess-w" {
		t.Errorf("--workgroup = %+v, want sess-w only", got)
	}
	sessionsWorkgroup, sessionsHarness = false, "agy"
	if got := runSessionsJSON(t); len(got) != 0 {
		t.Errorf("--harness agy = %+v, want none", got)
	}
}

func TestSessions_Human(t *testing.T) {
	setupSessions(t)
	out, err := stdout(t, func() error { return runSessions(sessionsCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		"Looked in: claude ~/.claude/projects (default)\n",
		"claude   shop     waiting  worker     Worker's task\n",
		"claude   shop     idle     -          A person's own session\n",
		"2 sessions, 1 in a workgroup.\n",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("output lacks %q:\n%s", want, out)
		}
	}
}

func TestSessions_Errors(t *testing.T) {
	setupSessions(t)
	sessionsWorkgroup, sessionsLocal = true, true
	if err := runSessions(sessionsCmd, nil); err == nil || !strings.Contains(err.Error(), "use one") {
		t.Errorf("--workgroup --local: err = %v", err)
	}
	sessionsWorkgroup, sessionsLocal, sessionsHarness = false, false, "codex"
	if err := runSessions(sessionsCmd, nil); err == nil || !strings.Contains(err.Error(), "claude, agy, kimi") {
		t.Errorf("--harness codex: err = %v", err)
	}
}

// A live agent whose backend keeps no transcript is listed under its
// backend's name, and --harness takes that name.
func TestSessions_BackendWithoutTranscript(t *testing.T) {
	setupSessions(t)
	if err := store.CreateAgent(context.Background(), &storage.Agent{Name: "aid", SessionName: "rpc-cli-aid", Backend: "aider", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	sessionsHarness = "aider"
	got := runSessionsJSON(t)
	if len(got) != 1 || got[0].Harness != "aider" || got[0].Path != "" || got[0].Workgroup == nil || got[0].Workgroup.Name != "aid" {
		t.Errorf("--harness aider = %+v, want aid with no transcript", got)
	}
}

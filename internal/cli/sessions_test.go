package cli

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
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
	reset := func() {
		sessionsJSON, sessionsHarness, sessionsWorkgroup, sessionsLocal, sessionsDirs = false, "", false, false, false
		sessionsLimit, sessionsAll, sessionsSince, sessionsProject = 0, false, "", ""
		sessionsCmd.Flags().Lookup("limit").Changed = false
	}
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

// --dirs prints where sessions looks: as the table's opening line, or each
// dir as JSON with whether it exists.
func TestSessions_Dirs(t *testing.T) {
	home := setupSessions(t)
	missing := filepath.Join(home, "no-such-kimi")
	cfg.KimiSessionsDir = &missing
	t.Cleanup(func() { cfg.KimiSessionsDir = nil })
	sessionsDirs = true
	out, err := stdout(t, func() error { return runSessions(sessionsCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(out, "Looked in: claude ~/.claude/projects (default)") || !strings.Contains(out, "(config, missing)") {
		t.Errorf("--dirs = %q", out)
	}
	sessionsJSON = true
	out, err = stdout(t, func() error { return runSessions(sessionsCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	var got []sessionDir
	if err := json.Unmarshal([]byte(out), &got); err != nil {
		t.Fatalf("decoding %q: %v", out, err)
	}
	want := []sessionDir{
		{Dir: discovery.Dir{Harness: "claude", Path: filepath.Join(home, ".claude", "projects"), Origin: "default"}, Exists: true},
		{Dir: discovery.Dir{Harness: "kimi", Path: missing, Origin: "config"}, Exists: false},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("--dirs --json = %+v, want %+v", got, want)
	}
}

// setLimit sets --limit as the command line would (it is told from unset).
func setLimit(t *testing.T, n int) {
	t.Helper()
	if err := sessionsCmd.Flags().Set("limit", strconv.Itoa(n)); err != nil {
		t.Fatal(err)
	}
}

// captureHints collects the hints a command prints.
func captureHints(t *testing.T) *strings.Builder {
	t.Helper()
	var hints strings.Builder
	old := hintOut
	t.Cleanup(func() { hintOut, hinted = old, false })
	hintOut, hinted = &hints, false
	return &hints
}

// manySessions adds n Claude sessions run in cwd to setupSessions' home,
// sess-000 the newest, all older than sess-w and sess-l.
func manySessions(t *testing.T, home, cwd string, n int) {
	t.Helper()
	for i := range n {
		claudeTranscript(t, home, fmt.Sprintf("sess-%03d", i), cwd, "", 2*time.Hour+time.Duration(i)*time.Minute)
	}
}

// The table shows the newest 25 and a hint says how many are left out;
// --all and --limit change that; --json prints every one unless --limit
// says otherwise.
func TestSessions_DefaultLimit(t *testing.T) {
	home := setupSessions(t)
	manySessions(t, home, "/home/user/shop", 30)

	hints := captureHints(t)
	out, err := stdout(t, func() error { return runSessions(sessionsCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "25 sessions, 1 in a workgroup.") || strings.Contains(out, "sess-023") || !strings.Contains(out, "(sess-022)") {
		t.Errorf("default table, want sess-w, sess-l and sess-000..022:\n%s", out)
	}
	if want := "hint: showing the newest 25 sessions and their subagents; 7 older left out: --all lists every one, --limit N the newest N\n"; hints.String() != want {
		t.Errorf("hint = %q, want %q", hints.String(), want)
	}

	hints.Reset()
	hinted = false
	sessionsAll = true
	out, err = stdout(t, func() error { return runSessions(sessionsCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "32 sessions, 1 in a workgroup.") || hints.Len() != 0 {
		t.Errorf("--all: hints %q, table:\n%s", hints.String(), out)
	}
	sessionsAll = false

	if got := runSessionsJSON(t); len(got) != 32 || hints.Len() != 0 {
		t.Errorf("--json printed %d sessions, hints %q; want all 32 and no hint", len(got), hints.String())
	}
	setLimit(t, 3)
	if got := runSessionsJSON(t); len(got) != 3 || got[0].SessionID != "sess-w" || got[2].SessionID != "sess-000" {
		t.Errorf("--json --limit 3 = %v", sessionIDs(got))
	}
	if !strings.Contains(hints.String(), "29 older left out") {
		t.Errorf("--json --limit 3: hint %q", hints.String())
	}
}

func sessionIDs(sessions []discovery.Session) []string {
	var out []string
	for _, s := range sessions {
		out = append(out, s.SessionID)
	}
	return out
}

// --since, --project and the limit combine with each other and with the
// filters there were, a subagent going with its session.
func TestSessions_Filters(t *testing.T) {
	home := setupSessions(t)
	notes := claudeTranscript(t, home, "notes", "/home/user/notes", "Notes", 3*time.Hour)
	claudeSubagent(t, notes, "/home/user/notes", 3*time.Hour+time.Minute)
	claudeTranscript(t, home, "deep", "/home/user/shop/web", "", 4*time.Hour)

	for _, tc := range []struct {
		name string
		set  func()
		want []string
	}{
		{"since 2h", func() { sessionsSince = "2h" }, []string{"sess-w", "sess-l"}},
		{"since 30m", func() { sessionsSince = "30m" }, []string{"sess-w"}},
		{"since a date", func() { sessionsSince = time.Now().Add(-200 * time.Minute).Format("2006-01-02T15:04:05") }, []string{"sess-w", "sess-l", "notes", "agent-x1"}},
		{"project name", func() { sessionsProject = "NOT" }, []string{"notes", "agent-x1"}},
		{"project dir", func() { sessionsProject = "/home/user/shop" }, []string{"sess-w", "sess-l", "deep"}},
		{"project dir, local", func() { sessionsProject, sessionsLocal = "/home/user/shop", true }, []string{"sess-l", "deep"}},
		{"project, since", func() { sessionsProject, sessionsSince = "/home/user/shop/", "2h" }, []string{"sess-w", "sess-l"}},
		{"limit with a subagent", func() { sessionsSince = "1d"; setLimit(t, 3) }, []string{"sess-w", "sess-l", "notes", "agent-x1"}},
		{"workgroup, project", func() { sessionsWorkgroup, sessionsProject = true, "notes" }, nil},
	} {
		sessionsLimit, sessionsSince, sessionsProject, sessionsLocal, sessionsWorkgroup = 0, "", "", false, false
		sessionsCmd.Flags().Lookup("limit").Changed = false
		tc.set()
		if got := sessionIDs(runSessionsJSON(t)); !slices.Equal(got, tc.want) {
			t.Errorf("%s: %v, want %v", tc.name, got, tc.want)
		}
	}
}

// A --project with a slash, or . or ~, is a directory, made absolute.
func TestSessions_ProjectDir(t *testing.T) {
	home := setupSessions(t)
	wd, _ := os.Getwd()
	for arg, want := range map[string]string{
		".":            wd,
		"~":            home,
		"~/src/app":    filepath.Join(home, "src", "app"),
		"sub/dir":      filepath.Join(wd, "sub", "dir"),
		"/abs/path/":   "/abs/path",
		"gomuxai":      "gomuxai",
		"My Project 2": "My Project 2",
	} {
		sessionsProject = arg
		q, err := sessionsQuery(sessionsCmd, time.Now())
		if err != nil || q.Project != want {
			t.Errorf("--project %s = %q, %v; want %q", arg, q.Project, err, want)
		}
	}
}

func TestSessions_LimitErrors(t *testing.T) {
	setupSessions(t)
	sessionsAll = true
	setLimit(t, 5)
	if err := runSessions(sessionsCmd, nil); err == nil || !strings.Contains(err.Error(), "--all and --limit don't go together") {
		t.Errorf("--all --limit: err = %v", err)
	}
	sessionsAll = false
	setLimit(t, 0)
	if err := runSessions(sessionsCmd, nil); err == nil || !strings.Contains(err.Error(), "give 1 or more") {
		t.Errorf("--limit 0: err = %v", err)
	}
	sessionsCmd.Flags().Lookup("limit").Changed = false
	sessionsSince = "yesterday"
	if err := runSessions(sessionsCmd, nil); err == nil || !strings.Contains(err.Error(), "--since yesterday: give a duration") {
		t.Errorf("--since yesterday: err = %v", err)
	}
}

func TestParseSince(t *testing.T) {
	now := time.Date(2026, 11, 8, 15, 0, 0, 0, time.Local)
	for in, want := range map[string]time.Time{
		"90m":                  now.Add(-90 * time.Minute),
		"2h":                   now.Add(-2 * time.Hour),
		"1h30m":                now.Add(-90 * time.Minute),
		"3d":                   now.AddDate(0, 0, -3),
		"1w":                   now.AddDate(0, 0, -7),
		"2026-11-01":           time.Date(2026, 11, 1, 0, 0, 0, 0, time.Local),
		"2026-11-01 13:30":     time.Date(2026, 11, 1, 13, 30, 0, 0, time.Local),
		"2026-11-01T13:30:05":  time.Date(2026, 11, 1, 13, 30, 5, 0, time.Local),
		"2026-11-01T13:30:00Z": time.Date(2026, 11, 1, 13, 30, 0, 0, time.UTC),
	} {
		if got, err := parseSince(in, now); err != nil || !got.Equal(want) {
			t.Errorf("parseSince(%q) = %v, %v; want %v", in, got, err, want)
		}
	}
	for _, in := range []string{"", "d", "3x", "-2h", "3dw", "11/01/2026"} {
		if _, err := parseSince(in, now); err == nil {
			t.Errorf("parseSince(%q) took it", in)
		}
	}
}

// A subagent's session found by id is read with the sessions above it,
// whose status its own follows.
func TestResolveTranscript_SubagentByID(t *testing.T) {
	home := setupSessions(t)
	parent := claudeTranscript(t, home, "sess-p", "/home/user/shop", "", time.Minute)
	claudeSubagent(t, parent, "/home/user/shop", time.Minute)
	manySessions(t, home, "/home/user/other", 5)

	target, err := resolveTranscript(context.Background(), "agent-x")
	if err != nil {
		t.Fatal(err)
	}
	if target.session.SessionID != "agent-x1" || len(target.above) != 1 || !samePath(target.above[0], parent) ||
		target.session.Status != "ended" || target.session.EndReason != "parent" {
		t.Errorf("agent-x: %+v, above %v; want agent-x1 under sess-p, ended (parent)", target.session, target.above)
	}
}

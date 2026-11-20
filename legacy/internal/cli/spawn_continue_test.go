package cli

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
)

// setupContinue is setupCLI with a temp home and two project dirs, a and
// b, for synthetic Claude sessions, and claude agents answering with
// running (nil: none run).
func setupContinue(t *testing.T, running func() map[string]types.Status) (home, a, b string) {
	t.Helper()
	setupCLI(t)
	forbidBus(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("TMUX_PANE", "")
	home = t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("CLAUDE_CONFIG_DIR", "")
	root := t.TempDir()
	a, b = filepath.Join(root, "a"), filepath.Join(root, "b")
	for _, d := range []string{a, b} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	old := claudeSessions
	claudeSessions = func(context.Context) (map[string]types.Status, error) {
		if running == nil {
			return map[string]types.Status{}, nil
		}
		return running(), nil
	}
	t.Cleanup(func() { claudeSessions = old })
	return home, a, b
}

// claudeTranscript writes Claude session id, run in cwd, last written
// ago before now (and a subagent of it if sub isn't empty).
func claudeTranscript(t *testing.T, home, id, cwd, title string, ago time.Duration) string {
	t.Helper()
	dir := filepath.Join(home, ".claude", "projects", strings.ReplaceAll(cwd, "/", "-"))
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, id+".jsonl")
	body := `{"type":"user","sessionId":"` + id + `","cwd":"` + cwd + `","message":{"role":"user","content":"hi"}}` + "\n"
	if title != "" {
		body += `{"type":"custom-title","customTitle":"` + title + `","sessionId":"` + id + `"}` + "\n"
	}
	writeTestFile(t, path, body)
	at := time.Now().Add(-ago)
	if err := os.Chtimes(path, at, at); err != nil {
		t.Fatal(err)
	}
	return path
}

func claudeSubagent(t *testing.T, parentPath, cwd string, ago time.Duration) {
	t.Helper()
	dir := filepath.Join(strings.TrimSuffix(parentPath, ".jsonl"), "subagents")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "agent-x1.jsonl")
	writeTestFile(t, path, `{"type":"user","isSidechain":true,"sessionId":"x1","cwd":"`+cwd+`","message":{"role":"user","content":"sub"}}`+"\n")
	at := time.Now().Add(-ago)
	if err := os.Chtimes(path, at, at); err != nil {
		t.Fatal(err)
	}
}

func TestSpawnContinue_PicksTheMostRecentHere(t *testing.T) {
	home, a, b := setupContinue(t, nil)
	claudeTranscript(t, home, "a-old", a, "", 3*time.Hour)
	newest := claudeTranscript(t, home, "a-new", a, "Fix the parser", 2*time.Hour)
	claudeSubagent(t, newest, a, time.Minute) // newer, but a subagent's
	claudeTranscript(t, home, "b-only", b, "", time.Hour)

	s, err := continueSession(context.Background(), a, "claude")
	if err != nil || s.SessionID != "a-new" {
		t.Fatalf("continueSession(a) = %q, %v; want a-new", s.SessionID, err)
	}
	choice := continueChoice(s, a)
	for _, want := range []string{`Continuing claude session a-new ("Fix the parser"), last active `, " (2h ago)", ": the most recent in " + a + "."} {
		if !strings.Contains(choice, want) {
			t.Errorf("choice %q lacks %q", choice, want)
		}
	}
	if s, err := continueSession(context.Background(), b, "claude"); err != nil || s.SessionID != "b-only" {
		t.Errorf("continueSession(b) = %q, %v; want b-only", s.SessionID, err)
	}
	empty := t.TempDir()
	if _, err := continueSession(context.Background(), empty, "claude"); err == nil || !strings.Contains(err.Error(), "no claude session to continue in "+empty) {
		t.Errorf("an empty dir: err = %v", err)
	}
}

// A session still running is refused as --resume refuses it, before
// anything is spawned.
func TestSpawnContinue_RunningIsRefused(t *testing.T) {
	home, a, _ := setupContinue(t, func() map[string]types.Status { return map[string]types.Status{"a-new": types.StatusIdle} })
	claudeTranscript(t, home, "a-old", a, "", 3*time.Hour)
	claudeTranscript(t, home, "a-new", a, "", time.Hour)
	cfg.Backend = "claude"
	spawnContinue = true
	t.Chdir(a)
	err := runSpawn(spawnCmd, []string{"t1"})
	if err == nil || !strings.Contains(err.Error(), "--continue picks a-new") || !strings.Contains(err.Error(), "is idle, not ended: quit the CLI running it first") {
		t.Errorf("err = %v; want a-new refused as running", err)
	}
	if ag, _ := store.GetAgent(context.Background(), "t1"); ag != nil {
		t.Errorf("spawned %+v after a refusal", ag)
	}
}

func TestSpawnContinue_FlagErrors(t *testing.T) {
	setupSpawnCLI(t)
	forbidBus(t)
	spawnContinue, spawnResume = true, "3f34adff"
	if err := runSpawn(spawnCmd, []string{"t1"}); err == nil || !strings.Contains(err.Error(), "--continue and --resume don't go together") {
		t.Errorf("--continue --resume: err = %v", err)
	}
	spawnResume = ""
	for _, b := range []string{"catty", "shell", "aider"} {
		cfg.Backend = b
		if err := runSpawn(spawnCmd, []string{"t1"}); err == nil || !strings.Contains(err.Error(), "backend "+b+" keeps no sessions to continue") {
			t.Errorf("--continue --backend %s: err = %v", b, err)
		}
	}
}

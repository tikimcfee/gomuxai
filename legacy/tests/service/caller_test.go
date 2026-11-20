package service_test

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/claudereg"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// registerCLI writes a Claude Code registry entry for the running process
// pid, with the start time Claude Code records, so it proves live.
func registerCLI(t *testing.T, dir string, pid int, sessionID, kind string) {
	t.Helper()
	stat, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		t.Fatal(err)
	}
	start, ok := claudereg.StatStartTime(string(stat))
	if !ok {
		t.Fatalf("no start time in %q", stat)
	}
	entry := fmt.Sprintf(`{"pid":%d,"sessionId":%q,"cwd":"/w","procStart":%q,"kind":%q,"status":"busy","updatedAt":%d}`,
		pid, sessionID, start, kind, time.Now().UnixMilli())
	if err := os.WriteFile(filepath.Join(dir, "sessions", strconv.Itoa(pid)+".json"), []byte(entry), 0o600); err != nil {
		t.Fatal(err)
	}
}

// panePID returns the pid of the process a tmux session's pane runs.
func panePID(t *testing.T, env *testEnv, session string) int {
	t.Helper()
	panes, err := env.tmux.PanePIDs()
	if err != nil {
		t.Fatal(err)
	}
	for pid, s := range panes {
		if s == session {
			return pid
		}
	}
	t.Fatalf("no pane for %s", session)
	return 0
}

// What friction #48-#50 hit on workstation: Claude Code 2.1.325 sent agent
// A's conversation to the background (/bg) into a spare host its daemon had
// started for agent B, so the session ran with B's environment (AGENT_NAME)
// and outside A's pane, under the daemon or B's CLI. The hook placed
// nothing on A, so A's long messages lost their text and its sends went
// out as B's. Its lineage says whose it is: A's transcript continued in it.
func TestBackgroundSessionBelongsToItsConversation(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("proves registry entries by /proc start times")
	}
	env := setupTestEnv(t)
	ctx := context.Background()
	cfgDir := t.TempDir()
	t.Setenv("CLAUDE_CONFIG_DIR", cfgDir)
	for _, d := range []string{"sessions", "projects/-w"} {
		if err := os.MkdirAll(filepath.Join(cfgDir, d), 0o700); err != nil {
			t.Fatal(err)
		}
	}

	// Agents a and b, each a pane whose process stands in for its CLI.
	for _, name := range []string{"a", "b"} {
		session := env.cfg.SessionName(name)
		if err := env.tmux.CreateSession(session, nil); err != nil {
			t.Fatal(err)
		}
		if err := env.store.CreateAgent(ctx, &storage.Agent{Name: name, SessionName: session, Backend: "claude", CreatedAt: time.Now()}); err != nil {
			t.Fatal(err)
		}
	}
	paneA, paneB := panePID(t, env, env.cfg.SessionName("a")), panePID(t, env, env.cfg.SessionName("b"))
	registerCLI(t, cfgDir, paneA, "a-old", claudereg.KindInteractive)
	registerCLI(t, cfgDir, paneB, "b-sess", claudereg.KindInteractive)
	if err := env.store.SetState(ctx, &storage.StateEntry{AgentName: "b", Key: "_session_id", Value: "b-sess", UpdatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}

	// The background session: a process outside a's pane (here, this
	// test's child) holding the moved session, and a hook it runs.
	host := exec.Command("sh", "-c", "sleep 60 & wait")
	if err := host.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = host.Process.Kill(); _ = host.Wait() })
	bg := host.Process.Pid
	hook := 0
	await.Until(t, "the background session's hook process", func() (string, bool) {
		data, err := os.ReadFile(fmt.Sprintf("/proc/%d/task/%d/children", bg, bg))
		hook, _ = strconv.Atoi(strings.TrimSpace(string(data)))
		return fmt.Sprint(string(data), err), hook != 0
	})
	t.Cleanup(func() { _ = syscall.Kill(hook, syscall.SIGKILL) })
	registerCLI(t, cfgDir, bg, "a-new", claudereg.KindBackground)
	// A tool the background session runs, under it, then the daemon's
	// tree up to b's pane.
	tool := 1 << 30
	chain := []int{tool, bg, paneB}

	// Not moved yet: the background session is nobody's, and it is never b's.
	if name, known, err := env.service.CallerAgent(ctx, chain); err != nil || !known || name != "" {
		t.Fatalf("before the move: %q %v %v, want a session no agent owns", name, known, err)
	}
	if hs, err := env.service.FindHookSession(ctx, hook, "a-new", ""); err != nil || hs.CLIPID != bg || hs.Owner != "" {
		t.Fatalf("hook before the move: %+v %v, want CLI %d, no owner", hs, err, bg)
	}

	// a's transcript records the move. (a's pane CLI may be gone: /bg
	// frees the terminal.)
	line := `{"type":"continued-in","sessionId":"a-old","continuedInSessionId":"a-new"}`
	old := filepath.Join(cfgDir, "projects", "-w", "a-old.jsonl")
	if err := os.WriteFile(old, []byte(line+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := env.store.SetState(ctx, &storage.StateEntry{AgentName: "a", Key: "_transcript", Value: old, UpdatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if name, known, err := env.service.CallerAgent(ctx, chain); err != nil || !known || name != "a" {
		t.Errorf("a's moved session: %q %v %v, want a", name, known, err)
	}
	// The hook it runs is placed on a too.
	if hs, err := env.service.FindHookSession(ctx, hook, "a-new", ""); err != nil || hs.Owner != "a" {
		t.Errorf("hook: %+v %v, want owner a", hs, err)
	}

	// b's own CLI, and a process under b's pane, are b's.
	if name, known, _ := env.service.CallerAgent(ctx, []int{tool, paneB}); !known || name != "b" {
		t.Errorf("under b's pane: %q %v, want b", name, known)
	}
	// Outside every pane, the processes don't say.
	if name, known, _ := env.service.CallerAgent(ctx, []int{tool, 1}); known || name != "" {
		t.Errorf("outside the panes: %q %v", name, known)
	}
	// Once a's record names the new session, it is found without the walk.
	if err := os.Remove(filepath.Join(cfgDir, "projects", "-w", "a-old.jsonl")); err != nil {
		t.Fatal(err)
	}
	if err := env.store.SetState(ctx, &storage.StateEntry{AgentName: "a", Key: "_session_id", Value: "a-new", UpdatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if name, _, _ := env.service.CallerAgent(ctx, chain); name != "a" {
		t.Errorf("a on its new session: %q", name)
	}
}

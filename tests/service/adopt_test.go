package service_test

import (
	"context"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/service"
)

func TestAdopt_ExistingShellAndCleanupLeavesSessionRunning(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()

	userSession := env.cfg.SessionPrefix + "user-dev"
	if err := env.tmux.CreateSession(userSession, nil); err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	defer env.tmux.KillSession(userSession)

	res, err := env.service.Adopt(ctx, service.AdoptOptions{
		Name:        "dev-box",
		SessionName: userSession,
	})
	if err != nil {
		t.Fatalf("Adopt: %v", err)
	}
	if !res.Agent.Adopted || res.Agent.SessionName != userSession || res.Agent.Backend != "shell" {
		t.Errorf("Adopt agent = %+v, want Adopted=true SessionName=%s Backend=shell", res.Agent, userSession)
	}

	// Reject adopting the same tmux session under a second agent name.
	if _, err := env.service.Adopt(ctx, service.AdoptOptions{
		Name:        "dev-box-2",
		SessionName: userSession,
	}); err == nil || !strings.Contains(err.Error(), "already registered as agent dev-box") {
		t.Errorf("duplicate session Adopt err = %v, want already registered", err)
	}

	// Cleanup unregisters the agent without killing the user's pre-existing tmux session.
	if err := env.service.Cleanup(ctx, "dev-box"); err != nil {
		t.Fatalf("Cleanup: %v", err)
	}
	exists, err := env.tmux.SessionExists(userSession)
	if err != nil || !exists {
		t.Errorf("after Cleanup of adopted agent, SessionExists(%s) = %v, %v; want true", userSession, exists, err)
	}
}

func TestAdopt_LiveClaudeInExistingSession(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()

	userSession := env.cfg.SessionPrefix + "claude-pane"
	if err := env.tmux.CreateSession(userSession, nil); err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	defer env.tmux.KillSession(userSession)

	fake := &fakeHookBackend{
		ClaudeBackend: namedagents.NewClaudeBackend(),
		sessionID:     "claude-sess-77",
		nativeFound:   true,
		native:        backend.StatusIdle,
		paneSession: &backend.PaneSession{
			ID:     "claude-sess-77",
			Status: backend.StatusIdle,
			Detail: "idle",
		},
	}
	env.backends.RegisterFactory("claude", func(backend.ProviderConfig) backend.Provider { return fake })

	res, err := env.service.Adopt(ctx, service.AdoptOptions{
		Name:        "adopted-claude",
		SessionName: userSession,
	})
	if err != nil {
		t.Fatalf("Adopt: %v", err)
	}
	if res.Agent.Backend != "claude" || !res.Agent.Adopted || res.SessionID != "claude-sess-77" {
		t.Errorf("Adopt = %+v (agent %+v), want claude session claude-sess-77", res, res.Agent)
	}
	if got := stateValue(t, env, "adopted-claude", service.StateKeySessionID); got != "claude-sess-77" {
		t.Errorf("_session_id = %q, want claude-sess-77", got)
	}
	info, err := env.service.StatusInfo(ctx, "adopted-claude")
	if err != nil || info.Status != service.AgentStatusIdle {
		t.Errorf("StatusInfo = %+v, %v; want idle", info, err)
	}
}

// An adopted agent runs in the person's own tmux session: resume refuses
// it, typing nothing into their shell and leaving the session as it was.
func TestAdopt_ResumeRefusesAdoptedAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()

	userSession := env.cfg.SessionPrefix + "keep-me"
	if err := env.tmux.CreateSession(userSession, nil); err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	defer env.tmux.KillSession(userSession)
	if err := env.tmux.SendKeys(userSession, "echo person-was-here"); err != nil {
		t.Fatal(err)
	}
	if err := env.tmux.SendSpecialKey(userSession, "Enter"); err != nil {
		t.Fatal(err)
	}
	before := await.Until(t, "the person's command to print", func() (string, bool) {
		out, _ := env.tmux.Capture(userSession, 20)
		return out, slices.Contains(strings.Split(out, "\n"), "person-was-here")
	})

	getResumed := registerFakeAgy(env)
	env.cfg.AgentBinary = "/opt/x/agent"
	if _, err := env.service.Adopt(ctx, service.AdoptOptions{Name: "adopted-agy", SessionName: userSession, Backend: "agy"}); err != nil {
		t.Fatalf("Adopt: %v", err)
	}
	// The CLI in it quit (e.g. the person ran /exit).
	if err := env.service.Report(ctx, "adopted-agy", service.StatusReport{
		Status: service.AgentStatusEnded, EndReason: backend.EndQuit, Event: "SessionEnd",
	}); err != nil {
		t.Fatal(err)
	}

	_, err := env.service.Resume(ctx, service.ResumeOptions{Name: "adopted-agy", SessionID: "conv-initial"})
	want := "agent adopted-agy is an adopted session (tmux session " + userSession + "): resume doesn't relaunch CLIs in sessions gomuxai didn't start; start the CLI there yourself (agy -c continues its last conversation), and adopt keeps tracking it"
	if err == nil || err.Error() != want {
		t.Fatalf("Resume err = %v\nwant %s", err, want)
	}
	if getResumed() != nil {
		t.Errorf("a launch was built for the adopted agent: %q", getResumed().launchedCmd)
	}
	// Its prompt may redraw (a git status in it), so the screen is checked
	// for what a launch would type, not compared whole.
	after, _ := env.tmux.Capture(userSession, 20)
	for _, typed := range []string{"launch.sh", "agy", "conv-initial", "AGENT_NAME"} {
		if strings.Contains(after, typed) {
			t.Errorf("%q was typed into the person's session:\nbefore:\n%s\nafter:\n%s", typed, before, after)
		}
	}
	if _, err := os.Stat(filepath.Join(env.cfg.AgentDir("adopted-agy"), "launch.sh")); !errors.Is(err, fs.ErrNotExist) {
		t.Errorf("a launch script was written: %v", err)
	}
	ag, err := env.store.GetAgent(ctx, "adopted-agy")
	if err != nil || ag == nil || !ag.Adopted || ag.SessionName != userSession {
		t.Errorf("agent = %+v, %v; want its adopted record unchanged", ag, err)
	}

	// With the person's session gone, resume still makes none of its own.
	if err := env.tmux.KillSession(userSession); err != nil {
		t.Fatal(err)
	}
	_, err = env.service.Resume(ctx, service.ResumeOptions{Name: "adopted-agy", SessionID: "conv-initial"})
	if err == nil || !strings.Contains(err.Error(), "(tmux session "+userSession+", which is gone)") ||
		!strings.Contains(err.Error(), "agent cleanup adopted-agy, then agent spawn adopted-agy --backend agy --continue") {
		t.Errorf("Resume with the session gone: err = %v", err)
	}
	if exists, _ := env.tmux.SessionExists(env.cfg.SessionName("adopted-agy")); exists {
		t.Error("resume created a session of its own for the adopted agent")
	}
}

func TestAdopt_ShellUpgradesToClaudeWhenStartedLater(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()

	userSession := env.cfg.SessionPrefix + "shell-to-claude"
	if err := env.tmux.CreateSession(userSession, nil); err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	defer env.tmux.KillSession(userSession)

	fake := &fakeHookBackend{
		ClaudeBackend: namedagents.NewClaudeBackend(),
	}
	env.backends.RegisterFactory("claude", func(backend.ProviderConfig) backend.Provider { return fake })

	res, err := env.service.Adopt(ctx, service.AdoptOptions{
		Name:        "shell-later",
		SessionName: userSession,
	})
	if err != nil {
		t.Fatalf("Adopt: %v", err)
	}
	if res.Agent.Backend != "shell" {
		t.Fatalf("initial Backend = %q, want shell", res.Agent.Backend)
	}

	// Simulate starting Claude Code later in that adopted shell pane.
	fake.sessionID = "claude-later-99"
	fake.nativeFound = true
	fake.native = backend.StatusIdle
	fake.paneSession = &backend.PaneSession{
		ID:     "claude-later-99",
		Status: backend.StatusIdle,
		Detail: "idle",
	}

	info, err := env.service.StatusInfo(ctx, "shell-later")
	if err != nil || info.Status != service.AgentStatusIdle {
		t.Fatalf("StatusInfo after Claude started = %+v, %v; want idle", info, err)
	}
	updated, err := env.store.GetAgent(ctx, "shell-later")
	if err != nil || updated == nil || updated.Backend != "claude" {
		t.Errorf("stored agent after upgrade = %+v, %v; want Backend=claude", updated, err)
	}
	if got := stateValue(t, env, "shell-later", service.StateKeySessionID); got != "claude-later-99" {
		t.Errorf("_session_id = %q, want claude-later-99", got)
	}
}

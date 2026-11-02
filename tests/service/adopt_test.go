package service_test

import (
	"context"
	"strings"
	"testing"

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

func TestAdopt_ResumeAdoptedAgentKeepsExistingTmuxSession(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()

	userSession := env.cfg.SessionPrefix + "keep-me"
	if err := env.tmux.CreateSession(userSession, nil); err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	defer env.tmux.KillSession(userSession)

	getResumed := registerFakeAgy(env)
	env.cfg.AgentBinary = "/opt/x/agent"

	res, err := env.service.Adopt(ctx, service.AdoptOptions{
		Name:        "adopted-agy",
		SessionName: userSession,
		Backend:     "agy",
	})
	if err != nil {
		t.Fatalf("Adopt: %v", err)
	}
	if !res.Agent.Adopted || res.Agent.SessionName != userSession {
		t.Fatalf("Adopt agent = %+v, want Adopted=true SessionName=%s", res.Agent, userSession)
	}

	// Mark the session ended while userSession is still open (e.g. user ran /exit).
	if err := env.service.Report(ctx, "adopted-agy", service.StatusReport{
		Status:    service.AgentStatusEnded,
		EndReason: backend.EndQuit,
		Event:     "SessionEnd",
	}); err != nil {
		t.Fatal(err)
	}

	resumed, err := env.service.Resume(ctx, service.ResumeOptions{
		Name:      "adopted-agy",
		SessionID: "conv-initial",
	})
	if err != nil {
		t.Fatalf("Resume: %v", err)
	}
	if !resumed.Adopted || resumed.SessionName != userSession {
		t.Errorf("after Resume, agent = %+v, want Adopted=true SessionName=%s", resumed, userSession)
	}
	if exists, _ := env.tmux.SessionExists(userSession); !exists {
		t.Errorf("userSession %s was killed during Resume", userSession)
	}
	if getResumed() == nil || !strings.Contains(getResumed().launchedCmd, "--conversation 'conv-initial'") {
		t.Errorf("resumed launch cmd = %v, want --conversation 'conv-initial'", getResumed())
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

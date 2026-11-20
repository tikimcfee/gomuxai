package service_test

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// registerFakeAgy makes "agy" in env's registry a fake that launches
// nothing (resume takes only claude and agy agents). It returns the last
// instance created with a session to resume.
func registerFakeAgy(env *testEnv) func() *fakeAgyBackend {
	var resumed *fakeAgyBackend
	env.backends.RegisterFactory("agy", func(cfg backend.ProviderConfig) backend.Provider {
		fake := &fakeAgyBackend{AgyBackend: namedagents.NewAgyBackendWithConfig(namedagents.AgyConfig{
			SkipPermissions: cfg.SkipPermissions,
			AgentDir:        cfg.AgentDir,
			Resume:          cfg.Resume,
		})}
		if cfg.Resume != "" {
			resumed = fake
		}
		return fake
	})
	return func() *fakeAgyBackend { return resumed }
}

func stateValue(t *testing.T, env *testEnv, name, key string) string {
	t.Helper()
	e, err := env.store.GetState(context.Background(), name, key)
	if err != nil {
		t.Fatal(err)
	}
	if e == nil {
		return ""
	}
	return e.Value
}

// A reboot (or a killed tmux server) takes every agent's session with it and
// fires no hook: an agent whose last report is idle is ended (gone).
func TestStatus_NoTmuxSessionIsEndedGone(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	registerFakeAgy(env)
	if _, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "ghost", Backend: "agy"}); err != nil {
		t.Fatal(err)
	}
	if err := env.service.Report(ctx, "ghost", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop"}); err != nil {
		t.Fatal(err)
	}
	if info, err := env.service.StatusInfo(ctx, "ghost"); err != nil || info.Status != service.AgentStatusIdle {
		t.Fatalf("status with its session = %+v, %v; want idle", info, err)
	}

	if err := env.tmux.KillSession(env.cfg.SessionName("ghost")); err != nil {
		t.Fatal(err)
	}
	info, err := env.service.StatusInfo(ctx, "ghost")
	if err != nil {
		t.Fatal(err)
	}
	if info.Status != service.AgentStatusEnded || info.EndReason != backend.EndGone {
		t.Errorf("status without its session = %s (%s), want ended (gone)", info.Status, info.EndReason)
	}
	if !strings.Contains(info.Detail, "agent resume ghost") {
		t.Errorf("detail %q doesn't say how to resume it", info.Detail)
	}

	_, err = env.service.Spawn(ctx, service.SpawnOptions{Name: "ghost", Backend: "agy"})
	if err == nil || !strings.Contains(err.Error(), "agent resume ghost") {
		t.Errorf("spawning over the ended agent: %v, want a refusal pointing at agent resume", err)
	}
}

func TestResume_RelaunchesEndedAgentInPlace(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/gomuxai/agent"
	ctx := context.Background()
	resumedFake := registerFakeAgy(env)

	if _, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "lead", Backend: "agy", SpawnedBy: "boss"}); err != nil {
		t.Fatal(err)
	}
	oldSpawnID := stateValue(t, env, "lead", service.StateKeySpawnID)
	if err := env.store.CreateAgent(ctx, &storage.Agent{Name: "lead-h", SessionName: env.cfg.SessionName("lead-h"), Backend: "agy", SpawnedBy: "lead", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if err := env.service.Report(ctx, "lead", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop", Result: "old reply"}); err != nil {
		t.Fatal(err)
	}
	now := time.Now()
	if err := env.store.SetStates(ctx, []*storage.StateEntry{{AgentName: "lead", Key: "notes", Value: "keep me", UpdatedAt: now}}); err != nil {
		t.Fatal(err)
	}
	if err := env.store.CreateMessage(ctx, &storage.Message{SenderID: "boss", ReceiverID: "lead", Content: "are you there?", Status: storage.MessageStatusPending, CreatedAt: now}); err != nil {
		t.Fatal(err)
	}
	var delivering []string
	env.service.SetDelivererStarter(func(name string) error { delivering = append(delivering, name); return nil })

	if err := env.tmux.KillSession(env.cfg.SessionName("lead")); err != nil {
		t.Fatal(err)
	}
	agent, err := env.service.Resume(ctx, service.ResumeOptions{Name: "lead", SessionID: "conv-123"})
	if err != nil {
		t.Fatalf("Resume: %v", err)
	}

	if agent.SpawnedBy != "boss" || agent.Backend != "agy" {
		t.Errorf("resumed agent = %+v, want boss's agy agent still", agent)
	}
	if exists, _ := env.tmux.SessionExists(env.cfg.SessionName("lead")); !exists {
		t.Error("resume started no tmux session")
	}
	if f := resumedFake(); f == nil || !strings.Contains(f.launchedCmd, "--conversation 'conv-123'") {
		t.Errorf("launched %v, want agy --conversation conv-123", f)
	}
	for key, want := range map[string]string{
		service.StateKeySessionID: "conv-123",
		service.StateKeyResult:    "old reply",
		"notes":                   "keep me",
		service.StateKeyStatus:    "",
		service.StateKeyEndReason: "",
	} {
		if got := stateValue(t, env, "lead", key); got != want {
			t.Errorf("%s after resume = %q, want %q", key, got, want)
		}
	}
	if id := stateValue(t, env, "lead", service.StateKeySpawnID); id == "" || id == oldSpawnID {
		t.Errorf("spawn id after resume = %q, want a new one (was %q)", id, oldSpawnID)
	}
	if info, err := env.service.StatusInfo(ctx, "lead"); err != nil || info.Status == service.AgentStatusEnded {
		t.Errorf("status after resume = %+v, %v; want it no longer ended", info, err)
	}
	if len(delivering) != 1 || delivering[0] != "lead" {
		t.Errorf("deliverers started = %v, want one for lead's held message", delivering)
	}
	if h, _ := env.store.GetAgent(ctx, "lead-h"); h == nil || h.SpawnedBy != "lead" {
		t.Errorf("helper after resume = %+v, want still lead's", h)
	}

	tree, err := env.store.ReadTree(ctx, true)
	if err != nil {
		t.Fatal(err)
	}
	var ended []string
	for _, r := range tree.Ended {
		ended = append(ended, r.Name+":"+r.Status+":"+r.EndReason)
	}
	if len(ended) != 1 || ended[0] != "lead:ended:gone" {
		t.Errorf("ended runs = %v, want lead's first run, ended (gone)", ended)
	}
}

func TestResume_RefusesLiveAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	registerFakeAgy(env)
	if _, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "busy", Backend: "agy"}); err != nil {
		t.Fatal(err)
	}
	if err := env.service.Report(ctx, "busy", service.StatusReport{Status: service.AgentStatusProcessing}); err != nil {
		t.Fatal(err)
	}
	_, err := env.service.Resume(ctx, service.ResumeOptions{Name: "busy", SessionID: "conv-1"})
	if err == nil || !strings.Contains(err.Error(), "not ended") {
		t.Errorf("Resume of a processing agent: %v, want a refusal", err)
	}
	if exists, _ := env.tmux.SessionExists(env.cfg.SessionName("busy")); !exists {
		t.Error("a refused resume killed the live agent's session")
	}
}

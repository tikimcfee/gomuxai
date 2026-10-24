package service_test

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// fakeAgyBackend wraps AgyBackend so tests don't launch the real agy binary
// while still exercising LaunchCommandWithHooks bundle generation and ParseHookEvent.
type fakeAgyBackend struct {
	*namedagents.AgyBackend
	reportCmd   string
	launchedCmd string
}

func (f *fakeAgyBackend) Name() string { return "fakeagy" }

func (f *fakeAgyBackend) LaunchCommandWithHooks(reportCmd, sessionID string) string {
	f.reportCmd = reportCmd
	f.launchedCmd = f.AgyBackend.LaunchCommandWithHooks(reportCmd, sessionID)
	return "true"
}

func (f *fakeAgyBackend) AppendPromptFlag(launchCmd, prompt string) string {
	f.launchedCmd = f.AgyBackend.AppendPromptFlag(f.launchedCmd, prompt)
	return "true"
}

func TestAgySpawn_GeneratesHooksBundleAndReportsLifecycle(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/gomuxai/agent"
	ctx := context.Background()

	var fake *fakeAgyBackend
	env.backends.RegisterFactory("fakeagy", func(cfg backend.ProviderConfig) backend.Provider {
		fake = &fakeAgyBackend{
			AgyBackend: namedagents.NewAgyBackendWithConfig(namedagents.AgyConfig{
				SkipPermissions: cfg.SkipPermissions,
				AgentDir:        cfg.AgentDir,
			}),
		}
		return fake
	})

	taskFile := filepath.Join(t.TempDir(), "task.md")
	if err := os.WriteFile(taskFile, []byte("Say hi"), 0644); err != nil {
		t.Fatalf("writing task file: %v", err)
	}

	if _, err := env.service.Spawn(ctx, service.SpawnOptions{
		Name:     "agy-worker",
		Backend:  "fakeagy",
		TaskFile: taskFile,
	}); err != nil {
		t.Fatalf("Spawn: %v", err)
	}

	// Register in provider map so StatusInfo/ReportHook can look up "fakeagy"
	env.backends.RegisterProvider(fake)

	// Verify .gomuxai/agents/agy-worker/.agents/hooks.json exists
	hooksPath := filepath.Join(env.cfg.AgentDir("agy-worker"), ".agents", "hooks.json")
	if _, err := os.Stat(hooksPath); err != nil {
		t.Fatalf("expected hooks bundle at %s: %v", hooksPath, err)
	}
	spawnID := spawnIDOf(t, fake.reportCmd)

	// Simulate PreToolUse hook report (with --spawn-id and --event)
	prePayload := `{"conversationId":"conv-99","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo hi"}}}`
	if err := env.service.ReportHook(ctx, "agy-worker", spawnID, "PreToolUse", []byte(prePayload)); err != nil {
		t.Fatalf("ReportHook(PreToolUse): %v", err)
	}
	info, err := env.service.StatusInfo(ctx, "agy-worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusProcessing || info.Detail != "run_command: echo hi" || info.Tool != "run_command" {
		t.Errorf("after PreToolUse: got %s (%q, tool=%q), want processing (\"run_command: echo hi\", tool=\"run_command\")", info.Status, info.Detail, info.Tool)
	}
	if entry, _ := env.store.GetState(ctx, "agy-worker", service.StateKeySessionID); entry == nil || entry.Value != "conv-99" {
		t.Errorf("session id = %+v, want conv-99", entry)
	}

	// Simulate Stop hook report
	stopPayload := `{"conversationId":"conv-99","error":"","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}`
	if err := env.service.ReportHook(ctx, "agy-worker", spawnID, "Stop", []byte(stopPayload)); err != nil {
		t.Fatalf("ReportHook(Stop): %v", err)
	}
	info, err = env.service.StatusInfo(ctx, "agy-worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusIdle || info.Detail != "turn finished" {
		t.Errorf("after Stop: got %s (%q), want idle (\"turn finished\")", info.Status, info.Detail)
	}

	// Reject a stale spawn id
	if err := env.service.ReportHook(ctx, "agy-worker", "stale-spawn-id", "PreInvocation", []byte(`{"conversationId":"old"}`)); err == nil {
		t.Error("expected stale spawn id to be rejected")
	}

	// Cleanup should remove the per-agent bundle directory
	if err := env.service.Cleanup(ctx, "agy-worker"); err != nil {
		t.Fatalf("Cleanup: %v", err)
	}
	if _, err := os.Stat(env.cfg.AgentDir("agy-worker")); !os.IsNotExist(err) {
		t.Errorf("expected agent dir %s to be removed on cleanup, err = %v", env.cfg.AgentDir("agy-worker"), err)
	}
}

package service_test

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

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

// spawnFakeAgy registers the fake agy backend and spawns an agent on it.
func spawnFakeAgy(t *testing.T, env *testEnv, opts service.SpawnOptions) *fakeAgyBackend {
	t.Helper()
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

	opts.Backend = "fakeagy"
	if _, err := env.service.Spawn(context.Background(), opts); err != nil {
		t.Fatalf("Spawn: %v", err)
	}
	return fake // the spawn's instance: later lookups make new ones
}

func TestAgySpawn_GeneratesHooksBundleAndReportsLifecycle(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/gomuxai/agent"
	ctx := context.Background()

	taskFile := filepath.Join(t.TempDir(), "task.md")
	if err := os.WriteFile(taskFile, []byte("Say hi"), 0644); err != nil {
		t.Fatalf("writing task file: %v", err)
	}

	fake := spawnFakeAgy(t, env, service.SpawnOptions{Name: "agy-worker", TaskFile: taskFile})

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

// Non-hook `agent report exited --as <name> --spawn-id <id>` is run by the
// shell wrapper after agy exits. If the agent was cleaned up and respawned
// with the same name in the meantime, the old wrapper must not mark the new
// spawn exited.
func TestReport_RejectsStaleSpawnID(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()

	old := spawnFake(t, env, "worker")
	oldSpawn := spawnIDOf(t, old.reportCmd)
	if err := env.service.Cleanup(ctx, "worker"); err != nil {
		t.Fatalf("cleanup: %v", err)
	}

	fresh := spawnFake(t, env, "worker")
	newSpawn := spawnIDOf(t, fresh.reportCmd)
	if err := env.service.Report(ctx, "worker", service.StatusReport{
		Status: service.AgentStatusIdle,
		Event:  "Stop",
	}); err != nil {
		t.Fatalf("Report(idle): %v", err)
	}

	// Late exit wrapper from the previous spawn arrives: must be rejected.
	_ = env.service.Report(ctx, "worker", service.StatusReport{
		Status:  service.AgentStatusExited,
		Event:   "report",
		SpawnID: oldSpawn,
	})
	if st, _ := env.service.Status(ctx, "worker"); st != service.AgentStatusIdle {
		t.Fatalf("status = %s, want idle (stale exit report overwrote respawn)", st)
	}

	// Current spawn's exit wrapper succeeds.
	if err := env.service.Report(ctx, "worker", service.StatusReport{
		Status:  service.AgentStatusExited,
		Event:   "report",
		SpawnID: newSpawn,
	}); err != nil {
		t.Fatalf("Report(exited, current spawn): %v", err)
	}
	if st, _ := env.service.Status(ctx, "worker"); st != service.AgentStatusExited {
		t.Errorf("status = %s, want exited", st)
	}
}

// agy fires PreToolUse before showing its permission dialog and fires nothing
// while the dialog is up. When the dialog is visible at the bottom of the
// pane after PreToolUse, StatusInfo should report waiting and RespondPermission
// should work without --force.
func TestAgyWaitingAndApprove_WhenPermissionDialogVisible(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/gomuxai/agent"
	ctx := context.Background()

	fake := spawnFakeAgy(t, env, service.SpawnOptions{Name: "agy-perm"})
	session := env.cfg.SessionName("agy-perm")
	spawnID := spawnIDOf(t, fake.reportCmd)

	// PreToolUse fires while no dialog is on screen: status is processing,
	// and approve without --force refuses.
	prePayload := `{"conversationId":"conv-1","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo gated"}}}`
	if err := env.service.ReportHook(ctx, "agy-perm", spawnID, "PreToolUse", []byte(prePayload)); err != nil {
		t.Fatalf("ReportHook(PreToolUse): %v", err)
	}
	if st, _ := env.service.Status(ctx, "agy-perm"); st != service.AgentStatusProcessing {
		t.Fatalf("without dialog on screen: status = %s, want processing", st)
	}
	if _, err := env.service.RespondPermission(ctx, "agy-perm", backend.PermissionApprove, service.PermissionOptions{}); err == nil {
		t.Fatal("approve succeeded without dialog on screen")
	}

	// Render agy's permission dialog at the bottom of the agent's pane and
	// block on read so the shell prompt doesn't appear below "esc to cancel".
	dialogCmd := `printf '\nRun this command?\n> 1. Yes, run command\n  2. Yes, and always allow\n  4. No, cancel\nesc to cancel\n'; read -r -n 1 _`
	if err := env.tmux.SendKeys(session, dialogCmd); err != nil {
		t.Fatalf("SendKeys: %v", err)
	}
	if err := env.tmux.SendSpecialKey(session, "Enter"); err != nil {
		t.Fatalf("SendSpecialKey: %v", err)
	}

	// Wait briefly for the shell to print the dialog lines.
	var info *service.StatusInfo
	for i := 0; i < 40; i++ {
		var err error
		info, err = env.service.StatusInfo(ctx, "agy-perm")
		if err != nil {
			t.Fatalf("StatusInfo: %v", err)
		}
		if info.Status == service.AgentStatusWaiting {
			break
		}
		time.Sleep(25 * time.Millisecond)
	}
	if info.Status != service.AgentStatusWaiting {
		screen, _ := env.tmux.Capture(session, 0)
		t.Fatalf("with dialog on screen: status = %s, want waiting; screen:\n%s", info.Status, screen)
	}
	if info.Detail != "permission: run_command: echo gated" || info.Tool != "run_command" {
		t.Errorf("got detail=%q tool=%q, want \"permission: run_command: echo gated\" / \"run_command\"", info.Detail, info.Tool)
	}

	// Approve without --force (and with --expect run_command) now succeeds.
	if _, err := env.service.RespondPermission(ctx, "agy-perm", backend.PermissionApprove, service.PermissionOptions{
		ExpectTool: "run_command",
	}); err != nil {
		t.Fatalf("RespondPermission without --force failed: %v", err)
	}
	if st, _ := env.service.Status(ctx, "agy-perm"); st != service.AgentStatusProcessing {
		t.Errorf("after approve: status = %s, want processing", st)
	}
}

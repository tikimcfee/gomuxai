package service_test

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// fakeHookBackend is the claude backend with the parts that would launch or
// query a real claude replaced, so tests control what the CLI "says".
type fakeHookBackend struct {
	*namedagents.ClaudeBackend

	reportCmd   string
	sessionID   string
	nativeFound bool // the CLI lists the session, with status native
	nativeGone  bool // the CLI answers but doesn't list the session (it died)
	native      backend.Status
	promptGone  bool // no permission dialog on screen (the test shell shows none)
}

// PermissionPromptVisible stands in for the screen check: the test session
// is a plain shell, so the real check would never see a dialog.
func (f *fakeHookBackend) PermissionPromptVisible(string) bool { return !f.promptGone }

// errNativeUnavailable is what the fake returns when the test set up neither
// nativeFound nor nativeGone: as if the backend CLI couldn't be run.
var errNativeUnavailable = errors.New("fake: native status unavailable")

func (f *fakeHookBackend) Name() string { return "fakehooks" }

// LaunchCommand is typed into the test shell; keep it harmless.
func (f *fakeHookBackend) LaunchCommand() string { return "true" }

func (f *fakeHookBackend) LaunchCommandWithHooks(reportCmd, sessionID string) string {
	f.reportCmd, f.sessionID = reportCmd, sessionID
	return "true"
}

func (f *fakeHookBackend) NativeStatus(ctx context.Context, sessionID string) (backend.Status, bool, error) {
	if !f.nativeFound && !f.nativeGone {
		return backend.StatusUnknown, false, errNativeUnavailable
	}
	if f.nativeGone || sessionID != f.sessionID {
		return backend.StatusUnknown, false, nil
	}
	return f.native, true, nil
}

// spawnFake registers a fake hook backend and spawns an agent on it.
func spawnFake(t *testing.T, env *testEnv, name string) *fakeHookBackend {
	t.Helper()
	fake := &fakeHookBackend{ClaudeBackend: namedagents.NewClaudeBackend()}
	env.backends.Register(fake)
	env.backends.RegisterFactory("fakehooks", func(backend.ProviderConfig) backend.Provider { return fake })

	if _, err := env.service.Spawn(context.Background(), service.SpawnOptions{Name: name, Backend: "fakehooks"}); err != nil {
		t.Fatalf("spawn: %v", err)
	}
	return fake
}

func TestSpawn_WiresHooksToAgentBinary(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/gomux ai/agent"
	ctx := context.Background()

	fake := spawnFake(t, env, "hooked")

	if fake.sessionID == "" {
		t.Fatal("expected a session id to be assigned")
	}
	wantCmd := `'/opt/gomux ai/agent' report --hook --as 'hooked' --spawn-id '` + fake.sessionID + `'`
	if fake.reportCmd != wantCmd {
		t.Errorf("report command = %q, want %q", fake.reportCmd, wantCmd)
	}
	entry, err := env.store.GetState(ctx, "hooked", service.StateKeySessionID)
	if err != nil || entry == nil || entry.Value != fake.sessionID {
		t.Errorf("stored session id = %+v (err %v), want %q", entry, err, fake.sessionID)
	}
}

func TestSpawn_NoHooksWithoutAgentBinary(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()

	fake := spawnFake(t, env, "plain")

	if fake.reportCmd != "" {
		t.Errorf("hooks should not be wired without AgentBinary, got %q", fake.reportCmd)
	}
	if entry, _ := env.store.GetState(ctx, "plain", service.StateKeySessionID); entry != nil {
		t.Errorf("unexpected session id: %+v", entry)
	}
}

func TestReportHook_UpdatesStatus(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"PermissionRequest","session_id":"s-2","tool_name":"Bash","tool_input":{"command":"rm -rf build"}}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusWaiting || info.Source != service.StatusSourceReport {
		t.Errorf("got %s from %s, want waiting from report", info.Status, info.Source)
	}
	if info.Detail != "permission: Bash: rm -rf build" {
		t.Errorf("detail = %q", info.Detail)
	}
	// The hook's session id replaces the spawn-time one (e.g. after /clear).
	if entry, _ := env.store.GetState(ctx, "worker", service.StateKeySessionID); entry == nil || entry.Value != "s-2" {
		t.Errorf("session id not updated from hook: %+v", entry)
	}
}

func TestReportHook_StopStoresResult(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"Found 3 issues."}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	entry, err := env.store.GetState(ctx, "worker", service.StateKeyResult)
	if err != nil || entry == nil || entry.Value != "Found 3 issues." {
		t.Errorf("result = %+v (err %v)", entry, err)
	}
}

func TestReportHook_UnknownAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	err := env.service.ReportHook(context.Background(), "ghost", "", "", []byte(`{"hook_event_name":"Stop"}`))
	if err == nil || !strings.Contains(err.Error(), "not found") {
		t.Errorf("expected not found error, got %v", err)
	}
}

func TestStatusInfo_NativeOverridesStaleReport(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")

	// A permission prompt was interrupted: no hook fired, the CLI is idle again.
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusWaiting, Event: "PermissionRequest", Detail: "permission: Bash"})
	fake.native, fake.nativeFound = backend.StatusIdle, true

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusIdle || info.Source != service.StatusSourceNative {
		t.Errorf("got %s from %s, want idle from native", info.Status, info.Source)
	}
	if info.Detail != "" {
		t.Errorf("stale detail should be dropped, got %q", info.Detail)
	}

	// When they agree, the hook detail rides along.
	fake.native = backend.StatusWaiting
	info, _ = env.service.StatusInfo(ctx, "worker")
	if info.Status != service.AgentStatusWaiting || info.Detail != "permission: Bash" {
		t.Errorf("got %s %q, want waiting with detail", info.Status, info.Detail)
	}
}

func TestStatusInfo_FallsBackToReportWhenNativeMissing(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	spawnFake(t, env, "worker") // nativeFound stays false

	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusExited, Event: "SessionEnd"})

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusExited || info.Source != service.StatusSourceReport {
		t.Errorf("got %s from %s, want exited from report", info.Status, info.Source)
	}
}

func TestStatusInfo_ScrapesWithoutReports(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	spawnFake(t, env, "worker")

	info, err := env.service.StatusInfo(context.Background(), "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Source != service.StatusSourceScrape {
		t.Errorf("source = %s, want scrape", info.Source)
	}
}

func TestRespondPermission_RequiresWaiting(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusProcessing})

	err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{})
	if err == nil || !strings.Contains(err.Error(), "not waiting") {
		t.Errorf("expected refusal, got %v", err)
	}
}

func TestRespondPermission_ApproveAndDeny(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	tests := []struct {
		resp backend.PermissionResponse
		want service.AgentStatus
	}{
		{backend.PermissionApprove, service.AgentStatusProcessing},
		{backend.PermissionDeny, service.AgentStatusIdle},
	}
	for _, tt := range tests {
		env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusWaiting})
		if err := env.service.RespondPermission(ctx, "worker", tt.resp, service.PermissionOptions{}); err != nil {
			t.Fatalf("RespondPermission(%v): %v", tt.resp, err)
		}
		if st, _ := env.service.Status(ctx, "worker"); st != tt.want {
			t.Errorf("after %v: status = %s, want %s", tt.resp, st, tt.want)
		}
	}
}

func TestParseAgentStatus(t *testing.T) {
	if _, err := service.ParseAgentStatus("waiting"); err != nil {
		t.Errorf("waiting should be valid: %v", err)
	}
	if _, err := service.ParseAgentStatus("done"); err == nil {
		t.Error("done should be rejected")
	}
}

package service_test

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

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
	return spawnFakeWith(t, env, name, service.SpawnOptions{})
}

// spawnFakeWith is spawnFake with extra spawn options.
func spawnFakeWith(t *testing.T, env *testEnv, name string, opts service.SpawnOptions) *fakeHookBackend {
	t.Helper()
	fake := &fakeHookBackend{ClaudeBackend: namedagents.NewClaudeBackend()}
	env.backends.Register(fake)
	env.backends.RegisterFactory("fakehooks", func(backend.ProviderConfig) backend.Provider { return fake })

	opts.Name, opts.Backend = name, "fakehooks"
	if _, err := env.service.Spawn(context.Background(), opts); err != nil {
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

func TestReportHook_StoresTranscript(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	const path = "/home/user/.claude/projects/p/s.jsonl"
	payload := `{"hook_event_name":"UserPromptSubmit","session_id":"s","transcript_path":"` + path + `"}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	// A payload without a path keeps the one stored.
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(`{"hook_event_name":"Stop","session_id":"s"}`)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	if entry, _ := env.store.GetState(ctx, "worker", service.StateKeyTranscript); entry == nil || entry.Value != path {
		t.Errorf("transcript = %+v, want %s", entry, path)
	}
}

// The transcripts an agent used are listed beside its log, so they outlive
// cleanup like the log does.
func TestTranscripts_OutliveCleanup(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	for _, path := range []string{"/t/one.jsonl", "/t/one.jsonl", "/t/two.jsonl"} {
		payload := `{"hook_event_name":"PostToolUse","transcript_path":"` + path + `"}`
		if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
			t.Fatalf("ReportHook: %v", err)
		}
	}
	want := []service.TranscriptRef{{Backend: "fakehooks", Path: "/t/one.jsonl"}, {Backend: "fakehooks", Path: "/t/two.jsonl"}}
	if got := env.service.Transcripts(ctx, "worker"); !slices.Equal(got, want) {
		t.Errorf("transcripts = %v, want %v", got, want)
	}
	if err := env.service.Cleanup(ctx, "worker"); err != nil {
		t.Fatalf("Cleanup: %v", err)
	}
	if got := env.service.Transcripts(ctx, "worker"); !slices.Equal(got, want) {
		t.Errorf("after cleanup: transcripts = %v, want %v", got, want)
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

	_, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{})
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
		if _, err := env.service.RespondPermission(ctx, "worker", tt.resp, service.PermissionOptions{}); err != nil {
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

// Cleanup removes .gomuxai/agents/<name>, so names must not escape that dir.
func TestSpawn_RejectsUnsafeNames(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	for _, name := range []string{"", "..", "../..", "a/b", ".hidden", "-flag", "has space", "semi;colon"} {
		_, err := env.service.Spawn(context.Background(), service.SpawnOptions{Name: name, Backend: "claude"})
		if err == nil || !strings.Contains(err.Error(), "invalid agent name") {
			t.Errorf("Spawn(%q): err = %v, want invalid agent name", name, err)
		}
	}
	for _, name := range []string{"helper", "api-analyzer", "rv_2", "A1"} {
		if err := service.ValidateAgentName(name); err != nil {
			t.Errorf("ValidateAgentName(%q): %v", name, err)
		}
	}
}

// Every spawned agent gets a briefing on reaching other agents, loaded at
// launch without writing into its project.
func TestSpawn_BriefsTheAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/gomux/agent"
	env.cfg.Host = "h1"

	spawnFakeWith(t, env, "worker", service.SpawnOptions{SpawnedBy: "lead"})

	path := filepath.Join(env.cfg.AgentDir("worker"), "instructions.md")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("instructions not written: %v", err)
	}
	for _, want := range []string{`agent "worker" on host "h1"`, `agent "lead"`, "full path, /opt/gomux/agent", "[from NAME@HOST]", "agent send",
		"agent spawn worker-review --backend claude", "agent cleanup worker-review"} {
		if !strings.Contains(string(data), want) {
			t.Errorf("instructions missing %q", want)
		}
	}
	if info, _ := os.Stat(path); info.Mode().Perm() != 0600 {
		t.Errorf("instructions mode %o, want 600", info.Mode().Perm())
	}

	// The launch command loads them (the fake backend launches `true`). It
	// runs from launch.sh in the agent's directory, so the shell is only
	// typed a short command.
	launch, err := os.ReadFile(filepath.Join(env.cfg.AgentDir("worker"), "launch.sh"))
	if err != nil {
		t.Fatalf("launch script not written: %v", err)
	}
	if !strings.Contains(string(launch), "--append-system-prompt-file") || !strings.Contains(string(launch), "instructions.md") {
		t.Errorf("launch command doesn't load the instructions:\n%s", launch)
	}
	waitForSession(t, env, env.cfg.SessionName("worker"), "launch.sh")

	// Cleanup removes them with the agent's directory.
	if err := env.service.Cleanup(context.Background(), "worker"); err != nil {
		t.Fatalf("cleanup: %v", err)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Errorf("instructions survived cleanup: %v", err)
	}
}

// A message's Enter answers whatever dialog is on screen (Yes at a permission
// prompt, "No, exit" at Claude's trust prompt), so send refuses at dialogs.
func TestSend_RefusesAtDialogs(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")

	// A permission dialog on screen.
	fake.promptGone = false
	if _, err := env.service.Send(ctx, "worker", "hello"); err == nil || !strings.Contains(err.Error(), "permission prompt on screen") {
		t.Errorf("send at a permission dialog: err = %v", err)
	}
	fake.promptGone = true

	// Claude's trust dialog on screen (shown here by the shell echoing it).
	if err := env.service.SendUnchecked(ctx, "worker", "printf ' ❯ No, exit\\n   Yes, I trust this folder\\n'"); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(2 * time.Second)
	for {
		_, err := env.service.Send(ctx, "worker", "hello")
		if err != nil && strings.Contains(err.Error(), "trust prompt") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("send at a trust dialog: err = %v", err)
		}
		time.Sleep(50 * time.Millisecond)
	}

	// No dialog: send goes through.
	env.tmux.Clear(env.cfg.SessionName("worker"))
	time.Sleep(200 * time.Millisecond)
	if _, err := env.service.Send(ctx, "worker", "echo ok"); err != nil {
		t.Errorf("send with no dialog: %v", err)
	}
}

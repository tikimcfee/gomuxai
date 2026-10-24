package service_test

import (
	"context"
	"os"
	"regexp"
	"sync"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// Regression tests from the status-from-hooks review. Tests for bugs that are
// confirmed but not fixed yet skip unless GOMUXAI_KNOWN_BUGS=1, so the suite
// stays green while the repro stays runnable.

func skipUnlessKnownBugs(t *testing.T) {
	t.Helper()
	if os.Getenv("GOMUXAI_KNOWN_BUGS") == "" {
		t.Skip("known bug, not fixed yet; run with GOMUXAI_KNOWN_BUGS=1 to reproduce")
	}
}

var spawnIDFlag = regexp.MustCompile(`--spawn-id '([^']+)'`)

// spawnIDOf extracts the spawn id that spawn baked into the hook command.
func spawnIDOf(t *testing.T, reportCmd string) string {
	t.Helper()
	m := spawnIDFlag.FindStringSubmatch(reportCmd)
	if m == nil {
		t.Fatalf("no --spawn-id in hook command %q", reportCmd)
	}
	return m[1]
}

// Repro (real claude): `agent cleanup X && agent spawn X`. Killing the old
// claude fires its SessionEnd hook ~100ms later, after the new agent record
// exists; observed in a DB trace writing _status=exited and the OLD
// _session_id into the new agent. If it lands after the new SessionStart, the
// new agent stays "exited" with a session id native lookup can't find.
func TestReportHook_IgnoresHooksFromPreviousSpawn(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()

	old := spawnFake(t, env, "worker")
	oldSpawn, oldSession := spawnIDOf(t, old.reportCmd), old.sessionID
	if err := env.service.Cleanup(ctx, "worker"); err != nil {
		t.Fatalf("cleanup: %v", err)
	}

	fresh := spawnFake(t, env, "worker")
	newSpawn := spawnIDOf(t, fresh.reportCmd)
	start := `{"hook_event_name":"SessionStart","source":"startup","session_id":"` + fresh.sessionID + `"}`
	if err := env.service.ReportHook(ctx, "worker", newSpawn, []byte(start)); err != nil {
		t.Fatalf("new SessionStart: %v", err)
	}

	// The dying first claude's SessionEnd arrives late.
	end := `{"hook_event_name":"SessionEnd","reason":"other","session_id":"` + oldSession + `"}`
	_ = env.service.ReportHook(ctx, "worker", oldSpawn, []byte(end))

	if st, _ := env.store.GetState(ctx, "worker", service.StateKeyStatus); st == nil || st.Value != string(service.AgentStatusIdle) {
		t.Errorf("_status = %+v, want idle (stale SessionEnd from the previous spawn was applied)", st)
	}
	if sid, _ := env.store.GetState(ctx, "worker", service.StateKeySessionID); sid == nil || sid.Value != fresh.sessionID {
		t.Errorf("_session_id = %+v, want %q", sid, fresh.sessionID)
	}
}

// Within one spawn the session id may change (/clear, /resume): still accepted.
func TestReportHook_AcceptsSessionChangeWithinSpawn(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"SessionStart","source":"clear","session_id":"after-clear"}`
	if err := env.service.ReportHook(ctx, "worker", spawnIDOf(t, fake.reportCmd), []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	if sid, _ := env.store.GetState(ctx, "worker", service.StateKeySessionID); sid == nil || sid.Value != "after-clear" {
		t.Errorf("_session_id = %+v, want after-clear", sid)
	}
}

// Repro (real claude): spawn, wait for idle, `kill -9 <claude pid>`. The pane
// drops to bash, `claude agents --json` no longer lists the session, and
// `agent status` said "idle (source: report)". `agent watch` then typed a
// queued message into bash, which ran it as a shell command.
func TestStatusInfo_CrashedSessionIsNotIdle(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")

	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop"})
	fake.nativeGone = true // the CLI answered, but the session is gone

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status == service.AgentStatusIdle {
		t.Fatalf("crashed agent reported idle (source %s); the watcher would deliver into the shell", info.Source)
	}
	if info.Status != service.AgentStatusExited {
		t.Errorf("status = %s, want exited", info.Status)
	}
}

// failingNativeBackend can't reach the backend CLI at all (e.g. claude not on
// PATH for the watcher): the last report is still the best answer.
type failingNativeBackend struct{ *fakeHookBackend }

func (f *failingNativeBackend) NativeStatus(context.Context, string) (backend.Status, bool, error) {
	return backend.StatusUnknown, false, os.ErrNotExist
}

func TestStatusInfo_NativeUnavailableKeepsReport(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")
	failing := &failingNativeBackend{fake}
	env.backends.RegisterFactory("fakehooks", func(backend.ProviderConfig) backend.Provider { return failing })

	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop"})
	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusIdle || info.Source != service.StatusSourceReport {
		t.Errorf("got %s from %s, want idle from report", info.Status, info.Source)
	}
}

// KNOWN BUG. Repro (real claude): ask the agent to use AskUserQuestion. Hooks
// report PermissionRequest "permission: AskUserQuestion", native says waiting,
// and `agent approve --always` pressed "2", answering the question ("Blue")
// on the user's behalf. approve should only answer real permission dialogs.
func TestRespondPermission_RefusesQuestionDialogs(t *testing.T) {
	skipUnlessKnownBugs(t)
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Favorite color?"}]}}`
	if err := env.service.ReportHook(ctx, "worker", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	if err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, false); err == nil {
		t.Error("approve answered an AskUserQuestion dialog; want a refusal without --force")
	} else {
		t.Logf("refused: %v", err)
	}
}

// KNOWN BUG. Report writes _status/_event/_detail as separate statements, so
// concurrent reports interleave: a stress run of 30 concurrent
// `agent report --hook` processes left 170/300 report groups torn in a DB
// trace. (Claude ran one session's hooks serially in probes, so in practice
// this needs overlapping writers, e.g. a hook racing approve/deny.)
func TestReport_ConcurrentReportsStayConsistent(t *testing.T) {
	skipUnlessKnownBugs(t)
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	reports := []service.StatusReport{
		{Status: service.AgentStatusWaiting, Event: "PermissionRequest", Detail: "permission: Bash"},
		{Status: service.AgentStatusProcessing, Event: "PostToolUse", Detail: "Read: f"},
	}
	for round := 0; round < 50; round++ {
		var wg sync.WaitGroup
		for _, r := range reports {
			wg.Add(1)
			go func(r service.StatusReport) {
				defer wg.Done()
				env.service.Report(ctx, "worker", r)
			}(r)
		}
		wg.Wait()
		st, _ := env.store.GetState(ctx, "worker", service.StateKeyStatus)
		ev, _ := env.store.GetState(ctx, "worker", service.StateKeyEvent)
		if (st.Value == string(service.AgentStatusWaiting)) != (ev.Value == "PermissionRequest") {
			t.Fatalf("round %d: torn report: _status=%s _event=%s", round, st.Value, ev.Value)
		}
	}
}

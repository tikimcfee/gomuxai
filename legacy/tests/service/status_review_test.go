package service_test

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// Regression tests from the status-from-hooks review.

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

// The session id may change (/clear, /resume): the hook's is recorded. (A
// stale hook from a previous spawn's CLI can't reach the agent: placeHook
// finds no pane above a CLI cleanup killed.)
func TestReportHook_AcceptsSessionChangeWithinSpawn(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"SessionStart","source":"clear","session_id":"after-clear"}`
	_ = fake
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	if sid, _ := env.store.GetState(ctx, "worker", service.StateKeySessionID); sid == nil || sid.Value != "after-clear" {
		t.Errorf("_session_id = %+v, want after-clear", sid)
	}
}

// Repro (real claude): spawn, wait for idle, `kill -9 <claude pid>`. The pane
// drops to bash, Claude's session registry no longer lists the session, and
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
	if info.Status != service.AgentStatusEnded {
		t.Errorf("status = %s, want exited", info.Status)
	}
}

// Repro (real claude): the CLI quits (/exit, SessionEnd), and a person types
// `claude` at the shell left in the pane. The new CLI was launched by hand,
// so no hook reports it; the registry and the process tree say it runs there.
func TestStatusInfo_AdoptsTheSessionThePaneRuns(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")
	old := fake.sessionID

	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop", Result: "old reply", Transcript: "/t/old.jsonl"})
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusEnded, Event: "SessionEnd", Detail: "prompt_input_exit"})
	fake.nativeGone = true
	fake.paneSession = &backend.PaneSession{ID: "by-hand", PID: 4242, Status: backend.StatusIdle, Detail: "at its prompt"}

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusIdle || info.Event != service.EventAdopted || info.Source != service.StatusSourceNative {
		t.Fatalf("got %s/%s/%s (%s), want idle/adopted/native", info.Status, info.Event, info.Source, info.Detail)
	}
	for _, want := range []string{"by-hand", "pid 4242", old} {
		if !strings.Contains(info.Detail, want) {
			t.Errorf("detail %q doesn't name %q", info.Detail, want)
		}
	}
	for key, want := range map[string]string{
		service.StateKeySessionID:  "by-hand",
		service.StateKeyTranscript: "", // the new session's transcript is no hook's yet
		service.StateKeyResult:     "", // nor is the old reply its reply
	} {
		if e, _ := env.store.GetState(ctx, "worker", key); e == nil || e.Value != want {
			t.Errorf("%s = %+v, want %q", key, e, want)
		}
	}

	// A CLI started by hand carries no hooks until the global hook reports
	// it, so nothing could hand it held messages: they are typed.
	if env.service.HoldsMessages(ctx, "worker") {
		t.Error("holds messages for a session no hook has reached")
	}

	// Recorded once: now the session on record is the one running.
	fake.nativeGone, fake.nativeFound, fake.native, fake.sessionID = false, true, backend.StatusProcessing, "by-hand"
	info, _ = env.service.StatusInfo(ctx, "worker")
	if info.Status != service.AgentStatusProcessing || info.Source != service.StatusSourceNative {
		t.Errorf("after adopting: %s/%s, want processing/native", info.Status, info.Source)
	}
}

// A pane that can't be read says so, beside the exit it can't overrule.
func TestStatusInfo_PaneErrorIsShown(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")

	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop"})
	fake.nativeGone = true
	fake.paneErr = errors.New("two Claude Code processes run at the same depth")

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusEnded || !strings.Contains(info.Detail, "its pane: two Claude Code processes") {
		t.Errorf("got %s (%s)", info.Status, info.Detail)
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
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Favorite color?"}]}}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{}); err == nil {
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

func TestRespondPermission_ForceAnswersQuestionDialogs(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"AskUserQuestion","tool_input":{}}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{Force: true}); err != nil {
		t.Errorf("--force should answer the dialog: %v", err)
	}
}

func TestRespondPermission_ExpectTool(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash","tool_input":{"command":"rm -rf build"}}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil || info.Tool != "Bash" {
		t.Fatalf("pending tool = %q (err %v), want Bash", info.Tool, err)
	}

	// The prompt you inspected was replaced by a different one: refuse.
	_, err = env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{ExpectTool: "Edit"})
	if err == nil {
		t.Fatal("approved a Bash prompt while expecting Edit")
	}
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{ExpectTool: "Bash"}); err != nil {
		t.Errorf("expected Bash approval to go through: %v", err)
	}
}

// Repro (real claude): reply "A" to one prompt, then send another (or /clear).
// _result kept saying "A" until the next Stop, so a coordinator polling it
// read the previous turn's reply as this one's. And a turn that launched
// background work stopped with an interim reply ("started") stored as the
// result and status idle, while the work and the real reply were still to come.
func TestReportHook_ResultTracksCurrentTurn(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")

	result := func() string {
		t.Helper()
		e, _ := env.store.GetState(ctx, "worker", service.StateKeyResult)
		if e == nil {
			return ""
		}
		return e.Value
	}
	hook := func(payload string) {
		t.Helper()
		if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
			t.Fatalf("ReportHook: %v", err)
		}
	}

	hook(`{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"A","background_tasks":[]}`)
	if got := result(); got != "A" {
		t.Fatalf("_result = %q, want A", got)
	}

	hook(`{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"now launch the job"}`)
	if got := result(); got != "" {
		t.Errorf("_result = %q after a new prompt, want empty (stale reply from the previous turn)", got)
	}

	hook(`{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"started","background_tasks":[{"id":"b1","type":"shell","status":"running","command":"make"}],"session_crons":[]}`)
	if st, _ := env.service.Status(ctx, "worker"); st != service.AgentStatusProcessing {
		t.Errorf("status = %s with a background task running, want processing", st)
	}
	if got := result(); got != "" {
		t.Errorf("_result = %q, want empty (interim reply recorded as the result)", got)
	}

	// The task's completion starts a turn of its own.
	hook(`{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"<task-notification>b1 completed</task-notification>"}`)
	hook(`{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"build passed","background_tasks":[],"session_crons":[]}`)
	if got := result(); got != "build passed" {
		t.Errorf("_result = %q, want the final reply", got)
	}

	hook(`{"hook_event_name":"SessionStart","session_id":"s2","source":"clear"}`)
	if got := result(); got != "" {
		t.Errorf("_result = %q after /clear, want empty", got)
	}
}

// Repro (race): the agent is waiting on a Bash prompt; someone else answers
// it while `agent deny` is between its status check and the keypress. deny
// then sent Escape into the turn now running, cancelling the work that was
// just approved. deny must re-check the screen and refuse.
func TestRespondPermission_RefusesWhenPromptLeftScreen(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")

	payload := `{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash","tool_input":{"command":"go test ./..."}}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	fake.promptGone = true // answered meanwhile; status still says waiting

	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{}); err == nil {
		t.Fatal("deny sent Escape with no prompt on screen; it would cancel the running turn")
	}
	if ev, _ := env.store.GetState(ctx, "worker", service.StateKeyEvent); ev == nil || ev.Value != "PermissionRequest" {
		t.Errorf("_event = %+v, want PermissionRequest (a refused deny must not record itself)", ev)
	}
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{Force: true}); err != nil {
		t.Errorf("--force should skip the screen check: %v", err)
	}
}

// Claude writes a status word gomuxai doesn't know: that agent's report
// stands, and status -v can say why.
func TestStatusInfo_UnknownRegistryValue(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop"})
	fake.nativeFound, fake.native = true, backend.StatusUnknown
	fake.nativeErr = errors.New(`status "compacting" is not one gomuxai knows, written by Claude Code 2.2.0`)

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatal(err)
	}
	if info.Status != service.AgentStatusIdle || info.Source != service.StatusSourceReport || !strings.Contains(info.NativeErr, `"compacting"`) {
		t.Errorf("got %s/%s, native %q", info.Status, info.Source, info.NativeErr)
	}
}

// Repro (monolith1, Claude Code 2.1.319): Claude moved an agent's
// conversation into a background session its daemon runs under the agent's
// CLI. The old session's registry entry kept "busy", its transcript ended
// with continued-in, and status said processing until someone noticed.
// Status follows the move to the session the pane now runs, keeping the
// agent's reply.
func TestStatusInfo_FollowsASessionClaudeMoved(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/bin/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")
	old := fake.sessionID

	dir := t.TempDir()
	oldPath := filepath.Join(dir, old+".jsonl")
	if err := os.WriteFile(oldPath, []byte(`{"type":"user","sessionId":"`+old+`"}`+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusProcessing, Event: "PostToolUse", Result: "", Transcript: oldPath, SessionID: old})
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop", Result: "the reply"})
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusProcessing, Event: "PostToolUse"})
	fake.nativeFound, fake.native = true, backend.StatusProcessing // the old entry, frozen at busy

	// Not moved yet: the pane's session is the one on record.
	fake.paneSession = &backend.PaneSession{ID: old, PID: 830233, Status: backend.StatusProcessing}
	if info, _ := env.service.StatusInfo(ctx, "worker"); info.Status != service.AgentStatusProcessing || info.Event == service.EventSessionMoved {
		t.Fatalf("before the move: %s/%s", info.Status, info.Event)
	}

	// The move: the transcript says so, and the pane runs the new session.
	f, err := os.OpenFile(oldPath, os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	_, _ = f.WriteString(`{"type":"continued-in","sessionId":"` + old + `","continuedInSessionId":"moved"}` + "\n")
	f.Close()
	// A continued-in the pane doesn't run (another process took it) isn't followed.
	if info, _ := env.service.StatusInfo(ctx, "worker"); info.Event == service.EventSessionMoved {
		t.Fatalf("followed a session its pane doesn't run: %s", info.Detail)
	}
	fake.paneSession = &backend.PaneSession{ID: "moved", PID: 1776758, Status: backend.StatusIdle, Detail: "at its prompt"}
	fake.sessionID = "moved" // the registry lists the new session, idle
	fake.native = backend.StatusIdle

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusIdle || info.Source != service.StatusSourceNative {
		t.Fatalf("after the move: %s/%s (%s), want idle/native", info.Status, info.Source, info.Detail)
	}
	for key, want := range map[string]string{
		service.StateKeySessionID:  "moved",
		service.StateKeyTranscript: filepath.Join(dir, "moved.jsonl"),
		service.StateKeyResult:     "the reply", // the same conversation: its reply stands
		service.StateKeyEvent:      service.EventSessionMoved,
	} {
		if e, _ := env.store.GetState(ctx, "worker", key); e == nil || e.Value != want {
			t.Errorf("%s = %+v, want %q", key, e, want)
		}
	}
	// Followed once: the next check finds the session on record running.
	if info, _ := env.service.StatusInfo(ctx, "worker"); info.Status != service.AgentStatusIdle {
		t.Errorf("second check: %s", info.Status)
	}
}

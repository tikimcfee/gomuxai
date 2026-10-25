package service_test

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// hookAs sends a Claude hook payload for agent name, with the fake's session
// id, so native status applies.
func hookAs(t *testing.T, env *testEnv, fake *fakeHookBackend, name, payload string) {
	t.Helper()
	payload = strings.Replace(payload, "{", fmt.Sprintf(`{"session_id":%q,`, fake.sessionID), 1)
	if err := env.service.ReportHook(context.Background(), name, "", "", []byte(payload)); err != nil {
		t.Fatalf("ReportHook %s: %v", payload, err)
	}
}

func readHook(event, path string) string {
	return fmt.Sprintf(`{"hook_event_name":%q,"tool_name":"Read","tool_input":{"file_path":%q}}`, event, path)
}

// wantPrompt checks the status describes the prompt for Read of path, with
// detail suffix (e.g. "; 2 more queued").
func wantPrompt(t *testing.T, env *testEnv, name, path, suffix string) *service.StatusInfo {
	t.Helper()
	info, err := env.service.StatusInfo(context.Background(), name)
	if err != nil {
		t.Fatal(err)
	}
	wantDetail := "permission: Read: " + path + suffix
	wantInput := fmt.Sprintf(`{"file_path":%q}`, path)
	if info.Status != service.AgentStatusWaiting || info.Tool != "Read" || info.Detail != wantDetail || info.ToolInput != wantInput {
		t.Fatalf("status = %s, tool %q, detail %q, input %s\nwant waiting on Read, %q, %s", info.Status, info.Tool, info.Detail, info.ToolInput, wantDetail, wantInput)
	}
	return info
}

func pendingKeys(t *testing.T, env *testEnv, name string) int {
	t.Helper()
	entries, err := env.store.ListState(context.Background(), name)
	if err != nil {
		t.Fatal(err)
	}
	n := 0
	for _, e := range entries {
		if strings.HasPrefix(e.Key, "_prompt.") {
			n++
		}
	}
	return n
}

// Repro (real claude): four parallel Reads raise four PermissionRequests at
// once, and Claude shows them one at a time ("1 of 4"). Status named the
// last one raised (/etc/group) while the screen asked about the first, and
// once that was answered, its PostToolUse left status waiting with no
// detail or tool, so approve --expect couldn't match it.
func TestStatus_QueuedPromptsDescribeTheOneOnScreen(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/x/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")
	fake.nativeFound, fake.native = true, backend.StatusWaiting

	for _, p := range []string{"/a", "/b", "/c", "/d"} {
		hookAs(t, env, fake, "worker", readHook("PermissionRequest", p))
	}
	wantPrompt(t, env, "worker", "/a", "; 3 more queued")

	// approve answers the prompt on screen, and the next one shows while
	// the approved call runs.
	info, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{ExpectTool: "Read"})
	if err != nil || info.Detail != "permission: Read: /a; 3 more queued" {
		t.Fatalf("approve: answered %q (err %v), want the /a prompt", info.Detail, err)
	}
	wantPrompt(t, env, "worker", "/b", "; 2 more queued")
	hookAs(t, env, fake, "worker", readHook("PostToolUse", "/a"))
	wantPrompt(t, env, "worker", "/b", "; 2 more queued")

	// A person answers on screen: the call's PostToolUse ends its prompt.
	hookAs(t, env, fake, "worker", readHook("PostToolUse", "/b"))
	wantPrompt(t, env, "worker", "/c", "; 1 more queued")

	// Deny's Escape cancels the rest of the batch.
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{}); err != nil {
		t.Fatalf("deny: %v", err)
	}
	if n := pendingKeys(t, env, "worker"); n != 0 {
		t.Errorf("%d prompts still pending after deny, want 0", n)
	}
}

// Repro (real claude): a background subagent's Bash prompt ("Bash command ·
// from the general-purpose agent") was up when the main turn's Stop fired,
// which overwrote the report: status said waiting with no detail.
func TestStatus_SubagentPromptOutlivesMainTurn(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/x/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")
	fake.nativeFound, fake.native = true, backend.StatusWaiting

	hookAs(t, env, fake, "worker", `{"hook_event_name":"PermissionRequest","agent_id":"a1","agent_type":"general-purpose","tool_name":"Bash","tool_input":{"command":"rmdir sub1"}}`)
	hookAs(t, env, fake, "worker", `{"hook_event_name":"Stop","last_assistant_message":"started","background_tasks":[{"id":"a1","type":"subagent","status":"running"}]}`)
	hookAs(t, env, fake, "worker", `{"hook_event_name":"UserPromptSubmit"}`) // the main agent's own prompts only
	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil || info.Tool != "Bash" || info.Detail != "permission: Bash: rmdir sub1 (from the general-purpose agent)" {
		t.Fatalf("status: tool %q, detail %q (err %v), want the subagent's Bash prompt", info.Tool, info.Detail, err)
	}

	// Denying a subagent's call ends only that call; the turn goes on.
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{ExpectTool: "Bash"}); err != nil {
		t.Fatalf("deny: %v", err)
	}
	if st, _ := env.store.GetState(ctx, "worker", service.StateKeyStatus); st.Value != string(service.AgentStatusProcessing) {
		t.Errorf("recorded %s after denying a subagent's call, want processing", st.Value)
	}
	if n := pendingKeys(t, env, "worker"); n != 0 {
		t.Errorf("%d prompts still pending after deny, want 0", n)
	}
}

// Repro (real claude): deny, then send at once. claude agents --json still
// said waiting for about half a second, so send refused ("waiting on a
// prompt"). A deny that left nothing pending or on screen is believed over
// a native waiting, briefly; a dialog on screen still stops the send.
func TestSend_RightAfterDeny(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/x/agent"
	ctx := context.Background()
	fake := spawnFake(t, env, "worker")
	fake.nativeFound, fake.native = true, backend.StatusWaiting

	hookAs(t, env, fake, "worker", readHook("PermissionRequest", "/a"))
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{}); err != nil {
		t.Fatalf("deny: %v", err)
	}
	// Native still says waiting, and the dialog is still on screen: refuse.
	if _, err := env.service.Send(ctx, "worker", "hello"); err == nil {
		t.Error("sent while a dialog was on screen")
	}
	fake.promptGone = true
	status, err := env.service.Send(ctx, "worker", "do it without reading /a")
	if err != nil || status != service.AgentStatusIdle {
		t.Errorf("send right after deny: status %s, err %v; want sent, idle", status, err)
	}

	// A deny long ago doesn't outweigh native.
	old := time.Now().Add(-time.Minute)
	if err := env.store.SetStates(ctx, []*storage.StateEntry{
		{AgentName: "worker", Key: service.StateKeyStatus, Value: "idle", UpdatedAt: old},
		{AgentName: "worker", Key: service.StateKeyEvent, Value: "denied", UpdatedAt: old},
	}); err != nil {
		t.Fatal(err)
	}
	if st, _ := env.service.Status(ctx, "worker"); st != service.AgentStatusWaiting {
		t.Errorf("a minute after deny: status %s, want native's waiting", st)
	}
}

// A turn that ends with nothing running can't leave a prompt up, so
// prompts nobody answered through agent (Esc pressed by a person) go.
func TestStatus_StopEndsPendingPrompts(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/x/agent"
	fake := spawnFake(t, env, "worker")

	hookAs(t, env, fake, "worker", readHook("PermissionRequest", "/a"))
	hookAs(t, env, fake, "worker", `{"hook_event_name":"PermissionRequest","agent_id":"a1","tool_name":"Bash","tool_input":{"command":"ls"}}`)
	hookAs(t, env, fake, "worker", `{"hook_event_name":"UserPromptSubmit"}`)
	if n := pendingKeys(t, env, "worker"); n != 1 {
		t.Errorf("after the main agent took a prompt: %d pending, want only the subagent's", n)
	}
	hookAs(t, env, fake, "worker", `{"hook_event_name":"Stop","last_assistant_message":"done"}`)
	if n := pendingKeys(t, env, "worker"); n != 0 {
		t.Errorf("after Stop: %d pending, want 0", n)
	}
}

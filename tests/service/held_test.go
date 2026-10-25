package service_test

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// spawnHolding spawns a fake Claude agent with hooks wired, so messages to it
// can be held, and counts the deliverers sends start.
func spawnHolding(t *testing.T, env *testEnv, name string) (*fakeHookBackend, *int) {
	t.Helper()
	env.cfg.AgentBinary = "/opt/x/agent"
	fake := spawnFake(t, env, name)
	started := 0
	env.service.SetDelivererStarter(func(string) error { started++; return nil })
	return fake, &started
}

// busy puts the fake agent mid-turn: its prompt was submitted and native
// status says busy.
func busy(t *testing.T, env *testEnv, fake *fakeHookBackend, name string) {
	t.Helper()
	fake.nativeFound, fake.native, fake.promptGone = true, backend.StatusProcessing, true
	hookAs(t, env, fake, name, `{"hook_event_name":"UserPromptSubmit"}`)
}

// stopHook runs the Stop hook as the hook itself does, returning its output.
func stopHook(t *testing.T, env *testEnv, fake *fakeHookBackend, name, payload string) []byte {
	t.Helper()
	payload = strings.Replace(payload, "{", `{"session_id":"`+fake.sessionID+`",`, 1)
	out, err := env.service.HandleHook(context.Background(), name, "", "", []byte(payload))
	if err != nil {
		t.Fatalf("HandleHook %s: %v", payload, err)
	}
	return out
}

// additionalContext decodes a Stop hook's delivery output.
func additionalContext(t *testing.T, out []byte) string {
	t.Helper()
	var v struct {
		HookSpecificOutput struct {
			HookEventName     string `json:"hookEventName"`
			AdditionalContext string `json:"additionalContext"`
		} `json:"hookSpecificOutput"`
	}
	if err := json.Unmarshal(out, &v); err != nil || v.HookSpecificOutput.HookEventName != "Stop" {
		t.Fatalf("hook output %s: not a Stop hookSpecificOutput (%v)", out, err)
	}
	return v.HookSpecificOutput.AdditionalContext
}

func stateOf(t *testing.T, env *testEnv, name, key string) string {
	t.Helper()
	e, err := env.store.GetState(context.Background(), name, key)
	if err != nil || e == nil {
		return ""
	}
	return e.Value
}

// A message to a busy agent is held, and the Stop hook hands every held
// message over at once, oldest first, as additionalContext. The agent goes
// on with them, so the status stays processing (a new turn for wait --next)
// and the interim reply isn't the result; the next Stop finds nothing held
// and ends the turn as before.
func TestSend_HeldUntilStopDelivers(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake, started := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")

	res, err := env.service.Send(ctx, "worker", "[from lead@h] first", service.SendOptions{From: "lead"})
	if err != nil || !res.Held || res.HeldCount != 1 || res.Typed != 0 || res.Status != service.AgentStatusProcessing {
		t.Fatalf("send to a busy agent: %+v, err %v; want held", res, err)
	}
	res, err = env.service.Send(ctx, "worker", "[from a person on h] second", service.SendOptions{})
	if err != nil || !res.Held || res.HeldCount != 2 {
		t.Fatalf("second send: %+v, err %v; want held behind the first", res, err)
	}
	if *started != 2 {
		t.Errorf("started %d deliverers, want one per held send", *started)
	}
	if info, _ := env.service.StatusInfo(ctx, "worker"); info.Held != 2 {
		t.Errorf("status shows %d held, want 2", info.Held)
	}
	sentAt := stateOf(t, env, "worker", service.StateKeySentAt)

	// The status-only path never claims.
	hookAs(t, env, fake, "worker", `{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}`)
	if env.service.HeldCount(ctx, "worker") != 2 {
		t.Fatal("ReportHook delivered held messages")
	}

	out := stopHook(t, env, fake, "worker", `{"hook_event_name":"Stop","last_assistant_message":"interim"}`)
	want := "2 messages arrived while you were working, oldest first:\n\n[from lead@h] first\n\n[from a person on h] second"
	if got := additionalContext(t, out); got != want {
		t.Errorf("delivered %q, want %q", got, want)
	}
	if env.service.HeldCount(ctx, "worker") != 0 {
		t.Error("messages still held after delivery")
	}
	if st, ev := stateOf(t, env, "worker", service.StateKeyStatus), stateOf(t, env, "worker", service.StateKeyEvent); st != "processing" || ev != service.EventDelivered {
		t.Errorf("after delivery: %s/%s, want processing/delivered", st, ev)
	}
	if d := stateOf(t, env, "worker", service.StateKeyDetail); d != "2 held messages delivered" {
		t.Errorf("detail %q", d)
	}
	if r := stateOf(t, env, "worker", service.StateKeyResult); r != "" {
		t.Errorf("result %q stored for a turn that goes on", r)
	}
	if turnAt := stateOf(t, env, "worker", service.StateKeyTurnAt); turnAt <= sentAt {
		t.Errorf("turn_at %s not after sent_at %s: wait --next would miss the delivered step", turnAt, sentAt)
	}

	out = stopHook(t, env, fake, "worker", `{"hook_event_name":"Stop","last_assistant_message":"done"}`)
	if out != nil {
		t.Errorf("Stop with nothing held printed %s", out)
	}
	if st, r := stateOf(t, env, "worker", service.StateKeyStatus), stateOf(t, env, "worker", service.StateKeyResult); st != "idle" || r != "done" {
		t.Errorf("final Stop: %s, result %q; want idle, done", st, r)
	}
}

// One held message is introduced as one.
func TestStopHook_OneMessage(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	fake, _ := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")
	if _, err := env.service.Send(context.Background(), "worker", "[from lead@h] hi", service.SendOptions{}); err != nil {
		t.Fatal(err)
	}
	// A Stop with background work running delivers too: the prompt is free.
	out := stopHook(t, env, fake, "worker", `{"hook_event_name":"Stop","background_tasks":[{"id":"b1","type":"shell","status":"running"}]}`)
	if got := additionalContext(t, out); got != "A message arrived while you were working:\n\n[from lead@h] hi" {
		t.Errorf("delivered %q", got)
	}
}

// The turn ended (its Stop found nothing held) before the message was
// stored, though native status, lagging, still says busy: send sees the
// Stop's report after storing, and types the message itself. A Stop that
// lands between send's status check and the store takes the same path.
func TestSend_TurnEndedDuringSend(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake, started := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")
	if out := stopHook(t, env, fake, "worker", `{"hook_event_name":"Stop","last_assistant_message":"done"}`); out != nil {
		t.Fatalf("nothing held, yet Stop printed %s", out)
	}
	// Native still says busy.
	res, err := env.service.Send(ctx, "worker", "echo raced-$((6*7))", service.SendOptions{})
	if err != nil || res.Held || res.Typed != 1 {
		t.Fatalf("send as the turn ended: %+v, err %v; want typed", res, err)
	}
	if *started != 0 {
		t.Errorf("started a deliverer for a typed message")
	}
	if env.service.HeldCount(ctx, "worker") != 0 {
		t.Error("message left held")
	}
	waitForOutput(t, env, "worker", "raced-42")
}

// After a Stop that left background work running, the prompt takes input
// though native status says busy: type, don't hold.
func TestSend_TypesAfterBackgroundStop(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	fake, _ := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")
	hookAs(t, env, fake, "worker", `{"hook_event_name":"Stop","background_tasks":[{"id":"b1","type":"shell","status":"running"}]}`)
	res, err := env.service.Send(context.Background(), "worker", "echo bg", service.SendOptions{})
	if err != nil || res.Held || res.Typed != 1 {
		t.Errorf("send during background work: %+v, err %v; want typed", res, err)
	}
}

// --now, and slash commands, are typed into a busy agent as before.
func TestSend_NowAndSlashCommandsAreTyped(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake, _ := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")
	for _, tc := range []struct {
		msg  string
		opts service.SendOptions
	}{
		{"echo now", service.SendOptions{Now: true}},
		{"/compact", service.SendOptions{}},
	} {
		res, err := env.service.Send(ctx, "worker", tc.msg, tc.opts)
		if err != nil || res.Held || res.Typed != 1 {
			t.Errorf("send %q %+v: %+v, err %v; want typed", tc.msg, tc.opts, res, err)
		}
	}
}

// A message to an agent at its prompt is typed after the ones still held
// (a turn that ended with no Stop), as one prompt, oldest first.
func TestSend_TypesHeldMessagesFirst(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake, _ := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")
	for _, m := range []string{"echo one-$((1))", "echo two-$((2))"} {
		if _, err := env.service.Send(ctx, "worker", m, service.SendOptions{}); err != nil {
			t.Fatal(err)
		}
	}
	fake.native = backend.StatusIdle // Esc: no hook fired
	res, err := env.service.Send(ctx, "worker", "echo three-$((3))", service.SendOptions{})
	if err != nil || res.Held || res.Typed != 3 {
		t.Fatalf("send at the prompt: %+v, err %v; want 3 typed", res, err)
	}
	screen := waitForOutput(t, env, "worker", "three-3")
	if i, j, k := strings.Index(screen, "one-1"), strings.Index(screen, "two-2"), strings.Index(screen, "three-3"); !(i >= 0 && i < j && j < k) {
		t.Errorf("typed out of order:\n%s", screen)
	}
}

// The deliverer types held messages once the agent is at its prompt with
// them still held (an interrupted turn: the report says processing, native
// idle), after the grace; it reports nothing held, and refuses an agent
// that has exited.
func TestDeliverHeld(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake, _ := spawnHolding(t, env, "worker")

	if _, err := env.service.DeliverHeld(ctx, "worker", service.DeliverOptions{}); !errors.Is(err, service.ErrNothingHeld) {
		t.Errorf("nothing held: err %v, want ErrNothingHeld", err)
	}

	busy(t, env, fake, "worker")
	if _, err := env.service.Send(ctx, "worker", "echo delivered-$((40+2))", service.SendOptions{}); err != nil {
		t.Fatal(err)
	}
	// Still busy: one check types nothing.
	if n, err := env.service.DeliverHeld(ctx, "worker", service.DeliverOptions{}); n != 0 || err != nil {
		t.Fatalf("busy agent: typed %d, err %v", n, err)
	}

	fake.native = backend.StatusIdle
	start := time.Now()
	n, err := env.service.DeliverHeld(ctx, "worker", service.DeliverOptions{Wait: true, Interval: 50 * time.Millisecond, Grace: 300 * time.Millisecond})
	if n != 1 || err != nil {
		t.Fatalf("interrupted turn: typed %d, err %v; want 1", n, err)
	}
	if waited := time.Since(start); waited < 300*time.Millisecond {
		t.Errorf("typed after %s, before the grace", waited)
	}
	waitForOutput(t, env, "worker", "delivered-42")

	busy(t, env, fake, "worker")
	if _, err := env.service.Send(ctx, "worker", "echo never", service.SendOptions{}); err != nil {
		t.Fatal(err)
	}
	fake.nativeFound, fake.nativeGone = false, true // the CLI died
	if _, err := env.service.DeliverHeld(ctx, "worker", service.DeliverOptions{Wait: true, Interval: 50 * time.Millisecond}); err == nil || !strings.Contains(err.Error(), "exited") {
		t.Errorf("exited agent: err %v", err)
	}
}

// Held messages are never typed into a person's half-written input: the
// deliverer and send wait for an empty input line, and a send that finds
// messages held and the line in use joins them.
func TestHeld_WaitsForAnEmptyInputLine(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake, _ := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")
	if _, err := env.service.Send(ctx, "worker", "echo first-$((1))", service.SendOptions{}); err != nil {
		t.Fatal(err)
	}
	fake.native = backend.StatusIdle // Esc: no hook fired
	fake.input = "half a thought"

	var blocked *service.InputBlockedError
	if n, err := env.service.DeliverHeld(ctx, "worker", service.DeliverOptions{}); n != 0 || !errors.As(err, &blocked) {
		t.Fatalf("deliver into a half-typed input: typed %d, err %v; want InputBlockedError", n, err)
	}
	if why := env.service.InputBlocked(ctx, "worker"); why != "it has text in it" {
		t.Errorf("InputBlocked = %q", why)
	}
	res, err := env.service.Send(ctx, "worker", "echo second-$((2))", service.SendOptions{})
	if err != nil || !res.Held || res.HeldCount != 2 || res.InputBlocked == "" {
		t.Fatalf("send with messages held and the input in use: %+v, err %v; want held behind the first", res, err)
	}
	fake.input = ""
	start := time.Now()
	n, err := env.service.DeliverHeld(ctx, "worker", service.DeliverOptions{Wait: true, Interval: 50 * time.Millisecond, Grace: 200 * time.Millisecond})
	if n != 2 || err != nil {
		t.Fatalf("input cleared: typed %d, err %v; want 2", n, err)
	}
	if time.Since(start) < 200*time.Millisecond {
		t.Error("typed before the grace")
	}
	screen := waitForOutput(t, env, "worker", "second-2")
	if i, j := strings.Index(screen, "first-1"), strings.Index(screen, "second-2"); !(i >= 0 && i < j) {
		t.Errorf("typed out of order:\n%s", screen)
	}

	// With nothing held too, a send to an agent at its prompt holds rather
	// than type into a person's input; --now types anyway.
	fake.input = "more"
	if res, err := env.service.Send(ctx, "worker", "echo third", service.SendOptions{}); err != nil || !res.Held || res.InputBlocked == "" {
		t.Errorf("send with nothing held, input in use: %+v, err %v; want held", res, err)
	}
	if res, err := env.service.Send(ctx, "worker", "echo fourth", service.SendOptions{Now: true}); err != nil || res.Typed != 1 {
		t.Errorf("send --now, input in use: %+v, err %v; want typed", res, err)
	}
}

// The deliverer's grace starts when the input line empties, not at the
// first check after a blocked attempt.
func TestDeliverHeld_GraceStartsWhenInputEmpties(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake, _ := spawnHolding(t, env, "worker")
	busy(t, env, fake, "worker")
	if _, err := env.service.Send(ctx, "worker", "echo late", service.SendOptions{}); err != nil {
		t.Fatal(err)
	}
	cleared := time.Now().Add(400 * time.Millisecond)
	fake.native, fake.input, fake.inputUntil = backend.StatusIdle, "typing", cleared

	n, err := env.service.DeliverHeld(ctx, "worker", service.DeliverOptions{Wait: true, Interval: 20 * time.Millisecond, Grace: 300 * time.Millisecond})
	if n != 1 || err != nil {
		t.Fatalf("typed %d, err %v; want 1", n, err)
	}
	if since := time.Since(cleared); since < 300*time.Millisecond {
		t.Errorf("typed %s after the input line emptied, before the grace", since)
	}
}

// waitForOutput waits for want on the agent's screen and returns the screen.
func waitForOutput(t *testing.T, env *testEnv, name, want string) string {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		screen, _ := env.service.Capture(context.Background(), name, 50, false)
		if strings.Contains(screen, want) {
			return screen
		}
		if time.Now().After(deadline) {
			t.Fatalf("%q never showed on screen:\n%s", want, screen)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

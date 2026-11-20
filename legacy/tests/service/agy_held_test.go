package service_test

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// fakeAgyScreen is the agy backend with its screen readings set by the
// test: the test session is a plain shell, which draws none of agy's.
type fakeAgyScreen struct {
	*namedagents.AgyBackend
	atPrompt bool
	how      string
	question bool
	input    string
}

func (f *fakeAgyScreen) Name() string { return "fakeagyscreen" }

func (f *fakeAgyScreen) LaunchCommandWithHooks(reportCmd, sessionID string) string { return "true" }

func (f *fakeAgyScreen) AtPrompt(string) (bool, string)        { return f.atPrompt, f.how }
func (f *fakeAgyScreen) QuestionPromptVisible(string) bool     { return f.question }
func (f *fakeAgyScreen) PermissionPromptVisible(string) bool   { return false }
func (f *fakeAgyScreen) InputText(string) (string, bool)       { return f.input, true }
func (f *fakeAgyScreen) AppendPromptFlag(cmd, _ string) string { return cmd }

// spawnAgyHolding spawns a fake agy agent with hooks wired, so messages to
// it can be held, and puts it mid-turn.
func spawnAgyHolding(t *testing.T, env *testEnv, name string) *fakeAgyScreen {
	t.Helper()
	env.cfg.AgentBinary = "/opt/x/agent"
	fake := &fakeAgyScreen{AgyBackend: namedagents.NewAgyBackend()}
	env.backends.RegisterFactory("fakeagyscreen", func(backend.ProviderConfig) backend.Provider { return fake })
	if _, err := env.service.Spawn(context.Background(), service.SpawnOptions{Name: name, Backend: "fakeagyscreen"}); err != nil {
		t.Fatalf("spawn: %v", err)
	}
	env.service.SetDelivererStarter(func(string) error { return nil })
	agyHook(t, env, name, "PreInvocation", `{"conversationId":"c1","invocationNum":0}`)
	return fake
}

// agyHook runs an agy hook as the hook itself does, returning its output.
func agyHook(t *testing.T, env *testEnv, name, event, payload string) []byte {
	t.Helper()
	out, err := env.service.HandleHook(context.Background(), name, "", event, []byte(payload))
	if err != nil {
		t.Fatalf("HandleHook %s %s: %v", event, payload, err)
	}
	return out
}

// reportedLongAgo makes the last report older than the time the screen
// gets to catch up with a hook.
func reportedLongAgo(t *testing.T, env *testEnv, name string) {
	t.Helper()
	ctx := context.Background()
	e, err := env.store.GetState(ctx, name, service.StateKeyStatus)
	if err != nil || e == nil {
		t.Fatalf("no reported status: %v", err)
	}
	e.UpdatedAt = time.Now().Add(-10 * time.Second)
	if err := env.store.SetState(ctx, e); err != nil {
		t.Fatal(err)
	}
}

// A message to a busy agy agent is held, and the next PostInvocation hands
// every held message over as its own userMessage, with force_continue. With
// nothing held, PostInvocation prints nothing and records nothing.
func TestAgy_HeldUntilPostInvocationDelivers(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnAgyHolding(t, env, "agyw")
	agyHook(t, env, "agyw", "PreToolUse", `{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"sleep 5"}}}`)

	if out := agyHook(t, env, "agyw", "PostInvocation", `{"conversationId":"c1","invocationNum":0}`); out != nil {
		t.Errorf("PostInvocation with nothing held printed %s", out)
	}
	if d := stateOf(t, env, "agyw", service.StateKeyDetail); d != "run_command: sleep 5" {
		t.Errorf("PostInvocation with nothing held recorded detail %q", d)
	}

	for i, m := range []string{"[from lead@h] first", "[from a person on h] second"} {
		res, err := env.service.Send(ctx, "agyw", m, service.SendOptions{From: "lead"})
		if err != nil || !res.Held || res.HeldCount != i+1 || res.HeldUntil != "its current step ends" {
			t.Fatalf("send %d to a busy agy agent: %+v, err %v; want held", i+1, res, err)
		}
	}
	sentAt := stateOf(t, env, "agyw", service.StateKeySentAt)
	// Its other hooks only report.
	for _, ev := range []string{"PostToolUse", "Stop"} {
		if out := agyHook(t, env, "agyw", ev, `{"conversationId":"c1","fullyIdle":false,"toolCall":{"name":"run_command","args":{"CommandLine":"sleep 5"}}}`); out != nil {
			t.Errorf("%s printed %s", ev, out)
		}
	}
	agyHook(t, env, "agyw", "PreInvocation", `{"conversationId":"c1","invocationNum":1}`)

	out := agyHook(t, env, "agyw", "PostInvocation", `{"conversationId":"c1","invocationNum":1}`)
	var v struct {
		InjectSteps []struct {
			UserMessage string `json:"userMessage"`
		} `json:"injectSteps"`
		TerminationBehavior string `json:"terminationBehavior"`
	}
	if err := json.Unmarshal(out, &v); err != nil || len(v.InjectSteps) != 2 || v.TerminationBehavior != "force_continue" ||
		v.InjectSteps[0].UserMessage != "[from lead@h] first" || v.InjectSteps[1].UserMessage != "[from a person on h] second" {
		t.Fatalf("delivery output %s (err %v)", out, err)
	}
	if env.service.HeldCount(ctx, "agyw") != 0 {
		t.Error("messages still held after delivery")
	}
	if st, ev, d := stateOf(t, env, "agyw", service.StateKeyStatus), stateOf(t, env, "agyw", service.StateKeyEvent), stateOf(t, env, "agyw", service.StateKeyDetail); st != "processing" || ev != service.EventDelivered || d != "2 held messages delivered" {
		t.Errorf("after delivery: %s/%s/%q, want processing/delivered/2 held messages delivered", st, ev, d)
	}
	if turnAt := stateOf(t, env, "agyw", service.StateKeyTurnAt); turnAt <= sentAt {
		t.Errorf("turn_at %s not after sent_at %s: wait --next would miss the delivered step", turnAt, sentAt)
	}

	if out := agyHook(t, env, "agyw", "PostInvocation", `{"conversationId":"c1","invocationNum":2}`); out != nil {
		t.Errorf("second PostInvocation printed %s", out)
	}
	agyHook(t, env, "agyw", "Stop", `{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}`)
	if st := stateOf(t, env, "agyw", service.StateKeyStatus); st != "idle" {
		t.Errorf("after Stop: %s, want idle", st)
	}
}

// Esc and Ctrl+C fire no hook: once the last report has stood a while and
// agy is back at its prompt, it is idle, and its held messages get typed.
func TestAgy_InterruptedTurnIsIdle(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake := spawnAgyHolding(t, env, "agyi")
	agyHook(t, env, "agyi", "PreToolUse", `{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"sleep 30"}}}`)
	if res, err := env.service.Send(ctx, "agyi", "echo typed-$((40+2))", service.SendOptions{}); err != nil || !res.Held {
		t.Fatalf("send to a busy agent: %+v, %v", res, err)
	}

	fake.atPrompt, fake.how = true, "interrupted"
	// Right after a hook, the screen may not have caught up.
	if info, _ := env.service.StatusInfo(ctx, "agyi"); info.Status != service.AgentStatusProcessing {
		t.Errorf("just after a hook: %s, want processing", info.Status)
	}
	reportedLongAgo(t, env, "agyi")
	info, err := env.service.StatusInfo(ctx, "agyi")
	if err != nil || info.Status != service.AgentStatusIdle || info.Detail != "interrupted; at its prompt" || info.Source != service.StatusSourceScrape {
		t.Fatalf("interrupted: %+v, %v; want idle from the screen", info, err)
	}
	if info.ReportedStatus != service.AgentStatusProcessing {
		t.Errorf("reported %s, want the last report kept", info.ReportedStatus)
	}

	fake.input = "a person's text"
	if n, err := env.service.DeliverHeld(ctx, "agyi", service.DeliverOptions{}); n != 0 || err == nil {
		t.Errorf("text in the input line: typed %d, err %v; want refused", n, err)
	}
	fake.input = ""
	if n, err := env.service.DeliverHeld(ctx, "agyi", service.DeliverOptions{}); n != 1 || err != nil {
		t.Fatalf("at its prompt: typed %d, err %v; want 1", n, err)
	}
	waitForOutput(t, env, "agyi", "typed-42")

	// A Stop that left background work running isn't overruled.
	agyHook(t, env, "agyi", "Stop", `{"conversationId":"c1","fullyIdle":false}`)
	reportedLongAgo(t, env, "agyi")
	fake.how = ""
	if info, _ := env.service.StatusInfo(ctx, "agyi"); info.Status != service.AgentStatusProcessing {
		t.Errorf("background work: %s, want processing", info.Status)
	}
	// Without the interrupted line, it says what it knows.
	agyHook(t, env, "agyi", "PreInvocation", `{"conversationId":"c1","invocationNum":0}`)
	reportedLongAgo(t, env, "agyi")
	if info, _ := env.service.StatusInfo(ctx, "agyi"); info.Status != service.AgentStatusIdle || !strings.Contains(info.Detail, "no hook reported") {
		t.Errorf("at its prompt: %s (%s)", info.Status, info.Detail)
	}
}

// ask_question's dialog has no hook: after its PreToolUse, the dialog on
// screen makes the agent waiting, so a send holds instead of answering it,
// and approve/deny and typed sends refuse.
func TestAgy_QuestionIsWaiting(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake := spawnAgyHolding(t, env, "agyq")
	agyHook(t, env, "agyq", "PreToolUse", `{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[{"options":["red","green"],"question":"Which color?"}]}}}`)

	if info, _ := env.service.StatusInfo(ctx, "agyq"); info.Status != service.AgentStatusProcessing {
		t.Errorf("no dialog on screen yet: %s, want processing", info.Status)
	}
	fake.question = true
	info, err := env.service.StatusInfo(ctx, "agyq")
	if err != nil || info.Status != service.AgentStatusWaiting || info.Detail != "question: ask_question: Which color? (red, green)" || info.Tool != "ask_question" {
		t.Fatalf("question on screen: %+v, %v", info, err)
	}

	res, err := env.service.Send(ctx, "agyq", "[from lead@h] hi", service.SendOptions{})
	if err != nil || !res.Held || res.Status != service.AgentStatusWaiting {
		t.Errorf("send at the question: %+v, %v; want held", res, err)
	}
	if _, err := env.service.Send(ctx, "agyq", "[from lead@h] now", service.SendOptions{Now: true}); err == nil || !strings.Contains(err.Error(), "pick an answer") {
		t.Errorf("send --now at the question: %v; want refused", err)
	}
	if _, err := env.service.RespondPermission(ctx, "agyq", backend.PermissionApprove, service.PermissionOptions{}); err == nil || !strings.Contains(err.Error(), "question for the user") {
		t.Errorf("approve at the question: %v; want refused", err)
	}
	// Even after the turn's end: the deliverer doesn't type into it.
	fake.atPrompt = false
	agyHook(t, env, "agyq", "Stop", `{"conversationId":"c1","fullyIdle":true}`)
	if n, err := env.service.DeliverHeld(ctx, "agyq", service.DeliverOptions{}); n != 0 || err == nil {
		t.Errorf("deliver at a question: typed %d, err %v; want refused", n, err)
	}
}

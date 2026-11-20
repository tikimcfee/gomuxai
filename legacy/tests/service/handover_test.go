package service_test

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// promptSubmitHook runs Claude's UserPromptSubmit hook for worker, as the
// agent's hook does, and returns its output.
func promptSubmitHook(t *testing.T, svc *service.AgentService, spawnID string) func(string) []byte {
	return func(prompt string) []byte {
		payload, _ := json.Marshal(map[string]string{"hook_event_name": "UserPromptSubmit", "prompt": prompt})
		out, err := svc.HandleHook(context.Background(), "worker", spawnID, "", payload)
		if err != nil {
			t.Errorf("UserPromptSubmit hook: %v", err)
		}
		return out
	}
}

// setupHookedClaude is setupClaudeTerminal with worker's hooks wired at
// spawn (so it holds messages), and term running its UserPromptSubmit hook
// unless noHook.
func setupHookedClaude(t *testing.T, term *claudeTerminal, noHook bool) (*service.AgentService, *storage.SQLiteStorage) {
	t.Helper()
	svc, store := setupClaudeTerminal(t, term, submitTestWait)
	ctx := context.Background()
	// No session id: the fake has no registry for native status to read.
	if err := store.SetState(ctx, &storage.StateEntry{AgentName: "worker", Key: service.StateKeySpawnID, Value: "s1", UpdatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if err := svc.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop"}); err != nil {
		t.Fatal(err)
	}
	svc.SetDelivererStarter(func(string) error { return nil })
	if !noHook {
		term.onSubmit = promptSubmitHook(t, svc, "s1")
	}
	return svc, store
}

var handoverLine = regexp.MustCompile(`^(.*) \[gomuxai message #([0-9]+), ([0-9.]+ K?B), ([0-9]+ lines?): the whole message follows as hook context\]$`)

// A message Claude would take as a paste (long, or 3+ line breaks) goes to
// an agent whose hook reaches it as one short line, and the hook hands the
// model its whole text with the prompt: whatever its size and characters.
func TestHandover_LongMessageArrivesAsHookContext(t *testing.T) {
	ctx := context.Background()
	strange := "quotes \"double\" 'single', $HOME $(id) `uname`, héllo wörld 日本語 🙂, a tab\tand \\backslash"
	for _, size := range []int{1000, 10000, 100000} {
		var b strings.Builder
		b.WriteString("[from lead@hub] Do these things, in order:\n" + strange + "\n")
		for i := 1; b.Len() < size; i++ {
			fmt.Fprintf(&b, "%d. step %d of the plan, with ~tildes~ and <angle> & ampersands\n", i, i)
		}
		msg := strings.TrimRight(b.String(), "\n")

		term := &claudeTerminal{}
		svc, store := setupHookedClaude(t, term, false)
		res, err := svc.Send(ctx, "worker", msg, service.SendOptions{From: "lead"})
		if err != nil || res.Held || res.Typed != 1 || res.Unchecked != "" {
			t.Fatalf("%d bytes: Send = %+v, %v; want it typed", len(msg), res, err)
		}
		_, submitted, box := term.state()
		if len(submitted) != 1 || box != "" {
			t.Fatalf("%d bytes: submitted %q, box %q; want one prompt", len(msg), submitted, box)
		}
		line := submitted[0]
		m := handoverLine.FindStringSubmatch(line)
		if m == nil || strings.Contains(line, "\n") || utf8.RuneCountInString(line) > 400 {
			t.Fatalf("%d bytes: typed %q; want one short handover line", len(msg), line)
		}
		if m[1] != "[from lead@hub] Do these things, in order: …" {
			t.Errorf("line starts %q; want the message's first line", m[1])
		}
		got := term.got()
		want := fmt.Sprintf("%s\ngomuxai message #%s, whole:\n\n%s", line, m[2], msg)
		if len(got) != 1 || got[0] != want {
			t.Errorf("%d bytes: the model got %d bytes, want %d: the line, then the whole message", len(msg), len(got[0]), len(want))
		}
		var id int64
		fmt.Sscan(m[2], &id)
		if st, err := store.GetMessageStatus(ctx, id); err != nil || st != storage.MessageStatusDelivered {
			t.Errorf("message status = %q, %v; want delivered", st, err)
		}
	}
}

// Short single-line messages, and slash commands, are typed as they are.
func TestHandover_ShortMessagesAreTyped(t *testing.T) {
	ctx := context.Background()
	for _, msg := range []string{
		"[from lead@hub] run the tests",
		"[from lead@hub] two lines:\nthe second",
		"/model claude-fable-5-1",
		"[from lead@hub] " + strings.Repeat("é", 600), // 600 characters, 1200 bytes
	} {
		term := &claudeTerminal{}
		svc, _ := setupHookedClaude(t, term, false)
		if _, err := svc.Send(ctx, "worker", msg, service.SendOptions{}); err != nil {
			t.Fatalf("%.30q: %v", msg, err)
		}
		if got := term.got(); len(got) != 1 || got[0] != msg {
			t.Errorf("%.30q: the model got %q; want it as typed", msg, got)
		}
	}
}

// Held messages typed together, oldest first, are one long prompt: handed
// over the same way.
func TestHandover_HeldMessagesTypedTogether(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{}
	svc, store := setupHookedClaude(t, term, false)
	for _, m := range []string{"[from a@h] one", "[from b@h] two"} {
		if err := store.CreateMessage(ctx, &storage.Message{SenderID: "x", ReceiverID: "worker", Content: m, Status: storage.MessageStatusPending, CreatedAt: time.Now()}); err != nil {
			t.Fatal(err)
		}
	}
	res, err := svc.Send(ctx, "worker", "[from c@h] three", service.SendOptions{})
	if err != nil || res.Typed != 3 {
		t.Fatalf("Send = %+v, %v; want 3 typed", res, err)
	}
	got := term.got()
	if len(got) != 1 || !strings.HasSuffix(got[0], "\n\n[from a@h] one\n\n[from b@h] two\n\n[from c@h] three") {
		t.Errorf("the model got %q; want all three, oldest first", got)
	}
}

// If the hook never takes the message, send says the agent got only the
// line, and a late hook hands over nothing; held messages typed with it
// aren't held again, so the deliverer can't type the line over and over.
func TestHandover_HookMissingIsReported(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{}
	svc, store := setupHookedClaude(t, term, true)
	if err := store.CreateMessage(ctx, &storage.Message{SenderID: "x", ReceiverID: "worker", Content: "[from a@h] held", Status: storage.MessageStatusPending, CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	msg := "[from lead@hub] " + strings.Repeat("long ", 300)
	start := time.Now()
	_, err := svc.Send(ctx, "worker", msg, service.SendOptions{})
	var he *service.HandoverError
	if !errors.As(err, &he) || !strings.Contains(err.Error(), "got only that line") {
		t.Fatalf("Send err = %v; want a HandoverError", err)
	}
	if waited := time.Since(start); waited < 3*submitTestWait {
		t.Errorf("gave up after %s; want 3 SubmitWaits", waited)
	}
	if st, _ := store.GetMessageStatus(ctx, he.ID); st != storage.MessageStatusFailed {
		t.Errorf("message status = %q; want failed", st)
	}
	if n := svc.HeldCount(ctx, "worker"); n != 0 {
		t.Errorf("%d held after the failed handover; want none", n)
	}
	_, submitted, _ := term.state()
	if out := promptSubmitHook(t, svc, "s1")(submitted[0]); out != nil {
		t.Errorf("a late hook handed over %s; want nothing", out)
	}
}

// A line Enter didn't submit sits in the input box: whoever submits it
// later gets the whole message from the hook.
func TestHandover_UnsubmittedLineStillHandsOver(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{deafEnter: true}
	svc, _ := setupHookedClaude(t, term, false)
	msg := "[from lead@hub] a\nb\nc\nd"
	_, err := svc.Send(ctx, "worker", msg, service.SendOptions{})
	var ue *service.UnsubmittedError
	if !errors.As(err, &ue) {
		t.Fatalf("Send err = %v; want UnsubmittedError", err)
	}
	term.mu.Lock()
	term.deafEnter = false
	term.mu.Unlock()
	if err := term.SendSpecialKey("", "Enter"); err != nil {
		t.Fatal(err)
	}
	if got := term.got(); len(got) != 1 || !strings.HasSuffix(got[0], "\n\n"+msg) {
		t.Errorf("the model got %q; want the whole message once submitted", got)
	}
}

// Only the prompt naming a message hands it over, and only to the agent it
// was sent to.
func TestHandover_TokenNamesTheAgentsOwnMessage(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{}
	svc, store := setupHookedClaude(t, term, false)
	other := &storage.Message{SenderID: "x", ReceiverID: "someone-else", Content: "secret", Status: storage.MessageStatusHandover, CreatedAt: time.Now()}
	if err := store.CreateMessage(ctx, other); err != nil {
		t.Fatal(err)
	}
	hook := promptSubmitHook(t, svc, "s1")
	for _, prompt := range []string{
		fmt.Sprintf("x [gomuxai message #%d, 1 KB, 3 lines: the whole message follows as hook context]", other.ID),
		"just a prompt",
	} {
		if out := hook(prompt); out != nil {
			t.Errorf("prompt %q got hook output %s; want none", prompt, out)
		}
	}
	if st, _ := store.GetMessageStatus(ctx, other.ID); st != storage.MessageStatusHandover {
		t.Errorf("another agent's message became %q", st)
	}
}

package service_test

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// claudeTerminal stands in for tmux and a Claude pane behind it, as far
// as typing a message goes: what is written to the pane waits there until
// Claude reads it, and Claude reads everything that has arrived at once.
// A read that brings text and an Enter together is a paste, where the
// Enter is a newline, and Claude then eats the next Enter (both as seen
// with Claude Code v2.1.310). Screens are synthesized like those in
// tests/backend's TestClaudeInputText.
type claudeTerminal struct {
	tmux.TmuxOps // only the methods below are used

	mu        sync.Mutex
	unread    string   // written to the pane, not read by Claude yet
	stalled   int      // captures Claude sits out before reading again
	box       []string // its input box's lines
	eatEnter  bool     // the next Enter does nothing
	deafEnter bool     // no Enter submits (to see what send says then)
	dialogOn  int      // the Enter after which a dialog replaces the box (1-based)
	dialog    bool
	enters    int
	submitted []string
	pastes    int
}

func (c *claudeTerminal) SessionExists(string) (bool, error) { return true, nil }

func (c *claudeTerminal) SendKeys(_ string, text string) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.unread += text
	c.read()
	return nil
}

func (c *claudeTerminal) SendSpecialKey(_ string, key string) error {
	if key != "Enter" {
		return fmt.Errorf("unexpected key %q", key)
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.enters++
	c.unread += "\r"
	c.read()
	return nil
}

func (c *claudeTerminal) CaptureStyled(string, int) (string, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.stalled > 0 {
		c.stalled--
	}
	c.read()
	return c.screen(), nil
}

func (c *claudeTerminal) Capture(session string, lines int) (string, error) {
	return c.CaptureStyled(session, lines)
}

// read has Claude read what has arrived, unless it is stalled.
func (c *claudeTerminal) read() {
	if c.stalled > 0 || c.unread == "" {
		return
	}
	chunk := c.unread
	c.unread = ""
	if strings.Trim(chunk, "\r") == "" {
		for range chunk {
			c.enter()
		}
		return
	}
	if strings.Contains(chunk, "\r") {
		c.eatEnter = true // the Enter came with the text: a newline
	}
	text := strings.ReplaceAll(chunk, "\r", "\n")
	if strings.Contains(strings.TrimRight(text, "\n"), "\n") || len(text) > 200 {
		c.pastes++
		c.box = append(c.box, fmt.Sprintf("[Pasted text #%d +%d lines]", c.pastes, strings.Count(text, "\n")))
		return
	}
	if len(c.box) == 0 {
		c.box = []string{""}
	}
	lines := strings.Split(text, "\n")
	c.box[len(c.box)-1] += lines[0]
	c.box = append(c.box, lines[1:]...)
}

// enter is Claude reading an Enter on its own.
func (c *claudeTerminal) enter() {
	if c.dialogOn > 0 && c.enters >= c.dialogOn {
		c.dialog = true
	}
	switch {
	case c.eatEnter:
		c.eatEnter = false
	case c.deafEnter || c.dialog:
	case strings.TrimSpace(strings.Join(c.box, "")) != "":
		c.submitted = append(c.submitted, strings.TrimRight(strings.Join(c.box, "\n"), "\n"))
		c.box = nil
	}
}

func (c *claudeTerminal) screen() string {
	if c.dialog {
		return "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel\n"
	}
	rule := "\x1b[38;5;244m" + strings.Repeat("─", 60)
	lines := []string{"● ok", "", rule}
	for i, l := range append(c.box, "")[:max(len(c.box), 1)] {
		if i == 0 {
			l = "\x1b[39m❯ " + l
		} else {
			l = "  " + l
		}
		lines = append(lines, l)
	}
	lines = append(lines, rule, "\x1b[39m  \x1b[38;5;220m⏵⏵ auto mode on\x1b[39m")
	return strings.Join(lines, "\n") + "\n"
}

func (c *claudeTerminal) state() (enters int, submitted []string, box string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.enters, c.submitted, strings.TrimSpace(strings.Join(c.box, "\n"))
}

// setupClaudeTerminal returns a service whose one agent, "worker", is a
// Claude behind term, and its store.
func setupClaudeTerminal(t *testing.T, term *claudeTerminal, wait time.Duration) (*service.AgentService, *storage.SQLiteStorage) {
	t.Helper()
	dir := t.TempDir()
	store, err := storage.NewSQLiteStorage(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { store.Close() })
	cfg := &config.Config{Backend: "claude", DatabasePath: filepath.Join(dir, "test.db"), LogDir: filepath.Join(dir, "logs"),
		StateDir: filepath.Join(dir, "state"), SessionPrefix: "test-", SubmitWait: wait}
	backends := backend.NewRegistry()
	backends.RegisterFactory("claude", func(backend.ProviderConfig) backend.Provider { return namedagents.NewClaudeBackend() })
	if err := store.CreateAgent(context.Background(), &storage.Agent{Name: "worker", SessionName: "test-worker", Backend: "claude", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	return service.NewAgentService(cfg, store, term, backends), store
}

const submitTestWait = 300 * time.Millisecond

// A message typed while Claude is busy reached it in one read with its
// Enter, which became a newline: the message sat in the input box until a
// person pressed Enter. Send now waits for the box to show the text before
// pressing Enter.
func TestSubmit_WaitsForTheBoxToShowTheText(t *testing.T) {
	for _, msg := range []string{
		"[from lead@hub] run the tests",
		"[from lead@hub] Screenshot:\n/tmp/shot.png\nlook at it",
		"[from lead@hub] " + strings.Repeat("a long line ", 40),
	} {
		// Without looking, as before: the stall merges text and Enter.
		term := &claudeTerminal{stalled: 3}
		svc, _ := setupClaudeTerminal(t, term, 0)
		if err := svc.SendUnchecked(context.Background(), "worker", msg); err != nil {
			t.Fatal(err)
		}
		term.CaptureStyled("", 0)
		term.CaptureStyled("", 0)
		term.CaptureStyled("", 0)
		if _, submitted, box := term.state(); len(submitted) != 0 || box == "" {
			t.Fatalf("the fake should reproduce the bug: submitted %q, box %q", submitted, box)
		}

		term = &claudeTerminal{stalled: 3}
		svc, _ = setupClaudeTerminal(t, term, submitTestWait)
		if err := svc.SendUnchecked(context.Background(), "worker", msg); err != nil {
			t.Fatalf("%q: %v", msg, err)
		}
		enters, submitted, box := term.state()
		if len(submitted) != 1 || box != "" || enters != 1 {
			t.Errorf("%q: submitted %q with %d Enters, box %q; want it submitted with one", msg, submitted, enters, box)
		}
		if len(submitted) == 1 && !strings.HasPrefix(msg, "[from lead@hub] Screenshot") && strings.Contains(submitted[0], "\n") {
			t.Errorf("%q: submitted with a newline: %q", msg, submitted[0])
		}
	}
}

// Claude stalled longer than send waits for the text: the text and Enter
// arrive together, and the next Enter is eaten. Send keeps pressing Enter
// while the box shows the message.
func TestSubmit_PressesEnterUntilTheBoxEmpties(t *testing.T) {
	term := &claudeTerminal{stalled: 10}
	svc, _ := setupClaudeTerminal(t, term, submitTestWait)
	if err := svc.SendUnchecked(context.Background(), "worker", "[from lead@hub] run the tests"); err != nil {
		t.Fatal(err)
	}
	enters, submitted, box := term.state()
	if len(submitted) != 1 || box != "" {
		t.Fatalf("submitted %q, box %q; want submitted", submitted, box)
	}
	if enters != 3 {
		t.Errorf("%d Enters; want 3 (one a newline, one eaten)", enters)
	}
	if _, _, ok := svc.Unsubmitted(context.Background(), "worker"); ok {
		t.Error("Unsubmitted after it was submitted")
	}
}

// When no Enter submits the message, send says so, and so do status and
// the next send, until the box empties.
func TestSubmit_SaysWhenTheMessageSitsInTheBox(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{deafEnter: true}
	svc, _ := setupClaudeTerminal(t, term, submitTestWait)
	err := svc.SendUnchecked(ctx, "worker", "[from lead@hub] run the tests")
	var unsubmitted *service.UnsubmittedError
	if !errors.As(err, &unsubmitted) {
		t.Fatalf("err %v; want UnsubmittedError", err)
	}
	if !strings.Contains(err.Error(), "agent key test-worker Enter") || !strings.Contains(err.Error(), "don't send it again") {
		t.Errorf("error doesn't say what to do: %v", err)
	}
	if enters, _, _ := term.state(); enters != 3 {
		t.Errorf("%d Enters; want 3", enters)
	}
	at, session, ok := svc.Unsubmitted(ctx, "worker")
	if !ok || session != "test-worker" || time.Since(at) > time.Minute {
		t.Errorf("Unsubmitted = %v, %q, %v; want it, just now", at, session, ok)
	}
	if why := svc.InputBlocked(ctx, "worker"); !strings.Contains(why, "wasn't submitted") {
		t.Errorf("InputBlocked = %q; want it to name the unsubmitted message", why)
	}

	// A person presses Enter.
	term.mu.Lock()
	term.deafEnter = false
	term.mu.Unlock()
	term.SendSpecialKey("", "Enter")
	if _, _, ok := svc.Unsubmitted(ctx, "worker"); ok {
		t.Error("Unsubmitted after the box emptied")
	}
	if why := svc.InputBlocked(ctx, "worker"); why != "" {
		t.Errorf("InputBlocked = %q after the box emptied", why)
	}
}

// An Enter that brings up a dialog (or anything that hides the box) ends
// the checking: another Enter would answer the dialog.
func TestSubmit_NoEnterOnceTheBoxIsGone(t *testing.T) {
	term := &claudeTerminal{deafEnter: true, dialogOn: 1}
	svc, _ := setupClaudeTerminal(t, term, submitTestWait)
	if err := svc.SendUnchecked(context.Background(), "worker", "[from lead@hub] run the tests"); err != nil {
		t.Fatalf("err %v; want none (it can't tell)", err)
	}
	if enters, _, _ := term.state(); enters != 1 {
		t.Errorf("%d Enters; want 1", enters)
	}
}

// Held messages typed in but not submitted sit in the input box: they
// aren't held again, which would deliver them twice.
func TestSubmit_UnsubmittedHeldMessagesStayDelivered(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{deafEnter: true}
	svc, store := setupClaudeTerminal(t, term, submitTestWait)
	if err := store.SetState(ctx, &storage.StateEntry{AgentName: "worker", Key: service.StateKeySpawnID, Value: "s1", UpdatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if err := store.CreateMessage(ctx, &storage.Message{SenderID: "lead", ReceiverID: "worker", Content: "[from lead@hub] run the tests",
		Status: storage.MessageStatusPending, CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	if err := svc.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Notification"}); err != nil {
		t.Fatal(err)
	}
	n, err := svc.DeliverHeld(ctx, "worker", service.DeliverOptions{})
	var unsubmitted *service.UnsubmittedError
	if n != 1 || !errors.As(err, &unsubmitted) {
		t.Fatalf("DeliverHeld = %d, %v; want 1, UnsubmittedError", n, err)
	}
	if held := svc.HeldCount(ctx, "worker"); held != 0 {
		t.Errorf("%d held after typing; want 0", held)
	}
}

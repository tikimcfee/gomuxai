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
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// claudeTerminal stands in for tmux and a Claude pane behind it, as far
// as typing a message goes: what is written to the pane waits there until
// Claude reads it, and Claude reads everything that has arrived at once.
// A read that brings text and an Enter together is a paste, where the
// Enter is a newline, and Claude then eats the next Enter (both as seen
// with Claude Code v2.1.310). Typed text longer than the pty's input queue
// arrives in pieces, of which Claude keeps only the last; text pasted with
// paste brackets is one paste however it arrives (both as seen with Claude
// Code on macOS, 2026-11). Screens are synthesized like those in
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
	strange   bool // a screen with no input box agent recognizes
	starting  int  // captures Claude is still starting for: no box drawn, nothing read
	enters    int
	submitted []string
	pastes    int
	pasted    map[string]string // a paste's placeholder: what was pasted
}

// ptyQueue is how much of what is written to a macOS pty its reader gets
// in one read: its input queue's 1024 bytes, less 2.
const ptyQueue = 1022

const pasteStart, pasteEnd = "\x1b[200~", "\x1b[201~"

func (c *claudeTerminal) SessionExists(string) (bool, error) { return true, nil }

func (c *claudeTerminal) SendKeys(_ string, text string) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.unread += text
	c.read()
	return nil
}

// PasteText pastes text with paste brackets: Claude asks for them.
func (c *claudeTerminal) PasteText(_ string, text string) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.unread += pasteStart + text + pasteEnd
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
	if c.starting > 0 {
		c.starting--
		return " ▐▛███▜▌   Claude Code v2.1.319\n", nil
	}
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
	if c.starting > 0 || c.stalled > 0 || c.unread == "" {
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
	if start, end := strings.Index(chunk, pasteStart), strings.Index(chunk, pasteEnd); start >= 0 && end > start {
		// One paste, however the pty split it.
		chunk = chunk[:start] + chunk[start+len(pasteStart):end] + chunk[end+len(pasteEnd):]
	} else if typed := strings.TrimRight(chunk, "\r"); len(typed) > ptyQueue {
		// Read in pieces, each taken for a paste of its own, of which the
		// last is kept.
		chunk = typed[(len(typed)-1)/ptyQueue*ptyQueue:] + chunk[len(typed):]
	}
	text := strings.ReplaceAll(chunk, "\r", "\n")
	if strings.Contains(strings.TrimRight(text, "\n"), "\n") || len(text) > 200 {
		c.pastes++
		placeholder := fmt.Sprintf("[Pasted text #%d +%d lines]", c.pastes, strings.Count(text, "\n"))
		if c.pasted == nil {
			c.pasted = map[string]string{}
		}
		c.pasted[placeholder] = strings.TrimRight(text, "\n")
		c.box = append(c.box, placeholder)
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
		prompt := strings.Join(c.box, "\n")
		for placeholder, text := range c.pasted {
			prompt = strings.ReplaceAll(prompt, placeholder, text)
		}
		c.submitted = append(c.submitted, strings.TrimRight(prompt, "\n"))
		c.box = nil
	}
}

func (c *claudeTerminal) screen() string {
	if c.strange {
		return "● ok\n\n> " + strings.Join(c.box, "\n") + "\n"
	}
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
		if _, err := svc.SendUnchecked(context.Background(), "worker", msg); err != nil {
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
		if _, err := svc.SendUnchecked(context.Background(), "worker", msg); err != nil {
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

// A message over 1022 bytes typed into Claude on macOS reached it in two
// reads, and Claude kept only the second: an agent got the last 44 bytes
// of a 1066-byte message, and the tail of a --now message (friction #36,
// #37). Messages are pasted with paste brackets now, which make the pieces
// one paste.
func TestSubmit_LongMessageArrivesWhole(t *testing.T) {
	ctx := context.Background()
	for _, size := range []int{ptyQueue + 44, 5000} {
		msg := "[from lead@hub] the instructions come first. " + strings.Repeat("x", size)
		for _, strange := range []bool{false, true} { // checked, and typed without checking
			term := &claudeTerminal{strange: strange}
			svc, _ := setupClaudeTerminal(t, term, submitTestWait)
			if _, err := svc.Send(ctx, "worker", msg, service.SendOptions{Now: true}); err != nil {
				t.Fatalf("%d bytes: %v", len(msg), err)
			}
			if _, submitted, _ := term.state(); len(submitted) != 1 || submitted[0] != msg {
				got := ""
				if len(submitted) > 0 {
					got = submitted[0]
				}
				t.Errorf("%d bytes (unchecked %v): Claude got %d bytes starting %.30q; want the whole message", len(msg), strange, len(got), got)
			}
		}
	}
}

// The same over the bus, as the reports came: a --now message to a busy
// agent (#36), and a message to an agent whose turn had stopped with
// background work running, which send types in at once (#37).
func TestSubmit_LongMessageOverTheBusArrivesWhole(t *testing.T) {
	ctx := context.Background()
	msg := "[from lead@hub] the instructions come first. " + strings.Repeat("y", 1100)
	for _, tt := range []struct {
		name string
		now  bool
		ev   string
	}{
		{"--now while it works", true, "PreToolUse"},
		{"held, its turn over but for background work", false, "Stop"},
	} {
		term := &claudeTerminal{}
		svc, store := setupClaudeTerminal(t, term, submitTestWait)
		if err := store.SetState(ctx, &storage.StateEntry{AgentName: "worker", Key: service.StateKeySpawnID, Value: "s1", UpdatedAt: time.Now()}); err != nil {
			t.Fatal(err)
		}
		if err := svc.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusProcessing, Event: tt.ev}); err != nil {
			t.Fatal(err)
		}
		svc.SetDelivererStarter(func(string) error { return nil })
		nc := natstest.Start(t)
		stop, err := natsbus.ServeRPC(nc, "h1", service.NewRPCHandler(svc), natsbus.DefaultRemoteOps)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(stop)
		client := natsbus.NewRPCClient(natstest.Connect(t, nc.ConnectedUrl()), 5*time.Second)

		resp, err := client.SendMessage(ctx, natsbus.AgentRef{Name: "worker", Host: "h1"}, natsbus.SendRequest{Message: msg, Now: tt.now})
		if err != nil || resp == nil || resp.Typed != 1 {
			t.Fatalf("%s: SendMessage = %+v, %v; want it typed", tt.name, resp, err)
		}
		if _, submitted, _ := term.state(); len(submitted) != 1 || submitted[0] != msg {
			t.Errorf("%s: Claude got %q; want the whole message", tt.name, submitted)
		}
	}
}

// Claude stalled longer than send waits for the text: the text and Enter
// arrive together, and the next Enter is eaten. Send keeps pressing Enter
// while the box shows the message.
func TestSubmit_PressesEnterUntilTheBoxEmpties(t *testing.T) {
	term := &claudeTerminal{stalled: 10}
	svc, _ := setupClaudeTerminal(t, term, submitTestWait)
	if _, err := svc.SendUnchecked(context.Background(), "worker", "[from lead@hub] run the tests"); err != nil {
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
	_, err := svc.SendUnchecked(ctx, "worker", "[from lead@hub] run the tests")
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
	if _, err := svc.SendUnchecked(context.Background(), "worker", "[from lead@hub] run the tests"); err != nil {
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

// When the input box can't be read before typing, send still types the
// message and presses Enter, but says it couldn't check, and status shows
// that until a turn starts: a box that goes unrecognized (as a named
// session's once did) mustn't fail quietly.
func TestSubmit_SaysWhenItCannotCheck(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{strange: true}
	svc, store := setupClaudeTerminal(t, term, submitTestWait)
	turnStarted(t, store, "worker") // it isn't starting up: no wait for the box
	res, err := svc.Send(ctx, "worker", "[from lead@hub] run the tests", service.SendOptions{Now: true})
	if err != nil || res.Typed != 1 {
		t.Fatalf("Send = %+v, %v; want typed", res, err)
	}
	if !strings.Contains(res.Unchecked, "input box isn't on screen") {
		t.Errorf("Unchecked = %q; want why it couldn't check", res.Unchecked)
	}
	if enters, submitted, _ := term.state(); enters != 1 || len(submitted) != 1 {
		t.Errorf("%d Enters, submitted %q; want typed and Enter pressed once, as before", enters, submitted)
	}
	why, at, ok := svc.Unchecked(ctx, "worker")
	if !ok || why != res.Unchecked || time.Since(at) > time.Minute {
		t.Errorf("Unchecked = %q, %v, %v; want it recorded", why, at, ok)
	}

	// A turn that starts after it shows the message was taken.
	time.Sleep(10 * time.Millisecond)
	if err := svc.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusProcessing, Event: "UserPromptSubmit", ClearResult: true}); err != nil {
		t.Fatal(err)
	}
	if why, _, ok := svc.Unchecked(ctx, "worker"); ok {
		t.Errorf("Unchecked = %q after a turn started; want none", why)
	}

	// A message it could check clears the record.
	term.mu.Lock()
	term.strange = false
	term.mu.Unlock()
	if _, err := svc.SendUnchecked(ctx, "worker", "[from lead@hub] again"); err != nil {
		t.Fatal(err)
	}
	if entry, _ := store.GetState(ctx, "worker", service.StateKeyUnchecked); entry != nil {
		t.Errorf("%s = %q after a checked message; want it deleted", service.StateKeyUnchecked, entry.Value)
	}
}

// turnStarted records that a turn of name's CLI has started, as the hook
// reporting it does.
func turnStarted(t *testing.T, store *storage.SQLiteStorage, name string) {
	t.Helper()
	if err := store.SetState(context.Background(), &storage.StateEntry{AgentName: name, Key: service.StateKeyTurnAt,
		Value: time.Now().Add(-time.Minute).Format(time.RFC3339Nano), UpdatedAt: time.Now().Add(-time.Minute)}); err != nil {
		t.Fatal(err)
	}
}

// A send right after spawn, to a Claude that hadn't drawn its input box
// yet, was typed in without checking: the text and its Enter waited in the
// terminal until Claude read them together, as a paste, and the message sat
// in the box unsent (friction #38). With no turn started yet, send now
// waits for the box to come up, then types and checks as usual.
func TestSubmit_WaitsForAStartingCLIsInputBox(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{starting: 8}
	svc, _ := setupClaudeTerminal(t, term, submitTestWait)
	res, err := svc.Send(ctx, "worker", "[from lead@hub] run the tests", service.SendOptions{})
	if err != nil || res.Typed != 1 || res.Unchecked != "" {
		t.Fatalf("Send = %+v, %v; want typed and checked", res, err)
	}
	if enters, submitted, box := term.state(); len(submitted) != 1 || box != "" {
		t.Errorf("submitted %q with %d Enters, box %q; want it submitted", submitted, enters, box)
	}
	if _, _, ok := svc.Unchecked(ctx, "worker"); ok {
		t.Error("Unchecked recorded for a checked message")
	}
}

// A box that never comes up is waited for only while no turn has started,
// and only so long; then the message is typed unchecked, as before, saying
// so.
func TestSubmit_WaitForAStartingCLIIsBounded(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{strange: true}
	svc, _ := setupClaudeTerminal(t, term, submitTestWait)
	start := time.Now()
	res, err := svc.Send(ctx, "worker", "[from lead@hub] run the tests", service.SendOptions{})
	if err != nil || res.Typed != 1 {
		t.Fatalf("Send = %+v, %v; want typed", res, err)
	}
	if took := time.Since(start); took < 10*submitTestWait || took > 20*submitTestWait {
		t.Errorf("took %v; want about %v", took, 10*submitTestWait)
	}
	if !strings.Contains(res.Unchecked, "input box isn't on screen") || !strings.Contains(res.Unchecked, "after waiting") {
		t.Errorf("Unchecked = %q; want why, and that it waited", res.Unchecked)
	}
}

// A dialog that comes up while send waits for a starting CLI's box ends
// the wait: the message's Enter would answer it. --force (SendUnchecked)
// types anyway, as it always has.
func TestSubmit_StartingCLIsDialogStopsTheSend(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{starting: 4, dialog: true}
	svc, _ := setupClaudeTerminal(t, term, submitTestWait)
	_, err := svc.Send(ctx, "worker", "[from lead@hub] run the tests", service.SendOptions{})
	if err == nil || !strings.Contains(err.Error(), "permission prompt on screen") {
		t.Fatalf("err %v; want the dialog named", err)
	}
	if _, _, box := term.state(); box != "" || term.unread != "" {
		t.Errorf("typed into the dialog: box %q, unread %q", box, term.unread)
	}
}

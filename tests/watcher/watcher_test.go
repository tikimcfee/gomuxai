package watcher_test

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
	"github.com/tikimcfee/gomuxai/internal/watcher"
)

// setupTestDB creates a test database
func setupTestDB(t *testing.T) (*storage.SQLiteStorage, func()) {
	t.Helper()

	tmpFile, err := os.CreateTemp("", "watcher-test-*.db")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	tmpFile.Close()

	store, err := storage.NewSQLiteStorage(tmpFile.Name())
	if err != nil {
		os.Remove(tmpFile.Name())
		t.Fatalf("creating storage: %v", err)
	}

	cleanup := func() {
		store.Close()
		os.Remove(tmpFile.Name())
	}

	return store, cleanup
}

// fakeTmux records what the watcher types; every session exists. With
// failSend set, typing fails and is only counted.
type fakeTmux struct {
	mu       sync.Mutex
	sent     []string
	attempts int
	failSend bool
}

func (f *fakeTmux) CreateSession(string, map[string]string) error           { return nil }
func (f *fakeTmux) CreateSessionIn(string, string, map[string]string) error { return nil }
func (f *fakeTmux) KillSession(string) error                                { return nil }
func (f *fakeTmux) RespawnPane(string) error                                { return nil }
func (f *fakeTmux) SessionExists(string) (bool, error)                      { return true, nil }
func (f *fakeTmux) ListSessions() ([]tmux.Session, error)                   { return nil, nil }
func (f *fakeTmux) SendSpecialKey(string, string) error                     { return nil }
func (f *fakeTmux) Capture(string, int) (string, error)                     { return "", nil }
func (f *fakeTmux) CaptureStyled(string, int) (string, error)               { return "", nil }
func (f *fakeTmux) PipePane(string, string) error                           { return nil }
func (f *fakeTmux) StopPipePane(string) error                               { return nil }
func (f *fakeTmux) Clear(string) error                                      { return nil }
func (f *fakeTmux) PanePIDs() (map[int]string, error)                       { return nil, nil }

// PasteText is how messages are typed (see tmux.Client.PasteText).
func (f *fakeTmux) PasteText(session string, text string) error {
	return f.SendKeys(session, text)
}

func (f *fakeTmux) SendKeys(_ string, text string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.attempts++
	if f.failSend {
		return errors.New("send failed")
	}
	f.sent = append(f.sent, text)
	return nil
}

func (f *fakeTmux) counts() (sent, attempts int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.sent), f.attempts
}

// deliveryEnv is an agent with one queued message, whose status comes from
// the last report (no session id, so no native lookup).
type deliveryEnv struct {
	store *storage.SQLiteStorage
	tmux  *fakeTmux
	svc   *service.AgentService
}

func setupDelivery(t *testing.T, status service.AgentStatus) *deliveryEnv {
	t.Helper()
	store, cleanup := setupTestDB(t)
	t.Cleanup(cleanup)
	ctx := context.Background()

	ft := &fakeTmux{}
	cfg := &config.Config{SessionPrefix: "test-"}
	svc := service.NewAgentService(cfg, store, ft, backend.NewRegistry())

	if err := store.CreateAgent(ctx, &storage.Agent{Name: "worker", SessionName: "test-worker", Backend: "claude", CreatedAt: time.Now()}); err != nil {
		t.Fatalf("creating agent: %v", err)
	}
	if err := svc.Report(ctx, "worker", service.StatusReport{Status: status, Event: "test"}); err != nil {
		t.Fatalf("report: %v", err)
	}
	msg := &storage.Message{SenderID: "coordinator", ReceiverID: "worker", Content: "next task", Status: storage.MessageStatusPending, CreatedAt: time.Now()}
	if err := store.CreateMessage(ctx, msg); err != nil {
		t.Fatalf("creating message: %v", err)
	}
	return &deliveryEnv{store: store, tmux: ft, svc: svc}
}

// logBuffer collects a watcher's log lines, which say what it decided on
// each tick.
type logBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *logBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

func (b *logBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.String()
}

// startWatcher runs a fast watcher, logging every decision to the returned
// buffer, until stop is called (or the test ends).
func (e *deliveryEnv) startWatcher(t *testing.T) (logs *logBuffer, stop func()) {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	logs = &logBuffer{}
	w := watcher.NewWatcher(e.svc, e.store, 5*time.Millisecond,
		watcher.WithLogger(slog.New(slog.NewTextHandler(logs, &slog.HandlerOptions{Level: slog.LevelDebug}))))
	w.Start(ctx)
	var once sync.Once
	stop = func() {
		once.Do(func() {
			w.Stop()
			cancel()
		})
	}
	t.Cleanup(stop)
	return logs, stop
}

// pending reports whether the message is still pending.
func (e *deliveryEnv) pending(t *testing.T) bool {
	t.Helper()
	msgs, err := e.store.GetPendingMessages(context.Background(), "worker", 1)
	if err != nil {
		t.Fatalf("reading messages: %v", err)
	}
	return len(msgs) > 0
}

// runWatcher runs a fast watcher until the message leaves pending, then
// stops it. The fake tmux tells delivered (typed) from failed (never
// typed).
func (e *deliveryEnv) runWatcher(t *testing.T) {
	t.Helper()
	logs, stop := e.startWatcher(t)
	defer stop()
	await.Until(t, "the message to leave pending", func() (string, bool) {
		return logs.String(), !e.pending(t)
	})
}

// waitLogged waits until the watcher has logged msg n times.
func waitLogged(t *testing.T, logs *logBuffer, msg string, n int) {
	t.Helper()
	await.Until(t, fmt.Sprintf("the watcher to log %q %d times", msg, n), func() (string, bool) {
		out := logs.String()
		return out, strings.Count(out, `msg="`+msg+`"`) >= n
	})
}

// A failed turn (Claude's StopFailure) leaves the CLI at its prompt, and the
// status stays error until the next prompt: deliver rather than wait forever.
func TestWatcher_DeliversToIdleOrErroredAgent(t *testing.T) {
	for _, st := range []service.AgentStatus{service.AgentStatusIdle, service.AgentStatusError} {
		t.Run(string(st), func(t *testing.T) {
			env := setupDelivery(t, st)
			env.runWatcher(t)
			if sent, _ := env.tmux.counts(); sent != 1 {
				t.Errorf("typed %d message(s), want 1", sent)
			}
		})
	}
}

// Repro: an agent's CLI exits (or crashes) with messages queued. The watcher
// skipped every status but idle and unknown, so they sat pending forever.
func TestWatcher_FailsMessagesToExitedAgent(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusEnded)

	env.runWatcher(t)
	if sent, _ := env.tmux.counts(); sent != 0 {
		t.Errorf("typed %d message(s) into an exited agent's shell", sent)
	}
}

func TestWatcher_HoldsMessagesWhileBusy(t *testing.T) {
	for _, st := range []service.AgentStatus{service.AgentStatusProcessing, service.AgentStatusWaiting} {
		t.Run(string(st), func(t *testing.T) {
			env := setupDelivery(t, st)
			// The watcher says when it holds a message: once it has, twice,
			// the message must still be waiting.
			logs, stop := env.startWatcher(t)
			waitLogged(t, logs, "agent busy, skipping delivery", 2)
			stop()
			if !env.pending(t) {
				t.Errorf("message left pending while the agent was %s", st)
			}
		})
	}
}

// An agent whose status stays unknown fails its message after a few checks
// instead of holding it forever.
func TestWatcher_FailsMessageAfterRepeatedUnknownStatus(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusUnknown)

	env.runWatcher(t)
	if _, attempts := env.tmux.counts(); attempts != 0 {
		t.Errorf("tried to type %d message(s) into an agent of unknown status", attempts)
	}
}

// A failed send leaves the message pending for a retry, and the retry waits
// out the backoff (1s after the first failure) rather than the next tick.
func TestWatcher_RetriesFailedSendAfterBackoff(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusIdle)
	env.tmux.failSend = true

	// The watcher claims the message (marks it delivered) before typing and
	// puts it back when typing fails, so look once it has passed it over
	// for the backoff on later ticks, not as soon as the message isn't
	// pending.
	logs, stop := env.startWatcher(t)
	waitLogged(t, logs, "delivery failed, will retry", 1)
	waitLogged(t, logs, "skipping delivery: backoff active", 2)
	stop()
	if !env.pending(t) {
		t.Fatal("message failed after one failed send, want pending for a retry")
	}
	if _, attempts := env.tmux.counts(); attempts != 1 {
		t.Errorf("tried to send %d time(s) within the backoff, want 1", attempts)
	}
}

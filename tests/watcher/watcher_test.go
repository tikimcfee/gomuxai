package watcher_test

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"os"
	"sync"
	"testing"
	"time"

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

func (f *fakeTmux) CreateSession(string, map[string]string) error { return nil }
func (f *fakeTmux) KillSession(string) error                      { return nil }
func (f *fakeTmux) SessionExists(string) (bool, error)            { return true, nil }
func (f *fakeTmux) ListSessions() ([]tmux.Session, error)         { return nil, nil }
func (f *fakeTmux) SendSpecialKey(string, string) error           { return nil }
func (f *fakeTmux) Capture(string, int, bool) (string, error)     { return "", nil }
func (f *fakeTmux) PipePane(string, string) error                 { return nil }
func (f *fakeTmux) StopPipePane(string) error                     { return nil }
func (f *fakeTmux) Clear(string) error                            { return nil }

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

// runWatcher runs a fast watcher until the message leaves pending or the
// deadline passes, and reports whether it is still pending. The fake tmux
// tells delivered (typed) from failed (never typed).
func (e *deliveryEnv) runWatcher(t *testing.T, d time.Duration) (pending bool) {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	w := watcher.NewWatcher(e.svc, e.store, 5*time.Millisecond,
		watcher.WithLogger(slog.New(slog.NewTextHandler(io.Discard, nil))))
	w.Start(ctx)
	defer w.Stop()

	for deadline := time.Now().Add(d); time.Now().Before(deadline); time.Sleep(5 * time.Millisecond) {
		msgs, err := e.store.GetPendingMessages(ctx, "worker", 1)
		if err != nil {
			t.Fatalf("reading messages: %v", err)
		}
		if len(msgs) == 0 {
			return false
		}
	}
	return true
}

// A failed turn (Claude's StopFailure) leaves the CLI at its prompt, and the
// status stays error until the next prompt: deliver rather than wait forever.
func TestWatcher_DeliversToIdleOrErroredAgent(t *testing.T) {
	for _, st := range []service.AgentStatus{service.AgentStatusIdle, service.AgentStatusError} {
		t.Run(string(st), func(t *testing.T) {
			env := setupDelivery(t, st)
			if env.runWatcher(t, time.Second) {
				t.Fatalf("message to the %s agent is still pending, want delivered", st)
			}
			if sent, _ := env.tmux.counts(); sent != 1 {
				t.Errorf("typed %d message(s), want 1", sent)
			}
		})
	}
}

// Repro: an agent's CLI exits (or crashes) with messages queued. The watcher
// skipped every status but idle and unknown, so they sat pending forever.
func TestWatcher_FailsMessagesToExitedAgent(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusExited)

	if env.runWatcher(t, time.Second) {
		t.Fatal("message to an exited agent is still pending, want failed")
	}
	if sent, _ := env.tmux.counts(); sent != 0 {
		t.Errorf("typed %d message(s) into an exited agent's shell", sent)
	}
}

func TestWatcher_HoldsMessagesWhileBusy(t *testing.T) {
	for _, st := range []service.AgentStatus{service.AgentStatusProcessing, service.AgentStatusWaiting} {
		t.Run(string(st), func(t *testing.T) {
			env := setupDelivery(t, st)
			if !env.runWatcher(t, 100*time.Millisecond) {
				t.Errorf("message left pending while the agent was %s", st)
			}
		})
	}
}

// An agent whose status stays unknown fails its message after a few checks
// instead of holding it forever.
func TestWatcher_FailsMessageAfterRepeatedUnknownStatus(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusUnknown)

	if env.runWatcher(t, time.Second) {
		t.Fatal("message to an agent of unknown status is still pending, want failed")
	}
	if _, attempts := env.tmux.counts(); attempts != 0 {
		t.Errorf("tried to type %d message(s) into an agent of unknown status", attempts)
	}
}

// A failed send leaves the message pending for a retry, and the retry waits
// out the backoff (1s after the first failure) rather than the next tick.
func TestWatcher_RetriesFailedSendAfterBackoff(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusIdle)
	env.tmux.failSend = true

	if !env.runWatcher(t, 200*time.Millisecond) {
		t.Fatal("message failed after one failed send, want pending for a retry")
	}
	if _, attempts := env.tmux.counts(); attempts != 1 {
		t.Errorf("tried to send %d time(s) within the backoff, want 1", attempts)
	}
}

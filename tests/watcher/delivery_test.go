package watcher_test

import (
	"context"
	"io"
	"log/slog"
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

// fakeTmux records what the watcher types; every session exists.
type fakeTmux struct {
	mu   sync.Mutex
	sent []string
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
	f.sent = append(f.sent, text)
	return nil
}

func (f *fakeTmux) sentCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.sent)
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

// Repro: an agent's CLI exits (or crashes) with messages queued. The watcher
// skipped every status but idle and unknown, so they sat pending forever.
func TestWatcher_FailsMessagesToExitedAgent(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusExited)

	if env.runWatcher(t, time.Second) {
		t.Fatal("message to an exited agent is still pending, want failed")
	}
	if n := env.tmux.sentCount(); n != 0 {
		t.Errorf("typed %d message(s) into an exited agent's shell", n)
	}
}

// A failed turn (Claude's StopFailure) leaves the CLI at its prompt, and the
// status stays error until the next prompt: deliver rather than wait forever.
func TestWatcher_DeliversToErroredAgent(t *testing.T) {
	env := setupDelivery(t, service.AgentStatusError)

	if env.runWatcher(t, time.Second) {
		t.Fatal("message to an errored agent is still pending, want delivered")
	}
	if n := env.tmux.sentCount(); n != 1 {
		t.Errorf("typed %d message(s), want 1", n)
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

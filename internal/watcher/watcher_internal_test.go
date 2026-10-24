package watcher

import (
	"context"
	"io"
	"log/slog"
	"path/filepath"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// nopTmux is a tmux where every session exists and nothing happens.
type nopTmux struct{}

func (nopTmux) CreateSession(string, map[string]string) error { return nil }
func (nopTmux) KillSession(string) error                      { return nil }
func (nopTmux) SessionExists(string) (bool, error)            { return true, nil }
func (nopTmux) ListSessions() ([]tmux.Session, error)         { return nil, nil }
func (nopTmux) SendKeys(string, string) error                 { return nil }
func (nopTmux) SendSpecialKey(string, string) error           { return nil }
func (nopTmux) Capture(string, int, bool) (string, error)     { return "", nil }
func (nopTmux) PipePane(string, string) error                 { return nil }
func (nopTmux) StopPipePane(string) error                     { return nil }
func (nopTmux) Clear(string) error                            { return nil }

// The unknown threshold counts unknowns in a row: busy checks between them
// start the count over, so an agent that is mostly busy keeps its message.
func TestUnknownCountResetsOnKnownStatus(t *testing.T) {
	ctx := context.Background()
	store, err := storage.NewSQLiteStorage(filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { store.Close() })
	svc := service.NewAgentService(&config.Config{SessionPrefix: "test-"}, store, nopTmux{}, backend.NewRegistry())

	agent := &storage.Agent{Name: "worker", SessionName: "test-worker", Backend: "claude", CreatedAt: time.Now()}
	if err := store.CreateAgent(ctx, agent); err != nil {
		t.Fatal(err)
	}
	msg := &storage.Message{SenderID: "coordinator", ReceiverID: "worker", Content: "next task", Status: storage.MessageStatusPending, CreatedAt: time.Now()}
	if err := store.CreateMessage(ctx, msg); err != nil {
		t.Fatal(err)
	}

	w := NewWatcher(svc, store, time.Hour, WithLogger(slog.New(slog.NewTextHandler(io.Discard, nil))))
	check := func(status service.AgentStatus) (pending bool) {
		t.Helper()
		if err := svc.Report(ctx, "worker", service.StatusReport{Status: status, Event: "test"}); err != nil {
			t.Fatal(err)
		}
		w.processAgent(ctx, agent)
		msgs, err := store.GetPendingMessages(ctx, "worker", 1)
		if err != nil {
			t.Fatal(err)
		}
		return len(msgs) == 1
	}

	busy := []service.AgentStatus{service.AgentStatusProcessing, service.AgentStatusWaiting}
	for i := range 2 * unknownThreshold {
		if !check(service.AgentStatusUnknown) || !check(busy[i%2]) {
			t.Fatalf("message failed after %d unknowns, each followed by a busy status", i+1)
		}
	}
	for i := 1; i < unknownThreshold; i++ {
		if !check(service.AgentStatusUnknown) {
			t.Fatalf("message failed after %d unknowns in a row, want %d", i, unknownThreshold)
		}
	}
	if check(service.AgentStatusUnknown) {
		t.Errorf("message still pending after %d unknowns in a row, want failed", unknownThreshold)
	}
}

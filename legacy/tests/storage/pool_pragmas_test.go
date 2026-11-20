package storage_test

import (
	"context"
	"sync"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/storage"
)

// TestCascadeDelete_HoldsOnEveryPooledConnection checks that the agents →
// state cascade (what makes cleanup drop an agent's state) is enforced on
// every connection in the pool, not just the one that ran the startup PRAGMA.
// SQLite's foreign_keys pragma is per-connection.
func TestCascadeDelete_HoldsOnEveryPooledConnection(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()
	ctx := context.Background()

	const agents = 16
	for i := 0; i < agents; i++ {
		name := string(rune('a' + i))
		if err := store.CreateAgent(ctx, &storage.Agent{Name: name, SessionName: "s-" + name, Backend: "claude", CreatedAt: time.Now()}); err != nil {
			t.Fatalf("create agent: %v", err)
		}
		if err := store.SetState(ctx, &storage.StateEntry{AgentName: name, Key: "_status", Value: "idle", UpdatedAt: time.Now()}); err != nil {
			t.Fatalf("set state: %v", err)
		}
	}

	// Delete concurrently so database/sql has to open extra connections.
	var wg sync.WaitGroup
	start := make(chan struct{})
	for i := 0; i < agents; i++ {
		wg.Add(1)
		go func(name string) {
			defer wg.Done()
			<-start
			if err := store.DeleteAgent(ctx, name); err != nil {
				t.Errorf("delete agent %s: %v", name, err)
			}
		}(string(rune('a' + i)))
	}
	close(start)
	wg.Wait()

	var leaked []string
	for i := 0; i < agents; i++ {
		name := string(rune('a' + i))
		if entry, _ := store.GetState(ctx, name, "_status"); entry != nil {
			leaked = append(leaked, name)
		}
	}
	if len(leaked) > 0 {
		t.Errorf("state survived agent deletion for %d/%d agents %v: foreign_keys is not on for every pooled connection", len(leaked), agents, leaked)
	}
}

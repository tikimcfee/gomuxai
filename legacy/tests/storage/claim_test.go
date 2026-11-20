package storage_test

import (
	"context"
	"sync"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/storage"
)

func holdMessage(t *testing.T, store *storage.SQLiteStorage, receiver, content string) *storage.Message {
	t.Helper()
	msg := &storage.Message{SenderID: "s", ReceiverID: receiver, Content: content, Status: storage.MessageStatusPending, CreatedAt: time.Now()}
	if err := store.CreateMessage(context.Background(), msg); err != nil {
		t.Fatalf("creating message: %v", err)
	}
	return msg
}

// ClaimPendingMessages returns the pending messages oldest first, marks
// them delivered, and applies the state change it is given in the same
// transaction.
func TestClaimPendingMessages(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()
	ctx := context.Background()
	if err := store.CreateAgent(ctx, &storage.Agent{Name: "worker", SessionName: "s", Backend: "claude", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	for _, c := range []string{"first", "second", "third"} {
		holdMessage(t, store, "worker", c)
	}
	holdMessage(t, store, "other", "not mine")

	var seen int
	claimed, err := store.ClaimPendingMessages(ctx, "worker", func(c []*storage.Message) storage.StateUpdate {
		seen = len(c)
		return storage.StateUpdate{Entries: []*storage.StateEntry{{AgentName: "worker", Key: "_status", Value: "processing", UpdatedAt: time.Now()}}}
	})
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, m := range claimed {
		got = append(got, m.Content)
	}
	if len(got) != 3 || got[0] != "first" || got[1] != "second" || got[2] != "third" || seen != 3 {
		t.Fatalf("claimed %q (update saw %d), want first, second, third", got, seen)
	}
	if e, _ := store.GetState(ctx, "worker", "_status"); e == nil || e.Value != "processing" {
		t.Errorf("state change not applied: %+v", e)
	}
	if left, _ := store.GetPendingMessages(ctx, "worker", 10); len(left) != 0 {
		t.Errorf("%d still pending after the claim", len(left))
	}
	if left, _ := store.GetPendingMessages(ctx, "other", 10); len(left) != 1 {
		t.Errorf("another agent's message was claimed")
	}

	// Nothing left: the update still runs, with no messages.
	seen = -1
	claimed, err = store.ClaimPendingMessages(ctx, "worker", func(c []*storage.Message) storage.StateUpdate {
		seen = len(c)
		return storage.StateUpdate{}
	})
	if err != nil || len(claimed) != 0 || seen != 0 {
		t.Errorf("second claim: %d messages, update saw %d, err %v", len(claimed), seen, err)
	}
}

// Of several callers racing to deliver the same messages, each message goes
// to exactly one.
func TestClaim_EachMessageOnce(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()
	ctx := context.Background()
	const n = 20
	ids := make([]int64, n)
	for i := range ids {
		ids[i] = holdMessage(t, store, "worker", "m").ID
	}

	var mu sync.Mutex
	counts := map[int64]int{}
	var wg sync.WaitGroup
	for w := 0; w < 4; w++ {
		wg.Add(2)
		go func() { // claims everything at once, like a Stop hook
			defer wg.Done()
			claimed, err := store.ClaimPendingMessages(ctx, "worker", nil)
			if err != nil {
				t.Error(err)
				return
			}
			mu.Lock()
			for _, m := range claimed {
				counts[m.ID]++
			}
			mu.Unlock()
		}()
		go func() { // claims one at a time, like the watcher
			defer wg.Done()
			for _, id := range ids {
				ok, err := store.ClaimMessage(ctx, id)
				if err != nil {
					t.Error(err)
					return
				}
				if ok {
					mu.Lock()
					counts[id]++
					mu.Unlock()
				}
			}
		}()
	}
	wg.Wait()
	for _, id := range ids {
		if counts[id] != 1 {
			t.Errorf("message %d claimed %d times, want once", id, counts[id])
		}
	}
}

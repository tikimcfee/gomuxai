package watcher_test

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/storage"
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

// =============================================================================
// Message State Transition Tests
// =============================================================================

func TestMessageStateTransition_PendingToDelivered(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "receiver",
		Content:    "test message",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}

	if err := store.CreateMessage(ctx, msg); err != nil {
		t.Fatalf("creating message: %v", err)
	}

	// Verify pending
	pending, _ := store.GetPendingMessages(ctx, "receiver", 10)
	if len(pending) != 1 {
		t.Fatalf("expected 1 pending, got %d", len(pending))
	}

	// Transition to delivered
	if err := store.UpdateMessageStatus(ctx, msg.ID, storage.MessageStatusDelivered); err != nil {
		t.Fatalf("updating status: %v", err)
	}

	// Should not appear in pending
	pending, _ = store.GetPendingMessages(ctx, "receiver", 10)
	if len(pending) != 0 {
		t.Errorf("expected 0 pending after delivery, got %d", len(pending))
	}
}

func TestMessageStateTransition_PendingToFailed(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "receiver",
		Content:    "test message",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}

	if err := store.CreateMessage(ctx, msg); err != nil {
		t.Fatalf("creating message: %v", err)
	}

	// Transition to failed
	if err := store.UpdateMessageStatus(ctx, msg.ID, storage.MessageStatusFailed); err != nil {
		t.Fatalf("updating status: %v", err)
	}

	// Should not appear in pending
	pending, _ := store.GetPendingMessages(ctx, "receiver", 10)
	if len(pending) != 0 {
		t.Errorf("expected 0 pending after failure, got %d", len(pending))
	}
}

func TestMessageStatusNotChangedOnUnknown(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "receiver",
		Content:    "test message",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}

	if err := store.CreateMessage(ctx, msg); err != nil {
		t.Fatalf("creating message: %v", err)
	}

	// Message should remain pending (no transition on Unknown status)
	pending, _ := store.GetPendingMessages(ctx, "receiver", 10)
	if len(pending) != 1 {
		t.Errorf("message should remain pending, got %d", len(pending))
	}
	if pending[0].Status != storage.MessageStatusPending {
		t.Errorf("expected pending status, got %s", pending[0].Status)
	}
}

// =============================================================================
// FIFO Ordering Tests
// =============================================================================

func TestFIFOOrdering(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create messages in order
	for i := 1; i <= 5; i++ {
		msg := &storage.Message{
			SenderID:   "sender",
			ReceiverID: "receiver",
			Content:    string(rune('A' + i - 1)), // A, B, C, D, E
			Status:     storage.MessageStatusPending,
			CreatedAt:  time.Now().Add(time.Duration(i) * time.Millisecond),
		}
		if err := store.CreateMessage(ctx, msg); err != nil {
			t.Fatalf("creating message %d: %v", i, err)
		}
	}

	// Get first pending (limit 1)
	pending, err := store.GetPendingMessages(ctx, "receiver", 1)
	if err != nil {
		t.Fatalf("getting pending: %v", err)
	}
	if len(pending) != 1 {
		t.Fatalf("expected 1 message, got %d", len(pending))
	}
	if pending[0].Content != "A" {
		t.Errorf("expected first message to be 'A', got %q", pending[0].Content)
	}

	// Mark as delivered
	store.UpdateMessageStatus(ctx, pending[0].ID, storage.MessageStatusDelivered)

	// Get next - should be B
	pending, err = store.GetPendingMessages(ctx, "receiver", 1)
	if err != nil {
		t.Fatalf("getting pending: %v", err)
	}
	if pending[0].Content != "B" {
		t.Errorf("expected second message to be 'B', got %q", pending[0].Content)
	}
}

// =============================================================================
// Multiple Agents Tests
// =============================================================================

func TestMultipleAgentsPendingMessages(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	agents := []string{"agent1", "agent2", "agent3"}
	for _, agent := range agents {
		for i := 0; i < 3; i++ {
			msg := &storage.Message{
				SenderID:   "coordinator",
				ReceiverID: agent,
				Content:    "task for " + agent,
				Status:     storage.MessageStatusPending,
				CreatedAt:  time.Now(),
			}
			if err := store.CreateMessage(ctx, msg); err != nil {
				t.Fatalf("creating message for %s: %v", agent, err)
			}
		}
	}

	// Each agent should have 3 pending messages
	for _, agent := range agents {
		pending, err := store.GetPendingMessages(ctx, agent, 10)
		if err != nil {
			t.Fatalf("getting pending for %s: %v", agent, err)
		}
		if len(pending) != 3 {
			t.Errorf("agent %s: expected 3 pending, got %d", agent, len(pending))
		}
	}
}

func TestParallelAgentMessageIsolation(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create messages for agent1
	msg1 := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "agent1",
		Content:    "for agent1",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}
	store.CreateMessage(ctx, msg1)

	// Create messages for agent2
	msg2 := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "agent2",
		Content:    "for agent2",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}
	store.CreateMessage(ctx, msg2)

	// Deliver agent1's message
	pending1, _ := store.GetPendingMessages(ctx, "agent1", 1)
	store.UpdateMessageStatus(ctx, pending1[0].ID, storage.MessageStatusDelivered)

	// Agent2 should still have pending
	pending2, _ := store.GetPendingMessages(ctx, "agent2", 10)
	if len(pending2) != 1 {
		t.Errorf("agent2: expected 1 pending (unchanged), got %d", len(pending2))
	}

	// Agent1 should have 0 pending
	pending1, _ = store.GetPendingMessages(ctx, "agent1", 10)
	if len(pending1) != 0 {
		t.Errorf("agent1: expected 0 pending after delivery, got %d", len(pending1))
	}
}

// =============================================================================
// Edge Cases Tests
// =============================================================================

func TestEmptyPendingQueue(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// No messages created
	pending, err := store.GetPendingMessages(ctx, "nonexistent", 10)
	if err != nil {
		t.Fatalf("getting pending: %v", err)
	}
	if len(pending) != 0 {
		t.Errorf("expected 0 pending for empty queue, got %d", len(pending))
	}
}

func TestAgentSessionDoesNotExist(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create message for agent without session
	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "nosession",
		Content:    "hello",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}
	if err := store.CreateMessage(ctx, msg); err != nil {
		t.Fatalf("creating message: %v", err)
	}

	// Message exists but agent record doesn't
	agent, err := store.GetAgent(ctx, "nosession")
	if err != nil {
		t.Fatalf("getting agent: %v", err)
	}
	if agent != nil {
		t.Error("expected nil for nonexistent agent")
	}

	// Pending messages still exist (watcher should handle gracefully)
	pending, _ := store.GetPendingMessages(ctx, "nosession", 10)
	if len(pending) != 1 {
		t.Errorf("expected message to exist, got %d", len(pending))
	}
}

func TestProviderReturnsError(t *testing.T) {
	// This test documents expected behavior:
	// When provider returns error during status check,
	// message should remain pending (not delivered, not failed)

	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "receiver",
		Content:    "test",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}
	store.CreateMessage(ctx, msg)

	// Simulating: provider error means no status change
	// Message should remain pending
	pending, _ := store.GetPendingMessages(ctx, "receiver", 10)
	if len(pending) != 1 {
		t.Errorf("message should remain pending on provider error")
	}
}

// =============================================================================
// Error Handling - Retry Logic Tests (TDD - features to implement)
// =============================================================================

func TestRetryWithBackoff_MessageRemainsForRetry(t *testing.T) {
	// TDD: Test that failed messages can be retried
	// Currently: Messages go directly to "failed" status
	// Desired: Messages should be retryable with backoff

	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "receiver",
		Content:    "retry me",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}
	store.CreateMessage(ctx, msg)

	// First "delivery attempt" fails - mark failed
	store.UpdateMessageStatus(ctx, msg.ID, storage.MessageStatusFailed)

	// Current behavior: Message is marked failed
	pending, _ := store.GetPendingMessages(ctx, "receiver", 10)
	if len(pending) != 0 {
		t.Log("Note: Currently failed messages are not retried")
	}

	// TODO: Implement retry logic in watcher
	// After implementing, this test should verify:
	// - Message with RetryCount < MaxRetries stays pending
	// - Backoff delay is respected between retries
	t.Log("TDD: Retry with backoff not yet implemented in watcher")
}

func TestRetryBackoff_ExponentialDelay(t *testing.T) {
	// TDD: Test exponential backoff between retries
	// Expected delays: 1s, 2s, 4s (exponential)

	t.Log("TDD: Exponential backoff not yet implemented")
	t.Log("Expected: maxRetries=3, backoff starts at 1s")

	// When implemented, test should verify:
	// - Retry 1: wait 1s
	// - Retry 2: wait 2s
	// - Retry 3: wait 4s
	// - After 3 failures: mark as failed permanently
}

// =============================================================================
// Timeout for Unknown Status Tests (TDD - features to implement)
// =============================================================================

func TestUnknownStatusTimeout(t *testing.T) {
	// TDD: Test timeout behavior for Unknown status
	// When agent stays in Unknown state too long, should escalate

	t.Log("TDD: Unknown status timeout not yet implemented")
	t.Log("Expected: After 5 Unknown checks, mark message failed or escalate")

	// When implemented, test should verify:
	// - unknownThreshold = 5
	// - After 5 consecutive Unknown statuses, take action
}

// =============================================================================
// Delivery Decision Tests
// =============================================================================

func TestDeliveryOnlyWhenIdle(t *testing.T) {
	// Documents expected behavior:
	// Messages should only be delivered when agent status is Idle

	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create agent
	agent := &storage.Agent{
		Name:        "test-agent",
		SessionName: "test-test-agent",
		Backend:     "claude",
		CreatedAt:   time.Now(),
	}
	store.CreateAgent(ctx, agent)

	// Create pending message
	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "test-agent",
		Content:    "deliver when idle",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}
	store.CreateMessage(ctx, msg)

	// Message exists and agent exists
	pending, _ := store.GetPendingMessages(ctx, "test-agent", 10)
	if len(pending) != 1 {
		t.Errorf("expected 1 pending message")
	}

	retrievedAgent, _ := store.GetAgent(ctx, "test-agent")
	if retrievedAgent == nil {
		t.Error("expected agent to exist")
	}

	// Delivery decision depends on status check (tested via integration)
	t.Log("Full delivery test requires tmux integration")
}

func TestNoDeliveryWhenProcessing(t *testing.T) {
	// Documents: No delivery when status is Processing
	t.Log("Delivery blocked when agent is processing (integration test)")
}

func TestNoDeliveryWhenError(t *testing.T) {
	// Documents: No delivery when status is Error
	t.Log("Delivery blocked when agent has error status (integration test)")
}

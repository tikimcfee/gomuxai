package storage_test

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/storage"
)

func setupTestDB(t *testing.T) (*storage.SQLiteStorage, func()) {
	t.Helper()

	tmpFile, err := os.CreateTemp("", "gomuxai-test-*.db")
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

func TestAgentCRUD(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create
	agent := &storage.Agent{
		Name:        "test-agent",
		SessionName: "agent-test-agent",
		Backend:     "claude",
		TaskFile:    "tasks/test.md",
		LogFile:     "/tmp/test.log",
		CreatedAt:   time.Now(),
	}

	err := store.CreateAgent(ctx, agent)
	if err != nil {
		t.Fatalf("creating agent: %v", err)
	}

	// Read
	retrieved, err := store.GetAgent(ctx, "test-agent")
	if err != nil {
		t.Fatalf("getting agent: %v", err)
	}
	if retrieved == nil {
		t.Fatal("expected agent, got nil")
	}
	if retrieved.Name != "test-agent" {
		t.Errorf("expected name test-agent, got %s", retrieved.Name)
	}
	if retrieved.Backend != "claude" {
		t.Errorf("expected backend claude, got %s", retrieved.Backend)
	}

	// List
	agents, err := store.ListAgents(ctx)
	if err != nil {
		t.Fatalf("listing agents: %v", err)
	}
	if len(agents) != 1 {
		t.Errorf("expected 1 agent, got %d", len(agents))
	}

	// Delete
	err = store.DeleteAgent(ctx, "test-agent")
	if err != nil {
		t.Fatalf("deleting agent: %v", err)
	}

	retrieved, err = store.GetAgent(ctx, "test-agent")
	if err != nil {
		t.Fatalf("getting deleted agent: %v", err)
	}
	if retrieved != nil {
		t.Error("expected nil after delete")
	}
}

func TestMessageCRUD(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create message
	msg := &storage.Message{
		SenderID:   "sender",
		ReceiverID: "receiver",
		Content:    "hello world",
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}

	err := store.CreateMessage(ctx, msg)
	if err != nil {
		t.Fatalf("creating message: %v", err)
	}
	if msg.ID == 0 {
		t.Error("expected message ID to be set")
	}

	// Get pending
	messages, err := store.GetPendingMessages(ctx, "receiver", 10)
	if err != nil {
		t.Fatalf("getting pending messages: %v", err)
	}
	if len(messages) != 1 {
		t.Errorf("expected 1 pending message, got %d", len(messages))
	}

	// Update status
	err = store.UpdateMessageStatus(ctx, msg.ID, storage.MessageStatusDelivered)
	if err != nil {
		t.Fatalf("updating message status: %v", err)
	}

	// Should not appear in pending anymore
	messages, err = store.GetPendingMessages(ctx, "receiver", 10)
	if err != nil {
		t.Fatalf("getting pending messages after update: %v", err)
	}
	if len(messages) != 0 {
		t.Errorf("expected 0 pending messages after delivery, got %d", len(messages))
	}

	// Delete by agent
	err = store.DeleteMessagesByAgent(ctx, "sender")
	if err != nil {
		t.Fatalf("deleting messages by agent: %v", err)
	}
}

// The watcher delivers the first pending message, so the order must be the
// order messages were queued, also within one second (created_at has whole
// seconds).
func TestPendingMessages_FIFO(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	queuedAt := time.Now()
	for _, content := range []string{"A", "B", "C", "D", "E"} {
		msg := &storage.Message{
			SenderID:   "sender",
			ReceiverID: "receiver",
			Content:    content,
			Status:     storage.MessageStatusPending,
			CreatedAt:  queuedAt,
		}
		if err := store.CreateMessage(ctx, msg); err != nil {
			t.Fatalf("creating message %s: %v", content, err)
		}
	}

	for _, want := range []string{"A", "B", "C", "D", "E"} {
		pending, err := store.GetPendingMessages(ctx, "receiver", 1)
		if err != nil {
			t.Fatalf("getting pending: %v", err)
		}
		if len(pending) != 1 {
			t.Fatalf("expected 1 message, got %d", len(pending))
		}
		if pending[0].Content != want {
			t.Fatalf("next pending message is %q, want %q", pending[0].Content, want)
		}
		if err := store.UpdateMessageStatus(ctx, pending[0].ID, storage.MessageStatusDelivered); err != nil {
			t.Fatalf("updating status: %v", err)
		}
	}
}

func TestPendingMessages_PerReceiver(t *testing.T) {
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

	// Settling agent1's messages leaves the others' alone.
	pending, err := store.GetPendingMessages(ctx, "agent1", 10)
	if err != nil {
		t.Fatalf("getting pending for agent1: %v", err)
	}
	statuses := []storage.MessageStatus{storage.MessageStatusDelivered, storage.MessageStatusFailed, storage.MessageStatusDelivered}
	for i, msg := range pending {
		if err := store.UpdateMessageStatus(ctx, msg.ID, statuses[i]); err != nil {
			t.Fatalf("updating status: %v", err)
		}
	}

	want := map[string]int{"agent1": 0, "agent2": 3, "agent3": 3, "nobody": 0}
	for agent, n := range want {
		pending, err := store.GetPendingMessages(ctx, agent, 10)
		if err != nil {
			t.Fatalf("getting pending for %s: %v", agent, err)
		}
		if len(pending) != n {
			t.Errorf("%s: expected %d pending, got %d", agent, n, len(pending))
		}
	}
}

func TestGetNonExistentAgent(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	agent, err := store.GetAgent(ctx, "nonexistent")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if agent != nil {
		t.Error("expected nil for nonexistent agent")
	}
}

func TestTaskCRUD(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Tasks are standalone - no agent required
	task := &storage.Task{
		Name:       "research-auth",
		Content:    "# Research Auth\n\nAnalyze authentication patterns.",
		SourceFile: "tasks/research-auth.md",
		CreatedAt:  time.Now(),
	}

	err := store.SetTask(ctx, task)
	if err != nil {
		t.Fatalf("setting task: %v", err)
	}

	// Get task
	retrieved, err := store.GetTask(ctx, "research-auth")
	if err != nil {
		t.Fatalf("getting task: %v", err)
	}
	if retrieved == nil {
		t.Fatal("expected task, got nil")
	}
	if retrieved.Name != "research-auth" {
		t.Errorf("expected name 'research-auth', got %q", retrieved.Name)
	}
	if retrieved.Content != task.Content {
		t.Errorf("expected content %q, got %q", task.Content, retrieved.Content)
	}
	if retrieved.SourceFile != task.SourceFile {
		t.Errorf("expected source_file %q, got %q", task.SourceFile, retrieved.SourceFile)
	}

	// Update task (upsert)
	task.Content = "# Updated Task\n\nNew content."
	err = store.SetTask(ctx, task)
	if err != nil {
		t.Fatalf("updating task: %v", err)
	}

	retrieved, err = store.GetTask(ctx, "research-auth")
	if err != nil {
		t.Fatalf("getting updated task: %v", err)
	}
	if retrieved.Content != "# Updated Task\n\nNew content." {
		t.Errorf("expected updated content, got %q", retrieved.Content)
	}

	// Delete task
	err = store.DeleteTask(ctx, "research-auth")
	if err != nil {
		t.Fatalf("deleting task: %v", err)
	}

	retrieved, err = store.GetTask(ctx, "research-auth")
	if err != nil {
		t.Fatalf("getting deleted task: %v", err)
	}
	if retrieved != nil {
		t.Error("expected nil after delete")
	}
}

func TestGetNonExistentTask(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	task, err := store.GetTask(ctx, "nonexistent")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if task != nil {
		t.Error("expected nil for nonexistent task")
	}
}

func TestListTasks(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create standalone tasks - no agents needed
	tasks := []*storage.Task{
		{Name: "research-auth", Content: "Research authentication", SourceFile: "tasks/auth.md", CreatedAt: time.Now()},
		{Name: "build-api", Content: "Build the API", CreatedAt: time.Now()},
		{Name: "write-docs", Content: "Write documentation", SourceFile: "tasks/docs.md", CreatedAt: time.Now()},
	}

	for _, task := range tasks {
		if err := store.SetTask(ctx, task); err != nil {
			t.Fatalf("setting task %s: %v", task.Name, err)
		}
	}

	// List all tasks
	listed, err := store.ListTasks(ctx)
	if err != nil {
		t.Fatalf("listing tasks: %v", err)
	}

	if len(listed) != 3 {
		t.Errorf("expected 3 tasks, got %d", len(listed))
	}

	// Verify tasks are present
	found := make(map[string]bool)
	for _, task := range listed {
		found[task.Name] = true
	}

	for _, name := range []string{"research-auth", "build-api", "write-docs"} {
		if !found[name] {
			t.Errorf("expected to find task %s", name)
		}
	}
}

func TestStateCRUD(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// First create an agent (state has foreign key to agents)
	agent := &storage.Agent{
		Name:        "test-agent",
		SessionName: "agent-test-agent",
		Backend:     "claude",
		CreatedAt:   time.Now(),
	}
	if err := store.CreateAgent(ctx, agent); err != nil {
		t.Fatalf("creating agent: %v", err)
	}

	// Set state entries
	entry1 := &storage.StateEntry{
		AgentName: "test-agent",
		Key:       "status",
		Value:     "working",
		UpdatedAt: time.Now(),
	}
	if err := store.SetState(ctx, entry1); err != nil {
		t.Fatalf("setting state 1: %v", err)
	}

	entry2 := &storage.StateEntry{
		AgentName: "test-agent",
		Key:       "progress",
		Value:     "50%",
		UpdatedAt: time.Now(),
	}
	if err := store.SetState(ctx, entry2); err != nil {
		t.Fatalf("setting state 2: %v", err)
	}

	// Get single state
	retrieved, err := store.GetState(ctx, "test-agent", "status")
	if err != nil {
		t.Fatalf("getting state: %v", err)
	}
	if retrieved == nil {
		t.Fatal("expected state entry, got nil")
	}
	if retrieved.Value != "working" {
		t.Errorf("expected value 'working', got %q", retrieved.Value)
	}

	// Update state (upsert)
	entry1.Value = "completed"
	if err := store.SetState(ctx, entry1); err != nil {
		t.Fatalf("updating state: %v", err)
	}

	retrieved, err = store.GetState(ctx, "test-agent", "status")
	if err != nil {
		t.Fatalf("getting updated state: %v", err)
	}
	if retrieved.Value != "completed" {
		t.Errorf("expected updated value 'completed', got %q", retrieved.Value)
	}

	// List all state
	entries, err := store.ListState(ctx, "test-agent")
	if err != nil {
		t.Fatalf("listing state: %v", err)
	}
	if len(entries) != 2 {
		t.Errorf("expected 2 state entries, got %d", len(entries))
	}

	// Delete all state for agent
	err = store.DeleteState(ctx, "test-agent")
	if err != nil {
		t.Fatalf("deleting state: %v", err)
	}

	entries, err = store.ListState(ctx, "test-agent")
	if err != nil {
		t.Fatalf("listing state after delete: %v", err)
	}
	if len(entries) != 0 {
		t.Errorf("expected 0 state entries after delete, got %d", len(entries))
	}
}

func TestGetNonExistentState(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	state, err := store.GetState(ctx, "nonexistent", "key")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if state != nil {
		t.Error("expected nil for nonexistent state")
	}
}

func TestCascadeDeleteOnAgentRemoval(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create agent
	agent := &storage.Agent{
		Name:        "cascade-test",
		SessionName: "agent-cascade-test",
		Backend:     "claude",
		CreatedAt:   time.Now(),
	}
	if err := store.CreateAgent(ctx, agent); err != nil {
		t.Fatalf("creating agent: %v", err)
	}

	// Add state
	state := &storage.StateEntry{
		AgentName: "cascade-test",
		Key:       "test-key",
		Value:     "test-value",
		UpdatedAt: time.Now(),
	}
	if err := store.SetState(ctx, state); err != nil {
		t.Fatalf("setting state: %v", err)
	}

	// Delete agent - should cascade delete state (NOT tasks - they're standalone)
	if err := store.DeleteAgent(ctx, "cascade-test"); err != nil {
		t.Fatalf("deleting agent: %v", err)
	}

	// Verify state is deleted
	states, err := store.ListState(ctx, "cascade-test")
	if err != nil {
		t.Fatalf("listing state after cascade: %v", err)
	}
	if len(states) != 0 {
		t.Errorf("expected state to be cascade deleted, got %d", len(states))
	}
}

func TestTasksAreStandalone(t *testing.T) {
	store, cleanup := setupTestDB(t)
	defer cleanup()

	ctx := context.Background()

	// Create a task without any agent
	task := &storage.Task{
		Name:      "standalone-task",
		Content:   "This task exists independently",
		CreatedAt: time.Now(),
	}
	if err := store.SetTask(ctx, task); err != nil {
		t.Fatalf("setting standalone task: %v", err)
	}

	// Task should be retrievable
	retrieved, err := store.GetTask(ctx, "standalone-task")
	if err != nil {
		t.Fatalf("getting standalone task: %v", err)
	}
	if retrieved == nil {
		t.Fatal("expected standalone task to exist")
	}

	// Multiple "agents" could reference the same task (conceptually)
	// The task persists regardless of agent lifecycle
	if retrieved.Name != "standalone-task" {
		t.Errorf("expected name 'standalone-task', got %q", retrieved.Name)
	}
}

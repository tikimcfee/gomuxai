package service_test

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// testEnv holds test environment resources.
type testEnv struct {
	cfg      *config.Config
	store    *storage.SQLiteStorage
	tmux     *tmux.Client
	backends *backend.Registry
	service  *service.AgentService
	cleanup  func()
}

// setupTestEnv creates a full test environment with real dependencies.
func setupTestEnv(t *testing.T) *testEnv {
	t.Helper()

	// Check tmux is available
	if _, err := exec.LookPath("tmux"); err != nil {
		t.Skip("tmux not available, skipping integration test")
	}

	// Create temp database
	tmpFile, err := os.CreateTemp("", "gomuxai-service-test-*.db")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	tmpFile.Close()

	// Create temp directories
	tmpDir, err := os.MkdirTemp("", "gomuxai-test-*")
	if err != nil {
		os.Remove(tmpFile.Name())
		t.Fatalf("creating temp dir: %v", err)
	}

	// Create config with unique prefix to avoid collision
	cfg := &config.Config{
		Backend:           "claude",
		DatabasePath:      tmpFile.Name(),
		LogDir:            tmpDir + "/logs",
		StateDir:          tmpDir + "/state",
		SessionPrefix:     fmt.Sprintf("test-%d-", time.Now().UnixNano()),
		DefaultScrollback: 100,
		StartupDelay:      0, // No delay in tests for speed
	}

	if err := cfg.EnsureDirs(); err != nil {
		os.Remove(tmpFile.Name())
		os.RemoveAll(tmpDir)
		t.Fatalf("ensuring dirs: %v", err)
	}

	// Create storage
	store, err := storage.NewSQLiteStorage(tmpFile.Name())
	if err != nil {
		os.Remove(tmpFile.Name())
		os.RemoveAll(tmpDir)
		t.Fatalf("creating storage: %v", err)
	}

	// Create tmux client
	tmuxClient, err := tmux.NewClient()
	if err != nil {
		store.Close()
		os.Remove(tmpFile.Name())
		os.RemoveAll(tmpDir)
		t.Fatalf("creating tmux client: %v", err)
	}

	// Create backend registry
	backends := backend.NewRegistry()

	// Create service
	svc := service.NewAgentService(cfg, store, tmuxClient, backends)

	// Track sessions created for cleanup
	var createdSessions []string

	cleanup := func() {
		// Kill any sessions we created
		for _, session := range createdSessions {
			tmuxClient.KillSession(session)
		}
		// Also try to clean up any sessions with our prefix
		sessions, _ := tmuxClient.ListSessions()
		for _, s := range sessions {
			if strings.HasPrefix(s.Name, cfg.SessionPrefix) {
				tmuxClient.KillSession(s.Name)
			}
		}
		store.Close()
		os.Remove(tmpFile.Name())
		os.RemoveAll(tmpDir)
	}

	return &testEnv{
		cfg:      cfg,
		store:    store,
		tmux:     tmuxClient,
		backends: backends,
		service:  svc,
		cleanup:  cleanup,
	}
}

// waitForSession waits until a session's screen shows every one of wants.
func waitForSession(t *testing.T, env *testEnv, sessionName string, wants ...string) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for {
		out, _ := env.tmux.Capture(sessionName, 20, true)
		missing := false
		for _, want := range wants {
			missing = missing || !strings.Contains(out, want)
		}
		if !missing {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("session %s doesn't show %q:\n%s", sessionName, wants, out)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

// TestSpawn_FullFlow tests the complete spawn operation.
func TestSpawn_FullFlow(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	opts := service.SpawnOptions{
		Name:    "test-spawn-agent",
		Backend: "claude",
	}

	agent, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	// Verify agent was created
	if agent.Name != opts.Name {
		t.Errorf("expected name %q, got %q", opts.Name, agent.Name)
	}

	expectedSession := env.cfg.SessionPrefix + opts.Name
	if agent.SessionName != expectedSession {
		t.Errorf("expected session %q, got %q", expectedSession, agent.SessionName)
	}

	// Verify session exists in tmux
	exists, err := env.tmux.SessionExists(agent.SessionName)
	if err != nil {
		t.Fatalf("checking session exists: %v", err)
	}
	if !exists {
		t.Error("expected tmux session to exist")
	}

	// Verify agent in database
	stored, err := env.store.GetAgent(ctx, opts.Name)
	if err != nil {
		t.Fatalf("getting stored agent: %v", err)
	}
	if stored == nil {
		t.Fatal("expected agent in database")
	}
}

// TestSpawn_DuplicateAgent tests that spawning a duplicate agent fails.
func TestSpawn_DuplicateAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	opts := service.SpawnOptions{
		Name:    "test-duplicate",
		Backend: "claude",
	}

	// First spawn should succeed
	_, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("first spawn failed: %v", err)
	}

	// Second spawn should fail
	_, err = env.service.Spawn(ctx, opts)
	if err == nil {
		t.Fatal("expected error for duplicate agent")
	}

	if !strings.Contains(err.Error(), "already exists") {
		t.Errorf("expected 'already exists' error, got: %v", err)
	}
}

// TestSpawn_InvalidBackend tests spawning with a non-existent backend.
func TestSpawn_InvalidBackend(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	opts := service.SpawnOptions{
		Name:    "test-invalid-backend",
		Backend: "nonexistent-backend",
	}

	_, err := env.service.Spawn(ctx, opts)
	if err == nil {
		t.Fatal("expected error for invalid backend")
	}

	if !strings.Contains(err.Error(), "not found") {
		t.Errorf("expected 'not found' error, got: %v", err)
	}
}

// TestSpawn_WithTaskFile tests spawning with a task file.
func TestSpawn_WithTaskFile(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Create a temp task file
	taskFile, err := os.CreateTemp("", "test-task-*.md")
	if err != nil {
		t.Fatalf("creating task file: %v", err)
	}
	taskFile.WriteString("# Test Task\nDo something.")
	taskFile.Close()
	defer os.Remove(taskFile.Name())

	opts := service.SpawnOptions{
		Name:     "test-with-task",
		Backend:  "claude",
		TaskFile: taskFile.Name(),
	}

	agent, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	if agent.TaskFile != taskFile.Name() {
		t.Errorf("expected task file %q, got %q", taskFile.Name(), agent.TaskFile)
	}
}

// Custom JSON backends (--backend-config) have no Provider factory: their
// command is typed into the session as is, and their status comes from
// their patterns.
func TestSpawn_CustomBackend(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()

	cfgPath := filepath.Join(t.TempDir(), "backend.json")
	cfgJSON := `{"name": "custom", "launch_command": "echo custom-launched", "init_message_format": "echo task %s", "idle_pattern": "custom-launched"}`
	if err := os.WriteFile(cfgPath, []byte(cfgJSON), 0644); err != nil {
		t.Fatal(err)
	}
	if err := env.backends.LoadFromFile(cfgPath); err != nil {
		t.Fatalf("loading backend config: %v", err)
	}

	agent, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "custom-worker", Backend: "custom", TaskFile: "plan.md"})
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}
	if agent.Backend != "custom" || agent.TaskFile != "plan.md" {
		t.Errorf("agent = %+v", agent)
	}
	if stored, err := env.store.GetAgent(ctx, "custom-worker"); err != nil || stored == nil {
		t.Fatalf("agent not stored: %v", err)
	}

	waitForSession(t, env, agent.SessionName, "echo custom-launched", "echo task plan.md")
	if _, err := os.Stat(filepath.Join(env.cfg.AgentDir("custom-worker"), "launch.sh")); !os.IsNotExist(err) {
		t.Errorf("custom backend should launch without a script: %v", err)
	}

	info, err := env.service.StatusInfo(ctx, "custom-worker")
	if err != nil {
		t.Fatalf("StatusInfo: %v", err)
	}
	if info.Status != service.AgentStatusIdle || info.Source != service.StatusSourceScrape {
		t.Errorf("got %s from %s, want idle from scrape", info.Status, info.Source)
	}
}

// TestSend_ToExistingAgent tests sending a message to an existing agent.
func TestSend_ToExistingAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// First spawn an agent
	opts := service.SpawnOptions{
		Name:    "test-send-target",
		Backend: "claude",
	}

	_, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	// Wait briefly for session to be ready
	time.Sleep(100 * time.Millisecond)

	// Send a message
	err = env.service.Send(ctx, opts.Name, "hello world")
	if err != nil {
		t.Fatalf("send failed: %v", err)
	}
}

// TestSend_ToMissingAgent tests sending to a non-existent agent.
func TestSend_ToMissingAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	err := env.service.Send(ctx, "nonexistent-agent", "hello")
	if err == nil {
		t.Fatal("expected error for missing agent")
	}

	if !strings.Contains(err.Error(), "not found") {
		t.Errorf("expected 'not found' error, got: %v", err)
	}
}

// TestQueue_MessageQueuing tests queuing a message for delivery.
func TestQueue_MessageQueuing(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Create receiver agent
	opts := service.SpawnOptions{
		Name:    "test-queue-receiver",
		Backend: "claude",
	}

	_, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	// Queue a message
	err = env.service.Queue(ctx, "sender-agent", opts.Name, "queued message content")
	if err != nil {
		t.Fatalf("queue failed: %v", err)
	}

	// Verify message was stored
	messages, err := env.store.GetPendingMessages(ctx, opts.Name, 10)
	if err != nil {
		t.Fatalf("getting messages: %v", err)
	}

	if len(messages) != 1 {
		t.Fatalf("expected 1 pending message, got %d", len(messages))
	}

	if messages[0].Content != "queued message content" {
		t.Errorf("expected content %q, got %q", "queued message content", messages[0].Content)
	}
}

// TestQueue_InvalidReceiver tests queuing to a non-existent receiver.
func TestQueue_InvalidReceiver(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	err := env.service.Queue(ctx, "sender", "nonexistent-receiver", "message")
	if err == nil {
		t.Fatal("expected error for invalid receiver")
	}

	if !strings.Contains(err.Error(), "not found") {
		t.Errorf("expected 'not found' error, got: %v", err)
	}
}

// TestCapture_Output tests capturing output from an agent's session.
func TestCapture_Output(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Spawn agent
	opts := service.SpawnOptions{
		Name:    "test-capture",
		Backend: "claude",
	}

	agent, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	// Wait for session to be ready
	time.Sleep(100 * time.Millisecond)

	// Capture output (should return something, even if minimal)
	output, err := env.service.Capture(ctx, opts.Name, 50, false)
	if err != nil {
		t.Fatalf("capture failed: %v", err)
	}

	// Output should be a string (may be empty initially, that's OK)
	_ = output
	_ = agent
}

// TestCapture_DefaultLineLimit tests that default line limit is applied.
func TestCapture_DefaultLineLimit(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Spawn agent
	opts := service.SpawnOptions{
		Name:    "test-capture-default",
		Backend: "claude",
	}

	_, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	time.Sleep(100 * time.Millisecond)

	// Capture with 0 lines should use default
	_, err = env.service.Capture(ctx, opts.Name, 0, false)
	if err != nil {
		t.Fatalf("capture with default lines failed: %v", err)
	}
}

// TestCapture_MissingAgent tests capturing from non-existent agent.
func TestCapture_MissingAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	_, err := env.service.Capture(ctx, "nonexistent", 50, false)
	if err == nil {
		t.Fatal("expected error for missing agent")
	}
}

// TestStatus_Detection tests status detection using backend patterns.
func TestStatus_Detection(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Spawn agent
	opts := service.SpawnOptions{
		Name:    "test-status",
		Backend: "claude",
	}

	_, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	// Wait for CLI to start
	time.Sleep(100 * time.Millisecond)

	// Get status (might be unknown, idle, or processing depending on timing)
	status, err := env.service.Status(ctx, opts.Name)
	if err != nil {
		t.Fatalf("status failed: %v", err)
	}

	// Status should be one of the valid values
	validStatuses := map[service.AgentStatus]bool{
		service.AgentStatusIdle:       true,
		service.AgentStatusProcessing: true,
		service.AgentStatusUnknown:    true,
	}

	if !validStatuses[status] {
		t.Errorf("unexpected status: %v", status)
	}
}

// TestStatus_MissingAgent tests status of non-existent agent.
func TestStatus_MissingAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	_, err := env.service.Status(ctx, "nonexistent")
	if err == nil {
		t.Fatal("expected error for missing agent")
	}
}

// TestList_EnumeratesAgents tests listing all agents.
func TestList_EnumeratesAgents(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Initially empty
	agents, err := env.service.List(ctx)
	if err != nil {
		t.Fatalf("list failed: %v", err)
	}
	if len(agents) != 0 {
		t.Errorf("expected 0 agents initially, got %d", len(agents))
	}

	// Spawn two agents
	for _, name := range []string{"list-agent-1", "list-agent-2"} {
		_, err := env.service.Spawn(ctx, service.SpawnOptions{
			Name:    name,
			Backend: "claude",
		})
		if err != nil {
			t.Fatalf("spawn %s failed: %v", name, err)
		}
	}

	// List should return both
	agents, err = env.service.List(ctx)
	if err != nil {
		t.Fatalf("list failed: %v", err)
	}
	if len(agents) != 2 {
		t.Errorf("expected 2 agents, got %d", len(agents))
	}
}

// isCleanupError checks if an error is a CleanupError (non-fatal cleanup issues).
func isCleanupError(err error) bool {
	_, ok := err.(*service.CleanupError)
	return ok
}

// TestCleanup_FullFlow tests the complete cleanup operation.
func TestCleanup_FullFlow(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Spawn agent
	opts := service.SpawnOptions{
		Name:    "test-cleanup",
		Backend: "claude",
	}

	agent, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	// Verify it exists
	exists, _ := env.tmux.SessionExists(agent.SessionName)
	if !exists {
		t.Fatal("session should exist before cleanup")
	}

	// Cleanup - may return CleanupError for non-fatal issues (e.g., pipe pane)
	err = env.service.Cleanup(ctx, opts.Name)
	if err != nil && !isCleanupError(err) {
		t.Fatalf("cleanup failed: %v", err)
	}

	// Verify session is gone
	exists, _ = env.tmux.SessionExists(agent.SessionName)
	if exists {
		t.Error("session should not exist after cleanup")
	}

	// Verify agent is gone from DB
	stored, err := env.store.GetAgent(ctx, opts.Name)
	if err != nil {
		t.Fatalf("getting agent: %v", err)
	}
	if stored != nil {
		t.Error("agent should not exist in DB after cleanup")
	}
}

// TestCleanup_MissingAgent tests cleanup of non-existent agent.
func TestCleanup_MissingAgent(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	err := env.service.Cleanup(ctx, "nonexistent")
	if err == nil {
		t.Fatal("expected error for missing agent")
	}
}

// TestCleanup_CleansMessages tests that cleanup removes associated messages.
func TestCleanup_CleansMessages(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	ctx := context.Background()

	// Spawn agent
	opts := service.SpawnOptions{
		Name:    "test-cleanup-messages",
		Backend: "claude",
	}

	_, err := env.service.Spawn(ctx, opts)
	if err != nil {
		t.Fatalf("spawn failed: %v", err)
	}

	// Queue a message to this agent
	err = env.service.Queue(ctx, "sender", opts.Name, "test message")
	if err != nil {
		t.Fatalf("queue failed: %v", err)
	}

	// Verify message exists
	messages, _ := env.store.GetPendingMessages(ctx, opts.Name, 10)
	if len(messages) != 1 {
		t.Fatal("expected 1 message before cleanup")
	}

	// Cleanup - may return CleanupError for non-fatal issues
	err = env.service.Cleanup(ctx, opts.Name)
	if err != nil && !isCleanupError(err) {
		t.Fatalf("cleanup failed: %v", err)
	}

	// Messages should be gone (can't check via service since agent is gone)
	// Direct storage check
	messages, _ = env.store.GetPendingMessages(ctx, opts.Name, 10)
	if len(messages) != 0 {
		t.Errorf("expected 0 messages after cleanup, got %d", len(messages))
	}
}

// TestSpawn_CleanupOnFailure tests that session is cleaned up if storage fails.
func TestSpawn_CleanupOnFailure(t *testing.T) {
	// This test would require error injection into the storage layer
	// which we can't do with real SQLite without mocks.
	// Skip for now - document as a limitation.
	t.Skip("requires storage error injection - covered by unit tests with mocks")
}

// TestSpawn_ContextCancellation tests that spawn respects context cancellation.
func TestSpawn_ContextCancellation(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()

	// Create a config with longer startup delay for this test
	env.cfg.StartupDelay = 5 * time.Second

	// Create a context that will be cancelled
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()

	opts := service.SpawnOptions{
		Name:    "test-context-cancel",
		Backend: "claude",
	}

	_, err := env.service.Spawn(ctx, opts)
	if err == nil {
		t.Fatal("expected error due to context cancellation")
	}

	// Should get context deadline exceeded or cancelled
	if err != context.DeadlineExceeded && err != context.Canceled {
		t.Errorf("expected context error, got: %v", err)
	}

	// Session should have been cleaned up
	sessionName := env.cfg.SessionPrefix + opts.Name
	exists, _ := env.tmux.SessionExists(sessionName)
	if exists {
		t.Error("session should be cleaned up after context cancellation")
		// Clean it up manually
		env.tmux.KillSession(sessionName)
	}
}

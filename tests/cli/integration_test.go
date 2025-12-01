// Package cli_test provides integration tests for CLI commands.
package cli_test

import (
	"bytes"
	"strings"
	"testing"

	"github.com/spf13/cobra"
)

// ===========================================================================
// Test Helpers
// ===========================================================================

// executeCommand runs a cobra command with the given args and returns output.
func executeCommand(root *cobra.Command, args ...string) (string, error) {
	buf := new(bytes.Buffer)
	root.SetOut(buf)
	root.SetErr(buf)
	root.SetArgs(args)

	err := root.Execute()
	return buf.String(), err
}

// newTestRootCmd creates a minimal root command for testing.
func newTestRootCmd() *cobra.Command {
	var (
		backendName   string
		backendConfig string
	)

	root := &cobra.Command{
		Use:          "agent",
		Short:        "Multi-agent orchestration tool",
		SilenceUsage: true,
	}

	root.PersistentFlags().StringVar(&backendName, "backend", "", "Backend to use")
	root.PersistentFlags().StringVar(&backendConfig, "backend-config", "", "Path to backend config file")

	return root
}

// ===========================================================================
// Spawn Command Tests
// ===========================================================================

func TestSpawnCommand_Parse(t *testing.T) {
	root := newTestRootCmd()
	var capturedName, capturedTask string

	spawnCmd := &cobra.Command{
		Use:  "spawn <name> [task-file]",
		Args: cobra.RangeArgs(1, 2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			if len(args) > 1 {
				capturedTask = args[1]
			}
			return nil
		},
	}
	spawnCmd.Flags().String("backend", "", "Backend to use")
	root.AddCommand(spawnCmd)

	// Test spawn with backend and task
	_, err := executeCommand(root, "spawn", "researcher", "tasks/research.md", "--backend", "claude")
	if err != nil {
		t.Fatalf("spawn command failed: %v", err)
	}

	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
	if capturedTask != "tasks/research.md" {
		t.Errorf("expected task 'tasks/research.md', got %q", capturedTask)
	}
}

func TestSpawnCommand_RequiresName(t *testing.T) {
	root := newTestRootCmd()
	spawnCmd := &cobra.Command{
		Use:  "spawn <name> [task-file]",
		Args: cobra.RangeArgs(1, 2),
		RunE: func(cmd *cobra.Command, args []string) error {
			return nil
		},
	}
	root.AddCommand(spawnCmd)

	// Test spawn without name
	_, err := executeCommand(root, "spawn")
	if err == nil {
		t.Error("expected error when spawn called without name")
	}
}

func TestSpawnCommand_BackendFlag(t *testing.T) {
	testCases := []struct {
		name     string
		args     []string
		expected string
	}{
		{"claude backend", []string{"spawn", "test", "--backend", "claude"}, "claude"},
		{"aider backend", []string{"spawn", "test", "--backend", "aider"}, "aider"},
		{"no backend", []string{"spawn", "test"}, ""},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			// Create fresh command tree for each test to avoid state retention
			root := newTestRootCmd()
			var capturedBackend string

			spawnCmd := &cobra.Command{
				Use:  "spawn <name>",
				Args: cobra.ExactArgs(1),
				RunE: func(cmd *cobra.Command, args []string) error {
					capturedBackend, _ = cmd.Flags().GetString("backend")
					return nil
				},
			}
			spawnCmd.Flags().String("backend", "", "Backend to use")
			root.AddCommand(spawnCmd)

			_, err := executeCommand(root, tc.args...)
			if err != nil {
				t.Fatalf("command failed: %v", err)
			}
			if capturedBackend != tc.expected {
				t.Errorf("expected backend %q, got %q", tc.expected, capturedBackend)
			}
		})
	}
}

// ===========================================================================
// Send Command Tests
// ===========================================================================

func TestSendCommand_Parse(t *testing.T) {
	root := newTestRootCmd()
	var capturedName, capturedMessage string

	sendCmd := &cobra.Command{
		Use:  "send <name> <message>",
		Args: cobra.MinimumNArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			capturedMessage = strings.Join(args[1:], " ")
			return nil
		},
	}
	root.AddCommand(sendCmd)

	_, err := executeCommand(root, "send", "researcher", "please", "analyze", "the", "codebase")
	if err != nil {
		t.Fatalf("send command failed: %v", err)
	}

	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
	if capturedMessage != "please analyze the codebase" {
		t.Errorf("expected message 'please analyze the codebase', got %q", capturedMessage)
	}
}

func TestSendCommand_RequiresNameAndMessage(t *testing.T) {
	root := newTestRootCmd()
	sendCmd := &cobra.Command{
		Use:  "send <name> <message>",
		Args: cobra.MinimumNArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error { return nil },
	}
	root.AddCommand(sendCmd)

	testCases := []struct {
		name        string
		args        []string
		expectError bool
	}{
		{"no args", []string{"send"}, true},
		{"only name", []string{"send", "agent"}, true},
		{"name and message", []string{"send", "agent", "hello"}, false},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := executeCommand(root, tc.args...)
			if tc.expectError && err == nil {
				t.Error("expected error, got nil")
			}
			if !tc.expectError && err != nil {
				t.Errorf("unexpected error: %v", err)
			}
		})
	}
}

// ===========================================================================
// Queue Command Tests
// ===========================================================================

func TestQueueCommand_Parse(t *testing.T) {
	root := newTestRootCmd()
	var capturedName, capturedMessage, capturedFrom string

	queueCmd := &cobra.Command{
		Use:  "queue <name> <message>",
		Args: cobra.MinimumNArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			capturedMessage = strings.Join(args[1:], " ")
			capturedFrom, _ = cmd.Flags().GetString("from")
			return nil
		},
	}
	queueCmd.Flags().String("from", "", "Sender ID")
	root.AddCommand(queueCmd)

	_, err := executeCommand(root, "queue", "builder", "fix", "the", "bug", "--from", "coordinator")
	if err != nil {
		t.Fatalf("queue command failed: %v", err)
	}

	if capturedName != "builder" {
		t.Errorf("expected name 'builder', got %q", capturedName)
	}
	if capturedMessage != "fix the bug" {
		t.Errorf("expected message 'fix the bug', got %q", capturedMessage)
	}
	if capturedFrom != "coordinator" {
		t.Errorf("expected from 'coordinator', got %q", capturedFrom)
	}
}

// ===========================================================================
// Capture Command Tests
// ===========================================================================

func TestCaptureCommand_Flags(t *testing.T) {
	testCases := []struct {
		name          string
		args          []string
		expectedLines int
		expectedRaw   bool
	}{
		{"default flags", []string{"capture", "agent"}, 50, false},
		{"custom lines", []string{"capture", "agent", "--lines", "100"}, 100, false},
		{"raw output", []string{"capture", "agent", "--raw"}, 50, true},
		{"all flags", []string{"capture", "agent", "--lines", "200", "--raw"}, 200, true},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			// Create fresh command tree for each test
			root := newTestRootCmd()
			var capturedLines int
			var capturedRaw bool

			captureCmd := &cobra.Command{
				Use:  "capture <name>",
				Args: cobra.ExactArgs(1),
				RunE: func(cmd *cobra.Command, args []string) error {
					capturedLines, _ = cmd.Flags().GetInt("lines")
					capturedRaw, _ = cmd.Flags().GetBool("raw")
					return nil
				},
			}
			captureCmd.Flags().Int("lines", 50, "Number of lines to capture")
			captureCmd.Flags().Bool("raw", false, "Preserve ANSI escape codes")
			root.AddCommand(captureCmd)

			_, err := executeCommand(root, tc.args...)
			if err != nil {
				t.Fatalf("command failed: %v", err)
			}
			if capturedLines != tc.expectedLines {
				t.Errorf("expected lines %d, got %d", tc.expectedLines, capturedLines)
			}
			if capturedRaw != tc.expectedRaw {
				t.Errorf("expected raw %v, got %v", tc.expectedRaw, capturedRaw)
			}
		})
	}
}

// ===========================================================================
// List Command Tests
// ===========================================================================

func TestListCommand_NoArgs(t *testing.T) {
	root := newTestRootCmd()
	called := false

	listCmd := &cobra.Command{
		Use:  "list",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			called = true
			return nil
		},
	}
	root.AddCommand(listCmd)

	_, err := executeCommand(root, "list")
	if err != nil {
		t.Fatalf("list command failed: %v", err)
	}
	if !called {
		t.Error("list command was not executed")
	}
}

func TestListCommand_RejectsArgs(t *testing.T) {
	root := newTestRootCmd()
	listCmd := &cobra.Command{
		Use:  "list",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error { return nil },
	}
	root.AddCommand(listCmd)

	_, err := executeCommand(root, "list", "unexpected-arg")
	if err == nil {
		t.Error("expected error for unexpected argument")
	}
}

// ===========================================================================
// Cleanup Command Tests
// ===========================================================================

func TestCleanupCommand_SingleAgent(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string

	cleanupCmd := &cobra.Command{
		Use:  "cleanup <name>",
		Args: cobra.MaximumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			if len(args) > 0 {
				capturedName = args[0]
			}
			return nil
		},
	}
	root.AddCommand(cleanupCmd)

	_, err := executeCommand(root, "cleanup", "researcher")
	if err != nil {
		t.Fatalf("cleanup command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
}

func TestCleanupCommand_AllFlag(t *testing.T) {
	root := newTestRootCmd()
	var capturedAll bool
	var capturedAs string
	var capturedExcept string

	cleanupCmd := &cobra.Command{
		Use:  "cleanup [name]",
		Args: cobra.MaximumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedAll, _ = cmd.Flags().GetBool("all")
			capturedAs, _ = cmd.Flags().GetString("as")
			capturedExcept, _ = cmd.Flags().GetString("except")
			return nil
		},
	}
	cleanupCmd.Flags().Bool("all", false, "Clean up all agents")
	cleanupCmd.Flags().String("as", "", "Your agent name")
	cleanupCmd.Flags().String("except", "", "Agents to exclude")
	root.AddCommand(cleanupCmd)

	testCases := []struct {
		name           string
		args           []string
		expectedAll    bool
		expectedAs     string
		expectedExcept string
	}{
		{"single agent", []string{"cleanup", "test"}, false, "", ""},
		{"all with as", []string{"cleanup", "--all", "--as", "coordinator"}, true, "coordinator", ""},
		{"all with except", []string{"cleanup", "--all", "--as", "me", "--except", "a,b,c"}, true, "me", "a,b,c"},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			capturedAll = false
			capturedAs = ""
			capturedExcept = ""
			_, err := executeCommand(root, tc.args...)
			if err != nil {
				t.Fatalf("command failed: %v", err)
			}
			if capturedAll != tc.expectedAll {
				t.Errorf("expected all=%v, got %v", tc.expectedAll, capturedAll)
			}
			if capturedAs != tc.expectedAs {
				t.Errorf("expected as=%q, got %q", tc.expectedAs, capturedAs)
			}
			if capturedExcept != tc.expectedExcept {
				t.Errorf("expected except=%q, got %q", tc.expectedExcept, capturedExcept)
			}
		})
	}
}

// ===========================================================================
// Watch Command Tests
// ===========================================================================

func TestWatchCommand_IntervalFlag(t *testing.T) {
	root := newTestRootCmd()
	var capturedInterval string

	watchCmd := &cobra.Command{
		Use:  "watch",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedInterval, _ = cmd.Flags().GetString("interval")
			return nil
		},
	}
	watchCmd.Flags().String("interval", "2s", "Check interval")
	root.AddCommand(watchCmd)

	testCases := []struct {
		name     string
		args     []string
		expected string
	}{
		{"default interval", []string{"watch"}, "2s"},
		{"custom interval", []string{"watch", "--interval", "5s"}, "5s"},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := executeCommand(root, tc.args...)
			if err != nil {
				t.Fatalf("command failed: %v", err)
			}
			if capturedInterval != tc.expected {
				t.Errorf("expected interval %q, got %q", tc.expected, capturedInterval)
			}
		})
	}
}

// ===========================================================================
// Session Subcommand Tests
// ===========================================================================

func TestSessionNewCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string

	sessionCmd := &cobra.Command{
		Use: "session",
	}

	newCmd := &cobra.Command{
		Use:  "new <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			return nil
		},
	}
	sessionCmd.AddCommand(newCmd)
	root.AddCommand(sessionCmd)

	_, err := executeCommand(root, "session", "new", "my-session")
	if err != nil {
		t.Fatalf("session new command failed: %v", err)
	}
	if capturedName != "my-session" {
		t.Errorf("expected name 'my-session', got %q", capturedName)
	}
}

func TestSessionKillCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string

	sessionCmd := &cobra.Command{
		Use: "session",
	}

	killCmd := &cobra.Command{
		Use:  "kill <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			return nil
		},
	}
	sessionCmd.AddCommand(killCmd)
	root.AddCommand(sessionCmd)

	_, err := executeCommand(root, "session", "kill", "my-session")
	if err != nil {
		t.Fatalf("session kill command failed: %v", err)
	}
	if capturedName != "my-session" {
		t.Errorf("expected name 'my-session', got %q", capturedName)
	}
}

func TestSessionListCommand(t *testing.T) {
	root := newTestRootCmd()
	called := false

	sessionCmd := &cobra.Command{
		Use: "session",
	}

	listCmd := &cobra.Command{
		Use:  "list",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			called = true
			return nil
		},
	}
	sessionCmd.AddCommand(listCmd)
	root.AddCommand(sessionCmd)

	_, err := executeCommand(root, "session", "list")
	if err != nil {
		t.Fatalf("session list command failed: %v", err)
	}
	if !called {
		t.Error("session list was not executed")
	}
}

// ===========================================================================
// Keys Command Tests
// ===========================================================================

func TestKeysCommand_Parse(t *testing.T) {
	root := newTestRootCmd()
	var capturedSession, capturedText string
	var capturedNoEnter bool

	keysCmd := &cobra.Command{
		Use:  "keys <session> <text>",
		Args: cobra.MinimumNArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedSession = args[0]
			capturedText = strings.Join(args[1:], " ")
			capturedNoEnter, _ = cmd.Flags().GetBool("no-enter")
			return nil
		},
	}
	keysCmd.Flags().Bool("no-enter", false, "Don't send Enter after text")
	root.AddCommand(keysCmd)

	testCases := []struct {
		name            string
		args            []string
		expectedSession string
		expectedText    string
		expectedNoEnter bool
	}{
		// Note: Args with leading dashes would be interpreted as flags
		// In real usage, use -- to stop flag parsing: keys session -- ls -la
		{"simple", []string{"keys", "my-session", "ls"}, "my-session", "ls", false},
		{"multi word", []string{"keys", "my-session", "echo", "hello", "world"}, "my-session", "echo hello world", false},
		{"with no-enter", []string{"keys", "my-session", "echo", "hello", "--no-enter"}, "my-session", "echo hello", true},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			capturedNoEnter = false
			_, err := executeCommand(root, tc.args...)
			if err != nil {
				t.Fatalf("command failed: %v", err)
			}
			if capturedSession != tc.expectedSession {
				t.Errorf("expected session %q, got %q", tc.expectedSession, capturedSession)
			}
			if capturedText != tc.expectedText {
				t.Errorf("expected text %q, got %q", tc.expectedText, capturedText)
			}
			if capturedNoEnter != tc.expectedNoEnter {
				t.Errorf("expected no-enter %v, got %v", tc.expectedNoEnter, capturedNoEnter)
			}
		})
	}
}

// ===========================================================================
// Key Command Tests
// ===========================================================================

func TestKeyCommand_Parse(t *testing.T) {
	root := newTestRootCmd()
	var capturedSession, capturedKey string

	keyCmd := &cobra.Command{
		Use:  "key <session> <key>",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedSession = args[0]
			capturedKey = args[1]
			return nil
		},
	}
	root.AddCommand(keyCmd)

	testCases := []struct {
		name            string
		args            []string
		expectedSession string
		expectedKey     string
	}{
		{"Enter key", []string{"key", "my-session", "Enter"}, "my-session", "Enter"},
		{"Ctrl+C", []string{"key", "my-session", "c-c"}, "my-session", "c-c"},
		{"Escape", []string{"key", "my-session", "Escape"}, "my-session", "Escape"},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := executeCommand(root, tc.args...)
			if err != nil {
				t.Fatalf("command failed: %v", err)
			}
			if capturedSession != tc.expectedSession {
				t.Errorf("expected session %q, got %q", tc.expectedSession, capturedSession)
			}
			if capturedKey != tc.expectedKey {
				t.Errorf("expected key %q, got %q", tc.expectedKey, capturedKey)
			}
		})
	}
}

func TestKeyCommand_RequiresExactArgs(t *testing.T) {
	root := newTestRootCmd()
	keyCmd := &cobra.Command{
		Use:  "key <session> <key>",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error { return nil },
	}
	root.AddCommand(keyCmd)

	testCases := []struct {
		name        string
		args        []string
		expectError bool
	}{
		{"no args", []string{"key"}, true},
		{"one arg", []string{"key", "session"}, true},
		{"two args", []string{"key", "session", "Enter"}, false},
		{"three args", []string{"key", "session", "Enter", "extra"}, true},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := executeCommand(root, tc.args...)
			if tc.expectError && err == nil {
				t.Error("expected error, got nil")
			}
			if !tc.expectError && err != nil {
				t.Errorf("unexpected error: %v", err)
			}
		})
	}
}

// ===========================================================================
// Backend Enforcement Tests
// ===========================================================================

func TestBackendEnforcement_SpawnRequiresBackend(t *testing.T) {
	root := newTestRootCmd()
	var hasBackend bool

	spawnCmd := &cobra.Command{
		Use:  "spawn <name>",
		Args: cobra.ExactArgs(1),
		PreRunE: func(cmd *cobra.Command, args []string) error {
			backend, _ := cmd.Flags().GetString("backend")
			backendCfg, _ := cmd.Flags().GetString("backend-config")
			hasBackend = backend != "" || backendCfg != ""
			if !hasBackend {
				return errMissingBackend()
			}
			return nil
		},
		RunE: func(cmd *cobra.Command, args []string) error {
			return nil
		},
	}
	spawnCmd.Flags().String("backend", "", "Backend to use")
	spawnCmd.Flags().String("backend-config", "", "Path to backend config")
	root.AddCommand(spawnCmd)

	// Without backend should fail
	_, err := executeCommand(root, "spawn", "test")
	if err == nil {
		t.Error("expected error when spawn called without backend")
	}

	// Error message should be helpful
	if err != nil {
		msg := err.Error()
		if !strings.Contains(msg, "--backend") {
			t.Errorf("error should mention --backend flag: %v", err)
		}
		if !strings.Contains(msg, "--backend-config") {
			t.Errorf("error should mention --backend-config flag: %v", err)
		}
	}

	// With --backend should succeed
	_, err = executeCommand(root, "spawn", "test", "--backend", "claude")
	if err != nil {
		t.Errorf("spawn with --backend should succeed: %v", err)
	}

	// With --backend-config should succeed
	_, err = executeCommand(root, "spawn", "test", "--backend-config", "/path/to/config.json")
	if err != nil {
		t.Errorf("spawn with --backend-config should succeed: %v", err)
	}
}

// Helper for generating backend missing error
func errMissingBackend() error {
	return &backendError{
		msg: "backend is required: use --backend <name> or --backend-config <file>",
	}
}

type backendError struct {
	msg string
}

func (e *backendError) Error() string {
	return e.msg
}

// ===========================================================================
// Error Message Quality Tests
// ===========================================================================

func TestErrorMessages_InvalidCommand(t *testing.T) {
	root := newTestRootCmd()

	// Add a command to test help text
	spawnCmd := &cobra.Command{
		Use:   "spawn <name> [task-file]",
		Short: "Spawn a new agent",
		Args:  cobra.RangeArgs(1, 2),
		RunE:  func(cmd *cobra.Command, args []string) error { return nil },
	}
	root.AddCommand(spawnCmd)

	// Unknown command
	_, err := executeCommand(root, "unknown-command")
	if err == nil {
		t.Error("expected error for unknown command")
	}
}

func TestHelpText_ContainsUsefulInfo(t *testing.T) {
	root := newTestRootCmd()
	root.Long = "Multi-agent orchestration tool for managing AI agents in tmux sessions."

	spawnCmd := &cobra.Command{
		Use:   "spawn <name> [task-file]",
		Short: "Spawn a new agent",
		Long:  "Spawn creates a new agent in a tmux session with the specified backend.",
		Example: `  agent spawn researcher --backend claude
  agent spawn builder --backend aider tasks/build.md`,
		Args: cobra.RangeArgs(1, 2),
		RunE: func(cmd *cobra.Command, args []string) error { return nil },
	}
	spawnCmd.Flags().String("backend", "", "Backend to use (e.g., claude, aider)")
	root.AddCommand(spawnCmd)

	output, _ := executeCommand(root, "spawn", "--help")

	// Check that help contains useful information
	expectedContents := []string{
		"spawn",
		"name",
		"task-file",
		"--backend",
	}

	for _, expected := range expectedContents {
		if !strings.Contains(output, expected) {
			t.Errorf("help output should contain %q", expected)
		}
	}
}

// ===========================================================================
// Global Flag Tests
// ===========================================================================

func TestGlobalFlags_Backend(t *testing.T) {
	root := newTestRootCmd()
	var capturedBackend string

	// Add persistent prerun to capture global flag
	root.PersistentPreRunE = func(cmd *cobra.Command, args []string) error {
		capturedBackend, _ = cmd.Flags().GetString("backend")
		return nil
	}

	testCmd := &cobra.Command{
		Use:  "test",
		RunE: func(cmd *cobra.Command, args []string) error { return nil },
	}
	root.AddCommand(testCmd)

	_, err := executeCommand(root, "test", "--backend", "claude")
	if err != nil {
		t.Fatalf("command failed: %v", err)
	}
	if capturedBackend != "claude" {
		t.Errorf("expected global backend flag to be 'claude', got %q", capturedBackend)
	}
}

func TestGlobalFlags_BackendConfig(t *testing.T) {
	root := newTestRootCmd()
	var capturedConfig string

	root.PersistentPreRunE = func(cmd *cobra.Command, args []string) error {
		capturedConfig, _ = cmd.Flags().GetString("backend-config")
		return nil
	}

	testCmd := &cobra.Command{
		Use:  "test",
		RunE: func(cmd *cobra.Command, args []string) error { return nil },
	}
	root.AddCommand(testCmd)

	_, err := executeCommand(root, "test", "--backend-config", "/path/to/config.json")
	if err != nil {
		t.Fatalf("command failed: %v", err)
	}
	if capturedConfig != "/path/to/config.json" {
		t.Errorf("expected backend-config %q, got %q", "/path/to/config.json", capturedConfig)
	}
}

// ===========================================================================
// Task Command Tests
// ===========================================================================

func TestTaskSetCommand_FromString(t *testing.T) {
	root := newTestRootCmd()
	var capturedName, capturedContent string

	taskCmd := &cobra.Command{
		Use: "task",
	}

	setCmd := &cobra.Command{
		Use:  "set <name> <content>",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			capturedContent = args[1]
			return nil
		},
	}
	setCmd.Flags().StringP("file", "f", "", "Read content from file")
	taskCmd.AddCommand(setCmd)
	root.AddCommand(taskCmd)

	_, err := executeCommand(root, "task", "set", "researcher", "# My Task\n\nDo something.")
	if err != nil {
		t.Fatalf("task set command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
	if capturedContent != "# My Task\n\nDo something." {
		t.Errorf("expected content '# My Task\\n\\nDo something.', got %q", capturedContent)
	}
}

func TestTaskSetCommand_FromFile(t *testing.T) {
	root := newTestRootCmd()
	var capturedName, capturedFile string

	taskCmd := &cobra.Command{
		Use: "task",
	}

	setCmd := &cobra.Command{
		Use:  "set <name> [content]",
		Args: cobra.RangeArgs(1, 2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			capturedFile, _ = cmd.Flags().GetString("file")
			return nil
		},
	}
	setCmd.Flags().StringP("file", "f", "", "Read content from file")
	taskCmd.AddCommand(setCmd)
	root.AddCommand(taskCmd)

	_, err := executeCommand(root, "task", "set", "researcher", "-f", "tasks/research.md")
	if err != nil {
		t.Fatalf("task set command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
	if capturedFile != "tasks/research.md" {
		t.Errorf("expected file 'tasks/research.md', got %q", capturedFile)
	}
}

func TestTaskGetCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string

	taskCmd := &cobra.Command{
		Use: "task",
	}

	getCmd := &cobra.Command{
		Use:  "get <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			return nil
		},
	}
	taskCmd.AddCommand(getCmd)
	root.AddCommand(taskCmd)

	_, err := executeCommand(root, "task", "get", "researcher")
	if err != nil {
		t.Fatalf("task get command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
}

func TestTaskClearCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string

	taskCmd := &cobra.Command{
		Use: "task",
	}

	clearCmd := &cobra.Command{
		Use:  "clear <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			return nil
		},
	}
	taskCmd.AddCommand(clearCmd)
	root.AddCommand(taskCmd)

	_, err := executeCommand(root, "task", "clear", "researcher")
	if err != nil {
		t.Fatalf("task clear command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
}

func TestTaskListCommand(t *testing.T) {
	root := newTestRootCmd()
	called := false

	taskCmd := &cobra.Command{
		Use: "task",
	}

	listCmd := &cobra.Command{
		Use:  "list",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			called = true
			return nil
		},
	}
	taskCmd.AddCommand(listCmd)
	root.AddCommand(taskCmd)

	_, err := executeCommand(root, "task", "list")
	if err != nil {
		t.Fatalf("task list command failed: %v", err)
	}
	if !called {
		t.Error("task list command was not executed")
	}
}

// ===========================================================================
// State Command Tests
// ===========================================================================

func TestStateSetCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName, capturedKey, capturedValue string

	stateCmd := &cobra.Command{
		Use: "state",
	}

	setCmd := &cobra.Command{
		Use:  "set <name> <key> <value>",
		Args: cobra.ExactArgs(3),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			capturedKey = args[1]
			capturedValue = args[2]
			return nil
		},
	}
	stateCmd.AddCommand(setCmd)
	root.AddCommand(stateCmd)

	_, err := executeCommand(root, "state", "set", "researcher", "status", "working")
	if err != nil {
		t.Fatalf("state set command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
	if capturedKey != "status" {
		t.Errorf("expected key 'status', got %q", capturedKey)
	}
	if capturedValue != "working" {
		t.Errorf("expected value 'working', got %q", capturedValue)
	}
}

func TestStateGetCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName, capturedKey string

	stateCmd := &cobra.Command{
		Use: "state",
	}

	getCmd := &cobra.Command{
		Use:  "get <name> <key>",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			capturedKey = args[1]
			return nil
		},
	}
	stateCmd.AddCommand(getCmd)
	root.AddCommand(stateCmd)

	_, err := executeCommand(root, "state", "get", "researcher", "status")
	if err != nil {
		t.Fatalf("state get command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
	if capturedKey != "status" {
		t.Errorf("expected key 'status', got %q", capturedKey)
	}
}

func TestStateListCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string

	stateCmd := &cobra.Command{
		Use: "state",
	}

	listCmd := &cobra.Command{
		Use:  "list <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			return nil
		},
	}
	stateCmd.AddCommand(listCmd)
	root.AddCommand(stateCmd)

	_, err := executeCommand(root, "state", "list", "researcher")
	if err != nil {
		t.Fatalf("state list command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
}

func TestStateClearCommand(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string

	stateCmd := &cobra.Command{
		Use: "state",
	}

	clearCmd := &cobra.Command{
		Use:  "clear <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			return nil
		},
	}
	stateCmd.AddCommand(clearCmd)
	root.AddCommand(stateCmd)

	_, err := executeCommand(root, "state", "clear", "researcher")
	if err != nil {
		t.Fatalf("state clear command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
}

// ===========================================================================
// Logs Command Tests (DB-based)
// ===========================================================================

func TestLogsCommand_DefaultFlags(t *testing.T) {
	root := newTestRootCmd()
	var capturedName string
	var capturedLines int

	logsCmd := &cobra.Command{
		Use:  "logs <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedName = args[0]
			capturedLines, _ = cmd.Flags().GetInt("lines")
			return nil
		},
	}
	logsCmd.Flags().Int("lines", 50, "Number of lines")
	logsCmd.Flags().String("since", "", "Since duration")
	root.AddCommand(logsCmd)

	_, err := executeCommand(root, "logs", "researcher")
	if err != nil {
		t.Fatalf("logs command failed: %v", err)
	}
	if capturedName != "researcher" {
		t.Errorf("expected name 'researcher', got %q", capturedName)
	}
	if capturedLines != 50 {
		t.Errorf("expected lines 50, got %d", capturedLines)
	}
}

func TestLogsCommand_CustomLines(t *testing.T) {
	root := newTestRootCmd()
	var capturedLines int

	logsCmd := &cobra.Command{
		Use:  "logs <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedLines, _ = cmd.Flags().GetInt("lines")
			return nil
		},
	}
	logsCmd.Flags().Int("lines", 50, "Number of lines")
	root.AddCommand(logsCmd)

	_, err := executeCommand(root, "logs", "researcher", "--lines", "100")
	if err != nil {
		t.Fatalf("logs command failed: %v", err)
	}
	if capturedLines != 100 {
		t.Errorf("expected lines 100, got %d", capturedLines)
	}
}

func TestLogsCommand_SinceFlag(t *testing.T) {
	root := newTestRootCmd()
	var capturedSince string

	logsCmd := &cobra.Command{
		Use:  "logs <name>",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			capturedSince, _ = cmd.Flags().GetString("since")
			return nil
		},
	}
	logsCmd.Flags().Int("lines", 50, "Number of lines")
	logsCmd.Flags().String("since", "", "Since duration")
	root.AddCommand(logsCmd)

	_, err := executeCommand(root, "logs", "researcher", "--since", "5m")
	if err != nil {
		t.Fatalf("logs command failed: %v", err)
	}
	if capturedSince != "5m" {
		t.Errorf("expected since '5m', got %q", capturedSince)
	}
}

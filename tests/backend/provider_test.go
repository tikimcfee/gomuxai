package backend_test

import (
	"context"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// TestClaudeGetStatusIdle tests that Claude provider correctly detects idle state.
func TestClaudeGetStatusIdle(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	ctx := context.Background()

	tests := []struct {
		name   string
		output string
	}{
		{
			name:   "simple prompt",
			output: "> ",
		},
		{
			name:   "prompt after output",
			output: "Some output here\n> ",
		},
		{
			name:   "prompt with newline",
			output: "Task completed\n>\n",
		},
		{
			name:   "prompt at line start",
			output: "Previous line\n> \n",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			status, err := provider.GetStatus(ctx, tt.output)
			if err != nil {
				t.Fatalf("GetStatus() error = %v", err)
			}
			if status != backend.StatusIdle {
				t.Errorf("GetStatus(%q) = %v, want StatusIdle", tt.output, status)
			}
		})
	}
}

// TestClaudeGetStatusProcessing tests that Claude provider correctly detects processing state.
func TestClaudeGetStatusProcessing(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	ctx := context.Background()

	tests := []struct {
		name   string
		output string
	}{
		{
			name:   "spinner braille dot 1",
			output: "Working ⠋",
		},
		{
			name:   "spinner braille dot 2",
			output: "Working ⠙",
		},
		{
			name:   "spinner braille dot 3",
			output: "Working ⠹",
		},
		{
			name:   "thinking indicator",
			output: "Thinking...",
		},
		{
			name:   "tool use",
			output: "Using tool: Read",
		},
		{
			name:   "writing file",
			output: "Writing to /path/to/file",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			status, err := provider.GetStatus(ctx, tt.output)
			if err != nil {
				t.Fatalf("GetStatus() error = %v", err)
			}
			if status != backend.StatusProcessing {
				t.Errorf("GetStatus(%q) = %v, want StatusProcessing", tt.output, status)
			}
		})
	}
}

// TestClaudeGetStatusError tests that Claude provider correctly detects error state.
func TestClaudeGetStatusError(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	ctx := context.Background()

	tests := []struct {
		name   string
		output string
	}{
		{
			name:   "permission denied",
			output: "Error: Permission denied",
		},
		{
			name:   "rate limit",
			output: "Rate limit exceeded. Please wait.",
		},
		{
			name:   "api error",
			output: "API Error: Connection failed",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			status, err := provider.GetStatus(ctx, tt.output)
			if err != nil {
				t.Fatalf("GetStatus() error = %v", err)
			}
			if status != backend.StatusError {
				t.Errorf("GetStatus(%q) = %v, want StatusError", tt.output, status)
			}
		})
	}
}

// TestClaudeGetStatusUnknown tests that Claude provider returns unknown for ambiguous output.
func TestClaudeGetStatusUnknown(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	ctx := context.Background()

	tests := []struct {
		name   string
		output string
	}{
		{
			name:   "empty string",
			output: "",
		},
		{
			name:   "random text",
			output: "Just some random text without clear indicators",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			status, err := provider.GetStatus(ctx, tt.output)
			if err != nil {
				t.Fatalf("GetStatus() error = %v", err)
			}
			if status != backend.StatusUnknown {
				t.Errorf("GetStatus(%q) = %v, want StatusUnknown", tt.output, status)
			}
		})
	}
}

// TestClaudeGetIdlePatternForLog tests that the idle pattern is valid.
func TestClaudeGetIdlePatternForLog(t *testing.T) {
	provider := namedagents.NewClaudeBackend()

	pattern := provider.GetIdlePatternForLog()
	if pattern == "" {
		t.Error("GetIdlePatternForLog() returned empty string")
	}
}

// TestClaudeExtractLastMessage tests message extraction from output.
func TestClaudeExtractLastMessage(t *testing.T) {
	provider := namedagents.NewClaudeBackend()

	tests := []struct {
		name     string
		output   string
		expected string
	}{
		{
			name:     "simple response",
			output:   "Here is my response\n> ",
			expected: "Here is my response",
		},
		{
			name:     "multiline response",
			output:   "Line 1\nLine 2\nLine 3\n> ",
			expected: "Line 1\nLine 2\nLine 3",
		},
		{
			name:     "empty response",
			output:   "> ",
			expected: "",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result := provider.ExtractLastMessage(tt.output)
			if result != tt.expected {
				t.Errorf("ExtractLastMessage(%q) = %q, want %q", tt.output, result, tt.expected)
			}
		})
	}
}

// TestClaudeExitCommand tests that the exit command is defined.
func TestClaudeExitCommand(t *testing.T) {
	provider := namedagents.NewClaudeBackend()

	cmd := provider.ExitCommand()
	if cmd == "" {
		t.Error("ExitCommand() returned empty string")
	}
}

// TestClaudeLifecycle tests Initialize and Cleanup methods.
func TestClaudeLifecycle(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	ctx := context.Background()

	// Initialize should not error for Claude (no setup required)
	if err := provider.Initialize(ctx); err != nil {
		t.Errorf("Initialize() error = %v", err)
	}

	// Cleanup should not error
	if err := provider.Cleanup(); err != nil {
		t.Errorf("Cleanup() error = %v", err)
	}
}

// TestAiderGetStatusIdle tests that Aider provider correctly detects idle state.
func TestAiderGetStatusIdle(t *testing.T) {
	provider := namedagents.NewAiderBackend()
	ctx := context.Background()

	tests := []struct {
		name   string
		output string
	}{
		{
			name:   "simple aider prompt",
			output: "aider> ",
		},
		{
			name:   "prompt after output",
			output: "Changes applied\naider> ",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			status, err := provider.GetStatus(ctx, tt.output)
			if err != nil {
				t.Fatalf("GetStatus() error = %v", err)
			}
			if status != backend.StatusIdle {
				t.Errorf("GetStatus(%q) = %v, want StatusIdle", tt.output, status)
			}
		})
	}
}

// TestAiderGetStatusProcessing tests that Aider provider correctly detects processing state.
func TestAiderGetStatusProcessing(t *testing.T) {
	provider := namedagents.NewAiderBackend()
	ctx := context.Background()

	tests := []struct {
		name   string
		output string
	}{
		{
			name:   "thinking",
			output: "Thinking about your request...",
		},
		{
			name:   "working",
			output: "Working on changes...",
		},
		{
			name:   "applying",
			output: "Applying changes to file.go",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			status, err := provider.GetStatus(ctx, tt.output)
			if err != nil {
				t.Fatalf("GetStatus() error = %v", err)
			}
			if status != backend.StatusProcessing {
				t.Errorf("GetStatus(%q) = %v, want StatusProcessing", tt.output, status)
			}
		})
	}
}

// TestAiderGetStatusError tests that Aider provider correctly detects error state.
func TestAiderGetStatusError(t *testing.T) {
	provider := namedagents.NewAiderBackend()
	ctx := context.Background()

	tests := []struct {
		name   string
		output string
	}{
		{
			name:   "api error",
			output: "Error: API request failed",
		},
		{
			name:   "git error",
			output: "Git error: Could not commit",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			status, err := provider.GetStatus(ctx, tt.output)
			if err != nil {
				t.Fatalf("GetStatus() error = %v", err)
			}
			if status != backend.StatusError {
				t.Errorf("GetStatus(%q) = %v, want StatusError", tt.output, status)
			}
		})
	}
}

// TestAiderGetIdlePatternForLog tests that the idle pattern is valid.
func TestAiderGetIdlePatternForLog(t *testing.T) {
	provider := namedagents.NewAiderBackend()

	pattern := provider.GetIdlePatternForLog()
	if pattern == "" {
		t.Error("GetIdlePatternForLog() returned empty string")
	}
}

// TestAiderExtractLastMessage tests message extraction from output.
func TestAiderExtractLastMessage(t *testing.T) {
	provider := namedagents.NewAiderBackend()

	tests := []struct {
		name     string
		output   string
		expected string
	}{
		{
			name:     "simple response",
			output:   "Here are the changes\naider> ",
			expected: "Here are the changes",
		},
		{
			name:     "empty response",
			output:   "aider> ",
			expected: "",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result := provider.ExtractLastMessage(tt.output)
			if result != tt.expected {
				t.Errorf("ExtractLastMessage(%q) = %q, want %q", tt.output, result, tt.expected)
			}
		})
	}
}

// TestAiderExitCommand tests that the exit command is defined.
func TestAiderExitCommand(t *testing.T) {
	provider := namedagents.NewAiderBackend()

	cmd := provider.ExitCommand()
	if cmd == "" {
		t.Error("ExitCommand() returned empty string")
	}
}

// TestAiderLifecycle tests Initialize and Cleanup methods.
func TestAiderLifecycle(t *testing.T) {
	provider := namedagents.NewAiderBackend()
	ctx := context.Background()

	if err := provider.Initialize(ctx); err != nil {
		t.Errorf("Initialize() error = %v", err)
	}

	if err := provider.Cleanup(); err != nil {
		t.Errorf("Cleanup() error = %v", err)
	}
}

// TestProviderInterface ensures both backends implement Provider interface.
func TestProviderInterface(t *testing.T) {
	// This test verifies at compile time that both backends implement Provider
	var _ backend.Provider = namedagents.NewClaudeBackend()
	var _ backend.Provider = namedagents.NewAiderBackend()
}

// TestStatusString tests the Status String() method.
func TestStatusString(t *testing.T) {
	tests := []struct {
		status   backend.Status
		expected string
	}{
		{backend.StatusUnknown, "unknown"},
		{backend.StatusIdle, "idle"},
		{backend.StatusProcessing, "processing"},
		{backend.StatusError, "error"},
	}

	for _, tt := range tests {
		t.Run(tt.expected, func(t *testing.T) {
			if got := tt.status.String(); got != tt.expected {
				t.Errorf("Status.String() = %q, want %q", got, tt.expected)
			}
		})
	}
}

// TestClaudeConfigSkipPermissions tests that skip-permissions config works.
func TestClaudeConfigSkipPermissions(t *testing.T) {
	// Default - no flag
	defaultBackend := namedagents.NewClaudeBackend()
	if cmd := defaultBackend.LaunchCommand(); cmd != "claude" {
		t.Errorf("Default LaunchCommand() = %q, want %q", cmd, "claude")
	}

	// With skip permissions
	skipBackend := namedagents.NewClaudeBackendWithConfig(namedagents.ClaudeConfig{
		SkipPermissions: true,
	})
	expected := "claude --dangerously-skip-permissions"
	if cmd := skipBackend.LaunchCommand(); cmd != expected {
		t.Errorf("SkipPermissions LaunchCommand() = %q, want %q", cmd, expected)
	}
}

// TestRegistryCreateProvider tests the factory pattern.
func TestRegistryCreateProvider(t *testing.T) {
	registry := backend.NewRegistry()

	// Create without skip permissions
	p1, err := registry.CreateProvider("claude", backend.ProviderConfig{})
	if err != nil {
		t.Fatalf("CreateProvider() error = %v", err)
	}
	if cmd := p1.LaunchCommand(); cmd != "claude" {
		t.Errorf("LaunchCommand() = %q, want %q", cmd, "claude")
	}

	// Create with skip permissions
	p2, err := registry.CreateProvider("claude", backend.ProviderConfig{
		SkipPermissions: true,
	})
	if err != nil {
		t.Fatalf("CreateProvider() error = %v", err)
	}
	expected := "claude --dangerously-skip-permissions"
	if cmd := p2.LaunchCommand(); cmd != expected {
		t.Errorf("LaunchCommand() = %q, want %q", cmd, expected)
	}

	// Verify they're different instances
	if p1 == p2 {
		t.Error("CreateProvider should return new instances")
	}
}

// TestRegistryCreateProviderUnknown tests error for unknown provider.
func TestRegistryCreateProviderUnknown(t *testing.T) {
	registry := backend.NewRegistry()

	_, err := registry.CreateProvider("unknown", backend.ProviderConfig{})
	if err == nil {
		t.Error("CreateProvider() should error for unknown provider")
	}
}

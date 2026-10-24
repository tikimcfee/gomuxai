package backend_test

import (
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// TestClaudeExitCommand tests that the exit command is defined.
func TestClaudeExitCommand(t *testing.T) {
	provider := namedagents.NewClaudeBackend()

	cmd := provider.ExitCommand()
	if cmd == "" {
		t.Error("ExitCommand() returned empty string")
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

	// Create agy provider via registry
	pAgy, err := registry.CreateProvider("agy", backend.ProviderConfig{
		SkipPermissions: true,
		AgentDir:        "/tmp/agy-test-dir",
	})
	if err != nil {
		t.Fatalf("CreateProvider(agy) error = %v", err)
	}
	if cmd := pAgy.LaunchCommand(); cmd != "env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy --dangerously-skip-permissions" {
		t.Errorf("agy LaunchCommand() = %q, want %q", cmd, "env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy --dangerously-skip-permissions")
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

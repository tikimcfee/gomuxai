package backend_test

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/util"
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

// claudeSettingsFlag is what every Claude launch command carries: the
// settings that keep startup and between-turn dialogs from coming up.
const claudeSettingsFlag = " --settings '" + namedagents.ClaudeSpawnSettings + "'"

// TestClaudeConfigSkipPermissions tests that skip-permissions config works.
func TestClaudeConfigSkipPermissions(t *testing.T) {
	// Default - no flag
	defaultBackend := namedagents.NewClaudeBackend()
	if cmd := defaultBackend.LaunchCommand(); cmd != "claude"+claudeSettingsFlag {
		t.Errorf("Default LaunchCommand() = %q, want %q", cmd, "claude"+claudeSettingsFlag)
	}

	// With skip permissions
	skipBackend := namedagents.NewClaudeBackendWithConfig(namedagents.ClaudeConfig{
		SkipPermissions: true,
	})
	expected := "claude --dangerously-skip-permissions" + claudeSettingsFlag
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
	if cmd := p1.LaunchCommand(); cmd != "claude"+claudeSettingsFlag {
		t.Errorf("LaunchCommand() = %q, want %q", cmd, "claude"+claudeSettingsFlag)
	}

	// Create with skip permissions
	p2, err := registry.CreateProvider("claude", backend.ProviderConfig{
		SkipPermissions: true,
	})
	if err != nil {
		t.Fatalf("CreateProvider() error = %v", err)
	}
	expected := "claude --dangerously-skip-permissions" + claudeSettingsFlag
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

func TestWithModel(t *testing.T) {
	claude := namedagents.NewClaudeBackend()
	if got := claude.WithModel(claude.LaunchCommandWithSession("s1"), "claude-fable-5-1"); got != "claude --settings "+util.ShellQuote(namedagents.ClaudeSpawnSettings)+" --session-id 's1' --model 'claude-fable-5-1'" {
		t.Errorf("claude: %q", got)
	}
	aider := namedagents.NewAiderBackend()
	if got := aider.WithModel(aider.LaunchCommand(), "sonnet"); got != "aider --model 'sonnet'" {
		t.Errorf("aider: %q", got)
	}
	// agy's launch line ends with the report of its exit: the model goes to
	// agy, ahead of it.
	agy := namedagents.NewAgyBackendWithConfig(namedagents.AgyConfig{AgentDir: t.TempDir()})
	launch := agy.WithModel(agy.LaunchCommandWithHooks("/bin/agent report --hook --as a", "sp1"), "gemini-3.8-flash-high")
	before, after, ok := strings.Cut(launch, "; ")
	if !ok || !strings.HasSuffix(before, " --model 'gemini-3.8-flash-high'") || strings.Contains(after, "--model") {
		t.Errorf("agy: %q", launch)
	}
}

// Claude takes text over 800 characters, or with 3+ line breaks, as a
// paste (2.1.319); TypedAsPaste says so from 640 characters, a margin.
func TestClaudeTypedAsPaste(t *testing.T) {
	b := namedagents.NewClaudeBackend()
	for text, want := range map[string]bool{
		"run the tests":                   false,
		"one\ntwo\nthree":                 false,
		"one\ntwo\nthree\nfour":           true,
		"a\r\nb\r\nc":                     false,
		"a\r\nb\r\nc\r\nd":                true,
		strings.Repeat("x", 640):          false,
		strings.Repeat("x", 641):          true,
		strings.Repeat("é", 640):          false, // characters, not bytes
		strings.Repeat("🙂", 321):          true,  // two UTF-16 units each, as Claude counts
		"/model claude-fable-5-1":         false,
		"[from lead@h] see\n/tmp/plan.md": false,
	} {
		if got := b.TypedAsPaste(text); got != want {
			t.Errorf("TypedAsPaste(%.20q, %d bytes) = %v, want %v", text, len(text), got, want)
		}
	}
}

// UserPromptSubmit carries the prompt, and its output's additionalContext
// goes to the model with it.
func TestClaudePromptHandover(t *testing.T) {
	b := namedagents.NewClaudeBackend()
	r, ok, err := b.ParseHookEvent("", []byte(`{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"hi [gomuxai message #4, x"}`))
	if err != nil || !ok {
		t.Fatal(ok, err)
	}
	if p, ok := b.SubmittedPrompt(r); !ok || p != "hi [gomuxai message #4, x" {
		t.Errorf("SubmittedPrompt = %q, %v", p, ok)
	}
	stop, _, _ := b.ParseHookEvent("", []byte(`{"hook_event_name":"Stop","session_id":"s"}`))
	if _, ok := b.SubmittedPrompt(stop); ok {
		t.Error("Stop counted as a submitted prompt")
	}
	var out struct {
		H map[string]string `json:"hookSpecificOutput"`
	}
	if err := json.Unmarshal(b.HandoverOutput("the whole text"), &out); err != nil || out.H["hookEventName"] != "UserPromptSubmit" || out.H["additionalContext"] != "the whole text" {
		t.Errorf("HandoverOutput = %+v, %v", out, err)
	}
}

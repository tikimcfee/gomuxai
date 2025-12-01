package namedagents

import (
	"context"
	"fmt"
	"regexp"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
)

// Aider status detection patterns
var (
	// Idle: aider prompt at end of output
	aiderIdlePattern = regexp.MustCompile(`(?m)^aider>\s*$`)

	// Processing: thinking, working, applying indicators
	aiderProcessingPattern = regexp.MustCompile(`(?i)(Thinking|Working|Applying|Generating|Streaming)`)

	// Error: API errors, git errors, file errors
	aiderErrorPattern = regexp.MustCompile(`(?i)(Error:|Git error|Could not|Failed|API.*failed)`)
)

// AiderBackend implements Provider for Aider CLI.
type AiderBackend struct{}

// NewAiderBackend creates a new Aider backend.
func NewAiderBackend() *AiderBackend {
	return &AiderBackend{}
}

// Identity

func (b *AiderBackend) Name() string {
	return "aider"
}

func (b *AiderBackend) SessionPrefix() string {
	return "agent-"
}

// Lifecycle

func (b *AiderBackend) Initialize(ctx context.Context) error {
	// Aider requires no special initialization
	return nil
}

func (b *AiderBackend) Cleanup() error {
	// Aider requires no special cleanup
	return nil
}

// Status detection - PROVIDER DOES THE ANALYSIS

func (b *AiderBackend) GetStatus(ctx context.Context, output string) (types.Status, error) {
	// Check for errors first (highest priority)
	if aiderErrorPattern.MatchString(output) {
		return types.StatusError, nil
	}

	// Check for processing indicators
	if aiderProcessingPattern.MatchString(output) {
		return types.StatusProcessing, nil
	}

	// Check for idle prompt at the end of output
	lines := strings.Split(strings.TrimRight(output, "\n"), "\n")
	if len(lines) > 0 {
		lastLine := lines[len(lines)-1]
		if aiderIdlePattern.MatchString(lastLine) || strings.TrimSpace(lastLine) == "aider>" {
			return types.StatusIdle, nil
		}
	}

	// Also check if idle pattern matches anywhere (for captured output)
	if aiderIdlePattern.MatchString(output) {
		return types.StatusIdle, nil
	}

	return types.StatusUnknown, nil
}

// GetIdlePatternForLog returns a regex pattern for file watcher optimization.
func (b *AiderBackend) GetIdlePatternForLog() string {
	return `(?m)^aider>\s*$`
}

// ExtractLastMessage extracts the last assistant message from output.
func (b *AiderBackend) ExtractLastMessage(output string) string {
	// Find everything before the last prompt
	output = strings.TrimRight(output, "\n\r\t ")

	// If output ends with aider prompt, remove it
	if strings.HasSuffix(output, "aider> ") {
		output = strings.TrimSuffix(output, "aider> ")
	}
	if strings.HasSuffix(output, "aider>") {
		output = strings.TrimSuffix(output, "aider>")
	}

	output = strings.TrimRight(output, "\n\r\t ")
	return output
}

// Commands

func (b *AiderBackend) LaunchCommand() string {
	return "aider"
}

func (b *AiderBackend) InitMessage(taskFile string) string {
	if taskFile != "" {
		return fmt.Sprintf("/read %s", taskFile)
	}
	return ""
}

func (b *AiderBackend) ExitCommand() string {
	return "/exit"
}

// Legacy Backend interface methods (for backwards compatibility)

func (b *AiderBackend) IdlePattern() string {
	// Aider shows "aider>" or similar prompt when idle
	return `(?m)^aider>\s*$`
}

func (b *AiderBackend) ProcessingPattern() string {
	// Aider shows various indicators when processing
	return `(Thinking|Working|Applying)`
}

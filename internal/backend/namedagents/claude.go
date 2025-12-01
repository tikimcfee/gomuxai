package namedagents

import (
	"context"
	"fmt"
	"regexp"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
)

// Claude status detection patterns
var (
	// Idle: prompt at end of output
	claudeIdlePattern = regexp.MustCompile(`(?m)^>\s*$`)

	// Processing: spinner characters, thinking indicators, tool use
	claudeProcessingPattern = regexp.MustCompile(`(⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Thinking|Using tool|Writing to|Reading)`)

	// Error: permission denied, rate limits, API errors
	claudeErrorPattern = regexp.MustCompile(`(?i)(Permission denied|Rate limit|Error:|API Error|failed|could not)`)
)

// ClaudeConfig holds configuration options for the Claude backend.
type ClaudeConfig struct {
	// SkipPermissions passes --dangerously-skip-permissions to claude CLI
	SkipPermissions bool
}

// ClaudeBackend implements Provider for Claude Code CLI.
type ClaudeBackend struct {
	config ClaudeConfig
}

// NewClaudeBackend creates a new Claude backend with default config.
func NewClaudeBackend() *ClaudeBackend {
	return &ClaudeBackend{}
}

// NewClaudeBackendWithConfig creates a Claude backend with the given config.
func NewClaudeBackendWithConfig(cfg ClaudeConfig) *ClaudeBackend {
	return &ClaudeBackend{config: cfg}
}

// Identity

func (b *ClaudeBackend) Name() string {
	return "claude"
}

func (b *ClaudeBackend) SessionPrefix() string {
	return "agent-"
}

// Lifecycle

func (b *ClaudeBackend) Initialize(ctx context.Context) error {
	// Claude Code requires no special initialization
	return nil
}

func (b *ClaudeBackend) Cleanup() error {
	// Claude Code requires no special cleanup
	return nil
}

// Status detection - PROVIDER DOES THE ANALYSIS

func (b *ClaudeBackend) GetStatus(ctx context.Context, output string) (types.Status, error) {
	// Check for errors first (highest priority)
	if claudeErrorPattern.MatchString(output) {
		return types.StatusError, nil
	}

	// Check for processing indicators
	if claudeProcessingPattern.MatchString(output) {
		return types.StatusProcessing, nil
	}

	// Check for idle prompt at the end of output
	// We need to check if the last meaningful content shows the prompt
	lines := strings.Split(strings.TrimRight(output, "\n"), "\n")
	if len(lines) > 0 {
		lastLine := lines[len(lines)-1]
		if claudeIdlePattern.MatchString(lastLine) || strings.TrimSpace(lastLine) == ">" {
			return types.StatusIdle, nil
		}
	}

	// Also check if idle pattern matches anywhere (for captured output)
	if claudeIdlePattern.MatchString(output) {
		return types.StatusIdle, nil
	}

	return types.StatusUnknown, nil
}

// GetIdlePatternForLog returns a regex pattern for file watcher optimization.
func (b *ClaudeBackend) GetIdlePatternForLog() string {
	return `(?m)^>\s*$`
}

// ExtractLastMessage extracts the last assistant message from output.
func (b *ClaudeBackend) ExtractLastMessage(output string) string {
	// Find everything before the last prompt
	output = strings.TrimRight(output, "\n\r\t ")

	// If output ends with prompt, remove it
	if strings.HasSuffix(output, "> ") {
		output = strings.TrimSuffix(output, "> ")
	}
	if strings.HasSuffix(output, ">") {
		output = strings.TrimSuffix(output, ">")
	}

	output = strings.TrimRight(output, "\n\r\t ")
	return output
}

// Commands

func (b *ClaudeBackend) LaunchCommand() string {
	if b.config.SkipPermissions {
		return "claude --dangerously-skip-permissions"
	}
	return "claude"
}

func (b *ClaudeBackend) InitMessage(taskFile string) string {
	if taskFile != "" {
		return fmt.Sprintf("Please read and execute the task in %s", taskFile)
	}
	return ""
}

func (b *ClaudeBackend) ExitCommand() string {
	return "/exit"
}

// Legacy Backend interface methods (for backwards compatibility)

func (b *ClaudeBackend) IdlePattern() string {
	return `(?m)^>\s*$`
}

func (b *ClaudeBackend) ProcessingPattern() string {
	return `(⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Thinking)`
}

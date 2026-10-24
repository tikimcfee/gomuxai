package namedagents

import "fmt"

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

func (b *ClaudeBackend) Name() string {
	return "claude"
}

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

// Backend interface: status scraping, used only for sessions without hooks.

func (b *ClaudeBackend) IdlePattern() string {
	return `(?m)^>\s*$`
}

func (b *ClaudeBackend) ProcessingPattern() string {
	return `(⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Thinking)`
}

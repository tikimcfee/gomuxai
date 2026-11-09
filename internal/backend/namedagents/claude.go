package namedagents

import (
	"fmt"

	"github.com/tikimcfee/gomuxai/internal/util"
)

// ClaudeConfig holds configuration options for the Claude backend.
type ClaudeConfig struct {
	// SkipPermissions passes --dangerously-skip-permissions to claude CLI
	SkipPermissions bool
	// Resume is the session id claude continues (--resume), or "".
	Resume string
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

// ClaudeSpawnSettings are the settings every Claude agent is launched with
// (--settings, layered over the person's own for that session only). They
// turn off /auto-mode-setup, whose offer ("Teach auto mode about your
// environment?") comes up when a turn ends in auto mode, fires no hook, and
// takes the keys of the next message typed in: its Enter starts the setup
// wizard, which reads the person's shell history and other repos.
const ClaudeSpawnSettings = `{"skillOverrides":{"auto-mode-setup":"off"}}`

func (b *ClaudeBackend) LaunchCommand() string {
	cmd := "claude"
	if b.config.SkipPermissions {
		cmd += " --dangerously-skip-permissions"
	}
	cmd += " --settings " + util.ShellQuote(ClaudeSpawnSettings)
	if b.config.Resume != "" {
		cmd += " --resume " + util.ShellQuote(b.config.Resume)
	}
	return cmd
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

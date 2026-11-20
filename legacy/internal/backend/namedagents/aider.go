package namedagents

import (
	"fmt"

	"github.com/tikimcfee/gomuxai/internal/util"
)

// AiderBackend implements Provider for Aider CLI.
type AiderBackend struct{}

// NewAiderBackend creates a new Aider backend.
func NewAiderBackend() *AiderBackend {
	return &AiderBackend{}
}

func (b *AiderBackend) Name() string {
	return "aider"
}

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

// Backend interface: status scraping, aider's only status source.

func (b *AiderBackend) IdlePattern() string {
	// Aider shows "aider>" or similar prompt when idle
	return `(?m)^aider>\s*$`
}

func (b *AiderBackend) ProcessingPattern() string {
	// Aider shows various indicators when processing
	return `(Thinking|Working|Applying)`
}

// WithModel runs aider on model (--model).
func (b *AiderBackend) WithModel(launchCmd, model string) string {
	return launchCmd + " --model " + util.ShellQuote(model)
}

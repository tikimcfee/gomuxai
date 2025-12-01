// Package backend provides backend definitions for different AI CLI tools.
package backend

import (
	"context"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
)

// Re-export Status type and constants for convenience
type Status = types.Status

const (
	StatusUnknown    = types.StatusUnknown
	StatusIdle       = types.StatusIdle
	StatusProcessing = types.StatusProcessing
	StatusError      = types.StatusError
)

// Provider defines the interface for AI CLI tool backends.
// Unlike the simpler Backend interface, Provider does status analysis internally
// rather than returning patterns for the caller to match.
type Provider interface {
	// Identity
	Name() string
	SessionPrefix() string

	// Lifecycle
	Initialize(ctx context.Context) error
	Cleanup() error

	// Status detection - PROVIDER DOES THE ANALYSIS
	GetStatus(ctx context.Context, output string) (Status, error)

	// Quick check for file watcher optimization
	GetIdlePatternForLog() string

	// Output parsing
	ExtractLastMessage(output string) string

	// Commands
	LaunchCommand() string
	InitMessage(taskFile string) string
	ExitCommand() string
}

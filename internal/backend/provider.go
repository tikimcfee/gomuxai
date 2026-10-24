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
	StatusWaiting    = types.StatusWaiting
	StatusExited     = types.StatusExited
)

type HookReport = types.HookReport
type PermissionResponse = types.PermissionResponse

const (
	PermissionApprove       = types.PermissionApprove
	PermissionApproveAlways = types.PermissionApproveAlways
	PermissionDeny          = types.PermissionDeny
)

// HookProvider is implemented by backends whose CLI reports its own status
// through lifecycle hooks. Hooks run reportCmd with the event payload on stdin.
type HookProvider interface {
	// LaunchCommandWithHooks returns the launch command with hooks wired to
	// reportCmd and the backend session pinned to sessionID.
	LaunchCommandWithHooks(reportCmd, sessionID string) string

	// ParseHookEvent maps a hook payload to a status report.
	// ok is false for events that carry no status change.
	ParseHookEvent(payload []byte) (report HookReport, ok bool, err error)
}

// NativeStatusProvider is implemented by backends whose CLI can report the
// status of a running session directly (no terminal scraping).
type NativeStatusProvider interface {
	// NativeStatus returns the session's status. found is false when the CLI
	// doesn't know the session (not started yet, exited, or unsupported).
	NativeStatus(ctx context.Context, sessionID string) (status Status, found bool, err error)
}

// PermissionResponder is implemented by backends that can answer a pending
// permission prompt with a keypress.
type PermissionResponder interface {
	PermissionKey(resp PermissionResponse) (string, error)
}

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

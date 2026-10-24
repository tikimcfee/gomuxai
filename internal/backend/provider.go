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
	// event is the hook event name when passed on the command line (for
	// backends whose stdin payload omits it); backends that embed the event
	// name in payload may prefer the payload field.
	// ok is false for events that carry no status change.
	ParseHookEvent(event string, payload []byte) (report HookReport, ok bool, err error)
}

// PromptFlagProvider is implemented by backends whose CLI can accept the
// initial task prompt via a launch flag (e.g. agy -i <prompt>).
type PromptFlagProvider interface {
	AppendPromptFlag(launchCmd, prompt string) string
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
	// PermissionKey returns the key that answers the prompt for tool. It
	// errors for tools whose dialogs aren't yes/no permission prompts (e.g. a
	// question to the user), where a "yes" key would pick an answer instead.
	// An empty tool means "treat it as a plain permission prompt".
	PermissionKey(resp PermissionResponse, tool string) (string, error)

	// PermissionPromptVisible reports whether a permission prompt is showing
	// on screen (the visible pane, ANSI-stripped). Checked right before the
	// keypress, since reported status can lag a prompt that was just answered.
	PermissionPromptVisible(screen string) bool
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

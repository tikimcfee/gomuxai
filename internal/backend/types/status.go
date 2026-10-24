// Package types provides shared types for the backend package.
package types

// Status represents the current state of an AI backend.
type Status int

const (
	StatusUnknown Status = iota
	StatusIdle
	StatusProcessing
	StatusError
	StatusWaiting // blocked on a human, e.g. a permission prompt
	StatusExited
)

// String returns a human-readable representation of the status.
func (s Status) String() string {
	switch s {
	case StatusIdle:
		return "idle"
	case StatusProcessing:
		return "processing"
	case StatusError:
		return "error"
	case StatusWaiting:
		return "waiting"
	case StatusExited:
		return "exited"
	default:
		return "unknown"
	}
}

// HookReport is a status update derived from a backend lifecycle hook payload.
type HookReport struct {
	Status    Status
	Event     string // backend's event name, e.g. "PreToolUse"
	Detail    string // short human-readable context, e.g. "Bash: go test ./..."
	Tool      string // tool the event concerns, if any
	Result    string // final assistant message, when the event carries one
	SessionID string // backend's session id, when the event carries one
}

// PermissionResponse is an answer to a pending permission prompt.
type PermissionResponse int

const (
	PermissionApprove PermissionResponse = iota
	PermissionApproveAlways
	PermissionDeny
)

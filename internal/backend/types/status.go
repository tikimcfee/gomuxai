// Package types provides shared types for the backend package.
package types

// Status represents the current state of an AI backend.
type Status int

const (
	StatusUnknown Status = iota
	StatusIdle
	StatusProcessing
	StatusError
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
	default:
		return "unknown"
	}
}

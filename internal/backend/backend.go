// Package backend provides backend definitions for different AI CLI tools.
package backend

// Backend defines the interface for AI CLI tool backends.
type Backend interface {
	// Name returns the backend identifier.
	Name() string

	// SessionPrefix returns the prefix for tmux session names.
	SessionPrefix() string

	// LaunchCommand returns the command to start the CLI tool.
	LaunchCommand() string

	// InitMessage returns the initial message to send after launch.
	// If taskFile is provided, it should be incorporated into the message.
	InitMessage(taskFile string) string

	// IdlePattern returns a regex pattern that indicates the backend is idle.
	IdlePattern() string

	// ProcessingPattern returns a regex pattern that indicates the backend is processing.
	ProcessingPattern() string
}

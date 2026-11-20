// Package backend provides backend definitions for different AI CLI tools.
package backend

// Backend describes a CLI tool by its launch command and the regex patterns
// that tell its status from terminal output. Custom JSON backends
// (--backend-config) implement only this; status scraping uses it for every
// backend, the built-in ones included.
type Backend interface {
	// Name returns the backend identifier.
	Name() string

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

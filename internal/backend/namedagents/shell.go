package namedagents

// ShellBackend implements Provider for a plain shell or terminal session with
// no AI CLI wrapper: tmux starts the shell directly, so LaunchCommand is empty.
type ShellBackend struct{}

// NewShellBackend creates a new shell backend.
func NewShellBackend() *ShellBackend {
	return &ShellBackend{}
}

func (b *ShellBackend) Name() string {
	return "shell"
}

func (b *ShellBackend) LaunchCommand() string {
	return ""
}

func (b *ShellBackend) InitMessage(taskFile string) string {
	return ""
}

func (b *ShellBackend) ExitCommand() string {
	return "exit"
}

func (b *ShellBackend) IdlePattern() string {
	return ""
}

func (b *ShellBackend) ProcessingPattern() string {
	return ""
}

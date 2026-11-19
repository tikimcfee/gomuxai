package termd

// Session describes one live terminal in a g3d-termd namespace.
type Session struct {
	Name      string
	Created   string
	Attached  bool
	SessionID string
}

// PaneInfo describes a terminal's id, root process PID, and working directory.
type PaneInfo struct {
	ID  string
	PID int
	Cwd string
}

// Ops defines the terminal multiplexer operations gomuxai uses.
type Ops interface {
	// CreateSession creates a new terminal running the user's shell in the
	// caller's working directory with the given request environment.
	CreateSession(name string, env map[string]string) error

	// CreateSessionIn creates a new terminal running the user's shell in dir
	// ("" for the caller's working directory).
	CreateSessionIn(name, dir string, env map[string]string) error

	// KillSession terminates a terminal and its process tree.
	KillSession(name string) error

	// RespawnPane kills whatever runs in session and starts a fresh shell under
	// the same terminal id, preserving its working directory and environment.
	RespawnPane(session string) error

	// SessionExists reports whether a live terminal named name exists.
	SessionExists(name string) (bool, error)

	// ListSessions returns all live terminals in the namespace.
	ListSessions() ([]Session, error)

	// SendKeys sends literal text to a terminal.
	SendKeys(session string, text string) error

	// PasteText pastes text into a terminal (bracketed when the running
	// program requested bracketed paste).
	PasteText(session string, text string) error

	// SendSpecialKey sends a special key or chord (such as Enter, Escape, c-c)
	// to a terminal.
	SendSpecialKey(session string, key string) error

	// Capture returns session's visible screen and up to lines rows of
	// scrollback as plain text.
	Capture(session string, lines int) (string, error)

	// CaptureStyled returns session's visible screen and up to lines rows of
	// scrollback with SGR styling escapes preserved.
	CaptureStyled(session string, lines int) (string, error)

	// PanePIDs maps the root process PID of each live terminal to its terminal id.
	PanePIDs() (map[int]string, error)
}

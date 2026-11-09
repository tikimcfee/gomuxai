// Package tmux provides tmux operations for the agent tool.
package tmux

import (
	"errors"
	"fmt"
	"os/exec"
	"sort"
	"strconv"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/util"
)

// Session represents a tmux session.
type Session struct {
	Name      string
	Created   string
	Attached  bool
	Windows   int
	SessionID string
}

// PaneInfo describes a tmux session's target pane.
type PaneInfo struct {
	ID  string // e.g. "%0"
	PID int    // process running directly in the pane (typically its shell)
	Cwd string // pane_current_path
}

// TmuxOps defines the interface for tmux operations.
type TmuxOps interface {
	// CreateSession creates a new tmux session with the given name and environment.
	CreateSession(name string, env map[string]string) error

	// CreateSessionIn is CreateSession with the session's first shell
	// started in dir ("" for the caller's working directory).
	CreateSessionIn(name, dir string, env map[string]string) error

	// KillSession terminates a tmux session.
	KillSession(name string) error

	// SessionExists checks if a session with the given name exists.
	SessionExists(name string) (bool, error)

	// ListSessions returns all tmux sessions.
	ListSessions() ([]Session, error)

	// SendKeys sends text to a tmux session.
	SendKeys(session string, text string) error

	// PasteText types text into a tmux session as a paste: bracketed, if
	// the application asked for bracketed paste, so it arrives as one
	// paste however the terminal splits it.
	PasteText(session string, text string) error

	// SendSpecialKey sends a special key (like c-c, enter) to a session.
	SendSpecialKey(session string, key string) error

	// Capture captures the visible pane and up to lines lines of scrollback
	// above it, as plain text.
	Capture(session string, lines int) (string, error)

	// CaptureStyled captures like Capture, keeping the styling escapes
	// (colors, bold, dim) that tell, say, a CLI's placeholder text from typed
	// text.
	CaptureStyled(session string, lines int) (string, error)

	// PipePane starts piping pane output to a file.
	PipePane(session string, logPath string) error

	// StopPipePane stops piping pane output.
	StopPipePane(session string) error

	// Clear clears the pane's screen and scrollback, without typing into
	// the program running in it.
	Clear(session string) error

	// PanePIDs maps the pid of the process each pane runs (its shell, for
	// an agent's session) to the session it is in, for every session.
	PanePIDs() (map[int]string, error)
}

// Client implements TmuxOps by running the tmux command. Its errors carry
// tmux's own message (see Error).
type Client struct{}

// NewClient creates a new tmux client.
func NewClient() (*Client, error) {
	if _, err := exec.LookPath("tmux"); err != nil {
		return nil, errors.New("initializing tmux: tmux isn't installed (not found on PATH): install it and try again")
	}
	return &Client{}, nil
}

// exact makes a session name a target that matches only that session:
// tmux otherwise falls back to a prefix or pattern match, so "agent-a" could
// pick "agent-ab".
func exact(session string) string { return "=" + session }

// CreateSession creates a new tmux session.
func (c *Client) CreateSession(name string, env map[string]string) error {
	return c.CreateSessionIn(name, "", env)
}

// CreateSessionIn creates a new tmux session whose first shell starts in
// dir, or in this process's working directory when dir is "".
func (c *Client) CreateSessionIn(name, dir string, env map[string]string) error {
	if exists, _ := c.SessionExists(name); exists {
		return fmt.Errorf("session %q already exists", name)
	}

	// Pass env with -e so the session's first shell starts with it;
	// set-environment after creation only reaches panes created later.
	args := []string{"new-session", "-d", "-s", name}
	if dir != "" {
		args = append(args, "-c", dir)
	}
	keys := make([]string, 0, len(env))
	for key := range env {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		args = append(args, "-e", key+"="+env[key])
	}

	if _, err := run(args...); err != nil {
		return fmt.Errorf("creating session %q: %w", name, err)
	}

	return nil
}

// KillSession terminates a tmux session.
func (c *Client) KillSession(name string) error {
	if _, err := run("kill-session", "-t", exact(name)); err != nil {
		if exists, _ := c.SessionExists(name); !exists {
			return fmt.Errorf("session %q not found", name)
		}
		return fmt.Errorf("killing session %q: %w", name, err)
	}

	return nil
}

// SessionExists checks if a session exists. With no tmux server running,
// or one with no sessions, none does.
func (c *Client) SessionExists(name string) (bool, error) {
	_, err := run("has-session", "-t", exact(name))
	switch {
	case err == nil:
		return true, nil
	case notFound(err):
		return false, nil
	}
	return false, fmt.Errorf("checking session %q: %w", name, err)
}

// ListSessions returns all tmux sessions. With no tmux server running there
// are none.
func (c *Client) ListSessions() ([]Session, error) {
	// Fields are space-separated with the name last, the only one that can
	// hold a space: tmux 3.8 prints a tab in -F output as "_".
	out, err := run("list-sessions", "-F", "#{session_id} #{session_attached} #{session_windows} #{session_created} #{session_name}")
	if err != nil {
		if noServer(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("listing sessions: %w", err)
	}

	var result []Session
	for _, line := range strings.Split(out, "\n") {
		fields := strings.SplitN(line, " ", 5)
		if len(fields) != 5 {
			continue
		}
		attached, _ := strconv.Atoi(fields[1])
		windows, _ := strconv.Atoi(fields[2])
		result = append(result, Session{
			Name:      fields[4],
			SessionID: fields[0],
			Attached:  attached > 0,
			Windows:   windows,
			Created:   fields[3],
		})
	}

	return result, nil
}

// paneTarget returns the id of the session's first pane, which SendKeys
// types into. Every session-level operation targets it, so keys, captures,
// and logs all hit the agent's pane even when another window or pane is
// active.
func (c *Client) paneTarget(sessionName string) (string, error) {
	pane, _, err := c.firstPaneMode(sessionName)
	return pane, err
}

// firstPaneMode returns the session's first pane (see paneTarget) and
// whether it is in a mode (copy mode, choose-tree, ...).
func (c *Client) firstPaneMode(sessionName string) (string, bool, error) {
	out, err := run("list-panes", "-s", "-t", exact(sessionName), "-F", "#{pane_id} #{pane_in_mode}")
	if err != nil {
		if exists, _ := c.SessionExists(sessionName); !exists {
			return "", false, fmt.Errorf("session %q not found", sessionName)
		}
		return "", false, fmt.Errorf("listing panes: %w", err)
	}
	first, _, _ := strings.Cut(strings.TrimSpace(out), "\n")
	pane, inMode, _ := strings.Cut(first, " ")
	if pane == "" {
		return "", false, fmt.Errorf("session %q has no panes", sessionName)
	}
	return pane, inMode == "1", nil
}

// inputTarget is paneTarget for typing: it takes the pane out of any mode
// first. In a mode, tmux hands send-keys' keys to the mode, not to the
// program in the pane: copy mode (which a scroll of the mouse wheel enters)
// runs each key's copy-mode binding, so typed text moves the cursor, opens
// prompts (and with a client attached, send-keys waits for a person to
// answer them), and fails with "not in a mode" once a binding has left the
// mode; nothing reaches the program, and an Enter or a "1" meant for it
// never arrives.
func (c *Client) inputTarget(sessionName string) (string, error) {
	pane, inMode, err := c.firstPaneMode(sessionName)
	if err == nil && inMode {
		leaveMode(pane) // best effort: send-keys says what's wrong if it can't type
	}
	return pane, err
}

// leaveMode takes pane out of every mode it is in. copy-mode -q (tmux 3.2
// and later) leaves any mode; cancel, older tmux's way, leaves copy mode.
// While a read-only client is attached tmux refuses both, but then keys go
// in by paste, which no mode intercepts.
func leaveMode(pane string) {
	if _, err := run("copy-mode", "-q", "-t", pane); err != nil && !IsReadOnly(err) {
		_, _ = run("send-keys", "-X", "-t", pane, "cancel")
	}
}

// SendKeys types text into a tmux session literally. Without -l, tmux reads
// text that happens to be a key name ("C-c", "Escape", "0x3") as that key,
// and without "--" text starting with '-' as send-keys flags ("-R" resets
// the terminal). Text can come from other hosts over the bus. A pane in a
// mode is taken out of it first (see inputTarget).
//
// While a read-only client is attached, tmux refuses send-keys, so the text
// is pasted instead, which types the same bytes.
func (c *Client) SendKeys(session string, text string) error {
	target, err := c.inputTarget(session)
	if err != nil {
		return err
	}

	_, err = run("send-keys", "-l", "-t", target, "--", text)
	if IsReadOnly(err) {
		err = paste(target, text)
	}
	if err != nil {
		return fmt.Errorf("sending keys: %w", err)
	}

	return nil
}

// PasteText types text into a tmux session through a paste buffer, as a
// bracketed paste (ESC [200~ text ESC [201~) if the application in the pane
// asked for bracketed paste mode, else byte for byte as SendKeys types it.
//
// Text typed with send-keys reaches the application in as many reads as
// the terminal splits it into: a pty's input queue holds 1024 bytes on
// macOS (4096 on Linux), so a longer message arrives in pieces. With no
// brackets to say where it starts and ends, a CLI that takes a burst of
// input for a paste (Claude Code does) can take the pieces for separate
// pastes and keep only the last one: the head of the message is lost. The
// brackets make the whole message one paste, as a person's paste is. A
// message can't end the paste early: ESC [201~ in it is dropped.
//
// tmux doesn't refuse a paste for read-only clients, as it does send-keys.
// A paste goes to the application even in a mode, but the pane is taken
// out of it first, as for SendKeys (see inputTarget): the person watching
// sees the message arrive, and the Enter that follows reaches it too.
func (c *Client) PasteText(session string, text string) error {
	target, err := c.inputTarget(session)
	if err != nil {
		return err
	}
	if err := pasteBracketed(target, strings.ReplaceAll(text, pasteEnd, "")); err != nil {
		return fmt.Errorf("pasting text: %w", err)
	}
	return nil
}

// pasteEnd ends a bracketed paste.
const pasteEnd = "\x1b[201~"

// SendSpecialKey sends a special key to a session. key is never read as a
// flag: one that isn't a key name is typed as text. As for SendKeys, a pane
// in a mode is taken out of it first.
//
// While a read-only client is attached, tmux refuses send-keys, so a key
// with fixed bytes (see keyBytes) is pasted instead; any other fails with a
// ReadOnlyError naming the client.
func (c *Client) SendSpecialKey(session string, key string) error {
	target, err := c.inputTarget(session)
	if err != nil {
		return err
	}
	_, err = run("send-keys", "-t", target, "--", key)
	if IsReadOnly(err) {
		err = c.pasteKey(target, key, err)
	}
	if err != nil {
		return fmt.Errorf("sending key %q: %w", key, err)
	}
	return nil
}

// pasteKey types key into target with a paste, for when tmux refused
// send-keys (refused) because of a read-only client.
func (c *Client) pasteKey(target, key string, refused error) error {
	cursorKeys := false
	if isArrow(key) {
		out, err := run("display-message", "-p", "-t", target, "#{keypad_cursor_flag}")
		if err != nil {
			return err
		}
		cursorKeys = strings.TrimSpace(out) == "1"
	}
	data, ok := keyBytes(key, cursorKeys)
	if !ok {
		return readOnlyError(key, refused)
	}
	return paste(target, data)
}

// Capture captures the pane as plain text.
func (c *Client) Capture(session string, lines int) (string, error) {
	return c.capture(session, lines)
}

// CaptureStyled captures the pane with its styling escapes (-e).
func (c *Client) CaptureStyled(session string, lines int) (string, error) {
	return c.capture(session, lines, "-e")
}

// capture runs capture-pane on the session's pane with extra flags.
func (c *Client) capture(session string, lines int, flags ...string) (string, error) {
	target, err := c.paneTarget(session)
	if err != nil {
		return "", err
	}

	// -p prints to stdout; -S starts that many lines back into the
	// scrollback (0 is the top of the visible pane).
	args := append([]string{"capture-pane", "-t", target, "-p", "-S", strconv.Itoa(-lines)}, flags...)
	output, err := run(args...)
	if err != nil {
		return "", fmt.Errorf("capturing pane: %w", err)
	}
	return output, nil
}

// PipePane starts piping pane output to a file.
func (c *Client) PipePane(session string, logPath string) error {
	target, err := c.paneTarget(session)
	if err != nil {
		return err
	}
	// tmux runs the command through a shell: quote the path so it can't
	// inject one.
	if _, err := run("pipe-pane", "-t", target, "cat >> "+util.ShellQuote(logPath)); err != nil {
		return fmt.Errorf("piping pane: %w", err)
	}
	return nil
}

// StopPipePane stops piping pane output.
func (c *Client) StopPipePane(session string) error {
	target, err := c.paneTarget(session)
	if err != nil {
		return err
	}
	// pipe-pane without a command stops the current pipe.
	if _, err := run("pipe-pane", "-t", target); err != nil {
		return fmt.Errorf("stopping pipe: %w", err)
	}
	return nil
}

// Clear clears the pane's screen and scrollback without involving the
// program running in it. tmux resets the pane's terminal (send-keys -R),
// which blanks the screen and homes the cursor; with scroll-on-clear on (the
// default) that pushes the old screen into the scrollback, so clear-history
// runs after it, in the same tmux command, and drops all of it. Nothing is
// typed: a shell's prompt isn't redrawn until it next prints, and a
// full-screen program's screen stays blank until it next draws.
//
// Typing "clear" instead raced the shell (friction #5): when the shell ran it
// after clear-history, scroll-on-clear put the old screen back.
func (c *Client) Clear(session string) error {
	target, err := c.paneTarget(session)
	if err != nil {
		return err
	}
	if _, err := run("send-keys", "-R", "-t", target, ";", "clear-history", "-t", target); err != nil {
		return fmt.Errorf("clearing pane: %w", err)
	}
	return nil
}

// SessionNames lists the sessions' names, cheaper than ListSessions. With no
// tmux server running there are none.
func (c *Client) SessionNames() ([]string, error) {
	out, err := run("list-sessions", "-F", "#{session_name}")
	if err != nil {
		if noServer(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("listing sessions: %w", err)
	}
	return strings.Fields(out), nil
}

// PanePIDs maps each session's first pane process to its session, in one tmux
// call. No server running is no panes.
func (c *Client) PanePIDs() (map[int]string, error) {
	out, err := run("list-panes", "-a", "-F", "#{pane_pid} #{session_name}")
	if err != nil {
		if strings.Contains(err.Error(), "no server running") || strings.Contains(err.Error(), "error connecting") {
			return map[int]string{}, nil
		}
		return nil, fmt.Errorf("listing panes: %w", err)
	}
	panes := map[int]string{}
	seenSession := map[string]bool{}
	for line := range strings.SplitSeq(strings.TrimSpace(out), "\n") {
		pidText, session, ok := strings.Cut(line, " ")
		pid, err := strconv.Atoi(pidText)
		if !ok || err != nil || pid <= 0 || seenSession[session] {
			continue
		}
		seenSession[session] = true
		panes[pid] = session
	}
	return panes, nil
}

// SessionEnv returns a variable from a session's environment, or "" if it
// isn't set there.
func (c *Client) SessionEnv(session, name string) string {
	out, err := run("show-environment", "-t", exact(session), name)
	if err != nil {
		return ""
	}
	_, value, _ := strings.Cut(strings.TrimSpace(out), "=")
	return value
}

// SessionOfPane returns the name of the tmux session containing pane (e.g.
// "$TMUX_PANE" like "%0"), or "" if the pane or server is gone.
func (c *Client) SessionOfPane(pane string) (string, error) {
	pane = strings.TrimSpace(pane)
	if pane == "" {
		return "", nil
	}
	out, err := run("display-message", "-p", "-t", pane, "#{session_name}")
	if err != nil {
		if notFound(err) {
			return "", nil
		}
		return "", fmt.Errorf("finding session for pane %q: %w", pane, err)
	}
	return strings.TrimSpace(out), nil
}

// FirstPane returns the id, shell pid, and current directory of sessionName's
// first pane (the same pane SendKeys, Capture, and PipePane target).
func (c *Client) FirstPane(sessionName string) (PaneInfo, error) {
	// Keep pane_current_path last so paths containing spaces survive SplitN.
	out, err := run("list-panes", "-s", "-t", exact(sessionName), "-F", "#{pane_id} #{pane_pid} #{pane_current_path}")
	if err != nil {
		if exists, _ := c.SessionExists(sessionName); !exists {
			return PaneInfo{}, fmt.Errorf("session %q not found", sessionName)
		}
		return PaneInfo{}, fmt.Errorf("listing panes: %w", err)
	}
	firstLine, _, _ := strings.Cut(strings.TrimSpace(out), "\n")
	fields := strings.SplitN(firstLine, " ", 3)
	if len(fields) < 2 || fields[0] == "" {
		return PaneInfo{}, fmt.Errorf("session %q has no panes", sessionName)
	}
	pid, err := strconv.Atoi(fields[1])
	if err != nil || pid <= 0 {
		return PaneInfo{}, fmt.Errorf("parsing pane pid for session %q: %w", sessionName, err)
	}
	cwd := ""
	if len(fields) == 3 {
		cwd = fields[2]
	}
	return PaneInfo{ID: fields[0], PID: pid, Cwd: cwd}, nil
}

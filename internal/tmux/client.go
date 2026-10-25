// Package tmux provides tmux operations for the agent tool.
package tmux

import (
	"fmt"
	"sort"
	"strconv"
	"strings"

	"github.com/GianlucaP106/gotmux/gotmux"
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

// TmuxOps defines the interface for tmux operations.
type TmuxOps interface {
	// CreateSession creates a new tmux session with the given name and environment.
	CreateSession(name string, env map[string]string) error

	// KillSession terminates a tmux session.
	KillSession(name string) error

	// SessionExists checks if a session with the given name exists.
	SessionExists(name string) (bool, error)

	// ListSessions returns all tmux sessions.
	ListSessions() ([]Session, error)

	// SendKeys sends text to a tmux session.
	SendKeys(session string, text string) error

	// SendSpecialKey sends a special key (like c-c, enter) to a session.
	SendSpecialKey(session string, key string) error

	// Capture captures the current pane content.
	// If stripANSI is true, ANSI escape codes are removed.
	Capture(session string, lines int, stripANSI bool) (string, error)

	// PipePane starts piping pane output to a file.
	PipePane(session string, logPath string) error

	// StopPipePane stops piping pane output.
	StopPipePane(session string) error

	// Clear clears the terminal screen and scrollback.
	Clear(session string) error
}

// Client implements TmuxOps using gotmux.
type Client struct {
	tmux *gotmux.Tmux
}

// NewClient creates a new tmux client.
func NewClient() (*Client, error) {
	t, err := gotmux.DefaultTmux()
	if err != nil {
		return nil, fmt.Errorf("initializing tmux: %w", err)
	}
	return &Client{tmux: t}, nil
}

// CreateSession creates a new tmux session.
func (c *Client) CreateSession(name string, env map[string]string) error {
	if c.tmux.HasSession(name) {
		return fmt.Errorf("session %q already exists", name)
	}

	// Pass env with -e so the session's first shell starts with it;
	// set-environment after creation only reaches panes created later.
	args := []string{"new-session", "-d", "-s", name}
	keys := make([]string, 0, len(env))
	for key := range env {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		args = append(args, "-e", key+"="+env[key])
	}

	if _, err := c.tmux.Command(args...); err != nil {
		return fmt.Errorf("creating session %q: %w", name, err)
	}

	return nil
}

// KillSession terminates a tmux session.
func (c *Client) KillSession(name string) error {
	session, err := c.getSession(name)
	if err != nil {
		return err
	}

	if err := session.Kill(); err != nil {
		return fmt.Errorf("killing session %q: %w", name, err)
	}

	return nil
}

// SessionExists checks if a session exists.
func (c *Client) SessionExists(name string) (bool, error) {
	return c.tmux.HasSession(name), nil
}

// ListSessions returns all tmux sessions.
func (c *Client) ListSessions() ([]Session, error) {
	sessions, err := c.tmux.ListSessions()
	if err != nil {
		return nil, fmt.Errorf("listing sessions: %w", err)
	}

	result := make([]Session, 0, len(sessions))
	for _, s := range sessions {
		result = append(result, Session{
			Name:      s.Name,
			SessionID: s.Id,
			Attached:  s.Attached > 0,
			Windows:   s.Windows,
			Created:   s.Created,
		})
	}

	return result, nil
}

// getSession looks up a session by name, failing if it doesn't exist.
func (c *Client) getSession(name string) (*gotmux.Session, error) {
	session, err := c.tmux.GetSessionByName(name)
	if err != nil {
		return nil, fmt.Errorf("getting session %q: %w", name, err)
	}
	if session == nil {
		return nil, fmt.Errorf("session %q not found", name)
	}
	return session, nil
}

// getFirstPane gets the first pane of a session.
func (c *Client) getFirstPane(sessionName string) (*gotmux.Pane, error) {
	session, err := c.getSession(sessionName)
	if err != nil {
		return nil, err
	}

	panes, err := session.ListPanes()
	if err != nil {
		return nil, fmt.Errorf("listing panes: %w", err)
	}

	if len(panes) == 0 {
		return nil, fmt.Errorf("session %q has no panes", sessionName)
	}

	return panes[0], nil
}

// paneTarget returns the id of the pane SendKeys types into. Every
// session-level operation targets it, so keys, captures, and logs all hit the
// agent's pane even when another window or pane is active.
func (c *Client) paneTarget(sessionName string) (string, error) {
	pane, err := c.getFirstPane(sessionName)
	if err != nil {
		return "", err
	}
	return pane.Id, nil
}

// SendKeys types text into a tmux session literally. Without -l, tmux reads
// text that happens to be a key name ("C-c", "Escape", "0x3") as that key,
// and without "--" text starting with '-' as send-keys flags ("-R" resets
// the terminal). Text can come from other hosts over the bus.
func (c *Client) SendKeys(session string, text string) error {
	target, err := c.paneTarget(session)
	if err != nil {
		return err
	}

	if _, err := c.tmux.Command("send-keys", "-l", "-t", target, "--", text); err != nil {
		return fmt.Errorf("sending keys: %w", err)
	}

	return nil
}

// SendSpecialKey sends a special key to a session. key is never read as a
// flag: one that isn't a key name is typed as text.
func (c *Client) SendSpecialKey(session string, key string) error {
	target, err := c.paneTarget(session)
	if err != nil {
		return err
	}
	if _, err := c.tmux.Command("send-keys", "-t", target, "--", key); err != nil {
		return fmt.Errorf("sending key %q: %w", key, err)
	}
	return nil
}

// Capture captures pane content.
func (c *Client) Capture(session string, lines int, stripANSI bool) (string, error) {
	target, err := c.paneTarget(session)
	if err != nil {
		return "", err
	}

	// -p prints to stdout; -S starts that many lines back into the
	// scrollback (0 is the top of the visible pane).
	output, err := c.tmux.Command("capture-pane", "-t", target, "-p", "-S", strconv.Itoa(-lines))
	if err != nil {
		return "", fmt.Errorf("capturing pane: %w", err)
	}

	if stripANSI {
		output = util.StripANSI(output)
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
	if _, err := c.tmux.Command("pipe-pane", "-t", target, "cat >> "+util.ShellQuote(logPath)); err != nil {
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
	if _, err := c.tmux.Command("pipe-pane", "-t", target); err != nil {
		return fmt.Errorf("stopping pipe: %w", err)
	}
	return nil
}

// Clear clears the terminal screen and scrollback.
func (c *Client) Clear(session string) error {
	if err := c.SendKeys(session, "clear"); err != nil {
		return err
	}
	if err := c.SendSpecialKey(session, "Enter"); err != nil {
		return err
	}

	// clear only wipes the screen; tmux keeps its own scrollback.
	target, err := c.paneTarget(session)
	if err != nil {
		return err
	}
	if _, err := c.tmux.Command("clear-history", "-t", target); err != nil {
		return fmt.Errorf("clearing history: %w", err)
	}

	return nil
}

// SessionNames lists the sessions' names, cheaper than ListSessions. With no
// tmux server running there are none.
func (c *Client) SessionNames() ([]string, error) {
	out, err := c.tmux.Command("list-sessions", "-F", "#{session_name}")
	if err != nil {
		if msg := err.Error(); strings.Contains(msg, "no server running") || strings.Contains(msg, "error connecting") {
			return nil, nil
		}
		return nil, fmt.Errorf("listing sessions: %w", err)
	}
	return strings.Fields(out), nil
}

// SessionEnv returns a variable from a session's environment, or "" if it
// isn't set there.
func (c *Client) SessionEnv(session, name string) string {
	out, err := c.tmux.Command("show-environment", "-t", session, name)
	if err != nil {
		return ""
	}
	_, value, _ := strings.Cut(strings.TrimSpace(out), "=")
	return value
}

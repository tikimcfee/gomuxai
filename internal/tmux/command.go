package tmux

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync/atomic"
	"time"
)

// Error is a tmux command that failed, with tmux's own message.
type Error struct {
	Command string // the tmux command, e.g. "send-keys"
	Stderr  string // what tmux printed, trimmed
	Err     error  // the exec error
}

func (e *Error) Error() string {
	if e.Stderr != "" {
		return "tmux " + e.Command + ": " + e.Stderr
	}
	return "tmux " + e.Command + ": " + e.Err.Error()
}

func (e *Error) Unwrap() error { return e.Err }

// commandError builds the error for a failed tmux command from its arguments
// and what it printed on stderr.
func commandError(args []string, stderr string, err error) *Error {
	command := ""
	if len(args) > 0 {
		command = args[0]
	}
	return &Error{Command: command, Stderr: strings.TrimSpace(stderr), Err: err}
}

// run runs a tmux command and returns its stdout. Its error is an *Error
// carrying tmux's stderr, which says what went wrong.
func run(args ...string) (string, error) {
	cmd := exec.Command("tmux", args...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		return "", commandError(args, stderr.String(), err)
	}
	return string(out), nil
}

// tmuxSays reports whether err is a tmux command that failed with a message
// containing any of msgs.
func tmuxSays(err error, msgs ...string) bool {
	var te *Error
	if !errors.As(err, &te) {
		return false
	}
	for _, msg := range msgs {
		if strings.Contains(te.Stderr, msg) {
			return true
		}
	}
	return false
}

// IsReadOnly reports whether tmux refused a command because a read-only
// client (tmux attach -r) is attached. tmux refuses send-keys to every
// session while one is attached anywhere on the server: with no -c, it
// checks the most recently active client, whichever session that watches.
func IsReadOnly(err error) bool {
	return tmuxSays(err, "client is read-only")
}

// noServer reports whether err is tmux finding no server to talk to.
func noServer(err error) bool {
	return tmuxSays(err, "no server running", "error connecting")
}

// readOnlyClient is a client attached with tmux attach -r.
type readOnlyClient struct {
	Name    string // its tty, which tmux detach-client -t takes
	Session string
}

// readOnlyClients lists the read-only clients attached to the server.
func readOnlyClients() ([]readOnlyClient, error) {
	out, err := run("list-clients", "-F", "#{client_readonly}\t#{client_name}\t#{session_name}")
	if err != nil {
		return nil, err
	}
	var clients []readOnlyClient
	for _, line := range strings.Split(out, "\n") {
		fields := strings.Split(line, "\t")
		if len(fields) == 3 && fields[0] == "1" {
			clients = append(clients, readOnlyClient{Name: fields[1], Session: fields[2]})
		}
	}
	return clients, nil
}

// ReadOnlyError is tmux refusing a key because a read-only client is
// attached, when the key can't be pasted past it instead.
type ReadOnlyError struct {
	Key     string
	Clients []readOnlyClient
	Err     error
}

func (e *ReadOnlyError) Error() string {
	if len(e.Clients) == 0 {
		return fmt.Sprintf("tmux refused key %q because a client is read-only, but none is attached now: try again", e.Key)
	}
	names := make([]string, len(e.Clients))
	for i, c := range e.Clients {
		names[i] = c.Name + " on " + c.Session
	}
	return fmt.Sprintf("tmux refuses send-keys to every session while a read-only client is attached (%s), and %q isn't a key agent can paste past it: detach that client (tmux detach-client -t %s), or watch agents with agent capture instead of tmux attach -r",
		strings.Join(names, ", "), e.Key, e.Clients[0].Name)
}

func (e *ReadOnlyError) Unwrap() error { return e.Err }

// readOnlyError explains a send-keys that tmux refused because of a
// read-only client, naming the clients to detach.
func readOnlyError(key string, err error) error {
	clients, lerr := readOnlyClients()
	if lerr != nil {
		return err
	}
	return &ReadOnlyError{Key: key, Clients: clients, Err: err}
}

// keyBytes returns what a terminal sends for the tmux key name key, for
// typing it with paste-buffer when send-keys is refused: Enter, Escape, Tab,
// BSpace, Space, the arrows, C-a to C-z, and single characters. cursorKeys
// is the pane's cursor key mode (DECCKM), which changes what the arrows
// send. Names tmux reads case-insensitively are matched that way.
func keyBytes(key string, cursorKeys bool) (string, bool) {
	arrow := "\x1b["
	if cursorKeys {
		arrow = "\x1bO"
	}
	switch strings.ToLower(key) {
	case "enter":
		return "\r", true
	case "escape":
		return "\x1b", true
	case "tab":
		return "\t", true
	case "bspace":
		return "\x7f", true
	case "space":
		return " ", true
	case "up":
		return arrow + "A", true
	case "down":
		return arrow + "B", true
	case "right":
		return arrow + "C", true
	case "left":
		return arrow + "D", true
	}
	if len(key) == 3 && (key[0] == 'C' || key[0] == 'c') && key[1] == '-' {
		if l := key[2] | 0x20; l >= 'a' && l <= 'z' {
			return string(rune(l & 0x1f)), true
		}
	}
	if r := []rune(key); len(r) == 1 && r[0] >= ' ' && r[0] != 0x7f {
		return key, true
	}
	return "", false
}

// isArrow reports whether key is an arrow key, whose bytes depend on the
// pane's cursor key mode.
func isArrow(key string) bool {
	switch strings.ToLower(key) {
	case "up", "down", "left", "right":
		return true
	}
	return false
}

// bufferSeq makes paste buffer names unique within the process.
var bufferSeq atomic.Uint64

// paste types data into a pane through a paste buffer, byte for byte, as
// send-keys -l does: -r keeps newlines as they are, -S keeps control
// characters (tmux 3.5 and later turn them into ^C otherwise; earlier ones
// don't, and don't know -S), and without -p there's no bracketed paste. tmux
// doesn't refuse it for read-only clients.
func paste(target, data string) error {
	if data == "" {
		return nil
	}
	buf := fmt.Sprintf("gomuxai-%d-%d-%d", os.Getpid(), time.Now().UnixNano(), bufferSeq.Add(1))
	if _, err := run("set-buffer", "-b", buf, "--", data); err != nil {
		return err
	}
	_, err := run("paste-buffer", "-d", "-r", "-S", "-b", buf, "-t", target)
	if tmuxSays(err, "unknown flag -S") {
		_, err = run("paste-buffer", "-d", "-r", "-b", buf, "-t", target)
	}
	if err != nil {
		_, _ = run("delete-buffer", "-b", buf)
		return err
	}
	return nil
}

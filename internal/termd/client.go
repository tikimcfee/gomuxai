// Package termd implements gomuxai's terminal multiplexer backend over
// g3d-termd's Unix socket protocol (version 1.5).
package termd

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"strconv"
	"strings"
	"sync"
	"unicode/utf8"

	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// DefaultNamespace is the daemon namespace gomuxai uses when none is
// configured: isolated from interactive ("default") and wall ("wall")
// terminals.
const DefaultNamespace = "agents"

// TestServerEnv names the scratch socket directory a test binary runs its
// private g3d-termd daemons in (internal/termdtest).
const TestServerEnv = "GOMUXAI_TEST_TERMD_DIR"

const (
	defaultCols = 120
	defaultRows = 40
	pasteEnd    = "\x1b[201~"
)

// Options configures a Client.
type Options struct {
	Namespace   string
	DaemonBin   string
	AttachBin   string
	InitialCols int
	InitialRows int
}

// KeyEvent is one W3C key event for termd's "key" op.
type KeyEvent struct {
	Code        string   `json:"code,omitempty"`
	Key         string   `json:"key,omitempty"`
	Mods        []string `json:"mods,omitempty"`
	Text        string   `json:"text,omitempty"`
	Action      string   `json:"action,omitempty"`
	OptionAsAlt string   `json:"option_as_alt,omitempty"`
}

// StatusDoc is the daemon's status document (op "status").
type StatusDoc struct {
	At        int64            `json:"at"`
	Daemon    DaemonStatus     `json:"daemon"`
	Terminals []TerminalStatus `json:"terminals"`
}

// DaemonStatus describes the running g3d-termd holder and brain.
type DaemonStatus struct {
	Version  string `json:"version"`
	Engine   string `json:"engine"`
	Proto    [2]int `json:"proto"`
	NS       string `json:"ns"`
	Socket   string `json:"socket"`
	Log      string `json:"log"`
	Exiting  bool   `json:"exiting"`
	Instance string `json:"instance"`
}

// TerminalStatus describes one live terminal in StatusDoc.
type TerminalStatus struct {
	ID          string             `json:"id"`
	Handle      uint32             `json:"handle"`
	PID         int                `json:"pid"`
	Argv        []string           `json:"argv"`
	Login       bool               `json:"login"`
	Cwd         string             `json:"cwd"`
	Pwd         string             `json:"pwd"`
	Title       string             `json:"title"`
	Cols        int                `json:"cols"`
	Rows        int                `json:"rows"`
	Alt         bool               `json:"alt"`
	CreatedAt   int64              `json:"created_at"`
	Processes   int                `json:"processes"`
	Subscribers []SubscriberStatus `json:"subscribers"`
}

// SubscriberStatus describes one active subscriber on a terminal.
type SubscriberStatus struct {
	Conn   uint64 `json:"conn"`
	Client string `json:"client"`
	PID    int    `json:"pid"`
	Role   string `json:"role"`
	Mode   string `json:"mode"`
	Flow   string `json:"flow"`
}

type pipeWatcher struct {
	conn   *wireConn
	handle uint32
	done   chan struct{}
}

// Client implements tmux.TmuxOps against a g3d-termd namespace.
type Client struct {
	ns        string
	daemonBin string
	attachBin string
	cols      int
	rows      int

	pipesMu sync.Mutex
	pipes   map[string]*pipeWatcher
}

// NewClient constructs a termd Client after validating its namespace and
// locating the daemon binary.
func NewClient(opts Options) (*Client, error) {
	if err := CheckTestServer(); err != nil {
		return nil, err
	}
	ns := opts.Namespace
	if ns == "" {
		ns = DefaultNamespace
	}
	if err := ValidateNamespace(ns); err != nil {
		return nil, err
	}
	daemonBin, err := DaemonBinary(opts.DaemonBin)
	if err != nil {
		return nil, err
	}
	cols := opts.InitialCols
	if cols <= 0 {
		cols = defaultCols
	}
	rows := opts.InitialRows
	if rows <= 0 {
		rows = defaultRows
	}
	return &Client{
		ns:        ns,
		daemonBin: daemonBin,
		attachBin: opts.AttachBin,
		cols:      cols,
		rows:      rows,
		pipes:     make(map[string]*pipeWatcher),
	}, nil
}

// Namespace returns the daemon namespace this client manages.
func (c *Client) Namespace() string { return c.ns }

// Status returns the namespace's status document. When no daemon is running
// in the namespace, it returns an empty StatusDoc (0 live terminals).
func (c *Client) Status() (*StatusDoc, error) {
	w, err := dialNamespace(c.ns)
	if errors.Is(err, errNotRunning) {
		return &StatusDoc{Terminals: []TerminalStatus{}}, nil
	}
	if err != nil {
		return nil, err
	}
	defer w.close()

	if w.hasFeature("status") {
		var reply struct {
			Status StatusDoc `json:"status"`
		}
		if err := w.request("status", nil, &reply); err != nil {
			return nil, fmt.Errorf("termd status: %w", err)
		}
		if reply.Status.Terminals == nil {
			reply.Status.Terminals = []TerminalStatus{}
		}
		return &reply.Status, nil
	}

	var list struct {
		Terminals []TerminalStatus `json:"terminals"`
	}
	if err := w.request("list", nil, &list); err != nil {
		return nil, fmt.Errorf("termd list: %w", err)
	}
	if list.Terminals == nil {
		list.Terminals = []TerminalStatus{}
	}
	return &StatusDoc{
		Daemon:    DaemonStatus{NS: c.ns, Proto: w.welcome.Proto, Instance: w.welcome.Instance},
		Terminals: list.Terminals,
	}, nil
}

// StopDaemon stops the namespace's daemon and kills every terminal in it.
func (c *Client) StopDaemon() error {
	c.stopAllPipes()
	w, err := dialNamespace(c.ns)
	if errors.Is(err, errNotRunning) {
		return nil
	}
	if err != nil {
		return err
	}
	defer w.close()
	return w.request("stop", nil, nil)
}

// AttachCommand returns the g3d executable and argv to replace this process
// with when attaching a local terminal to session.
func (c *Client) AttachCommand(session string) (string, []string, error) {
	if err := ValidateTerminalID(session); err != nil {
		return "", nil, err
	}
	bin, err := AttachBinary(c.attachBin, c.daemonBin)
	if err != nil {
		return "", nil, err
	}
	return bin, []string{"g3d", "--ns", c.ns, "attach", "--take", session}, nil
}

// CreateSession creates a new terminal running the user's shell in the
// current working directory.
func (c *Client) CreateSession(name string, env map[string]string) error {
	return c.CreateSessionIn(name, "", env)
}

// CreateSessionIn creates a new terminal running the user's shell in dir
// (or the current working directory when dir is "").
func (c *Client) CreateSessionIn(name, dir string, env map[string]string) error {
	if err := ValidateTerminalID(name); err != nil {
		return err
	}
	cwd := dir
	if cwd == "" {
		cwd, _ = os.Getwd()
	}
	w, err := ensureDaemon(c.ns, c.daemonBin)
	if err != nil {
		return err
	}
	defer w.close()

	fields := map[string]any{
		"id":        name,
		"cols":      c.cols,
		"rows":      c.rows,
		"if_exists": "error",
		"env_base":  CallerEnv(os.Environ()),
	}
	if cwd != "" {
		fields["cwd"] = cwd
	}
	if reqEnv := sanitizeRequestEnv(env); len(reqEnv) > 0 {
		fields["env"] = reqEnv
	}
	if err := w.request("create", fields, nil); err != nil {
		if isWireCode(err, "exists") {
			return fmt.Errorf("session %q already exists", name)
		}
		return fmt.Errorf("creating session %q: %w", name, err)
	}
	return nil
}

// KillSession terminates a terminal and its entire process tree.
func (c *Client) KillSession(name string) error {
	c.stopPipe(name)
	w, err := dialNamespace(c.ns)
	if errors.Is(err, errNotRunning) {
		return fmt.Errorf("session %q not found", name)
	}
	if err != nil {
		return err
	}
	defer w.close()

	if err := w.request("kill", map[string]any{"id": name}, nil); err != nil {
		if isWireCode(err, "no_terminal") {
			return fmt.Errorf("session %q not found", name)
		}
		return fmt.Errorf("killing session %q: %w", name, err)
	}
	return nil
}

// RespawnPane kills what runs in session and starts a fresh shell under the
// same terminal id, preserving its starting directory and request env.
func (c *Client) RespawnPane(session string) error {
	st, err := c.Status()
	if err != nil {
		return err
	}
	var found *TerminalStatus
	for i := range st.Terminals {
		if st.Terminals[i].ID == session {
			found = &st.Terminals[i]
			break
		}
	}
	if found == nil {
		return fmt.Errorf("session %q not found", session)
	}
	reqEnv := c.requestEnvOf(session)
	if err := c.KillSession(session); err != nil {
		return err
	}
	return c.CreateSessionIn(session, ResolveTerminalCwd(found.Pwd, found.Cwd), reqEnv)
}

// ResolveTerminalCwd extracts a local directory path from a terminal's OSC 7
// pwd (which shells like macOS zsh report as "file://hostname/abs/path") and
// falls back to cwd when pwd is absent or not an existing local directory.
func ResolveTerminalCwd(osc7Pwd, fallbackCwd string) string {
	cand := strings.TrimSpace(osc7Pwd)
	if strings.HasPrefix(cand, "file://") {
		if u, err := url.Parse(cand); err == nil && u.Path != "" {
			cand = u.Path
		} else {
			cand = ""
		}
	}
	if strings.HasPrefix(cand, "/") {
		if info, err := os.Stat(cand); err == nil && info.IsDir() {
			return cand
		}
	}
	return fallbackCwd
}

func (c *Client) requestEnvOf(session string) map[string]string {
	w, err := dialNamespace(c.ns)
	if err != nil || !w.hasFeature("env") {
		if w != nil {
			_ = w.close()
		}
		return nil
	}
	defer w.close()
	var reply struct {
		Env []struct {
			Name   string `json:"name"`
			Value  string `json:"value"`
			Source string `json:"source"`
		} `json:"env"`
	}
	if err := w.request("env", map[string]any{"id": session}, &reply); err != nil {
		return nil
	}
	out := map[string]string{}
	for _, e := range reply.Env {
		if e.Source == "request" {
			out[e.Name] = e.Value
		}
	}
	return out
}

// SessionExists reports whether a live terminal named name exists.
func (c *Client) SessionExists(name string) (bool, error) {
	st, err := c.Status()
	if err != nil {
		return false, err
	}
	for _, t := range st.Terminals {
		if t.ID == name {
			return true, nil
		}
	}
	return false, nil
}

// ListSessions lists every live terminal in the namespace as a tmux.Session.
func (c *Client) ListSessions() ([]tmux.Session, error) {
	st, err := c.Status()
	if err != nil {
		return nil, err
	}
	out := make([]tmux.Session, 0, len(st.Terminals))
	for _, t := range st.Terminals {
		attached := false
		for _, sub := range t.Subscribers {
			if sub.Role == "own" || sub.Role == "attach" {
				attached = true
				break
			}
		}
		out = append(out, tmux.Session{
			Name:      t.ID,
			Created:   strconv.FormatInt(t.CreatedAt/1000, 10),
			Attached:  attached,
			Windows:   1,
			SessionID: fmt.Sprintf("$%d", t.Handle),
		})
	}
	return out, nil
}

// SessionNames lists the ids of live terminals in the namespace.
func (c *Client) SessionNames() ([]string, error) {
	st, err := c.Status()
	if err != nil {
		return nil, err
	}
	names := make([]string, len(st.Terminals))
	for i, t := range st.Terminals {
		names[i] = t.ID
	}
	return names, nil
}

// SessionEnv returns a variable from session's starting environment, or "".
func (c *Client) SessionEnv(session, name string) string {
	w, err := dialNamespace(c.ns)
	if err != nil {
		return ""
	}
	defer w.close()
	if !w.hasFeature("env") {
		return ""
	}
	var reply struct {
		Env []struct {
			Name  string `json:"name"`
			Value string `json:"value"`
		} `json:"env"`
	}
	if err := w.request("env", map[string]any{"id": session}, &reply); err != nil {
		return ""
	}
	for _, e := range reply.Env {
		if e.Name == name {
			return e.Value
		}
	}
	return ""
}

func (c *Client) withWriter(session, feature string, fn func(w *wireConn, handle uint32) error) error {
	w, err := dialNamespace(c.ns)
	if errors.Is(err, errNotRunning) {
		return fmt.Errorf("session %q not found", session)
	}
	if err != nil {
		return err
	}
	defer w.close()

	if feature != "" && !w.hasFeature(feature) {
		return fmt.Errorf("termd daemon %s lacks feature %q; upgrade g3d-termd", w.welcome.Daemon, feature)
	}

	var sub struct {
		Handle uint32 `json:"handle"`
	}
	err = w.request("subscribe", map[string]any{
		"id":   session,
		"mode": "none",
		"role": "attach",
	}, &sub)
	if err != nil {
		if isWireCode(err, "no_terminal") {
			return fmt.Errorf("session %q not found", session)
		}
		return err
	}

	opErr := fn(w, sub.Handle)
	// Synchronize through the terminal actor (g3d's done_writing pattern):
	// unsubscribe's reply arrives after any error event caused by INPUT.
	_ = w.request("unsubscribe", map[string]any{"handle": sub.Handle}, nil)
	if opErr != nil {
		return opErr
	}
	for {
		select {
		case f, ok := <-w.frames:
			if !ok {
				return nil
			}
			if f.Type == frameEvent && f.Stream == sub.Handle {
				var ev struct {
					Ev      string `json:"ev"`
					Code    string `json:"code"`
					Message string `json:"message"`
				}
				if json.Unmarshal(f.Payload, &ev) == nil && ev.Ev == "error" {
					return &WireError{Code: ev.Code, Message: ev.Message}
				}
			}
		default:
			return nil
		}
	}
}

// SendKeys writes literal bytes to session's PTY without key-name parsing or
// bracketed-paste framing, matching tmux send-keys -l / g3d send.
func (c *Client) SendKeys(session, text string) error {
	err := c.withWriter(session, "", func(w *wireConn, handle uint32) error {
		b := []byte(text)
		const chunk = 1 << 20
		for len(b) > 0 {
			n := min(len(b), chunk)
			if err := w.input(handle, b[:n]); err != nil {
				return err
			}
			b = b[n:]
		}
		return nil
	})
	if err != nil {
		return fmt.Errorf("sending keys: %w", err)
	}
	return nil
}

// PasteText pastes text into session via termd's paste op (streamed in chunks
// when paste_stream is available), wrapping in bracketed paste when the
// program requested mode 2004 and stripping embedded bracket-end sequences.
func (c *Client) PasteText(session, text string) error {
	clean := strings.ReplaceAll(text, pasteEnd, "")
	if clean == "" {
		exists, err := c.SessionExists(session)
		if err != nil {
			return err
		}
		if !exists {
			return fmt.Errorf("session %q not found", session)
		}
		return nil
	}
	err := c.withWriter(session, "paste", func(w *wireConn, handle uint32) error {
		chunkMax := min(1<<20, max((w.maxFrame()-1024)/6, 4))
		if !w.hasFeature("paste_stream") || len(clean) <= chunkMax {
			return w.request("paste", map[string]any{
				"handle":       handle,
				"text":         clean,
				"allow_unsafe": true,
			}, nil)
		}
		rest := clean
		first := true
		for len(rest) > 0 {
			cut := min(len(rest), chunkMax)
			for cut < len(rest) && !utf8.RuneStart(rest[cut]) {
				cut--
			}
			if cut == 0 {
				_, size := utf8.DecodeRuneInString(rest)
				cut = size
			}
			piece := rest[:cut]
			rest = rest[cut:]
			more := len(rest) > 0
			fields := map[string]any{
				"handle": handle,
				"text":   piece,
			}
			if first {
				fields["allow_unsafe"] = true
				first = false
			}
			if more {
				fields["more"] = true
			}
			if err := w.request("paste", fields, nil); err != nil {
				return err
			}
		}
		return nil
	})
	if err != nil {
		return fmt.Errorf("pasting text: %w", err)
	}
	return nil
}

// SendSpecialKey encodes and sends a special key or chord via termd's "key"
// op (honoring Kitty keyboard flags, modifyOtherKeys, and DECCKM), falling
// back to literal INPUT bytes for unrecognized multi-character strings as
// tmux send-keys does.
func (c *Client) SendSpecialKey(session, key string) error {
	ev, ok := ParseSpecialKey(key)
	if !ok {
		return c.SendKeys(session, key)
	}
	err := c.withWriter(session, "key", func(w *wireConn, handle uint32) error {
		fields := map[string]any{
			"handle": handle,
		}
		if ev.Code != "" {
			fields["code"] = ev.Code
		}
		if ev.Key != "" {
			fields["key"] = ev.Key
		}
		if len(ev.Mods) > 0 {
			fields["mods"] = ev.Mods
		}
		if ev.Text != "" {
			fields["text"] = ev.Text
		}
		if w.hasFeature("option_as_alt") {
			fields["option_as_alt"] = "auto"
		}
		return w.request("key", fields, nil)
	})
	if err != nil {
		return fmt.Errorf("sending key %q: %w", key, err)
	}
	return nil
}

// Capture returns session's visible screen and up to lines rows of scrollback
// as plain text.
func (c *Client) Capture(session string, lines int) (string, error) {
	return c.capture(session, lines, "plain")
}

// CaptureStyled returns session's visible screen and up to lines rows of
// scrollback with SGR styling escapes preserved and CRLF normalized to LF.
func (c *Client) CaptureStyled(session string, lines int) (string, error) {
	return c.capture(session, lines, "vt")
}

func (c *Client) capture(session string, lines int, format string) (string, error) {
	w, err := dialNamespace(c.ns)
	if errors.Is(err, errNotRunning) {
		return "", fmt.Errorf("session %q not found", session)
	}
	if err != nil {
		return "", err
	}
	defer w.close()

	var reply struct {
		Text string `json:"text"`
	}
	err = w.request("capture", map[string]any{
		"id":           session,
		"format":       format,
		"history_rows": max(lines, 0),
	}, &reply)
	if err != nil {
		if isWireCode(err, "no_terminal") {
			return "", fmt.Errorf("session %q not found", session)
		}
		return "", fmt.Errorf("capturing pane: %w", err)
	}
	text := strings.ReplaceAll(reply.Text, "\r\n", "\n")
	if text != "" && !strings.HasSuffix(text, "\n") {
		text += "\n"
	}
	return text, nil
}

// PipePane subscribes to session in watch mode and appends live PTY output
// directly to logPath (no shell invocation).
func (c *Client) PipePane(session, logPath string) error {
	c.stopPipe(session)

	w, err := dialNamespace(c.ns)
	if errors.Is(err, errNotRunning) {
		return fmt.Errorf("session %q not found", session)
	}
	if err != nil {
		return err
	}

	var sub struct {
		Handle uint32 `json:"handle"`
	}
	err = w.request("subscribe", map[string]any{
		"id":   session,
		"mode": "raw",
		"role": "watch",
		"flow": "lossy",
	}, &sub)
	if err != nil {
		_ = w.close()
		if isWireCode(err, "no_terminal") {
			return fmt.Errorf("session %q not found", session)
		}
		return fmt.Errorf("piping pane: %w", err)
	}

	f, err := os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		_ = w.close()
		return fmt.Errorf("opening pipe log: %w", err)
	}

	pw := &pipeWatcher{
		conn:   w,
		handle: sub.Handle,
		done:   make(chan struct{}),
	}
	c.pipesMu.Lock()
	c.pipes[session] = pw
	c.pipesMu.Unlock()

	go func() {
		defer close(pw.done)
		defer f.Close()
		for frame := range w.frames {
			switch {
			case frame.Type == frameOutput && frame.Stream == sub.Handle && len(frame.Payload) > 8:
				_, _ = f.Write(frame.Payload[8:])
			case frame.Type == frameEvent && frame.Stream == sub.Handle:
				var ev struct {
					Ev string `json:"ev"`
				}
				if json.Unmarshal(frame.Payload, &ev) == nil && ev.Ev == "closed" {
					return
				}
			}
		}
	}()
	return nil
}

// StopPipePane stops any active PipePane watcher on session.
func (c *Client) StopPipePane(session string) error {
	if c.stopPipe(session) {
		return nil
	}
	exists, err := c.SessionExists(session)
	if err != nil {
		return err
	}
	if !exists {
		return fmt.Errorf("session %q not found", session)
	}
	return nil
}

func (c *Client) stopPipe(session string) bool {
	c.pipesMu.Lock()
	pw, ok := c.pipes[session]
	if ok {
		delete(c.pipes, session)
	}
	c.pipesMu.Unlock()
	if !ok {
		return false
	}
	_ = pw.conn.request("unsubscribe", map[string]any{"handle": pw.handle}, nil)
	_ = pw.conn.close()
	<-pw.done
	return true
}

func (c *Client) stopAllPipes() {
	c.pipesMu.Lock()
	sessions := make([]string, 0, len(c.pipes))
	for s := range c.pipes {
		sessions = append(sessions, s)
	}
	c.pipesMu.Unlock()
	for _, s := range sessions {
		c.stopPipe(s)
	}
}

// Clear fails at the seam: g3d-termd owns the program's emulator screen and
// offers no client-side screen-wipe operation behind the program's back.
func (c *Client) Clear(session string) error {
	exists, err := c.SessionExists(session)
	if err != nil {
		return err
	}
	if !exists {
		return fmt.Errorf("session %q not found", session)
	}
	return fmt.Errorf("clearing %q: termd has no client-side clear op (clear inside the running program instead)", session)
}

// PanePIDs maps each live terminal's root process PID to its terminal id.
func (c *Client) PanePIDs() (map[int]string, error) {
	st, err := c.Status()
	if err != nil {
		return nil, err
	}
	out := make(map[int]string, len(st.Terminals))
	for _, t := range st.Terminals {
		if t.PID > 0 {
			out[t.PID] = t.ID
		}
	}
	return out, nil
}

// FirstPane returns the id, root PID, and working directory of session.
func (c *Client) FirstPane(session string) (tmux.PaneInfo, error) {
	st, err := c.Status()
	if err != nil {
		return tmux.PaneInfo{}, err
	}
	for _, t := range st.Terminals {
		if t.ID == session {
			return tmux.PaneInfo{
				ID:  t.ID,
				PID: t.PID,
				Cwd: ResolveTerminalCwd(t.Pwd, t.Cwd),
			}, nil
		}
	}
	return tmux.PaneInfo{}, fmt.Errorf("session %q not found", session)
}

// SessionOfPane maps a G3D_TERMD_ID value to its terminal name if live.
func (c *Client) SessionOfPane(pane string) (string, error) {
	pane = strings.TrimSpace(pane)
	if pane == "" {
		return "", nil
	}
	exists, err := c.SessionExists(pane)
	if err != nil || !exists {
		return "", err
	}
	return pane, nil
}

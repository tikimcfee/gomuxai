// Package termdwire implements the binary framing, handshake, request/reply
// multiplexing, daemon lifecycle helpers, and Unix-socket security checks for
// the g3d-termd wire protocol.
//
// Mirror locations — keep in lockstep (sync both copies if edited; `diff -ru`
// must exit 0):
//   - glyph3d-js: cli/termdwire
//   - gomuxai: internal/termdwire
package termdwire

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// Frame type bytes (PROTOCOL.md "Frame types").
const (
	FrameHello   = 0x01
	FrameRequest = 0x02
	FrameInput   = 0x03
	FrameWelcome = 0x81
	FrameReply   = 0x82
	FrameOutput  = 0x83
	FrameEvent   = 0x84
	FrameRepaint = 0x85
	FrameError   = 0xFF

	// DefaultMaxFrame is the largest payload protocol 1.x allows when WELCOME
	// does not advertise a smaller ceiling.
	DefaultMaxFrame = 16 << 20
)

// DefaultProto is the wire protocol version advertised in HELLO when
// DialOptions.Proto is unset.
var DefaultProto = [2]int{1, 5}

var (
	// ErrNotRunning indicates that nothing listens on the target termd socket.
	ErrNotRunning = errors.New("termd is not running")

	// ErrUnsafeSocket indicates that the socket directory or peer credentials
	// failed ownership, permission, or file-type verification.
	ErrUnsafeSocket = errors.New("unsafe termd socket")
)

// WireError is a structured protocol refusal from a failed REPLY or ERROR frame.
type WireError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func (e *WireError) Error() string {
	return e.Code + ": " + e.Message
}

// IsCode reports whether err unwraps to a *WireError with the given code.
func IsCode(err error, code string) bool {
	var we *WireError
	return errors.As(err, &we) && we.Code == code
}

// Frame is one framed message on the termd Unix socket.
type Frame struct {
	Type    byte
	Stream  uint32
	Payload []byte
}

// EncodeFrame serializes typ, stream, and payload into a 12-byte little-endian
// header followed by payload.
func EncodeFrame(typ byte, stream uint32, payload []byte) []byte {
	b := make([]byte, 12+len(payload))
	binary.LittleEndian.PutUint32(b[0:], uint32(8+len(payload)))
	b[4] = typ
	binary.LittleEndian.PutUint32(b[8:], stream)
	copy(b[12:], payload)
	return b
}

// ReadFrame reads one frame from r, rejecting malformed or oversized lengths.
func ReadFrame(r *bufio.Reader) (Frame, error) {
	var head [12]byte
	if _, err := io.ReadFull(r, head[:]); err != nil {
		return Frame{}, err
	}
	n := binary.LittleEndian.Uint32(head[0:])
	if n < 8 || n-8 > DefaultMaxFrame {
		return Frame{}, fmt.Errorf("termd: bad frame length %d", n)
	}
	f := Frame{
		Type:    head[4],
		Stream:  binary.LittleEndian.Uint32(head[8:]),
		Payload: make([]byte, n-8),
	}
	_, err := io.ReadFull(r, f.Payload)
	return f, err
}

// ReplyMsg is the asynchronous result delivered on the channel returned by Conn.Send.
type ReplyMsg struct {
	Payload []byte
	Err     error
}

// Welcome is the daemon's WELCOME handshake payload.
type Welcome struct {
	Proto      [2]int   `json:"proto"`
	Daemon     string   `json:"daemon"`
	Engine     string   `json:"engine"`
	NS         string   `json:"ns"`
	PID        int      `json:"pid"`
	Features   []string `json:"features"`
	MaxFrame   int      `json:"max_frame"`
	Instance   string   `json:"instance"`
	Generation int      `json:"generation"`
	Holder     struct {
		PID int `json:"pid"`
	} `json:"holder"`
}

// DialOptions configures the HELLO handshake sent when dialing termd.
type DialOptions struct {
	Client string
	Proto  [2]int
}

func (o DialOptions) normalized() DialOptions {
	if o.Client == "" {
		o.Client = "termd-client"
	}
	if o.Proto == ([2]int{}) {
		o.Proto = DefaultProto
	}
	return o
}

// Conn is an established client connection to g3d-termd. For connections
// opened via Dial or DialSocket, a background reader matches FrameReply frames
// to pending requests and forwards non-reply frames (OUTPUT, REPAINT, EVENT)
// in order to Frames.
type Conn struct {
	conn    net.Conn
	wmu     sync.Mutex
	nextReq atomic.Uint64
	mu      sync.Mutex
	pending map[uint64]chan ReplyMsg
	err     error

	Frames  chan Frame
	Welcome Welcome
}

// NewConn constructs a Conn wrapping nc and welcome with initialized pending
// request tracking and Frames channel, without starting a background readLoop.
func NewConn(nc net.Conn, welcome Welcome) *Conn {
	return &Conn{
		conn:    nc,
		pending: make(map[uint64]chan ReplyMsg),
		Frames:  make(chan Frame, 256),
		Welcome: welcome,
	}
}

// Dial validates ns and connects to SocketPath(ns). It never starts a daemon.
func Dial(ns string, opts DialOptions) (*Conn, error) {
	if err := ValidateName("termd namespace", ns); err != nil {
		return nil, err
	}
	return DialSocket(SocketPath(ns), opts)
}

// DialSocket verifies the socket parent directory, connects to path, verifies
// kernel peer credentials before sending any bytes, and performs the HELLO/WELCOME
// handshake.
func DialSocket(path string, opts DialOptions) (*Conn, error) {
	if strings.TrimSpace(path) == "" {
		return nil, fmt.Errorf("%w: empty socket path", ErrUnsafeSocket)
	}
	if err := CheckSocketDir(filepath.Dir(path)); err != nil {
		if os.IsNotExist(err) {
			return nil, fmt.Errorf("%w at %s", ErrNotRunning, path)
		}
		return nil, fmt.Errorf("termd at %s: %w", path, err)
	}
	nc, err := net.DialTimeout("unix", path, 2*time.Second)
	if err != nil {
		if errors.Is(err, syscall.ENOENT) || errors.Is(err, syscall.ECONNREFUSED) {
			return nil, fmt.Errorf("%w at %s", ErrNotRunning, path)
		}
		return nil, fmt.Errorf("termd at %s: %w", path, err)
	}
	if err := VerifyPeerCreds(nc); err != nil {
		_ = nc.Close()
		if !errors.Is(err, ErrUnsafeSocket) {
			return nil, fmt.Errorf("termd at %s: %w: %v", path, ErrUnsafeSocket, err)
		}
		return nil, fmt.Errorf("termd at %s: %w", path, err)
	}

	opts = opts.normalized()
	r := bufio.NewReaderSize(nc, 256<<10)
	hello, err := json.Marshal(map[string]any{
		"proto":  opts.Proto,
		"client": opts.Client,
	})
	if err != nil {
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s: hello encode: %w", path, err)
	}
	_ = nc.SetDeadline(time.Now().Add(5 * time.Second))
	// The holder may emit an early ERROR frame (e.g. brain_failed) before reading
	// HELLO; still read what is waiting when Write fails so that refusal surfaces.
	_, werr := nc.Write(EncodeFrame(FrameHello, 0, hello))
	f, rerr := ReadFrame(r)
	_ = nc.SetDeadline(time.Time{})
	if werr != nil && (rerr != nil || f.Type != FrameError) {
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s: hello: %w", path, werr)
	}
	if rerr != nil {
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s: welcome: %w", path, rerr)
	}

	var welcome Welcome
	switch f.Type {
	case FrameWelcome:
		if err := json.Unmarshal(f.Payload, &welcome); err != nil {
			_ = nc.Close()
			return nil, fmt.Errorf("termd at %s: welcome: %w", path, err)
		}
	case FrameError:
		var we WireError
		_ = json.Unmarshal(f.Payload, &we)
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s refused us: %w", path, &we)
	default:
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s: expected WELCOME, got frame type %#x", path, f.Type)
	}

	c := NewConn(nc, welcome)
	go c.readLoop(r)
	return c, nil
}

func (c *Conn) readLoop(r *bufio.Reader) {
	var err error
	for {
		var f Frame
		f, err = ReadFrame(r)
		if err != nil {
			break
		}
		if f.Type == FrameError {
			var we WireError
			_ = json.Unmarshal(f.Payload, &we)
			err = &we
			break
		}
		if f.Type == FrameReply {
			var head struct {
				Req   uint64     `json:"req"`
				OK    bool       `json:"ok"`
				Error *WireError `json:"error"`
			}
			if json.Unmarshal(f.Payload, &head) == nil {
				c.mu.Lock()
				ch := c.pending[head.Req]
				delete(c.pending, head.Req)
				c.mu.Unlock()
				if ch != nil {
					if !head.OK && head.Error != nil {
						ch <- ReplyMsg{Err: head.Error}
					} else {
						ch <- ReplyMsg{Payload: f.Payload}
					}
				}
			}
			continue
		}
		c.Frames <- f
	}
	if errors.Is(err, io.EOF) {
		err = errors.New("termd closed the connection")
	}
	c.mu.Lock()
	c.err = err
	for _, ch := range c.pending {
		ch <- ReplyMsg{Err: err}
	}
	c.pending = make(map[uint64]chan ReplyMsg)
	c.mu.Unlock()
	close(c.Frames)
}

func (c *Conn) write(b []byte) error {
	c.wmu.Lock()
	defer c.wmu.Unlock()
	if c.conn == nil {
		return errors.New("termd: nil connection")
	}
	_, err := c.conn.Write(b)
	return err
}

// Send writes an asynchronous request frame without blocking on its reply; the
// reply is delivered on the returned single-element channel. Requests exceeding
// MaxFrame are refused unsent with WireError code "too_large".
func (c *Conn) Send(op string, fields map[string]any) (<-chan ReplyMsg, error) {
	req := c.nextReq.Add(1)
	body := make(map[string]any, len(fields)+2)
	for k, v := range fields {
		body[k] = v
	}
	body["req"] = req
	body["op"] = op
	payload, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	if max := c.MaxFrame(); len(payload) > max {
		return nil, &WireError{
			Code:    "too_large",
			Message: fmt.Sprintf("the %s request is %d B as JSON; termd takes at most %d B (max_frame)", op, len(payload), max),
		}
	}
	ch := make(chan ReplyMsg, 1)
	c.mu.Lock()
	if c.err != nil {
		err := c.err
		c.mu.Unlock()
		return nil, err
	}
	c.pending[req] = ch
	c.mu.Unlock()

	if err := c.write(EncodeFrame(FrameRequest, 0, payload)); err != nil {
		c.mu.Lock()
		delete(c.pending, req)
		c.mu.Unlock()
		return nil, err
	}
	return ch, nil
}

// Request sends op with fields, waits up to 30s for its reply, and JSON-decodes
// the reply payload into out when out is non-nil.
func (c *Conn) Request(op string, fields map[string]any, out any) error {
	ch, err := c.Send(op, fields)
	if err != nil {
		return err
	}
	select {
	case r := <-ch:
		if r.Err != nil {
			return r.Err
		}
		if out != nil {
			return json.Unmarshal(r.Payload, out)
		}
		return nil
	case <-time.After(30 * time.Second):
		return fmt.Errorf("termd: no reply to %s in 30s", op)
	}
}

// Input writes raw PTY input bytes to a subscribed terminal handle.
func (c *Conn) Input(handle uint32, b []byte) error {
	return c.write(EncodeFrame(FrameInput, handle, b))
}

// Has reports whether the daemon advertised feature in WELCOME.features.
func (c *Conn) Has(feature string) bool {
	return slices.Contains(c.Welcome.Features, feature)
}

// Lacks returns the subset of want not advertised in WELCOME.features.
func (c *Conn) Lacks(want []string) []string {
	out := []string{}
	for _, f := range want {
		if !c.Has(f) {
			out = append(out, f)
		}
	}
	return out
}

// MaxFrame returns WELCOME.max_frame if positive, else DefaultMaxFrame.
func (c *Conn) MaxFrame() int {
	if c.Welcome.MaxFrame > 0 {
		return c.Welcome.MaxFrame
	}
	return DefaultMaxFrame
}

// Err returns the terminal error that ended readLoop, or nil while active.
func (c *Conn) Err() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.err
}

// Close closes the underlying socket connection.
func (c *Conn) Close() error {
	if c.conn == nil {
		return nil
	}
	return c.conn.Close()
}

// EnsureDaemon dials namespace ns, spawning `daemonBin --ns ns --daemon` with a
// scrubbed environment when ErrNotRunning is returned and polling up to 3s for
// the socket to answer. Any ErrUnsafeSocket or non-ErrNotRunning error fails
// closed immediately without spawning or polling.
func EnsureDaemon(ns, daemonBin string, opts DialOptions) (*Conn, error) {
	c, err := Dial(ns, opts)
	if err == nil || !errors.Is(err, ErrNotRunning) {
		return c, err
	}
	if strings.TrimSpace(daemonBin) == "" {
		return nil, errors.New("termd: empty daemon binary path")
	}
	cmd := exec.Command(daemonBin, "--ns", ns, "--daemon")
	cmd.Env = DaemonEnv(os.Environ())
	out, startErr := cmd.CombinedOutput()
	deadline := time.Now().Add(3 * time.Second)
	wait := 20 * time.Millisecond
	for {
		c, err = Dial(ns, opts)
		if err == nil {
			return c, nil
		}
		if errors.Is(err, ErrUnsafeSocket) {
			return nil, err
		}
		if !errors.Is(err, ErrNotRunning) || time.Now().After(deadline) {
			break
		}
		time.Sleep(wait)
		if wait < 250*time.Millisecond {
			wait *= 2
		}
	}
	logPath := LogPath(ns)
	tail := ""
	if b, rerr := ReadOwnedRegularFile(logPath, 64<<10); rerr == nil {
		lines := strings.Split(strings.TrimRight(string(b), "\n"), "\n")
		if len(lines) > 10 {
			lines = lines[len(lines)-10:]
		}
		tail = strings.Join(lines, "\n")
	}
	return nil, fmt.Errorf("termd did not come up for namespace %q (%v); %s --ns %s --daemon: %v %s; log %s:\n%s",
		ns, err, daemonBin, ns, startErr, strings.TrimSpace(string(out)), logPath, tail)
}

// Redial rides out a g3d-termd brain restart within window. Planned restarts
// reconnect after up to 100ms of jitter; unplanned disconnects back off from
// 100ms doubling to 2s. ErrNotRunning, ErrUnsafeSocket, and instance mismatches
// fail immediately.
func Redial(ns, instance string, window time.Duration, planned bool, opts DialOptions) (*Conn, error) {
	if err := ValidateName("termd namespace", ns); err != nil {
		return nil, err
	}
	deadline := time.Now().Add(window)
	wait := 100 * time.Millisecond
	if planned {
		jitter := uint64(time.Now().UnixNano()) % uint64(100*time.Millisecond)
		time.Sleep(time.Duration(jitter))
	} else {
		time.Sleep(wait)
	}
	var last error
	for {
		c, err := Dial(ns, opts)
		switch {
		case err == nil && instance != "" && c.Welcome.Instance != instance:
			_ = c.Close()
			return nil, fmt.Errorf("termd namespace %q is a new one (instance %s, was %s): its terminals are gone", ns, c.Welcome.Instance, instance)
		case err == nil:
			return c, nil
		case errors.Is(err, ErrNotRunning), errors.Is(err, ErrUnsafeSocket):
			return nil, err
		}
		last = err
		if time.Now().After(deadline) {
			return nil, fmt.Errorf("no termd brain answered for %s: %w", window, last)
		}
		time.Sleep(wait)
		if wait < 2*time.Second {
			wait *= 2
		}
	}
}

// StopSocket stops a g3d-termd daemon listening at sockPath. If nothing is
// running at sockPath, StopSocket returns nil.
func StopSocket(sockPath string, opts DialOptions) error {
	c, err := DialSocket(sockPath, opts)
	if errors.Is(err, ErrNotRunning) {
		return nil
	}
	if err != nil {
		return err
	}
	defer c.Close()
	return c.Request("stop", nil, nil)
}

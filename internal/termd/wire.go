package termd

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
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

const (
	frameHello   = 0x01
	frameRequest = 0x02
	frameInput   = 0x03
	frameWelcome = 0x81
	frameReply   = 0x82
	frameOutput  = 0x83
	frameEvent   = 0x84
	frameRepaint = 0x85
	frameError   = 0xFF

	defaultMaxFrame = 16 << 20
)

var errNotRunning = errors.New("termd daemon is not running")

// WireError is an error returned by g3d-termd in a REPLY or ERROR frame.
type WireError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func (e *WireError) Error() string { return e.Code + ": " + e.Message }

func isWireCode(err error, code string) bool {
	var we *WireError
	return errors.As(err, &we) && we.Code == code
}

type rawFrame struct {
	Type    byte
	Stream  uint32
	Payload []byte
}

func encodeFrame(typ byte, stream uint32, payload []byte) []byte {
	b := make([]byte, 12+len(payload))
	binary.LittleEndian.PutUint32(b[0:], uint32(8+len(payload)))
	b[4] = typ
	binary.LittleEndian.PutUint32(b[8:], stream)
	copy(b[12:], payload)
	return b
}

func readFrame(r *bufio.Reader) (rawFrame, error) {
	var head [12]byte
	if _, err := io.ReadFull(r, head[:]); err != nil {
		return rawFrame{}, err
	}
	n := binary.LittleEndian.Uint32(head[0:])
	if n < 8 || n-8 > defaultMaxFrame {
		return rawFrame{}, fmt.Errorf("termd: invalid frame length %d", n)
	}
	f := rawFrame{
		Type:    head[4],
		Stream:  binary.LittleEndian.Uint32(head[8:]),
		Payload: make([]byte, n-8),
	}
	_, err := io.ReadFull(r, f.Payload)
	return f, err
}

type replyMsg struct {
	payload []byte
	err     error
}

type welcomeMsg struct {
	Proto      [2]int   `json:"proto"`
	Daemon     string   `json:"daemon"`
	Engine     string   `json:"engine"`
	NS         string   `json:"ns"`
	PID        int      `json:"pid"`
	Instance   string   `json:"instance"`
	Generation int      `json:"generation"`
	Features   []string `json:"features"`
	MaxFrame   int      `json:"max_frame"`
}

type wireConn struct {
	nc      net.Conn
	wmu     sync.Mutex
	nextReq atomic.Uint64
	mu      sync.Mutex
	pending map[uint64]chan replyMsg
	err     error
	frames  chan rawFrame
	welcome welcomeMsg
}

func dialNamespace(ns string) (*wireConn, error) {
	if err := CheckTestServer(); err != nil {
		return nil, err
	}
	if err := ValidateNamespace(ns); err != nil {
		return nil, err
	}
	return dialSocket(SocketPath(ns))
}

func dialSocket(path string) (*wireConn, error) {
	nc, err := net.DialTimeout("unix", path, 2*time.Second)
	if err != nil {
		if errors.Is(err, syscall.ENOENT) || errors.Is(err, syscall.ECONNREFUSED) {
			return nil, fmt.Errorf("%w at %s", errNotRunning, path)
		}
		return nil, fmt.Errorf("termd at %s: %w", path, err)
	}
	r := bufio.NewReaderSize(nc, 256<<10)
	hello, _ := json.Marshal(map[string]any{
		"proto":  []int{1, 5},
		"client": "gomuxai",
	})
	_ = nc.SetDeadline(time.Now().Add(5 * time.Second))
	_, werr := nc.Write(encodeFrame(frameHello, 0, hello))
	f, rerr := readFrame(r)
	_ = nc.SetDeadline(time.Time{})
	if werr != nil && (rerr != nil || f.Type != frameError) {
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s: hello: %w", path, werr)
	}
	if rerr != nil {
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s: welcome: %w", path, rerr)
	}
	c := &wireConn{
		nc:      nc,
		pending: make(map[uint64]chan replyMsg),
		frames:  make(chan rawFrame, 256),
	}
	switch f.Type {
	case frameWelcome:
		if err := json.Unmarshal(f.Payload, &c.welcome); err != nil {
			_ = nc.Close()
			return nil, fmt.Errorf("termd at %s: welcome decode: %w", path, err)
		}
	case frameError:
		var we WireError
		_ = json.Unmarshal(f.Payload, &we)
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s refused connection: %w", path, &we)
	default:
		_ = nc.Close()
		return nil, fmt.Errorf("termd at %s: unexpected handshake frame %#x", path, f.Type)
	}
	go c.readLoop(r)
	return c, nil
}

func (c *wireConn) readLoop(r *bufio.Reader) {
	var err error
	for {
		var f rawFrame
		f, err = readFrame(r)
		if err != nil {
			break
		}
		if f.Type == frameError {
			var we WireError
			_ = json.Unmarshal(f.Payload, &we)
			err = &we
			break
		}
		if f.Type == frameReply {
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
						ch <- replyMsg{err: head.Error}
					} else {
						ch <- replyMsg{payload: f.Payload}
					}
				}
			}
			continue
		}
		c.frames <- f
	}
	if errors.Is(err, io.EOF) {
		err = errors.New("termd closed the connection")
	}
	c.mu.Lock()
	c.err = err
	for _, ch := range c.pending {
		ch <- replyMsg{err: err}
	}
	c.pending = map[uint64]chan replyMsg{}
	c.mu.Unlock()
	close(c.frames)
}

func (c *wireConn) close() error { return c.nc.Close() }

func (c *wireConn) write(b []byte) error {
	c.wmu.Lock()
	defer c.wmu.Unlock()
	_, err := c.nc.Write(b)
	return err
}

func (c *wireConn) maxFrame() int {
	if c.welcome.MaxFrame > 0 {
		return c.welcome.MaxFrame
	}
	return defaultMaxFrame
}

func (c *wireConn) hasFeature(f string) bool {
	return slices.Contains(c.welcome.Features, f)
}

func (c *wireConn) request(op string, fields map[string]any, out any) error {
	req := c.nextReq.Add(1)
	body := make(map[string]any, len(fields)+2)
	for k, v := range fields {
		body[k] = v
	}
	body["req"] = req
	body["op"] = op
	payload, err := json.Marshal(body)
	if err != nil {
		return err
	}
	if limit := c.maxFrame(); len(payload) > limit {
		return &WireError{
			Code:    "too_large",
			Message: fmt.Sprintf("%s request is %d bytes; max_frame is %d", op, len(payload), limit),
		}
	}
	ch := make(chan replyMsg, 1)
	c.mu.Lock()
	if c.err != nil {
		err := c.err
		c.mu.Unlock()
		return err
	}
	c.pending[req] = ch
	c.mu.Unlock()

	if err := c.write(encodeFrame(frameRequest, 0, payload)); err != nil {
		c.mu.Lock()
		delete(c.pending, req)
		c.mu.Unlock()
		return err
	}
	select {
	case r := <-ch:
		if r.err != nil {
			return r.err
		}
		if out != nil {
			return json.Unmarshal(r.payload, out)
		}
		return nil
	case <-time.After(30 * time.Second):
		return fmt.Errorf("termd: timed out waiting 30s for %s reply", op)
	}
}

func (c *wireConn) input(handle uint32, data []byte) error {
	return c.write(encodeFrame(frameInput, handle, data))
}

func ensureDaemon(ns, daemonBin string) (*wireConn, error) {
	c, err := dialNamespace(ns)
	if err == nil || !errors.Is(err, errNotRunning) {
		return c, err
	}
	bin, err := DaemonBinary(daemonBin)
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(bin, "--ns", ns, "--daemon")
	for _, kv := range os.Environ() {
		if !strings.HasPrefix(kv, "G3D_TERMD_SOCKET=") && !strings.HasPrefix(kv, "G3D_TERMD_ID=") {
			cmd.Env = append(cmd.Env, kv)
		}
	}
	out, startErr := cmd.CombinedOutput()
	deadline := time.Now().Add(3 * time.Second)
	wait := 20 * time.Millisecond
	for {
		c, err = dialNamespace(ns)
		if err == nil {
			return c, nil
		}
		if !errors.Is(err, errNotRunning) || time.Now().After(deadline) {
			break
		}
		time.Sleep(wait)
		if wait < 250*time.Millisecond {
			wait *= 2
		}
	}
	tail := ""
	if b, rerr := os.ReadFile(LogPath(ns)); rerr == nil {
		lines := strings.Split(strings.TrimRight(string(b), "\n"), "\n")
		if len(lines) > 10 {
			lines = lines[len(lines)-10:]
		}
		tail = strings.Join(lines, "\n")
	}
	return nil, fmt.Errorf("termd did not come up for namespace %q (%v); %s --ns %s --daemon: %v %s; log %s:\n%s",
		ns, err, bin, ns, startErr, strings.TrimSpace(string(out)), LogPath(ns), tail)
}

// StopSocket stops a g3d-termd daemon listening at sockPath, if any.
func StopSocket(sockPath string) error {
	c, err := dialSocket(sockPath)
	if errors.Is(err, errNotRunning) {
		return nil
	}
	if err != nil {
		return err
	}
	defer c.close()
	return c.request("stop", nil, nil)
}

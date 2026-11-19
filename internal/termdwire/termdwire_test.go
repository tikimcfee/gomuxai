//go:build unix

package termdwire

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func shortTempDir(t *testing.T, pattern string) string {
	t.Helper()
	// Keep Unix socket paths well within macOS's 104-byte SUN_LEN limit.
	dir, err := os.MkdirTemp("/tmp", pattern)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	return dir
}

func startMockWelcomeServer(t *testing.T, sockPath, ns string) (gotHello chan map[string]any) {
	t.Helper()
	gotHello = make(chan map[string]any, 1)
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() })

	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		r := bufio.NewReader(conn)
		f, err := ReadFrame(r)
		if err != nil || f.Type != FrameHello {
			select {
			case gotHello <- nil:
			default:
			}
			return
		}
		var hello map[string]any
		_ = json.Unmarshal(f.Payload, &hello)
		select {
		case gotHello <- hello:
		default:
		}
		welcome, _ := json.Marshal(map[string]any{
			"proto":      []int{1, 5},
			"daemon":     "g3d-termd/test",
			"engine":     "mock",
			"ns":         ns,
			"pid":        os.Getpid(),
			"features":   []string{"key", "paste", "status"},
			"max_frame":  DefaultMaxFrame,
			"instance":   "inst-test",
			"generation": 2,
			"holder":     map[string]any{"pid": os.Getpid()},
		})
		if _, err := conn.Write(EncodeFrame(FrameWelcome, 0, welcome)); err != nil {
			return
		}
		for {
			reqFrame, err := ReadFrame(r)
			if err != nil {
				return
			}
			if reqFrame.Type != FrameRequest {
				continue
			}
			var req struct {
				Req uint64 `json:"req"`
				Op  string `json:"op"`
			}
			if json.Unmarshal(reqFrame.Payload, &req) != nil {
				continue
			}
			reply, _ := json.Marshal(map[string]any{
				"req": req.Req,
				"ok":  true,
				"op":  req.Op,
			})
			_, _ = conn.Write(EncodeFrame(FrameReply, 0, reply))
			if req.Op == "stop" {
				return
			}
		}
	}()
	return gotHello
}

func TestSocketDirAndPaths(t *testing.T) {
	if got := SocketDirFor("/run/user/1000", 1000); got != "/run/user/1000/g3d" {
		t.Fatalf("SocketDirFor(runtime) = %q, want /run/user/1000/g3d", got)
	}
	if got := SocketDirFor("", 501); got != "/tmp/g3d-501" {
		t.Fatalf("SocketDirFor(empty) = %q, want /tmp/g3d-501", got)
	}

	t.Setenv("G3D_TERMD_DIR", "")
	t.Setenv("XDG_RUNTIME_DIR", "/nonexistent/runtime")
	t.Setenv("TMPDIR", "/nonexistent/tmp")
	desktop := SocketDir()
	t.Setenv("XDG_RUNTIME_DIR", "")
	t.Setenv("TMPDIR", "")
	stripped := SocketDir()
	if desktop != stripped {
		t.Fatalf("SocketDir varied with XDG_RUNTIME_DIR/TMPDIR: %q vs %q", desktop, stripped)
	}

	t.Setenv("G3D_TERMD_DIR", "/tmp/custom-g3d")
	if got := SocketPath("agents"); got != "/tmp/custom-g3d/termd-agents.sock" {
		t.Fatalf("SocketPath = %q", got)
	}
	if got := LogPath("agents"); got != "/tmp/custom-g3d/termd-agents.log" {
		t.Fatalf("LogPath(G3D_TERMD_DIR) = %q", got)
	}

	t.Setenv("G3D_TERMD_DIR", "")
	t.Setenv("XDG_STATE_HOME", "/tmp/xdg-state")
	if got := LogPath("agents"); got != "/tmp/xdg-state/g3d/termd-agents.log" {
		t.Fatalf("LogPath(XDG_STATE_HOME) = %q", got)
	}

	t.Setenv("XDG_STATE_HOME", "")
	t.Setenv("HOME", "/tmp/fakehome")
	if got := LogPath("agents"); got != "/tmp/fakehome/.local/state/g3d/termd-agents.log" {
		t.Fatalf("LogPath(HOME) = %q", got)
	}
}

func TestValidateName(t *testing.T) {
	for _, valid := range []string{"default", "wall", "agents", "a1.b_c-2", strings.Repeat("z", 64)} {
		if err := ValidateName("termd namespace", valid); err != nil {
			t.Errorf("ValidateName(%q) = %v, want nil", valid, err)
		}
	}
	for _, bad := range []string{"", "has space", "slash/ns", "../up", "bad\x00nul", "é", strings.Repeat("a", 65)} {
		if err := ValidateName("termd namespace", bad); err == nil {
			t.Errorf("ValidateName(%q) = nil, want error", bad)
		}
	}
	if _, err := Dial("../escape", DialOptions{}); err == nil {
		t.Fatal("Dial(../escape) succeeded, want ValidateName error")
	}
}

func TestDaemonEnv(t *testing.T) {
	in := []string{
		"PATH=/usr/bin:/bin",
		"G3D_TERMD_DIR=/tmp/g3d-keep",
		"G3D_TERMD_SOCKET=/tmp/g3d-keep/termd-wall.sock",
		"G3D_TERMD_ID=pane-1",
		"G3D_RELAY_TOKEN=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
		"G3D_RELAY_PORT=8080",
		"KEEP_VAR=G3D_RELAY_TOKEN=value_not_key",
	}
	got := DaemonEnv(in)
	want := []string{
		"PATH=/usr/bin:/bin",
		"G3D_TERMD_DIR=/tmp/g3d-keep",
		"KEEP_VAR=G3D_RELAY_TOKEN=value_not_key",
	}
	if !slices.Equal(got, want) {
		t.Fatalf("DaemonEnv = %v, want %v", got, want)
	}
}

func TestFrameRoundTripAndMaxFrameUnsent(t *testing.T) {
	raw := EncodeFrame(FrameOutput, 42, []byte("hello world"))
	f, err := ReadFrame(bufio.NewReader(bytes.NewReader(raw)))
	if err != nil {
		t.Fatalf("ReadFrame failed: %v", err)
	}
	if f.Type != FrameOutput || f.Stream != 42 || string(f.Payload) != "hello world" {
		t.Fatalf("unexpected frame: %+v", f)
	}

	// Reject frame length < 8.
	badShort := make([]byte, 12)
	badShort[0] = 4
	if _, err := ReadFrame(bufio.NewReader(bytes.NewReader(badShort))); err == nil {
		t.Fatal("ReadFrame accepted frame length < 8")
	}

	// Verify Conn.Send refuses oversized requests unsent and checks features cleanly.
	a, b := net.Pipe()
	t.Cleanup(func() { _ = a.Close(); _ = b.Close() })
	var mu sync.Mutex
	readBytes := 0
	go func() {
		buf := make([]byte, 64<<10)
		for {
			n, err := b.Read(buf)
			mu.Lock()
			readBytes += n
			mu.Unlock()
			if err != nil {
				return
			}
		}
	}()
	readCount := func() int {
		time.Sleep(40 * time.Millisecond)
		mu.Lock()
		defer mu.Unlock()
		return readBytes
	}

	c := NewConn(a, Welcome{
		Daemon:   "g3d-termd/test",
		Features: []string{"key"},
		MaxFrame: 1024,
	})
	if !c.Has("key") || c.Has("paste") {
		t.Fatalf("Has mismatch on %+v", c.Welcome.Features)
	}
	if lacks := c.Lacks([]string{"key", "paste", "mouse"}); !slices.Equal(lacks, []string{"paste", "mouse"}) {
		t.Fatalf("Lacks = %v, want [paste mouse]", lacks)
	}
	if c.MaxFrame() != 1024 {
		t.Fatalf("MaxFrame = %d, want 1024", c.MaxFrame())
	}
	if def := NewConn(nil, Welcome{}).MaxFrame(); def != DefaultMaxFrame {
		t.Fatalf("default MaxFrame = %d, want %d", def, DefaultMaxFrame)
	}

	_, err = c.Send("paste", map[string]any{"handle": 1, "text": strings.Repeat("\x01", 200)})
	if !IsCode(err, "too_large") || !strings.Contains(err.Error(), "max_frame") {
		t.Fatalf("Send(>MaxFrame) = %v, want WireError too_large mentioning max_frame", err)
	}
	if n := readCount(); n != 0 {
		t.Fatalf("%d bytes of oversized request were written to the socket", n)
	}

	if _, err := c.Send("paste", map[string]any{"handle": 1, "text": "hi"}); err != nil {
		t.Fatalf("Send(small) = %v", err)
	}
	if readCount() == 0 {
		t.Fatal("small request was not written")
	}
}

func TestSecurity_Rejects0755SocketDir(t *testing.T) {
	dir := shortTempDir(t, "tw755-")
	if err := os.Chmod(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	const ns = "sec755"
	sockPath := filepath.Join(dir, "termd-"+ns+".sock")
	gotHello := startMockWelcomeServer(t, sockPath, ns)
	t.Setenv("G3D_TERMD_DIR", dir)

	if err := CheckSocketDir(dir); !errors.Is(err, ErrUnsafeSocket) {
		t.Fatalf("CheckSocketDir(0755) = %v, want ErrUnsafeSocket", err)
	}
	if _, err := Dial(ns, DialOptions{}); !errors.Is(err, ErrUnsafeSocket) {
		t.Fatalf("Dial(0755) = %v, want ErrUnsafeSocket", err)
	}
	if err := StopSocket(sockPath, DialOptions{}); !errors.Is(err, ErrUnsafeSocket) {
		t.Fatalf("StopSocket(0755) = %v, want ErrUnsafeSocket", err)
	}

	marker := filepath.Join(dir, "spawned")
	fakeBin := filepath.Join(dir, "fake-termd")
	if err := os.WriteFile(fakeBin, []byte("#!/bin/sh\ntouch '"+marker+"'\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	start := time.Now()
	if _, err := EnsureDaemon(ns, fakeBin, DialOptions{}); !errors.Is(err, ErrUnsafeSocket) {
		t.Fatalf("EnsureDaemon(0755) = %v, want ErrUnsafeSocket", err)
	}
	if elapsed := time.Since(start); elapsed > time.Second {
		t.Fatalf("EnsureDaemon(0755) took %s instead of failing fast", elapsed)
	}
	if _, err := os.Stat(marker); err == nil {
		t.Fatal("EnsureDaemon executed daemon binary despite unsafe 0755 socket directory")
	}
	select {
	case <-gotHello:
		t.Fatal("Dial connected to socket inside 0755 directory")
	default:
	}
}

func TestSecurity_RejectsSymlinkSocketDirAndMissingIsNotRunning(t *testing.T) {
	realDir := shortTempDir(t, "twreal-")
	if err := os.Chmod(realDir, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := shortTempDir(t, "twlnk-")
	linkDir := filepath.Join(parent, "socklink")
	if err := os.Symlink(realDir, linkDir); err != nil {
		t.Fatal(err)
	}
	danglingDir := filepath.Join(parent, "dangling")
	if err := os.Symlink(filepath.Join(parent, "does-not-exist"), danglingDir); err != nil {
		t.Fatal(err)
	}

	const ns = "seclnk"
	gotHello := startMockWelcomeServer(t, filepath.Join(realDir, "termd-"+ns+".sock"), ns)
	fakeBin := filepath.Join(parent, "never-run")

	for _, candidate := range []string{linkDir, linkDir + "/", danglingDir} {
		t.Setenv("G3D_TERMD_DIR", candidate)
		if err := CheckSocketDir(candidate); !errors.Is(err, ErrUnsafeSocket) {
			t.Fatalf("CheckSocketDir(%q) = %v, want ErrUnsafeSocket", candidate, err)
		}
		if _, err := Dial(ns, DialOptions{}); !errors.Is(err, ErrUnsafeSocket) {
			t.Fatalf("Dial(%q) = %v, want ErrUnsafeSocket", candidate, err)
		}
		if _, err := EnsureDaemon(ns, fakeBin, DialOptions{}); !errors.Is(err, ErrUnsafeSocket) {
			t.Fatalf("EnsureDaemon(%q) = %v, want ErrUnsafeSocket", candidate, err)
		}
	}
	select {
	case <-gotHello:
		t.Fatal("Dial connected through symlinked socket directory")
	default:
	}

	// Missing directory must report os.IsNotExist -> ErrNotRunning (not ErrUnsafeSocket).
	missingDir := filepath.Join(parent, "missing-subdir")
	if err := CheckSocketDir(missingDir); err == nil || !os.IsNotExist(err) || errors.Is(err, ErrUnsafeSocket) {
		t.Fatalf("CheckSocketDir(missing) = %v, want os.IsNotExist without ErrUnsafeSocket", err)
	}
	t.Setenv("G3D_TERMD_DIR", missingDir)
	if _, err := Dial(ns, DialOptions{}); !errors.Is(err, ErrNotRunning) {
		t.Fatalf("Dial(missing dir) = %v, want ErrNotRunning", err)
	}
}

func TestSecurity_Accepts0700SameUIDAndVerifiesPeerCreds(t *testing.T) {
	dir := shortTempDir(t, "twok-")
	if err := os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	const ns = "secok"
	sockPath := filepath.Join(dir, "termd-"+ns+".sock")
	gotHello := startMockWelcomeServer(t, sockPath, ns)
	t.Setenv("G3D_TERMD_DIR", dir)

	c, err := Dial(ns, DialOptions{Client: "unit-test", Proto: [2]int{1, 5}})
	if err != nil {
		t.Fatalf("Dial(0700 same uid) failed: %v", err)
	}
	if c.Welcome.Daemon != "g3d-termd/test" || c.Welcome.Instance != "inst-test" || c.Welcome.Holder.PID != os.Getpid() {
		_ = c.Close()
		t.Fatalf("unexpected Welcome: %+v", c.Welcome)
	}
	hello := <-gotHello
	if hello["client"] != "unit-test" {
		_ = c.Close()
		t.Fatalf("unexpected HELLO payload: %+v", hello)
	}
	var pingReply struct {
		Op string `json:"op"`
	}
	if err := c.Request("ping", nil, &pingReply); err != nil || pingReply.Op != "ping" {
		_ = c.Close()
		t.Fatalf("Request(ping) = %+v, %v", pingReply, err)
	}
	_ = c.Close()

	// StopSocket on non-existent socket returns nil.
	if err := StopSocket(filepath.Join(dir, "termd-absent.sock"), DialOptions{}); err != nil {
		t.Fatalf("StopSocket(absent) = %v, want nil", err)
	}

	// VerifyPeerCreds and VerifyPeerCredsForUID(uid+1) on live Unix socket + non-UnixConn refusal.
	mismatchSock := filepath.Join(dir, "peercred.sock")
	ln, err := net.Listen("unix", mismatchSock)
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	accepted := make(chan net.Conn, 1)
	go func() {
		if sc, err := ln.Accept(); err == nil {
			accepted <- sc
		}
	}()
	rawConn, err := net.DialTimeout("unix", mismatchSock, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer rawConn.Close()

	if err := VerifyPeerCreds(rawConn); err != nil {
		t.Fatalf("VerifyPeerCreds(same uid) = %v, want nil", err)
	}
	if err := VerifyPeerCredsForUID(rawConn, uint32(os.Getuid())+1); !errors.Is(err, ErrUnsafeSocket) {
		t.Fatalf("VerifyPeerCredsForUID(uid+1) = %v, want ErrUnsafeSocket", err)
	}
	pa, pb := net.Pipe()
	defer pa.Close()
	defer pb.Close()
	if err := VerifyPeerCreds(pa); !errors.Is(err, ErrUnsafeSocket) {
		t.Fatalf("VerifyPeerCreds(net.Pipe) = %v, want ErrUnsafeSocket", err)
	}
	select {
	case sc := <-accepted:
		_ = sc.Close()
	case <-time.After(time.Second):
	}
}

func TestHandshakeEarlyBrainFailedAndRedial(t *testing.T) {
	dir := shortTempDir(t, "twerr-")
	if err := os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	const ns = "earlyerr"
	sockPath := filepath.Join(dir, "termd-"+ns+".sock")
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		payload, _ := json.Marshal(WireError{Code: "brain_failed", Message: "brain crashed"})
		_, _ = conn.Write(EncodeFrame(FrameError, 0, payload))
	}()

	t.Setenv("G3D_TERMD_DIR", dir)
	if _, err := Dial(ns, DialOptions{}); !IsCode(err, "brain_failed") {
		t.Fatalf("Dial(early ERROR) = %v, want WireError code brain_failed", err)
	}

	// Redial detects instance replacement immediately.
	const ns2 = "redialinst"
	_ = startMockWelcomeServer(t, filepath.Join(dir, "termd-"+ns2+".sock"), ns2)
	if _, err := Redial(ns2, "different-instance", 500*time.Millisecond, true, DialOptions{}); err == nil || !strings.Contains(err.Error(), "different-instance") {
		t.Fatalf("Redial(mismatched instance) = %v, want instance mismatch error", err)
	}
}

func TestHelperTermdDaemon(t *testing.T) {
	envOut := os.Getenv("G3D_TEST_HELPER_ENV_OUT")
	sockPath := os.Getenv("G3D_TEST_HELPER_SOCK")
	if envOut == "" || sockPath == "" {
		return
	}
	_ = os.WriteFile(envOut, []byte(strings.Join(os.Environ(), "\n")+"\n"), 0o600)
	if err := os.MkdirAll(filepath.Dir(sockPath), 0o700); err != nil {
		os.Exit(2)
	}
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		os.Exit(3)
	}
	done := make(chan struct{})
	go func() {
		defer close(done)
		defer ln.Close()
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		r := bufio.NewReader(conn)
		f, err := ReadFrame(r)
		if err != nil || f.Type != FrameHello {
			return
		}
		welcome, _ := json.Marshal(map[string]any{
			"proto":      []int{1, 5},
			"daemon":     "g3d-termd/helper",
			"engine":     "helper",
			"ns":         "envscrub",
			"pid":        os.Getpid(),
			"features":   []string{"key", "paste"},
			"max_frame":  DefaultMaxFrame,
			"instance":   "inst-helper",
			"generation": 1,
		})
		_, _ = conn.Write(EncodeFrame(FrameWelcome, 0, welcome))
		_, _ = io.Copy(io.Discard, conn)
	}()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
	}
}

func TestSecurity_EnsureDaemonStripsRelayAndTermdVars(t *testing.T) {
	base := shortTempDir(t, "twenv-")
	sockDir := filepath.Join(base, "run") // created by helper on spawn
	const ns = "envscrub"
	sockPath := filepath.Join(sockDir, "termd-"+ns+".sock")
	envOut := filepath.Join(base, "daemon.env")
	wrapper := filepath.Join(base, "fake-g3d-termd")

	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	script := "#!/bin/sh\n'" + exe + "' -test.run='^TestHelperTermdDaemon$' >/dev/null 2>&1 &\nexit 0\n"
	if err := os.WriteFile(wrapper, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}

	const secret = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
	t.Setenv("G3D_TERMD_DIR", sockDir)
	t.Setenv("G3D_RELAY_TOKEN", secret)
	t.Setenv("G3D_RELAY_EXTRA", "relay-control")
	t.Setenv("G3D_TERMD_SOCKET", "/tmp/outer.sock")
	t.Setenv("G3D_TERMD_ID", "outer-pane")
	t.Setenv("G3D_TEST_HELPER_ENV_OUT", envOut)
	t.Setenv("G3D_TEST_HELPER_SOCK", sockPath)
	t.Setenv("G3D_TEST_KEEP_MARKER", "kept-ok")

	c, err := EnsureDaemon(ns, wrapper, DialOptions{Client: "ensure-test"})
	if err != nil {
		t.Fatalf("EnsureDaemon failed: %v", err)
	}
	_ = c.Close()

	b, err := os.ReadFile(envOut)
	if err != nil {
		t.Fatalf("reading daemon env dump: %v", err)
	}
	envDump := string(b)
	for _, forbidden := range []string{"G3D_RELAY_TOKEN=", secret, "G3D_RELAY_EXTRA=", "G3D_TERMD_SOCKET=", "G3D_TERMD_ID="} {
		if strings.Contains(envDump, forbidden) {
			t.Fatalf("spawned daemon inherited %q:\n%s", forbidden, envDump)
		}
	}
	if !strings.Contains(envDump, "G3D_TEST_KEEP_MARKER=kept-ok") || !strings.Contains(envDump, "G3D_TERMD_DIR="+sockDir) {
		t.Fatalf("spawned daemon lost expected environment variables:\n%s", envDump)
	}
}

func TestSecurity_ReadOwnedRegularFileRejectsSymlinkAndFIFO(t *testing.T) {
	dir := shortTempDir(t, "twlog-")
	reg := filepath.Join(dir, "real.log")
	if err := os.WriteFile(reg, []byte("alpha\nbeta\ngamma\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	got, err := ReadOwnedRegularFile(reg, 64<<10)
	if err != nil || string(got) != "alpha\nbeta\ngamma\n" {
		t.Fatalf("ReadOwnedRegularFile(regular) = %q, %v", got, err)
	}
	tail, err := ReadOwnedRegularFile(reg, 6)
	if err != nil || string(tail) != "gamma\n" {
		t.Fatalf("ReadOwnedRegularFile(tail 6) = %q, %v", tail, err)
	}

	link := filepath.Join(dir, "symlink.log")
	if err := os.Symlink(reg, link); err != nil {
		t.Fatal(err)
	}
	if b, err := ReadOwnedRegularFile(link, 64<<10); err == nil {
		t.Fatalf("ReadOwnedRegularFile(symlink) succeeded with %q, want error", b)
	}

	fifo := filepath.Join(dir, "pipe.log")
	if err := unix.Mkfifo(fifo, 0o600); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		_, err := ReadOwnedRegularFile(fifo, 64<<10)
		done <- err
	}()
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("ReadOwnedRegularFile(fifo) succeeded, want error")
		}
	case <-time.After(time.Second):
		t.Fatal("ReadOwnedRegularFile(fifo) blocked on FIFO without a writer")
	}
}

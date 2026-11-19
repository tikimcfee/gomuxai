package termd_test

import (
	"bufio"
	"encoding/json"
	"errors"
	"net"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/termd"
	"github.com/tikimcfee/gomuxai/internal/termdwire"
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

func TestValidateNamespace(t *testing.T) {
	for _, valid := range []string{"agents", "agents-dev", "a1.b_c-2"} {
		if err := termd.ValidateNamespace(valid); err != nil {
			t.Errorf("ValidateNamespace(%q) = %v, want nil", valid, err)
		}
	}
	for _, reserved := range []string{"default", "wall"} {
		err := termd.ValidateNamespace(reserved)
		if err == nil || !strings.Contains(err.Error(), "reserved") {
			t.Errorf("ValidateNamespace(%q) = %v, want reserved refusal", reserved, err)
		}
	}
	for _, bad := range []string{"", "has space", "slash/ns", "../up", strings.Repeat("a", 65)} {
		if err := termd.ValidateNamespace(bad); err == nil {
			t.Errorf("ValidateNamespace(%q) = nil, want error", bad)
		}
	}
}

func TestCheckTestServer(t *testing.T) {
	t.Setenv(termd.TestServerEnv, "")
	t.Setenv("G3D_TERMD_DIR", "")
	if err := termd.CheckTestServer(); err == nil || !strings.Contains(err.Error(), "termdtest") {
		t.Fatalf("CheckTestServer without %s = %v, want error naming termdtest", termd.TestServerEnv, err)
	}

	dir := t.TempDir()
	t.Setenv(termd.TestServerEnv, dir)
	t.Setenv("G3D_TERMD_DIR", dir)
	t.Setenv("G3D_TERMD_SOCKET", "")
	if err := termd.CheckTestServer(); err != nil {
		t.Fatalf("CheckTestServer with matching dir = %v, want nil", err)
	}

	t.Setenv("G3D_TERMD_SOCKET", dir+"/termd-wall.sock")
	if err := termd.CheckTestServer(); err == nil || !strings.Contains(err.Error(), "G3D_TERMD_SOCKET") {
		t.Fatalf("CheckTestServer with G3D_TERMD_SOCKET set = %v, want error", err)
	}
}

func TestParseSpecialKey(t *testing.T) {
	tests := []struct {
		in   string
		code string
		key  string
		mods []string
		text string
	}{
		{"Enter", "Enter", "Enter", nil, ""},
		{"Escape", "Escape", "Escape", nil, ""},
		{"Tab", "Tab", "Tab", nil, ""},
		{"BTab", "Tab", "Tab", []string{"Shift"}, ""},
		{"BSpace", "Backspace", "Backspace", nil, ""},
		{"Space", "Space", " ", nil, " "},
		{"Up", "ArrowUp", "ArrowUp", nil, ""},
		{"Down", "ArrowDown", "ArrowDown", nil, ""},
		{"Left", "ArrowLeft", "ArrowLeft", nil, ""},
		{"Right", "ArrowRight", "ArrowRight", nil, ""},
		{"PgUp", "PageUp", "PageUp", nil, ""},
		{"PPage", "PageUp", "PageUp", nil, ""},
		{"PgDn", "PageDown", "PageDown", nil, ""},
		{"NPage", "PageDown", "PageDown", nil, ""},
		{"DC", "Delete", "Delete", nil, ""},
		{"IC", "Insert", "Insert", nil, ""},
		{"Home", "Home", "Home", nil, ""},
		{"End", "End", "End", nil, ""},
		{"F5", "F5", "F5", nil, ""},
		{"C-c", "KeyC", "c", []string{"Control"}, "c"},
		{"c-d", "KeyD", "d", []string{"Control"}, "d"},
		{"M-b", "KeyB", "b", []string{"Alt"}, "b"},
		{"shift+enter", "Enter", "Enter", []string{"Shift"}, ""},
		{"ctrl+c", "KeyC", "c", []string{"Control"}, "c"},
		{"alt+left", "ArrowLeft", "ArrowLeft", []string{"Alt"}, ""},
		{"1", "Digit1", "1", nil, "1"},
		{"!", "Digit1", "!", []string{"Shift"}, "!"},
		{"A", "KeyA", "A", []string{"Shift"}, "A"},
		{"x", "KeyX", "x", nil, "x"},
		{"+", "Equal", "+", []string{"Shift"}, "+"},
		{"ctrl++", "Equal", "+", []string{"Control", "Shift"}, "+"},
	}
	for _, tt := range tests {
		ev, ok := termd.ParseSpecialKey(tt.in)
		if !ok {
			t.Errorf("ParseSpecialKey(%q): ok = false, want true", tt.in)
			continue
		}
		if ev.Code != tt.code || ev.Key != tt.key || !slices.Equal(ev.Mods, tt.mods) || ev.Text != tt.text {
			t.Errorf("ParseSpecialKey(%q) = (%q, %q, %v, %q), want (%q, %q, %v, %q)",
				tt.in, ev.Code, ev.Key, ev.Mods, ev.Text, tt.code, tt.key, tt.mods, tt.text)
		}
	}

	for _, literal := range []string{"-N2", "not-a-key", ""} {
		if _, ok := termd.ParseSpecialKey(literal); ok {
			t.Errorf("ParseSpecialKey(%q): ok = true, want false (literal fallback)", literal)
		}
	}
}

func TestCallerEnv(t *testing.T) {
	got := termd.CallerEnv([]string{
		"FOO=bar=baz",
		"EMPTY=",
		"G3D_BIN=/usr/local/bin/g3d",
		"GOMUXAI_TERMD_NS=agents",
		"BASH_FUNC_fn%%=() { :; }",
		"BAD-NAME=1",
		"INVALID_UTF8=\xff",
		"NUL_VAL=abc\x00def",
		"HUGE=" + strings.Repeat("x", 130*1024),
		"G3D_RELAY_TOKEN=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
		"G3D_RELAY_TOKEN_DIR=/tmp/relay-tokens",
		"G3D_RELAY_PORT=8080",
		"G3D_TERMD_DIR=/tmp/g3d-501",
		"G3D_TERMD_SOCKET=/tmp/g3d-501/termd-wall.sock",
		"G3D_TERMD_ID=pane-1",
		"G3D_TERMD_CONFIG=/tmp/termd.toml",
		"SHLVL=3",
		"OLDPWD=/tmp/prev",
		"PWD=/tmp/curr",
		"_=/usr/bin/env",
	})
	if got["FOO"] != "bar=baz" {
		t.Errorf("FOO = %q, want bar=baz", got["FOO"])
	}
	if v, ok := got["EMPTY"]; !ok || v != "" {
		t.Errorf("EMPTY = %q, %v; want empty present", v, ok)
	}
	if got["G3D_BIN"] != "/usr/local/bin/g3d" {
		t.Errorf("G3D_BIN = %q, want /usr/local/bin/g3d", got["G3D_BIN"])
	}
	if got["GOMUXAI_TERMD_NS"] != "agents" {
		t.Errorf("GOMUXAI_TERMD_NS = %q, want agents", got["GOMUXAI_TERMD_NS"])
	}
	for _, rejected := range []string{
		"BASH_FUNC_fn%%",
		"BAD-NAME",
		"INVALID_UTF8",
		"NUL_VAL",
		"HUGE",
		"G3D_RELAY_TOKEN",
		"G3D_RELAY_TOKEN_DIR",
		"G3D_RELAY_PORT",
		"G3D_TERMD_DIR",
		"G3D_TERMD_SOCKET",
		"G3D_TERMD_ID",
		"G3D_TERMD_CONFIG",
		"SHLVL",
		"OLDPWD",
		"PWD",
		"_",
	} {
		if _, ok := got[rejected]; ok {
			t.Errorf("%s should be excluded from CallerEnv", rejected)
		}
	}
}

func TestCreateSessionScrubsRelayAndTermdVarsFromWire(t *testing.T) {
	dir := shortTempDir(t, "gmxt-env-")
	if err := os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	const ns = "agents"
	sockPath := filepath.Join(dir, "termd-"+ns+".sock")
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() })

	createCh := make(chan map[string]any, 1)
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		r := bufio.NewReader(conn)
		f, err := termdwire.ReadFrame(r)
		if err != nil || f.Type != termdwire.FrameHello {
			return
		}
		welcome, _ := json.Marshal(map[string]any{
			"proto":     []int{1, 5},
			"daemon":    "g3d-termd/mock",
			"engine":    "mock",
			"ns":        ns,
			"pid":       os.Getpid(),
			"features":  []string{"status"},
			"max_frame": termdwire.DefaultMaxFrame,
			"instance":  "inst-mock",
		})
		if _, err := conn.Write(termdwire.EncodeFrame(termdwire.FrameWelcome, 0, welcome)); err != nil {
			return
		}
		reqFrame, err := termdwire.ReadFrame(r)
		if err != nil || reqFrame.Type != termdwire.FrameRequest {
			return
		}
		var req map[string]any
		_ = json.Unmarshal(reqFrame.Payload, &req)
		createCh <- req
		reply, _ := json.Marshal(map[string]any{"req": req["req"], "ok": true})
		_, _ = conn.Write(termdwire.EncodeFrame(termdwire.FrameReply, 0, reply))
	}()

	fakeBin := filepath.Join(t.TempDir(), "fake-g3d-termd")
	if err := os.WriteFile(fakeBin, []byte("#!/bin/sh\nexit 1\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("G3D_TERMD_DIR", dir)
	t.Setenv(termd.TestServerEnv, dir)
	t.Setenv("G3D_TERMD_SOCKET", "")
	t.Setenv("G3D_TERMD_ID", "caller-pane")
	t.Setenv("G3D_RELAY_TOKEN", "secret-capability-token")
	t.Setenv("G3D_RELAY_TOKEN_DIR", "/tmp/secret-token-dir")
	t.Setenv("SHLVL", "4")
	t.Setenv("OLDPWD", "/tmp/old")

	c, err := termd.NewClient(termd.Options{Namespace: ns, DaemonBin: fakeBin})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	err = c.CreateSession("worker", map[string]string{
		"AGENT_NAME":          "worker",
		"G3D_RELAY_TOKEN":     "req-leak",
		"G3D_RELAY_TOKEN_DIR": "/req-leak",
		"G3D_TERMD_DIR":       "/req-leak",
		"G3D_TERMD_CONFIG":    "/req-leak",
		"G3D_TERMD_SOCKET":    "/req-leak",
		"G3D_TERMD_ID":        "req-leak",
		"PWD":                 "/req-leak",
	})
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}

	req := <-createCh
	envBase, _ := req["env_base"].(map[string]any)
	for _, forbidden := range []string{
		"G3D_RELAY_TOKEN",
		"G3D_RELAY_TOKEN_DIR",
		"G3D_TERMD_DIR",
		"G3D_TERMD_SOCKET",
		"G3D_TERMD_ID",
		"SHLVL",
		"OLDPWD",
		"PWD",
		"_",
	} {
		if v, ok := envBase[forbidden]; ok {
			t.Errorf("create.env_base leaked %s=%v", forbidden, v)
		}
	}
	reqEnv, _ := req["env"].(map[string]any)
	if reqEnv["AGENT_NAME"] != "worker" {
		t.Errorf("create.env[AGENT_NAME] = %v, want worker", reqEnv["AGENT_NAME"])
	}
	for _, forbidden := range []string{
		"G3D_RELAY_TOKEN",
		"G3D_RELAY_TOKEN_DIR",
		"G3D_TERMD_DIR",
		"G3D_TERMD_CONFIG",
		"G3D_TERMD_SOCKET",
		"G3D_TERMD_ID",
		"PWD",
	} {
		if v, ok := reqEnv[forbidden]; ok {
			t.Errorf("create.env leaked %s=%v", forbidden, v)
		}
	}
}

func startImpostorListener(t *testing.T, sockPath string) <-chan struct{} {
	t.Helper()
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() })
	accepted := make(chan struct{}, 1)
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		_ = conn.Close()
		select {
		case accepted <- struct{}{}:
		default:
		}
	}()
	return accepted
}

func TestSocketTrust_FailsClosedOnUnsafeDirAndSymlink(t *testing.T) {
	const ns = "agents"
	binDir := t.TempDir()
	spawnMarker := filepath.Join(binDir, "daemon-spawned")
	fakeBin := filepath.Join(binDir, "fake-g3d-termd")
	if err := os.WriteFile(fakeBin, []byte("#!/bin/sh\ntouch '"+spawnMarker+"'\nexit 0\n"), 0o700); err != nil {
		t.Fatal(err)
	}

	assertFailsClosed := func(t *testing.T, sockDir string) {
		t.Helper()
		t.Setenv("G3D_TERMD_DIR", sockDir)
		t.Setenv(termd.TestServerEnv, sockDir)
		t.Setenv("G3D_TERMD_SOCKET", "")

		c, err := termd.NewClient(termd.Options{Namespace: ns, DaemonBin: fakeBin})
		if err != nil {
			t.Fatalf("NewClient(%q): %v", sockDir, err)
		}

		st, err := c.Status()
		if !errors.Is(err, termd.ErrUnsafeSocket) {
			t.Fatalf("Status(%q) = (%+v, %v), want error wrapping ErrUnsafeSocket", sockDir, st, err)
		}
		if st != nil {
			t.Fatalf("Status(%q) returned non-nil StatusDoc %+v alongside error", sockDir, st)
		}

		start := time.Now()
		err = c.CreateSession("sess", nil)
		if !errors.Is(err, termd.ErrUnsafeSocket) {
			t.Fatalf("CreateSession(%q) = %v, want error wrapping ErrUnsafeSocket", sockDir, err)
		}
		if elapsed := time.Since(start); elapsed > time.Second {
			t.Fatalf("CreateSession(%q) took %s instead of failing closed immediately", sockDir, elapsed)
		}
		if _, statErr := os.Stat(spawnMarker); statErr == nil {
			t.Fatalf("CreateSession(%q) spawned daemon despite unsafe socket directory", sockDir)
		}

		sockPath := filepath.Join(sockDir, "termd-"+ns+".sock")
		if err := termd.StopSocket(sockPath); !errors.Is(err, termd.ErrUnsafeSocket) {
			t.Fatalf("StopSocket(%q) = %v, want error wrapping ErrUnsafeSocket", sockPath, err)
		}
	}

	t.Run("mode_0755", func(t *testing.T) {
		dir := shortTempDir(t, "gmxt755-")
		accepted := startImpostorListener(t, filepath.Join(dir, "termd-"+ns+".sock"))
		if err := os.Chmod(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		assertFailsClosed(t, dir)
		select {
		case <-accepted:
			t.Fatal("connected to impostor socket inside 0755 directory")
		default:
		}
	})

	t.Run("symlink_and_dangling", func(t *testing.T) {
		realDir := shortTempDir(t, "gmxtreal-")
		if err := os.Chmod(realDir, 0o700); err != nil {
			t.Fatal(err)
		}
		accepted := startImpostorListener(t, filepath.Join(realDir, "termd-"+ns+".sock"))

		parent := shortTempDir(t, "gmxtlnk-")
		linkDir := filepath.Join(parent, "socklink")
		if err := os.Symlink(realDir, linkDir); err != nil {
			t.Fatal(err)
		}
		danglingDir := filepath.Join(parent, "dangling")
		if err := os.Symlink(filepath.Join(parent, "missing"), danglingDir); err != nil {
			t.Fatal(err)
		}

		for _, candidate := range []string{linkDir, linkDir + "/", danglingDir} {
			assertFailsClosed(t, candidate)
		}
		select {
		case <-accepted:
			t.Fatal("connected to impostor socket through symlinked G3D_TERMD_DIR")
		default:
		}
	})
}

func TestResolveTerminalCwd(t *testing.T) {
	dir := t.TempDir()
	// macOS zsh OSC 7 reports file://<host>/<escaped-path>.
	osc7 := "file://some-host" + strings.ReplaceAll(dir, " ", "%20")
	if got := termd.ResolveTerminalCwd(osc7, "/tmp"); got != dir {
		t.Errorf("ResolveTerminalCwd(%q) = %q, want %q", osc7, got, dir)
	}
	if got := termd.ResolveTerminalCwd("file://some-host/nonexistent/path/xyz", dir); got != dir {
		t.Errorf("fallback on nonexistent OSC 7 = %q, want %q", got, dir)
	}
}

func TestNewClient_LazyDaemonBinary(t *testing.T) {
	dir := shortTempDir(t, "gmxt-lazy-")
	if err := os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("G3D_TERMD_DIR", dir)
	t.Setenv(termd.TestServerEnv, dir)
	t.Setenv("G3D_TERMD_SOCKET", "")
	t.Setenv("G3D_TERMD_BIN", "/nonexistent/g3d-termd")

	// NewClient and read-only Status succeed without resolving DaemonBinary.
	c, err := termd.NewClient(termd.Options{Namespace: "agents"})
	if err != nil {
		t.Fatalf("NewClient should not resolve DaemonBinary eagerly: %v", err)
	}
	st, err := c.Status()
	if err != nil || len(st.Terminals) != 0 {
		t.Fatalf("Status with no daemon = (%+v, %v), want empty nil", st, err)
	}
	if err := c.CreateSession("w", nil); err == nil || !strings.Contains(err.Error(), "G3D_TERMD_BIN") {
		t.Fatalf("CreateSession with bad G3D_TERMD_BIN = %v, want DaemonBinary error", err)
	}
}

func TestSessionEnv_PrefersRequestAndRedialsOnBusy(t *testing.T) {
	dir := shortTempDir(t, "gmxt-senv-")
	if err := os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	const ns = "agents"
	sockPath := filepath.Join(dir, "termd-"+ns+".sock")
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() })

	go func() {
		// Attempt 1: simulate transient daemon busy/restarting frame on handshake.
		if c1, err := ln.Accept(); err == nil {
			r1 := bufio.NewReader(c1)
			_, _ = termdwire.ReadFrame(r1)
			busy, _ := json.Marshal(map[string]any{"code": "restarting", "message": "brain restarting"})
			_, _ = c1.Write(termdwire.EncodeFrame(termdwire.FrameError, 0, busy))
			_ = c1.Close()
		}
		// Attempt 2: serve WELCOME and op "env" with base PATH before request PATH.
		c2, err := ln.Accept()
		if err != nil {
			return
		}
		defer c2.Close()
		r2 := bufio.NewReader(c2)
		if _, err := termdwire.ReadFrame(r2); err != nil {
			return
		}
		welcome, _ := json.Marshal(map[string]any{
			"proto":     []int{1, 5},
			"daemon":    "g3d-termd/mock",
			"engine":    "mock",
			"ns":        ns,
			"pid":       os.Getpid(),
			"features":  []string{"status", "env"},
			"max_frame": termdwire.DefaultMaxFrame,
			"instance":  "inst-mock",
		})
		_, _ = c2.Write(termdwire.EncodeFrame(termdwire.FrameWelcome, 0, welcome))
		reqFrame, err := termdwire.ReadFrame(r2)
		if err != nil {
			return
		}
		var req map[string]any
		_ = json.Unmarshal(reqFrame.Payload, &req)
		reply, _ := json.Marshal(map[string]any{
			"req": req["req"],
			"ok":  true,
			"env": []map[string]string{
				{"name": "PATH", "value": "/usr/bin:/bin", "source": "caller"},
				{"name": "BASE_ONLY", "value": "from-base", "source": "caller"},
				{"name": "PATH", "value": "/opt/worktree/bin:/usr/bin:/bin", "source": "request"},
			},
		})
		_, _ = c2.Write(termdwire.EncodeFrame(termdwire.FrameReply, 0, reply))
	}()

	t.Setenv("G3D_TERMD_DIR", dir)
	t.Setenv(termd.TestServerEnv, dir)
	t.Setenv("G3D_TERMD_SOCKET", "")

	c, err := termd.NewClient(termd.Options{Namespace: ns})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	if got := c.SessionEnv("agent-x", "PATH"); got != "/opt/worktree/bin:/usr/bin:/bin" {
		t.Errorf("SessionEnv(PATH) = %q, want /opt/worktree/bin:/usr/bin:/bin", got)
	}
}

func TestRespawnPane_PreservesBaseAndRequestEnv(t *testing.T) {
	dir := shortTempDir(t, "gmxt-resp-")
	if err := os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	const ns = "agents"
	sockPath := filepath.Join(dir, "termd-"+ns+".sock")
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() })

	workDir := t.TempDir()
	createCh := make(chan map[string]any, 1)
	go func() {
		for range 4 {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			r := bufio.NewReader(conn)
			if _, err := termdwire.ReadFrame(r); err != nil {
				_ = conn.Close()
				continue
			}
			welcome, _ := json.Marshal(map[string]any{
				"proto":     []int{1, 5},
				"daemon":    "g3d-termd/mock",
				"engine":    "mock",
				"ns":        ns,
				"pid":       os.Getpid(),
				"features":  []string{"status", "env"},
				"max_frame": termdwire.DefaultMaxFrame,
				"instance":  "inst-mock",
			})
			_, _ = conn.Write(termdwire.EncodeFrame(termdwire.FrameWelcome, 0, welcome))
			reqFrame, err := termdwire.ReadFrame(r)
			if err != nil {
				_ = conn.Close()
				continue
			}
			var req map[string]any
			_ = json.Unmarshal(reqFrame.Payload, &req)
			var reply any
			switch req["op"] {
			case "status":
				reply = map[string]any{
					"req": req["req"],
					"ok":  true,
					"status": map[string]any{
						"daemon": map[string]any{"ns": ns},
						"terminals": []map[string]any{
							{"id": "gomux-bus", "handle": 1, "pid": 4321, "cwd": workDir},
						},
					},
				}
			case "env":
				reply = map[string]any{
					"req": req["req"],
					"ok":  true,
					"env": []map[string]string{
						{"name": "AQUA_CONTEXT", "value": "kept-from-gui", "source": "caller"},
						{"name": "AGENT_HOST", "value": "laptop-2", "source": "request"},
						{"name": "G3D_TERMD_ID", "value": "gomux-bus", "source": "termd"},
					},
				}
			case "kill":
				reply = map[string]any{"req": req["req"], "ok": true}
			case "create":
				createCh <- req
				reply = map[string]any{"req": req["req"], "ok": true}
			}
			raw, _ := json.Marshal(reply)
			_, _ = conn.Write(termdwire.EncodeFrame(termdwire.FrameReply, 0, raw))
			_ = conn.Close()
		}
	}()

	t.Setenv("G3D_TERMD_DIR", dir)
	t.Setenv(termd.TestServerEnv, dir)
	t.Setenv("G3D_TERMD_SOCKET", "")
	t.Setenv("SSH_ONLY_VAR", "from-ssh-caller")

	c, err := termd.NewClient(termd.Options{Namespace: ns})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	if err := c.RespawnPane("gomux-bus"); err != nil {
		t.Fatalf("RespawnPane: %v", err)
	}

	created := <-createCh
	envBase, _ := created["env_base"].(map[string]any)
	if envBase["AQUA_CONTEXT"] != "kept-from-gui" {
		t.Errorf("env_base[AQUA_CONTEXT] = %v, want kept-from-gui", envBase["AQUA_CONTEXT"])
	}
	if _, leaked := envBase["SSH_ONLY_VAR"]; leaked {
		t.Errorf("env_base replaced with SSH caller's environment: %v", envBase)
	}
	if _, leaked := envBase["G3D_TERMD_ID"]; leaked {
		t.Errorf("env_base leaked layer-6 G3D_TERMD_ID: %v", envBase)
	}
	reqEnv, _ := created["env"].(map[string]any)
	if reqEnv["AGENT_HOST"] != "laptop-2" {
		t.Errorf("create.env[AGENT_HOST] = %v, want laptop-2", reqEnv["AGENT_HOST"])
	}
}

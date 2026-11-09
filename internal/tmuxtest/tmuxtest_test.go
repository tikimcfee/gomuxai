package tmuxtest

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/tmux"
)

func TestMain(m *testing.M) { os.Exit(Run(m)) }

// Sessions this binary creates are on its own server, in its socket
// directory, whatever server the environment it started in named.
func TestRun_SessionsAreOnThePrivateServer(t *testing.T) {
	if _, err := exec.LookPath("tmux"); err != nil {
		t.Skip("tmux not available")
	}
	Require(t)
	dir := os.Getenv(tmux.TestServerEnv)
	if os.Getenv("TMUX") != "" || os.Getenv("TMUX_PANE") != "" || os.Getenv("TMUX_TMPDIR") != dir {
		t.Fatalf("env: TMUX=%q TMUX_PANE=%q TMUX_TMPDIR=%q, want TMUX and TMUX_PANE unset and TMUX_TMPDIR %q",
			os.Getenv("TMUX"), os.Getenv("TMUX_PANE"), os.Getenv("TMUX_TMPDIR"), dir)
	}
	c, err := tmux.NewClient()
	if err != nil {
		t.Fatal(err)
	}
	if err := c.CreateSession("tmuxtest-probe", nil); err != nil {
		t.Fatal(err)
	}
	defer c.KillSession("tmuxtest-probe")
	out, err := exec.Command("tmux", "display-message", "-p", "-t", "tmuxtest-probe", "#{socket_path}").Output()
	if err != nil {
		t.Fatal(err)
	}
	// On macOS the temp directory is under /var, a link to /private/var.
	want, err := filepath.EvalSymlinks(socketPath(dir))
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.TrimSpace(string(out)); got != want {
		t.Errorf("session's server socket = %q, want %q", got, want)
	}
}

// Killing the server's only session leaves the server running, as the
// shared one always is: tests that kill a session never race its shutdown.
func TestRun_ServerOutlivesItsLastSession(t *testing.T) {
	if _, err := exec.LookPath("tmux"); err != nil {
		t.Skip("tmux not available")
	}
	Require(t)
	c, err := tmux.NewClient()
	if err != nil {
		t.Fatal(err)
	}
	if err := c.CreateSession("tmuxtest-last", nil); err != nil {
		t.Fatal(err)
	}
	if err := c.KillSession("tmuxtest-last"); err != nil {
		t.Fatal(err)
	}
	// An empty server answers has-session with "no current target", not
	// "can't find session": still a session that doesn't exist.
	if exists, err := c.SessionExists("tmuxtest-last"); err != nil || exists {
		t.Errorf("SessionExists after killing the last session = %v, %v; want false, nil", exists, err)
	}
	sessions, err := c.ListSessions()
	if err != nil {
		t.Fatalf("listing sessions after killing the last one: %v", err)
	}
	if len(sessions) != 0 {
		t.Errorf("sessions = %+v, want none (and no keepalive session)", sessions)
	}
	if out, err := exec.Command("tmux", "show-options", "-g", "exit-empty").CombinedOutput(); err != nil ||
		strings.TrimSpace(string(out)) != "exit-empty off" {
		t.Errorf("server after its last session: %q, %v; want it running with exit-empty off", out, err)
	}
}

// Require fails a test that would run tmux off its private server.
func TestRequire_FailsOffPrivateServer(t *testing.T) {
	t.Setenv(tmux.TestServerEnv, "")
	if err := tmux.CheckTestServer(); err == nil || !strings.Contains(err.Error(), "tmuxtest.Run") {
		t.Errorf("CheckTestServer = %v, want it to point at tmuxtest.Run", err)
	}
}

func TestSocketDir_FitsATmuxSocket(t *testing.T) {
	dir, err := socketDir()
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(dir)
	if n := len(socketPath(dir)); n > maxSocketPath {
		t.Errorf("socket path is %d bytes, over %d", n, maxSocketPath)
	}
}

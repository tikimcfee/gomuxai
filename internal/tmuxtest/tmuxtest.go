// Package tmuxtest runs a test binary on a private tmux server of its own.
//
// Tests that create tmux sessions would otherwise do it on whatever server
// the environment names: the developer's own, where the real agents and the
// bus run. Two test runs at once then collide on session names, and a
// cleanup's kill-server can take the real server down. Inside a tmux pane it
// is worse than it looks: tmux uses $TMUX's socket ahead of TMUX_TMPDIR, so
// "TMUX_TMPDIR=$dir tmux kill-server" run there kills the server the pane
// lives in.
//
// A package whose tests run tmux calls Run from its TestMain:
//
//	func TestMain(m *testing.M) { os.Exit(tmuxtest.Run(m)) }
//
// internal/tmux refuses to run tmux in a test binary that hasn't, so a
// package that forgets fails loudly instead of reaching the real server.
package tmuxtest

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// maxSocketPath is the longest socket path tmux can bind on every platform
// we run on (sun_path is 104 bytes on macOS, 108 on Linux, including the
// terminating NUL). A longer one fails with "File name too long".
const maxSocketPath = 103

// Run runs m's tests on a private tmux server and returns their exit code:
// it makes a short socket directory, unsets TMUX and TMUX_PANE and points
// TMUX_TMPDIR at the directory for the whole process (so every tmux the code
// and its children run uses that server), and after the tests kills that
// server, by its socket, and removes the directory.
func Run(m *testing.M) int {
	dir, err := socketDir()
	if err != nil {
		fmt.Fprintln(os.Stderr, "tmuxtest:", err)
		return 1
	}
	defer os.RemoveAll(dir)
	// TMUX_PANE goes too: the CLI reads it to find the agent it runs in,
	// and a test must not depend on the developer's own pane.
	for _, k := range []string{"TMUX", "TMUX_PANE"} {
		if err := os.Unsetenv(k); err != nil {
			fmt.Fprintln(os.Stderr, "tmuxtest:", err)
			return 1
		}
	}
	for k, v := range map[string]string{"TMUX_TMPDIR": dir, tmux.TestServerEnv: dir} {
		if err := os.Setenv(k, v); err != nil {
			fmt.Fprintln(os.Stderr, "tmuxtest:", err)
			return 1
		}
	}
	defer killServer(dir)
	return m.Run()
}

// Require fails t unless this process runs tmux on its private server
// (Run). Call it before a test runs tmux itself, rather than through
// internal/tmux, which checks on every command.
func Require(t testing.TB) {
	t.Helper()
	if err := tmux.CheckTestServer(); err != nil {
		t.Fatal(err)
	}
}

// socketDir makes the private server's socket directory: under the temp
// directory if its socket path fits (macOS's $TMPDIR does), else under /tmp.
func socketDir() (string, error) {
	for _, parent := range []string{os.TempDir(), "/tmp"} {
		dir, err := os.MkdirTemp(parent, "gmx")
		if err != nil {
			continue
		}
		if len(socketPath(dir)) <= maxSocketPath {
			return dir, nil
		}
		os.RemoveAll(dir)
	}
	return "", fmt.Errorf("no temp directory short enough for a tmux socket (at most %d bytes): set TMPDIR to a shorter one", maxSocketPath)
}

// socketPath is where tmux puts the default server's socket with
// TMUX_TMPDIR=dir.
func socketPath(dir string) string {
	return filepath.Join(dir, fmt.Sprintf("tmux-%d", os.Getuid()), "default")
}

// killServer kills the private server in dir, named by its socket and with
// TMUX left out of tmux's environment, so nothing but that server can be hit.
func killServer(dir string) {
	sock := socketPath(dir)
	if _, err := os.Stat(sock); err != nil {
		return // no test started a server
	}
	cmd := exec.Command("tmux", "-S", sock, "kill-server")
	for _, kv := range os.Environ() {
		if !strings.HasPrefix(kv, "TMUX=") {
			cmd.Env = append(cmd.Env, kv)
		}
	}
	_ = cmd.Run() // a server that already exited is fine
}

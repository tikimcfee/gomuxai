package tmux

import (
	"errors"
	"fmt"
	"os"
	"testing"
)

// TestServerEnv names the socket directory of the private tmux server a test
// binary runs on. internal/tmuxtest sets it, with TMUX_TMPDIR, in a
// package's TestMain.
const TestServerEnv = "GOMUXAI_TEST_TMUX_TMPDIR"

// CheckTestServer says why this process must not run tmux: it is a test
// binary that isn't on its own private tmux server. Tests would otherwise
// create and kill sessions on the server the environment points at, the
// developer's own, where the real agents and the bus run; and inside a tmux
// pane $TMUX names that server ahead of TMUX_TMPDIR, so a kill-server meant
// for a test server takes the real one down. Outside a test binary it is
// always nil.
func CheckTestServer() error {
	if !testing.Testing() {
		return nil
	}
	dir := os.Getenv(TestServerEnv)
	switch {
	case dir == "":
		return errors.New("a test ran tmux without a private tmux server: " +
			"call tmuxtest.Run from this package's TestMain (internal/tmuxtest)")
	case os.Getenv("TMUX") != "":
		return fmt.Errorf("a test ran tmux with $TMUX set (%s), which tmux uses ahead of the private server in %s: "+
			"don't set TMUX in a test that runs tmux", os.Getenv("TMUX"), dir)
	case os.Getenv("TMUX_TMPDIR") != dir:
		return fmt.Errorf("a test ran tmux with TMUX_TMPDIR %q, not its private server's %q: "+
			"don't change TMUX_TMPDIR in a test that runs tmux", os.Getenv("TMUX_TMPDIR"), dir)
	}
	return nil
}

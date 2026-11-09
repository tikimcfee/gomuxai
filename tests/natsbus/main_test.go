package natsbus_test

import (
	"os"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/tmuxtest"
)

// TestMain runs the package's tests on a private tmux server, never the one
// the environment names (internal/tmuxtest).
func TestMain(m *testing.M) { os.Exit(tmuxtest.Run(m)) }

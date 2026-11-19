package watcher_test

import (
	"os"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/termdtest"
)

// TestMain isolates the package's tests in a private g3d-termd socket
// directory (internal/termdtest).
func TestMain(m *testing.M) { os.Exit(termdtest.Run(m)) }

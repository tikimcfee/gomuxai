package cli

import (
	"os"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/termdtest"
)

// TestMain runs the package's tests in UTC, so times print the same on
// every machine, and against an isolated g3d-termd socket directory, never
// the user's real sockets (internal/termdtest). TZ is set before anything
// loads the local zone, instead of tests swapping time.Local as they run:
// that raced with goroutines still reading it, such as a test bus's
// nats-server, which reads file times until the process exits.
func TestMain(m *testing.M) {
	os.Setenv("TZ", "UTC")
	os.Exit(termdtest.Run(m))
}

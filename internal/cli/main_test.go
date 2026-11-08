package cli

import (
	"os"
	"testing"
)

// TestMain runs the package's tests in UTC, so times print the same on
// every machine. TZ is set before anything loads the local zone, instead of
// tests swapping time.Local as they run: that raced with goroutines still
// reading it, such as a test bus's nats-server, which reads file times
// until the process exits.
func TestMain(m *testing.M) {
	os.Setenv("TZ", "UTC")
	os.Exit(m.Run())
}

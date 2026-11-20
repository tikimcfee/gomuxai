package await

import (
	"fmt"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestPoll_ReturnsOnceMet(t *testing.T) {
	var n atomic.Int32
	state, ok := Poll(time.Second, func() (string, bool) {
		v := n.Add(1)
		return fmt.Sprint(v), v == 3
	})
	if !ok || state != "3" {
		t.Errorf("Poll = %q, %v; want 3, true", state, ok)
	}
}

func TestPoll_TimesOutWithTheLastState(t *testing.T) {
	var n atomic.Int32
	state, ok := Poll(50*time.Millisecond, func() (string, bool) {
		return fmt.Sprint(n.Add(1)), false
	})
	if ok || state != fmt.Sprint(n.Load()) {
		t.Errorf("Poll = %q, %v; want the last state (%d), false", state, ok, n.Load())
	}
}

// fakeT records a failure instead of ending the test.
type fakeT struct {
	testing.TB
	msg string
}

func (f *fakeT) Helper() {}
func (f *fakeT) Fatalf(format string, args ...any) {
	f.msg = fmt.Sprintf(format, args...)
}

func TestWithin_FailsSayingWhatAndTheLastState(t *testing.T) {
	ft := &fakeT{TB: t}
	Within(ft, 30*time.Millisecond, "the shell's prompt", func() (string, bool) { return "screen: $ ", false })
	if !strings.Contains(ft.msg, "waiting for the shell's prompt") || !strings.Contains(ft.msg, "screen: $ ") {
		t.Errorf("failure = %q; want what was awaited and the last state", ft.msg)
	}
}

package termd

import (
	"errors"
	"io"
	"syscall"
	"time"

	"github.com/tikimcfee/gomuxai/internal/termdwire"
)

// WireError is an error returned by g3d-termd in a REPLY or ERROR frame.
type WireError = termdwire.WireError

var (
	errNotRunning = termdwire.ErrNotRunning

	// ErrUnsafeSocket indicates that the termd socket directory or peer
	// credentials failed ownership, permission, or file-type verification.
	ErrUnsafeSocket = termdwire.ErrUnsafeSocket

	isWireCode = termdwire.IsCode

	dialOpts = termdwire.DialOptions{
		Client: "gomuxai",
		Proto:  [2]int{1, 5},
	}

	readRetryDelays = []time.Duration{
		25 * time.Millisecond,
		50 * time.Millisecond,
		100 * time.Millisecond,
	}
)

func dialNamespace(ns string) (*termdwire.Conn, error) {
	if err := CheckTestServer(); err != nil {
		return nil, err
	}
	if err := ValidateNamespace(ns); err != nil {
		return nil, err
	}
	return termdwire.Dial(ns, dialOpts)
}

func isTransientReadErr(err error) bool {
	if err == nil || errors.Is(err, errNotRunning) || errors.Is(err, ErrUnsafeSocket) {
		return false
	}
	for _, code := range []string{"restart", "restarting", "busy"} {
		if isWireCode(err, code) {
			return true
		}
	}
	return errors.Is(err, io.EOF) ||
		errors.Is(err, io.ErrUnexpectedEOF) ||
		errors.Is(err, syscall.ECONNRESET) ||
		errors.Is(err, syscall.EPIPE)
}

// withReadConn runs a read-only op against ns, briefly redialing if the daemon
// reports a transient brain restart or busy state.
func withReadConn(ns string, fn func(w *termdwire.Conn) error) error {
	for attempt := 0; ; attempt++ {
		w, err := dialNamespace(ns)
		if err == nil {
			err = fn(w)
			_ = w.Close()
		}
		if err == nil || attempt >= len(readRetryDelays) || !isTransientReadErr(err) {
			return err
		}
		time.Sleep(readRetryDelays[attempt])
	}
}

func ensureDaemon(ns, daemonBin string) (*termdwire.Conn, error) {
	c, err := dialNamespace(ns)
	if err == nil || !errors.Is(err, errNotRunning) {
		return c, err
	}
	bin, err := DaemonBinary(daemonBin)
	if err != nil {
		return nil, err
	}
	return termdwire.EnsureDaemon(ns, bin, dialOpts)
}

// StopSocket stops a g3d-termd daemon listening at sockPath, if any.
func StopSocket(sockPath string) error {
	return termdwire.StopSocket(sockPath, dialOpts)
}

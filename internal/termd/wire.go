package termd

import (
	"errors"

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

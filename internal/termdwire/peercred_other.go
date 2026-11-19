//go:build !unix

package termdwire

import (
	"fmt"
	"net"
	"runtime"
)

func errUnsupportedPlatform() error {
	return fmt.Errorf("%w: termd is unsupported on %s", ErrUnsafeSocket, runtime.GOOS)
}

// UserRuntimeDir returns "" on non-Unix platforms.
func UserRuntimeDir() string {
	return ""
}

// CheckSocketDir fails closed on non-Unix platforms.
func CheckSocketDir(dir string) error {
	return errUnsupportedPlatform()
}

// VerifyPeerCreds fails closed on non-Unix platforms.
func VerifyPeerCreds(conn net.Conn) error {
	return errUnsupportedPlatform()
}

// VerifyPeerCredsForUID fails closed on non-Unix platforms.
func VerifyPeerCredsForUID(conn net.Conn, wantUID uint32) error {
	return errUnsupportedPlatform()
}

// ReadOwnedRegularFile fails closed on non-Unix platforms.
func ReadOwnedRegularFile(path string, maxBytes int64) ([]byte, error) {
	return nil, errUnsupportedPlatform()
}

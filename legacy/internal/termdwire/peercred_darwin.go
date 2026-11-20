//go:build darwin

package termdwire

import (
	"fmt"
	"net"
	"os"

	"golang.org/x/sys/unix"
)

// VerifyPeerCreds verifies via LOCAL_PEERCRED that conn is a *net.UnixConn
// whose peer effective UID matches os.Getuid().
func VerifyPeerCreds(conn net.Conn) error {
	return VerifyPeerCredsForUID(conn, uint32(os.Getuid()))
}

// VerifyPeerCredsForUID verifies via LOCAL_PEERCRED that conn is a *net.UnixConn
// whose peer effective UID equals wantUID.
func VerifyPeerCredsForUID(conn net.Conn, wantUID uint32) error {
	uc, ok := conn.(*net.UnixConn)
	if !ok || uc == nil {
		return fmt.Errorf("%w: termd connection is %T, want *net.UnixConn", ErrUnsafeSocket, conn)
	}
	raw, err := uc.SyscallConn()
	if err != nil {
		return fmt.Errorf("%w: termd peercred syscall conn: %w", ErrUnsafeSocket, err)
	}
	var (
		cred    *unix.Xucred
		sockErr error
	)
	if ctlErr := raw.Control(func(fd uintptr) {
		cred, sockErr = unix.GetsockoptXucred(int(fd), unix.SOL_LOCAL, unix.LOCAL_PEERCRED)
	}); ctlErr != nil {
		return fmt.Errorf("%w: termd peercred control: %w", ErrUnsafeSocket, ctlErr)
	}
	if sockErr != nil {
		return fmt.Errorf("%w: termd peercred: %w", ErrUnsafeSocket, sockErr)
	}
	if cred.Uid != wantUID {
		return fmt.Errorf("%w: socket peer uid is %d, want %d", ErrUnsafeSocket, cred.Uid, wantUID)
	}
	return nil
}

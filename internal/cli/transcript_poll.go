//go:build linux || darwin

package cli

import (
	"os"

	"golang.org/x/sys/unix"
)

// readerGone reports whether the reader of f, a pipe or a socket, has
// closed its end, without writing to it: poll reports POLLERR (a pipe on
// Linux) or POLLHUP (a socket; a pipe on macOS). macOS reports either only
// when POLLOUT is asked for, so it is, though a full pipe never has room.
// A terminal or a file reports neither (macOS's /dev/null: POLLNVAL).
func readerGone(f *os.File) bool {
	rc, err := f.SyscallConn()
	if err != nil {
		return false
	}
	gone := false
	rc.Control(func(fd uintptr) {
		fds := []unix.PollFd{{Fd: int32(fd), Events: unix.POLLOUT}}
		if n, err := unix.Poll(fds, 0); err == nil && n > 0 {
			gone = fds[0].Revents&(unix.POLLERR|unix.POLLHUP) != 0
		}
	})
	return gone
}

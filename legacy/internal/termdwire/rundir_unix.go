//go:build unix

package termdwire

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"syscall"

	"golang.org/x/sys/unix"
)

// UserRuntimeDir returns /run/user/<uid> when it exists as a real directory
// (not a symlink) owned by the current uid with mode 0700 (no group/other bits),
// and "" otherwise.
func UserRuntimeDir() string {
	uid := os.Getuid()
	dir := fmt.Sprintf("/run/user/%d", uid)
	fi, err := os.Lstat(dir)
	if err != nil || fi.Mode()&os.ModeSymlink != 0 || !fi.IsDir() || fi.Mode().Perm()&0o077 != 0 {
		return ""
	}
	if st, ok := fi.Sys().(*syscall.Stat_t); !ok || int(st.Uid) != uid {
		return ""
	}
	return dir
}

// CheckSocketDir opens dir with O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC and
// verifies via fstat on the open descriptor that it is a real directory owned
// by the current uid with no group/other permission bits. Missing directories
// return an error matching os.IsNotExist(err); symlinks, wrong file types,
// foreign owners, or permissive modes wrap ErrUnsafeSocket.
func CheckSocketDir(dir string) error {
	if strings.TrimSpace(dir) == "" {
		return fmt.Errorf("%w: empty socket directory", ErrUnsafeSocket)
	}
	clean := filepath.Clean(dir)
	var (
		fd  int
		err error
	)
	for {
		fd, err = unix.Open(clean, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if err != unix.EINTR {
			break
		}
	}
	if err != nil {
		if os.IsNotExist(err) {
			return &os.PathError{Op: "open", Path: clean, Err: err}
		}
		return fmt.Errorf("%w: open socket directory %s: %v", ErrUnsafeSocket, clean, err)
	}
	defer unix.Close(fd)

	var st unix.Stat_t
	if err := unix.Fstat(fd, &st); err != nil {
		return fmt.Errorf("%w: fstat socket directory %s: %v", ErrUnsafeSocket, clean, err)
	}
	if st.Mode&unix.S_IFMT != unix.S_IFDIR {
		return fmt.Errorf("%w: socket directory %s is not a real directory (mode %#o)", ErrUnsafeSocket, clean, st.Mode)
	}
	uid := os.Getuid()
	if int(st.Uid) != uid {
		return fmt.Errorf("%w: socket directory %s is owned by uid %d, want %d", ErrUnsafeSocket, clean, st.Uid, uid)
	}
	if perm := uint32(st.Mode) & 0o777; perm&0o077 != 0 {
		return fmt.Errorf("%w: socket directory %s has unsafe mode %04o (group/other access must be 0)", ErrUnsafeSocket, clean, perm)
	}
	return nil
}

// ReadOwnedRegularFile opens path with O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC,
// verifies on the open descriptor that it is a regular file owned by the current
// uid, and reads at most the trailing maxBytes bytes (default 64 KiB when <= 0).
func ReadOwnedRegularFile(path string, maxBytes int64) ([]byte, error) {
	if strings.TrimSpace(path) == "" {
		return nil, fmt.Errorf("%w: empty file path", ErrUnsafeSocket)
	}
	clean := filepath.Clean(path)
	var (
		fd  int
		err error
	)
	for {
		fd, err = unix.Open(clean, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_NONBLOCK|unix.O_CLOEXEC, 0)
		if err != unix.EINTR {
			break
		}
	}
	if err != nil {
		return nil, err
	}
	defer unix.Close(fd)

	var st unix.Stat_t
	if err := unix.Fstat(fd, &st); err != nil {
		return nil, err
	}
	if st.Mode&unix.S_IFMT != unix.S_IFREG {
		return nil, fmt.Errorf("%s is not a regular file (mode %#o)", clean, st.Mode)
	}
	uid := os.Getuid()
	if int(st.Uid) != uid {
		return nil, fmt.Errorf("%s is owned by uid %d, want %d", clean, st.Uid, uid)
	}
	limit := int64(64 << 10)
	if maxBytes > 0 {
		limit = maxBytes
	}
	if size := int64(st.Size); size > limit {
		if _, err := unix.Seek(fd, size-limit, io.SeekStart); err != nil {
			return nil, err
		}
	}
	var out []byte
	buf := make([]byte, 8192)
	for int64(len(out)) < limit {
		want := len(buf)
		if rem := int(limit) - len(out); rem < want {
			want = rem
		}
		n, rerr := unix.Read(fd, buf[:want])
		if rerr == unix.EINTR {
			continue
		}
		if n > 0 {
			out = append(out, buf[:n]...)
		}
		if rerr != nil {
			return nil, rerr
		}
		if n == 0 {
			break
		}
	}
	return out, nil
}

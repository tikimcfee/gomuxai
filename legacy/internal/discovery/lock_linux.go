package discovery

import (
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

// newLockProbe reads /proc/locks once, so finding whether agy holds a
// presence lock takes no lock at all (a probe's brief shared lock could
// make agy's own non-blocking exclusive one fail). /proc/locks names a file
// by device and inode, but its device is the superblock's, which differs
// from stat's on btrfs subvolumes; so a lock on the same inode counts when
// the holder has the file open (its /proc/<pid>/fd), or when that can't be
// read. Without /proc/locks it falls back to flockHeld.
func newLockProbe() func(path string) bool {
	data, err := os.ReadFile("/proc/locks")
	if err != nil {
		return flockHeld
	}
	holders := map[uint64][]string{} // inode -> pids holding a lock on it
	for line := range strings.SplitSeq(string(data), "\n") {
		// "78: FLOCK  ADVISORY  WRITE 730124 00:23:15646352 0 EOF"; a
		// process waiting for the lock has "->" after the number.
		f := strings.Fields(line)
		if len(f) < 6 || f[1] == "->" {
			continue
		}
		parts := strings.Split(f[5], ":")
		if len(parts) != 3 {
			continue
		}
		ino, err := strconv.ParseUint(parts[2], 10, 64)
		if err != nil {
			continue
		}
		holders[ino] = append(holders[ino], f[4])
	}
	return func(path string) bool {
		info, err := os.Stat(path)
		if err != nil {
			return false
		}
		st, ok := info.Sys().(*syscall.Stat_t)
		if !ok {
			return flockHeld(path)
		}
		for _, pid := range holders[st.Ino] {
			if opens(pid, path) {
				return true
			}
		}
		return false
	}
}

// opens reports whether process pid has path open, or true if its open
// files can't be read.
func opens(pid, path string) bool {
	fds, err := os.ReadDir(filepath.Join("/proc", pid, "fd"))
	if err != nil {
		return true
	}
	want, err := filepath.EvalSymlinks(path)
	if err != nil {
		want = path
	}
	for _, fd := range fds {
		if target, err := os.Readlink(filepath.Join("/proc", pid, "fd", fd.Name())); err == nil && target == want {
			return true
		}
	}
	return false
}

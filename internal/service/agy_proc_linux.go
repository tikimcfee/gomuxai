//go:build linux

package service

import (
	"context"
	"os"
	"path/filepath"
	"strconv"
)

// openFilesForPIDs returns the target paths of open file descriptors for pids.
func openFilesForPIDs(_ context.Context, pids []int) []string {
	var paths []string
	for _, pid := range pids {
		fdDir := filepath.Join("/proc", strconv.Itoa(pid), "fd")
		entries, err := os.ReadDir(fdDir)
		if err != nil {
			continue
		}
		for _, e := range entries {
			if target, err := os.Readlink(filepath.Join(fdDir, e.Name())); err == nil && target != "" {
				paths = append(paths, target)
			}
		}
	}
	return paths
}

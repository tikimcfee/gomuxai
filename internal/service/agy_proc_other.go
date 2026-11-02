//go:build !linux

package service

import (
	"context"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

// openFilesForPIDs returns open file paths for pids using lsof.
func openFilesForPIDs(ctx context.Context, pids []int) []string {
	if len(pids) == 0 {
		return nil
	}
	strs := make([]string, 0, len(pids))
	for _, pid := range pids {
		if pid > 0 {
			strs = append(strs, strconv.Itoa(pid))
		}
	}
	if len(strs) == 0 {
		return nil
	}
	subCtx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	out, err := exec.CommandContext(subCtx, "lsof", "-w", "-n", "-P", "-a", "-p", strings.Join(strs, ","), "-Fn").Output()
	if err != nil && len(out) == 0 {
		return nil
	}
	var paths []string
	for line := range strings.SplitSeq(string(out), "\n") {
		if strings.HasPrefix(line, "n") && len(line) > 1 {
			paths = append(paths, line[1:])
		}
	}
	return paths
}

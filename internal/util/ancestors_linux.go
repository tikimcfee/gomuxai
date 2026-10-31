package util

import (
	"context"
	"os"
	"strconv"
	"strings"
)

// ProcAncestors returns pid's parent, its parent, and so on up to (not
// including) pid 1, read from /proc one process at a time: a hook needs only
// its own line of descent, not the whole table.
func ProcAncestors(ctx context.Context, pid int) ([]int, error) {
	var out []int
	for range 4096 {
		data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
		if err != nil {
			break // it exited, or isn't visible
		}
		stat := string(data)
		i := strings.LastIndexByte(stat, ')')
		f := strings.Fields(stat[i+1:])
		if i < 0 || len(f) < 2 {
			break
		}
		parent, err := strconv.Atoi(f[1]) // field 4, ppid
		if err != nil || parent <= 1 || parent == pid {
			break
		}
		out = append(out, parent)
		pid = parent
	}
	return out, nil
}

package util

import (
	"context"
	"fmt"
	"os/exec"
	"strconv"
	"strings"
)

// ProcTree is the host's processes as child → parent pids, read at one
// moment.
type ProcTree map[int]int

// ReadProcTree lists every process's pid and parent pid with ps, which reads
// the same on Linux and macOS.
func ReadProcTree(ctx context.Context) (ProcTree, error) {
	out, err := exec.CommandContext(ctx, "ps", "-A", "-o", "pid=,ppid=").Output()
	if err != nil {
		return nil, fmt.Errorf("ps: %w", err)
	}
	return ParseProcTree(string(out)), nil
}

// ParseProcTree reads ps's "pid ppid" lines; a line that isn't two numbers
// is skipped.
func ParseProcTree(out string) ProcTree {
	t := ProcTree{}
	for line := range strings.SplitSeq(out, "\n") {
		f := strings.Fields(line)
		if len(f) != 2 {
			continue
		}
		pid, err1 := strconv.Atoi(f[0])
		ppid, err2 := strconv.Atoi(f[1])
		if err1 != nil || err2 != nil {
			continue
		}
		t[pid] = ppid
	}
	return t
}

// Depth is how many parent steps lead from pid up to root: 0 for root
// itself; ok is false when root isn't among pid's ancestors (or pid isn't
// running).
func (t ProcTree) Depth(pid, root int) (int, bool) {
	for d := 0; d <= len(t); d++ {
		if pid == root {
			return d, true
		}
		parent, ok := t[pid]
		if !ok || parent == pid || parent <= 0 {
			return 0, false
		}
		pid = parent
	}
	return 0, false
}

// Ancestors returns pid's parent, its parent, and so on up to (not
// including) pid 1 or a process the tree doesn't know.
func (t ProcTree) Ancestors(pid int) []int {
	var out []int
	for range len(t) {
		parent, ok := t[pid]
		if !ok || parent <= 1 || parent == pid {
			break
		}
		out = append(out, parent)
		pid = parent
	}
	return out
}

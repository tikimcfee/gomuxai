package util

import (
	"context"
	"fmt"
	"os/exec"
	"sort"
	"strconv"
	"strings"
)

// ProcInfo describes one process from ps.
type ProcInfo struct {
	PID   int
	PPID  int
	State string // ps stat column (starts with 'T'/'t' when stopped, 'Z' when zombie)
}

// Stopped reports whether the process is stopped by job control or tracing.
func (p ProcInfo) Stopped() bool {
	return strings.HasPrefix(p.State, "T") || strings.HasPrefix(p.State, "t")
}

// Zombie reports whether the process has exited and awaits reaping.
func (p ProcInfo) Zombie() bool {
	return strings.HasPrefix(p.State, "Z")
}

// Foreground reports whether the process is in its terminal's foreground
// process group ('+' in ps stat).
func (p ProcInfo) Foreground() bool {
	return strings.Contains(p.State, "+")
}

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

// ReadProcs lists every process's pid, parent pid, and stat string with ps.
func ReadProcs(ctx context.Context) (map[int]ProcInfo, ProcTree, error) {
	out, err := exec.CommandContext(ctx, "ps", "-A", "-o", "pid=,ppid=,stat=").Output()
	if err != nil {
		return nil, nil, fmt.Errorf("ps: %w", err)
	}
	procs, tree := ParseProcs(string(out))
	return procs, tree, nil
}

// ParseProcs parses ps "pid ppid stat" lines into a ProcInfo map and ProcTree.
func ParseProcs(out string) (map[int]ProcInfo, ProcTree) {
	procs := map[int]ProcInfo{}
	tree := ProcTree{}
	for line := range strings.SplitSeq(out, "\n") {
		f := strings.Fields(line)
		if len(f) < 3 {
			continue
		}
		pid, err1 := strconv.Atoi(f[0])
		ppid, err2 := strconv.Atoi(f[1])
		if err1 != nil || err2 != nil {
			continue
		}
		procs[pid] = ProcInfo{PID: pid, PPID: ppid, State: f[2]}
		tree[pid] = ppid
	}
	return procs, tree
}

// DescendantsExcluding returns all descendant PIDs strictly below root, ordered
// shallowest-first (then ascending PID), excluding selfPID and the direct-child
// branch of root that contains selfPID.
func (t ProcTree) DescendantsExcluding(root, selfPID int) []int {
	if root <= 0 {
		return nil
	}
	skipBranch := 0
	if selfPID > 0 && selfPID != root {
		cur := selfPID
		for range len(t) {
			parent, ok := t[cur]
			if !ok || parent <= 0 || parent == cur {
				break
			}
			if parent == root {
				skipBranch = cur
				break
			}
			cur = parent
		}
	}
	type item struct {
		pid   int
		depth int
	}
	var items []item
	for pid := range t {
		if pid == root || pid == selfPID {
			continue
		}
		d, ok := t.Depth(pid, root)
		if !ok || d <= 0 {
			continue
		}
		if skipBranch > 0 {
			if _, underSelf := t.Depth(pid, skipBranch); underSelf {
				continue
			}
		}
		items = append(items, item{pid: pid, depth: d})
	}
	sort.Slice(items, func(i, j int) bool {
		if items[i].depth != items[j].depth {
			return items[i].depth < items[j].depth
		}
		return items[i].pid < items[j].pid
	})
	out := make([]int, len(items))
	for i, it := range items {
		out[i] = it.pid
	}
	return out
}

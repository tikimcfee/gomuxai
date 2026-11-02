package util

import (
	"context"
	"errors"
	"os"
	"slices"
	"syscall"
	"testing"
)

func TestParseProcsAndDescendantsExcluding(t *testing.T) {
	raw := `
  100     1 Ss
  200   100 T+
  201   200 T+
  300   100 S+
  301   300 R+
  400   100 Z
`
	procs, tree := ParseProcs(raw)
	if !procs[200].Stopped() {
		t.Errorf("pid 200 Stopped() = false, state = %q", procs[200].State)
	}
	if procs[300].Stopped() {
		t.Errorf("pid 300 Stopped() = true, state = %q", procs[300].State)
	}
	if !procs[400].Zombie() {
		t.Errorf("pid 400 Zombie() = false, state = %q", procs[400].State)
	}
	if !procs[300].Foreground() || procs[100].Foreground() {
		t.Errorf("Foreground() = (%v, %v), want (true, false)", procs[300].Foreground(), procs[100].Foreground())
	}

	got := tree.DescendantsExcluding(100, 301)
	want := []int{200, 400, 201}
	if !slices.Equal(got, want) {
		t.Errorf("DescendantsExcluding(100, 301) = %v, want %v", got, want)
	}
}

func TestReadProcsAndProcArgs(t *testing.T) {
	procs, tree, err := ReadProcs(context.Background())
	if err != nil {
		if errors.Is(err, syscall.EPERM) {
			t.Skipf("ps blocked by sandbox: %v", err)
		}
		t.Fatalf("ReadProcs: %v", err)
	}
	self := os.Getpid()
	if _, ok := procs[self]; !ok {
		t.Errorf("self pid %d missing from ReadProcs", self)
	}
	if _, ok := tree[self]; !ok {
		t.Errorf("self pid %d missing from tree", self)
	}
	if args, ok := ProcArgs(self); !ok || len(args) == 0 {
		t.Errorf("ProcArgs(self) = %v, %v", args, ok)
	}
}

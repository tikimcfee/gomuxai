//go:build !linux

package util

import "context"

// ProcAncestors returns pid's parent, its parent, and so on up to (not
// including) pid 1, from one ps listing.
func ProcAncestors(ctx context.Context, pid int) ([]int, error) {
	t, err := ReadProcTree(ctx)
	if err != nil {
		return nil, err
	}
	return t.Ancestors(pid), nil
}

//go:build !linux

package util

import (
	"context"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

// ProcArgs returns a process's command line, as ps prints it split on
// spaces (so a word holding a space comes back as two), or ok false if it
// can't be read.
func ProcArgs(pid int) ([]string, bool) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, "ps", "-ww", "-o", "args=", "-p", strconv.Itoa(pid)).Output()
	if err != nil {
		return nil, false
	}
	f := strings.Fields(string(out))
	return f, len(f) > 0
}

package claudereg

import (
	"context"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

// procStart runs `ps -o lstart=` in the C locale and UTC, which is how Claude
// Code reads a process's start time on macOS.
func procStart(pid int) (string, bool) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, "ps", "-o", "lstart=", "-p", strconv.Itoa(pid))
	cmd.Env = append(os.Environ(), "LC_ALL=C", "TZ=UTC")
	out, err := cmd.Output()
	if err != nil {
		return "", false
	}
	s := strings.TrimSpace(string(out))
	return s, s != ""
}

// pidDomain: Claude Code records none gomuxai can rebuild on macOS, so an
// entry's domain is not checked there.
func pidDomain() string { return "" }

// zombie: not checked on macOS, where it would cost a ps per entry; a CLI's
// parent shell reaps it at once.
func zombie(int) bool { return false }

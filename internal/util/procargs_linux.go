package util

import (
	"os"
	"strconv"
	"strings"
)

// ProcArgs returns a process's command line as its words, or ok false if
// it can't be read.
func ProcArgs(pid int) ([]string, bool) {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/cmdline")
	if err != nil || len(data) == 0 {
		return nil, false
	}
	return strings.Split(strings.TrimRight(string(data), "\x00"), "\x00"), true
}

package claudereg

import (
	"os"
	"strconv"
	"strings"
	"sync"
)

// procStart reads the starttime field of /proc/<pid>/stat (clock ticks
// since boot), which is what Claude Code writes as procStart on Linux.
func procStart(pid int) (string, bool) {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return "", false
	}
	return StatStartTime(string(data))
}

var domainOnce = sync.OnceValue(func() string {
	id, err := os.ReadFile("/etc/machine-id")
	if err != nil {
		return ""
	}
	ns, err := os.Readlink("/proc/self/ns/pid")
	if err != nil {
		return ""
	}
	return "linux:" + strings.TrimSpace(string(id)) + ":" + ns
})

// pidDomain is "linux:<machine id>:<pid namespace>", as Claude Code writes
// pidDomain.
func pidDomain() string { return domainOnce() }

// zombie reports whether pid has exited and waits for its parent to reap it
// (state Z in /proc/<pid>/stat): signal 0 still reaches it, but it runs
// nothing.
func zombie(pid int) bool {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return false
	}
	stat := string(data)
	i := strings.LastIndexByte(stat, ')')
	f := strings.Fields(stat[i+1:])
	return i >= 0 && len(f) > 0 && f[0] == "Z"
}

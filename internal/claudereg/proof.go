package claudereg

import (
	"errors"
	"fmt"
	"strings"
	"syscall"
)

// Verdict is what the running processes say about an entry.
type Verdict int

const (
	// Live: its pid is running, and that process started when the entry
	// says. The entry describes a running session.
	Live Verdict = iota
	// Stale: its pid isn't running, or now belongs to a process that started
	// at another time. The CLI exited without removing its file (a crash, a
	// kill -9); the session isn't running.
	Stale
	// Foreign: written in another pid space (another container or host
	// sharing the directory); its pid says nothing about this host.
	Foreign
	// Unproven: its pid is running, but whether it is the same process
	// can't be told (the entry has no procStart, or this OS doesn't give a
	// start time gomuxai can read).
	Unproven
)

func (v Verdict) String() string {
	switch v {
	case Live:
		return "live"
	case Stale:
		return "stale"
	case Foreign:
		return "foreign"
	case Unproven:
		return "unproven"
	}
	return fmt.Sprintf("Verdict(%d)", int(v))
}

// Host is what Proof asks of the running system.
type Host interface {
	// Running reports whether pid names a running process.
	Running(pid int) bool
	// ProcStart returns the process's start time in the form Claude Code
	// records it, or ok false if it can't be read.
	ProcStart(pid int) (start string, ok bool)
	// PIDDomain returns this process's pid space in the form Claude Code
	// records it, or "" if it can't be told.
	PIDDomain() string
}

// Proof checks an entry against the running processes.
func Proof(e Entry, h Host) Verdict {
	if e.PIDDomain != "" {
		if d := h.PIDDomain(); d != "" && d != e.PIDDomain {
			return Foreign
		}
	}
	if !h.Running(e.PID) {
		return Stale
	}
	if e.ProcStart == "" {
		return Unproven
	}
	start, ok := h.ProcStart(e.PID)
	if !ok {
		return Unproven
	}
	if start != e.ProcStart {
		if e.ProcStartFt {
			return Unproven
		}
		return Stale
	}
	return Live
}

// System is the Host of this machine.
type System struct{}

// Running reports whether pid names a running process: signal 0 reaches it,
// or it exists but belongs to another user.
func (System) Running(pid int) bool {
	if pid <= 1 {
		return false
	}
	err := syscall.Kill(pid, 0)
	return (err == nil || errors.Is(err, syscall.EPERM)) && !zombie(pid)
}

// ProcStart is procStart (proof_linux.go, proof_darwin.go, proof_other.go).
func (System) ProcStart(pid int) (string, bool) { return procStart(pid) }

// PIDDomain is pidDomain (proof_linux.go, ...).
func (System) PIDDomain() string { return pidDomain() }

// StatStartTime returns the starttime field (the 22nd) of a /proc/<pid>/stat
// line. The command name (the 2nd) is in parentheses and may hold spaces or
// parentheses itself, so fields are counted from the last ")".
func StatStartTime(stat string) (string, bool) {
	i := strings.LastIndexByte(stat, ')')
	if i < 0 {
		return "", false
	}
	f := strings.Fields(stat[i+1:])
	// f[0] is field 3 (state), so field 22 is f[19].
	if len(f) < 20 {
		return "", false
	}
	return f[19], true
}

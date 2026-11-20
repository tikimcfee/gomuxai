// Package claudereg reads Claude Code's session registry: one file per
// running CLI process, <config dir>/sessions/<pid>.json, which Claude Code
// writes at start, rewrites as its status moves, and removes at exit.
//
// The registry is Claude Code's own record of which session each of its
// processes holds, so it is how gomuxai knows what runs where. Its format is
// not a published contract (it is what `claude agents --json` is built on), so
// this package reads only the fields it names, and treats a value outside the
// vocabulary below as a problem to report, never as a guess.
//
// An entry is trusted only when it is proven: its pid is running and that
// process started when the entry says (procStart), the same check Claude Code
// makes before it believes an entry. An entry's tmux field is NOT used to
// place it: Claude copies it from its environment, so a `claude -p` run from
// inside an agent's pane names that agent's pane too. Where a session runs is
// read from the process tree instead (see Running.UnderPane).
package claudereg

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Kinds of Claude Code process, as the registry names them.
const (
	KindInteractive  = "interactive"   // a CLI in a terminal (also `claude -p`)
	KindBackground   = "bg"            // a `claude --bg` session
	KindDaemon       = "daemon"        // the background-session supervisor
	KindDaemonWorker = "daemon-worker" // a session the daemon runs
)

// Statuses an entry can have, as the registry names them.
const (
	// StatusBusy: a turn is running.
	StatusBusy = "busy"
	// StatusIdle: at its prompt, nothing running.
	StatusIdle = "idle"
	// StatusShell: its turn over, while a shell command the session started
	// still runs in the background (its end starts the next turn).
	StatusShell = "shell"
	// StatusWaiting: stopped on something only a person can answer
	// (WaitingFor says what, when Claude records it).
	StatusWaiting = "waiting"
)

var kinds = map[string]bool{KindInteractive: true, KindBackground: true, KindDaemon: true, KindDaemonWorker: true}
var statuses = map[string]bool{StatusBusy: true, StatusIdle: true, StatusShell: true, StatusWaiting: true}

// Entry is one registry file.
type Entry struct {
	Path      string // the file it was read from
	PID       int
	SessionID string
	Cwd       string
	Version   string // the Claude Code version that wrote it
	Kind      string
	// Entrypoint is how the process was started: "cli" for a person's
	// interactive CLI, "sdk-cli" for `claude -p`, and others.
	Entrypoint string
	Status     string
	WaitingFor string
	Name       string
	NameSource string // "derived", "user", "peer", ...
	StartedAt  time.Time
	UpdatedAt  time.Time
	// ProcStart is the process's start time as the OS gives it (Linux: the
	// starttime field of /proc/<pid>/stat; macOS: `ps -o lstart=` in UTC),
	// which Proof compares with the running process's.
	ProcStart string
	// ProcStartFt says ProcStart came from procStartFt, whose form gomuxai
	// hasn't seen written: a value unlike this OS's start time can't prove
	// the entry stale, only unproven.
	ProcStartFt bool
	// PIDDomain names the pid space the pid belongs to (Linux: machine id and
	// pid namespace), so an entry written in another container sharing the
	// directory is not read as one of this host's processes.
	PIDDomain string
	// TmuxHint is the entry's tmux field: Claude's environment's idea of its
	// pane. A hint for people only; see the package comment.
	TmuxHint string
	// Unknown says which of the entry's values gomuxai doesn't know ("" if
	// none): its status then reads as unknown.
	Unknown string
}

// Problem is a registry file that couldn't be read as an entry, and why.
type Problem struct {
	Path string
	Err  error
}

func (p Problem) Error() string { return p.Path + ": " + p.Err.Error() }

// Dir returns the registry directory: $CLAUDE_CONFIG_DIR/sessions, else
// ~/.claude/sessions.
func Dir() (string, error) {
	if d := os.Getenv("CLAUDE_CONFIG_DIR"); d != "" {
		return filepath.Join(d, "sessions"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("finding the Claude Code session registry: %w", err)
	}
	return filepath.Join(home, ".claude", "sessions"), nil
}

// ReadDir reads every <pid>.json in dir. A missing dir is no entries (no
// Claude Code has run). A file that can't be read as an entry is a Problem;
// the others are still returned. Entries come sorted by pid.
func ReadDir(dir string) ([]Entry, []Problem, error) {
	files, err := os.ReadDir(dir)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil, nil
	}
	if err != nil {
		return nil, nil, fmt.Errorf("reading the Claude Code session registry: %w", err)
	}
	var entries []Entry
	var problems []Problem
	for _, f := range files {
		name := f.Name()
		stem, ok := strings.CutSuffix(name, ".json")
		if !ok || f.IsDir() {
			continue // <pid>.<hash>.key files and anything else
		}
		pid, err := strconv.Atoi(stem)
		if err != nil || strconv.Itoa(pid) != stem {
			continue // not a <pid>.json
		}
		path := filepath.Join(dir, name)
		data, err := os.ReadFile(path)
		if errors.Is(err, fs.ErrNotExist) {
			continue // its process exited between the listing and the read
		}
		if err != nil {
			problems = append(problems, Problem{path, err})
			continue
		}
		e, err := ParseEntry(data)
		if err != nil {
			problems = append(problems, Problem{path, err})
			continue
		}
		if e.PID != pid {
			problems = append(problems, Problem{path, fmt.Errorf("names pid %d, not the %d its file name says", e.PID, pid)})
			continue
		}
		e.Path = path
		entries = append(entries, e)
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].PID < entries[j].PID })
	return entries, problems, nil
}

// ParseEntry reads one registry file's JSON. It refuses a file without a
// pid or session id. A kind or status outside the vocabulary above is kept
// raw with Unknown saying so: a Claude Code that writes a new value needs
// this package taught what it means, and until then that one session's
// status is unknown, never guessed (its process can still be proven and
// placed).
func ParseEntry(data []byte) (Entry, error) {
	var raw struct {
		PID             int    `json:"pid"`
		SessionID       string `json:"sessionId"`
		Cwd             string `json:"cwd"`
		Version         string `json:"version"`
		Kind            string `json:"kind"`
		Entrypoint      string `json:"entrypoint"`
		Status          string `json:"status"`
		WaitingFor      string `json:"waitingFor"`
		Name            string `json:"name"`
		NameSource      string `json:"nameSource"`
		StartedAt       int64  `json:"startedAt"`
		UpdatedAt       int64  `json:"updatedAt"`
		StatusUpdatedAt int64  `json:"statusUpdatedAt"`
		ProcStart       string `json:"procStart"`
		ProcStartFt     string `json:"procStartFt"`
		PIDDomain       string `json:"pidDomain"`
		Tmux            string `json:"tmux"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return Entry{}, fmt.Errorf("not a registry entry: %w", err)
	}
	if raw.PID <= 1 {
		return Entry{}, fmt.Errorf("no pid")
	}
	if raw.SessionID == "" {
		return Entry{}, fmt.Errorf("no sessionId")
	}
	// Claude Code reads a missing kind as interactive and a missing status
	// as not yet known; so does this.
	if raw.Kind == "" {
		raw.Kind = KindInteractive
	}
	var unknown []string
	if !kinds[raw.Kind] {
		unknown = append(unknown, fmt.Sprintf("kind %q is not one gomuxai knows (%s)", raw.Kind, vocab(kinds)))
	}
	if raw.Status != "" && !statuses[raw.Status] {
		unknown = append(unknown, fmt.Sprintf("status %q is not one gomuxai knows (%s)", raw.Status, vocab(statuses)))
	}
	unknownText := ""
	if len(unknown) > 0 {
		unknownText = strings.Join(unknown, "; ") + ", written by Claude Code " + orUnknown(raw.Version)
	}
	// Claude Code writes the start time as procStart, or (behind a switch
	// it ships off) as procStartFt instead: the same value under its new name.
	start, ft := raw.ProcStart, false
	if start == "" {
		start, ft = raw.ProcStartFt, raw.ProcStartFt != ""
	}
	updated := raw.StatusUpdatedAt
	if updated == 0 {
		updated = raw.UpdatedAt
	}
	return Entry{
		PID: raw.PID, SessionID: raw.SessionID, Cwd: raw.Cwd, Version: raw.Version,
		Kind: raw.Kind, Entrypoint: raw.Entrypoint, Status: raw.Status, WaitingFor: raw.WaitingFor,
		Name: raw.Name, NameSource: raw.NameSource,
		StartedAt: millis(raw.StartedAt), UpdatedAt: millis(updated),
		ProcStart: start, ProcStartFt: ft, PIDDomain: raw.PIDDomain, TmuxHint: raw.Tmux, Unknown: unknownText,
	}, nil
}

func millis(ms int64) time.Time {
	if ms <= 0 {
		return time.Time{}
	}
	return time.UnixMilli(ms)
}

func vocab(set map[string]bool) string {
	keys := make([]string, 0, len(set))
	for k := range set {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return strings.Join(keys, ", ")
}

func orUnknown(v string) string {
	if v == "" {
		return "(version not recorded)"
	}
	return v
}

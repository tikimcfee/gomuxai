package claudereg

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// A Claude Code session can move to another process mid-conversation:
// Claude Code 2.1.319 hands an interactive session over to a background
// session its daemon runs ("kind": "bg"). The daemon is a child of the
// interactive CLI, so the new process runs under the same pane, and the
// pane's CLI goes on drawing the conversation, but the new process has a
// session id of its own and runs every hook from then on, while the old
// process's registry entry keeps the old id and its last status.
//
// The move is recorded twice, and Continues checks both: the new process
// is launched with --resume <the old session's transcript> (and
// --fork-session --session-id <new>), and the old transcript gains a
// {"type":"continued-in","continuedInSessionId":"<new>"} line. A
// `claude --resume <id> --fork-session` a person runs elsewhere starts a
// separate conversation, and writes no continued-in into the old one.

// continuedTail is how much of the end of a transcript ContinuedIn reads
// for the continued-in line: the move appends it, and the old process,
// whose conversation has gone elsewhere, writes little after it. Status
// reads it on every check of a Claude agent, so it is kept small.
const continuedTail = 64 << 10

// Continues reports whether the process launched with args carries on
// session oldID as session newID: its --resume names oldID's transcript
// (a path ending in <oldID>.jsonl), and that transcript records that it
// continued in newID.
func Continues(args []string, oldID, newID string) bool {
	if oldID == "" || newID == "" || oldID == newID {
		return false
	}
	path := resumedPath(args)
	if path == "" || filepath.Base(path) != oldID+".jsonl" {
		return false
	}
	return ContinuedIn(path) == newID
}

// resumedPath returns the transcript path a Claude Code command line
// resumes (--resume <path> or --resume=<path>), or "" if it resumes none or
// names a session by id.
func resumedPath(args []string) string {
	for i, a := range args {
		v := ""
		switch {
		case a == "--resume" && i+1 < len(args):
			v = args[i+1]
		case strings.HasPrefix(a, "--resume="):
			v = strings.TrimPrefix(a, "--resume=")
		default:
			continue
		}
		if strings.HasSuffix(v, ".jsonl") && filepath.IsAbs(v) {
			return v
		}
	}
	return ""
}

// ContinuedIn returns the session the transcript at path says it continued
// in (its last continued-in line), or "" if it says none.
func ContinuedIn(path string) string {
	f, err := os.Open(path)
	if err != nil {
		return ""
	}
	defer f.Close()
	if info, err := f.Stat(); err == nil && info.Size() > continuedTail {
		if _, err := f.Seek(-continuedTail, io.SeekEnd); err != nil {
			return ""
		}
	}
	marker := []byte(`"continued-in"`)
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 0, 64<<10), 16<<20)
	last := ""
	for sc.Scan() {
		line := sc.Bytes()
		if !bytes.Contains(line, marker) {
			continue
		}
		var r struct {
			Type        string `json:"type"`
			ContinuedIn string `json:"continuedInSessionId"`
		}
		if json.Unmarshal(line, &r) == nil && r.Type == "continued-in" && r.ContinuedIn != "" {
			last = r.ContinuedIn
		}
	}
	return last
}

// Continuation returns the running CLI that carries on outer's session,
// following moves one after another (see Continues): a proven-live entry
// whose process descends from outer's, launched to continue its session.
// args reads a process's command line. found is false when outer's session
// hasn't moved.
func (s *Snapshot) Continuation(t Tree, outer Proven, args func(pid int) ([]string, bool)) (e Proven, found bool) {
	seen := map[int]bool{outer.PID: true}
	for cur := outer; ; {
		next, ok := s.continuedBy(t, cur, args, seen)
		if !ok {
			return e, found
		}
		e, found, cur = next, true, next
		seen[next.PID] = true
	}
}

func (s *Snapshot) continuedBy(t Tree, outer Proven, args func(pid int) ([]string, bool), seen map[int]bool) (Proven, bool) {
	for _, e := range s.Entries {
		if e.Verdict != Live || seen[e.PID] {
			continue
		}
		if _, ok := t.Depth(e.PID, outer.PID); !ok {
			continue
		}
		if a, ok := args(e.PID); ok && Continues(a, outer.SessionID, e.SessionID) {
			return e, true
		}
	}
	return Proven{}, false
}

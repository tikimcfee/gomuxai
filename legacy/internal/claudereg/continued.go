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
// Claude Code (2.1.319 on) hands an interactive session over to a
// background session its daemon runs ("kind": "bg"). The pane's CLI goes on
// drawing the conversation, and takes what is typed into it, but the new
// process has a session id of its own and runs every hook from then on,
// while the old process's registry entry keeps the old id and its last
// status.
//
// Where the new process runs says nothing about whose conversation it
// carries: the daemon is one per user, a child of whichever CLI started it
// first, and from 2.1.325 it hands a moved session to a spare host it
// started ahead of time ("bg claimed-spare" in ~/.claude/daemon.log). So a
// moved session can run under another agent's pane, with that agent's
// environment (friction #48-#50). What does say it is the old transcript:
// the move appends a {"type":"continued-in","continuedInSessionId":"<new>"}
// line to it. A `claude --resume <id> --fork-session` a person runs starts
// a separate conversation and writes no such line, so the line alone proves
// the move, and Continuation follows it.

// continuedTail is how much of the end of a transcript ContinuedIn reads
// for the continued-in line: the move appends it, and the old process,
// whose conversation has gone elsewhere, writes little after it. Status
// reads it on every check of a Claude agent, so it is kept small.
const continuedTail = 64 << 10

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

// FindTranscript returns the path of Claude Code's transcript for
// sessionID (<config dir>/projects/<project>/<id>.jsonl, the config dir
// being $CLAUDE_CONFIG_DIR, else ~/.claude), or "" if there is none.
func FindTranscript(sessionID string) string {
	dir, err := Dir()
	if err != nil || sessionID == "" || strings.ContainsAny(sessionID, `/\*?[`) {
		return ""
	}
	matches, _ := filepath.Glob(filepath.Join(filepath.Dir(dir), "projects", "*", sessionID+".jsonl"))
	if len(matches) == 0 {
		return ""
	}
	return matches[0]
}

// maxMoves bounds how many moves Continuation follows.
const maxMoves = 16

// Continuation follows outer's conversation through the moves Claude Code
// made (its transcript's continued-in line, then that session's, and so
// on) to the session it goes on in now, wherever the process holding it
// runs. transcriptOf finds a session's transcript (FindTranscript). moved
// is false when outer's session hasn't moved. When it has, e is the
// running entry that holds the last session, or, if no running process
// holds it (the daemon retired the background session), an entry with only
// its SessionID and the verdict Stale: the conversation is still that
// session, which the pane's CLI starts again on its next input.
func (s *Snapshot) Continuation(outer Proven, transcriptOf func(sessionID string) string) (e Proven, moved bool) {
	seen := map[string]bool{outer.SessionID: true}
	cur := outer.SessionID
	for range maxMoves {
		next := ContinuedIn(transcriptOf(cur))
		if next == "" || seen[next] {
			break
		}
		seen[next], cur, moved = true, next, true
	}
	if !moved {
		return Proven{}, false
	}
	if h, ok := s.holder(cur); ok {
		return h, true
	}
	return Proven{Entry: Entry{SessionID: cur}, Verdict: Stale}, true
}

// holder returns the running entry that holds session id: a background
// session's own process first (a person may have resumed the same session
// elsewhere too), then as Sessions picks.
func (s *Snapshot) holder(id string) (Proven, bool) {
	var best Proven
	found := false
	for _, e := range s.Entries {
		if e.SessionID != id || !running(e.Verdict) {
			continue
		}
		bg, bestBG := e.Kind == KindBackground, best.Kind == KindBackground
		if !found || bg && !bestBG || bg == bestBG && newer(e, best) {
			best, found = e, true
		}
	}
	return best, found
}

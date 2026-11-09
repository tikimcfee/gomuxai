package claudereg

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// writeTranscript writes a transcript at <dir>/<id>.jsonl from lines.
func writeTranscript(t *testing.T, dir, id string, lines ...string) string {
	t.Helper()
	path := filepath.Join(dir, id+".jsonl")
	if err := os.WriteFile(path, []byte(strings.Join(lines, "\n")+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

// The tail Claude Code 2.1.319 wrote when it moved a session into the
// background (seen on monolith1, ids shortened).
const (
	costLine  = `{"type":"cost-state","sessionId":"old","totalCostUSD":30.4}`
	movedLine = `{"type":"continued-in","timestamp":"2026-11-09T03:14:51.363Z","sessionId":"old","continuedInSessionId":"new"}`
)

func TestContinues(t *testing.T) {
	dir := t.TempDir()
	path := writeTranscript(t, dir, "old", `{"type":"user","sessionId":"old"}`, costLine, movedLine)
	bg := []string{"/v/2.1.319", "--session-id", "new", "--fork-session", "--resume", path, "--reply-on-resume"}
	if !Continues(bg, "old", "new") {
		t.Error("the background CLI that took the session over")
	}
	if !Continues([]string{"claude", "--resume=" + path}, "old", "new") {
		t.Error("--resume=<path>")
	}
	for name, c := range map[string]struct {
		args         []string
		oldID, newID string
	}{
		"another new session":         {bg, "old", "other"},
		"another old session":         {bg, "older", "new"},
		"resumed by id, not path":     {[]string{"claude", "--resume", "old", "--fork-session"}, "old", "new"},
		"no --resume":                 {[]string{"claude", "--session-id", "new"}, "old", "new"},
		"the same session":            {bg, "new", "new"},
		"a transcript that's missing": {[]string{"claude", "--resume", filepath.Join(dir, "gone.jsonl")}, "gone", "new"},
	} {
		if Continues(c.args, c.oldID, c.newID) {
			t.Errorf("%s: continues", name)
		}
	}
	// A person's `claude --resume <path> --fork-session` elsewhere: no
	// continued-in in the old transcript.
	forked := writeTranscript(t, dir, "kept", `{"type":"user","sessionId":"kept"}`, costLine)
	if Continues([]string{"claude", "--fork-session", "--resume", forked}, "kept", "new") {
		t.Error("a fork that left the old session as it was")
	}
}

func TestContinuedIn(t *testing.T) {
	dir := t.TempDir()
	twice := writeTranscript(t, dir, "a", movedLine,
		`{"type":"continued-in","sessionId":"a","continuedInSessionId":"newer"}`)
	if got := ContinuedIn(twice); got != "newer" {
		t.Errorf("moved twice: %q, want the last", got)
	}
	// The line comes after more than the tail holds of earlier history.
	long := writeTranscript(t, dir, "b", `{"type":"user","text":"`+strings.Repeat("x", 2*continuedTail)+`"}`, movedLine)
	if got := ContinuedIn(long); got != "new" {
		t.Errorf("long transcript: %q", got)
	}
	// A mention in a message isn't the record.
	quoted := writeTranscript(t, dir, "c", `{"type":"user","text":"the \"continued-in\" line"}`)
	if got := ContinuedIn(quoted); got != "" {
		t.Errorf("quoted: %q", got)
	}
	if got := ContinuedIn(filepath.Join(dir, "none.jsonl")); got != "" {
		t.Errorf("missing: %q", got)
	}
}

func TestContinuation(t *testing.T) {
	dir := t.TempDir()
	oldPath := writeTranscript(t, dir, "old", movedLine)
	// pane 100 -> sh 101 -> claude 102 (old) -> daemon 103 -> pty host 104
	// -> claude 105 (new); a background job 106 under the same daemon; a
	// person's fork 200 elsewhere.
	pt := tree{101: 100, 102: 101, 103: 102, 104: 103, 105: 104, 106: 104, 200: 1}
	args := map[int][]string{
		105: {"claude", "--session-id", "new", "--fork-session", "--resume", oldPath},
		106: {"claude", "--session-id", "job", "-p", "run the job"},
		200: {"claude", "--session-id", "new", "--fork-session", "--resume", oldPath},
	}
	argsOf := func(pid int) ([]string, bool) { a, ok := args[pid]; return a, ok }
	outer := Proven{Entry: Entry{PID: 102, SessionID: "old"}, Verdict: Live}
	s := &Snapshot{Entries: []Proven{
		outer,
		{Entry: Entry{PID: 105, SessionID: "new", Kind: "bg"}, Verdict: Live},
		{Entry: Entry{PID: 106, SessionID: "job", Kind: "bg"}, Verdict: Live},
		{Entry: Entry{PID: 200, SessionID: "new"}, Verdict: Live},
	}}
	e, ok := s.Continuation(pt, outer, argsOf)
	if !ok || e.PID != 105 {
		t.Fatalf("got %+v %v, want pid 105", e, ok)
	}
	// Under the pane, UnderPane finds the old CLI, and the move leads on.
	under, _, _ := s.UnderPane(pt, 100)
	if e, ok := s.Continuation(pt, under, argsOf); !ok || e.SessionID != "new" {
		t.Errorf("from the pane: %+v %v", e, ok)
	}
	// Not moved: no continuation.
	if _, ok := s.Continuation(pt, Proven{Entry: Entry{PID: 106, SessionID: "job"}, Verdict: Live}, argsOf); ok {
		t.Error("the job's session didn't move")
	}
	// A stale entry (the background CLI exited) isn't one.
	s.Entries[1].Verdict = Stale
	if _, ok := s.Continuation(pt, outer, argsOf); ok {
		t.Error("an exited background CLI")
	}
}

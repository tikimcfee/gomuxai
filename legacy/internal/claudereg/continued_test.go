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
// background (seen on workstation, ids shortened).
const (
	costLine  = `{"type":"cost-state","sessionId":"old","totalCostUSD":30.4}`
	movedLine = `{"type":"continued-in","timestamp":"2026-11-09T03:14:51.363Z","sessionId":"old","continuedInSessionId":"new"}`
)

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
	writeTranscript(t, dir, "old", movedLine)
	transcriptOf := func(id string) string { return filepath.Join(dir, id+".jsonl") }
	outer := Proven{Entry: Entry{PID: 102, SessionID: "old"}, Verdict: Live}
	// The background CLI holding the new session runs wherever the daemon
	// put it: Claude Code 2.1.325 hands the move to a spare host it started
	// under another agent's CLI (friction #48-#50), so nothing about its
	// place in the process tree counts. A person who resumed the same
	// session in a terminal (pid 200) doesn't take it from the bg process.
	s := &Snapshot{Entries: []Proven{
		outer,
		{Entry: Entry{PID: 200, SessionID: "new", Kind: KindInteractive}, Verdict: Live},
		{Entry: Entry{PID: 905, SessionID: "new", Kind: KindBackground}, Verdict: Live},
		{Entry: Entry{PID: 106, SessionID: "job", Kind: KindBackground}, Verdict: Live},
	}}
	e, ok := s.Continuation(outer, transcriptOf)
	if !ok || e.PID != 905 {
		t.Fatalf("got %+v %v, want pid 905", e, ok)
	}
	// Not moved: no continuation.
	if _, ok := s.Continuation(Proven{Entry: Entry{PID: 106, SessionID: "job"}, Verdict: Live}, transcriptOf); ok {
		t.Error("the job's session didn't move")
	}
	// Moved twice: the conversation goes on in the last.
	writeTranscript(t, dir, "new", `{"type":"continued-in","sessionId":"new","continuedInSessionId":"newer"}`)
	s.Entries = append(s.Entries, Proven{Entry: Entry{PID: 907, SessionID: "newer", Kind: KindBackground}, Verdict: Live})
	if e, ok := s.Continuation(outer, transcriptOf); !ok || e.PID != 907 {
		t.Errorf("moved twice: %+v %v, want pid 907", e, ok)
	}
	// The daemon retired the background session: no process holds it, but
	// the conversation is still that session, not the old one.
	s.Entries = s.Entries[:4]
	s.Entries[2].Verdict = Stale
	s.Entries[1].Verdict = Stale
	e, ok = s.Continuation(outer, transcriptOf)
	if !ok || e.SessionID != "newer" || e.PID != 0 || e.Verdict != Stale {
		t.Errorf("retired: %+v %v, want session newer, running nowhere", e, ok)
	}
	// A loop of continued-in lines ends.
	writeTranscript(t, dir, "newer", `{"type":"continued-in","sessionId":"newer","continuedInSessionId":"old"}`)
	if _, ok := s.Continuation(outer, transcriptOf); !ok {
		t.Error("a loop")
	}
}

// FindTranscript finds a session's file under the config dir's projects,
// whatever its project folder.
func TestFindTranscript(t *testing.T) {
	cfg := t.TempDir()
	t.Setenv("CLAUDE_CONFIG_DIR", cfg)
	proj := filepath.Join(cfg, "projects", "-home-u-dev")
	if err := os.MkdirAll(proj, 0o700); err != nil {
		t.Fatal(err)
	}
	path := writeTranscript(t, proj, "abc-123", costLine)
	if got := FindTranscript("abc-123"); got != path {
		t.Errorf("got %q, want %q", got, path)
	}
	for _, id := range []string{"", "missing", "*", "../x"} {
		if got := FindTranscript(id); got != "" {
			t.Errorf("%q: %q", id, got)
		}
	}
}

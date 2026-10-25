// Package transcript reads the transcripts agent CLIs keep of their own
// sessions (Claude Code's session JSONL, agy's transcript_full.jsonl, Kimi
// Code's wire.jsonl) into
// one harness-neutral stream of events (events.go; docs/EVENTS.md is the
// contract): a prompt, a thinking block, a reply, a tool call and its
// result, a harness record, each where it happened and tagged with the
// turn it belongs to.
//
// A Reader's Parser reads a transcript line by line, so the same code
// reads a whole file, a window of one, and the lines a live session
// appends. The formats are the CLIs' internal files, not documented APIs:
// a line that isn't a JSON object is skipped and counted
// (Parser.Skipped), and a record of an unknown shape is ignored.
//
// The behaviour is ported from glyph3d-rs native/src/agent_transcript/
// and glyph3d-js packages/glyph3d-core/src/collections/sessionAdapter.js,
// and checked against the conformance corpus in testdata/corpus.
package transcript

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// parseTime reads an RFC 3339 timestamp, or returns the zero time.
func parseTime(s string) time.Time {
	t, err := time.Parse(time.RFC3339Nano, s)
	if err != nil {
		return time.Time{}
	}
	return t
}

// Detect guesses a transcript's format from its first lines: Kimi Code's
// records are typed like context.append_loop_event and turn.prompt, after
// a metadata line naming its protocol_version; agy's steps carry
// step_index and step types like PLANNER_RESPONSE; anything else is read
// as Claude Code's.
func Detect(data []byte) Reader {
	n := 0
	for line := range bytes.SplitSeq(data, []byte{'\n'}) {
		if n++; n > 30 {
			break
		}
		switch {
		case bytes.Contains(line, []byte(`"context.append_loop_event"`)) || bytes.Contains(line, []byte(`"turn.prompt"`)) ||
			bytes.Contains(line, []byte(`"protocol_version"`)) && bytes.Contains(line, []byte(`"metadata"`)):
			return Kimi{}
		case bytes.Contains(line, []byte(`"step_index"`)) || bytes.Contains(line, []byte(`"PLANNER_RESPONSE"`)) || bytes.Contains(line, []byte(`"USER_INPUT"`)):
			return Agy{}
		}
	}
	return Claude{}
}

// FileOptions returns the Options for parsing the transcript at path with
// harness's reader: what the files beside it say that its records don't.
// For a Claude subagent, the lookup of its parent's turn indexes
// (ClaudeSubagentTurns); for agy, which log it is and, for a subagent, its
// parent conversation and the call and turn that started it; for Kimi
// Code, the session's title and working directory, and a subagent's parent
// session. ReadFile parses with these, and so should a resumed parse
// (ResumeAfter).
func FileOptions(harness, path string) Options {
	switch harness {
	case "claude":
		return Options{Turns: ClaudeSubagentTurns(path)}
	case "agy":
		return agyFileOptions(path)
	case "kimi":
		return kimiFileOptions(path)
	}
	return Options{}
}

// ErrUnparseable is returned for a transcript none of whose lines is a JSON
// object: not a format any Reader knows.
var ErrUnparseable = errors.New("unparseable transcript")

// Read sizes for ReadFile: how much it reads at a time, and how much of
// the head it reads to detect the format.
const (
	readChunk = 1 << 20
	headChunk = 256 << 10
)

// File is a transcript ReadFile read.
type File struct {
	Session Session
	// Events is the end of the stream the transcript makes, updates
	// included: its last events, as Latest counts them, and the updates to
	// them.
	Events []Event
	// Partial is set when the stream had more events than Events keeps.
	Partial bool
	// Lines counts the non-blank lines read; Skipped, those that weren't
	// records of the format.
	Lines   int
	Skipped int
}

// ReadFile reads the transcript at path with r and keeps the last
// minEvents events it makes (counted as Latest counts them), with their
// updates. It parses the whole file, since what a line means depends on
// the lines before it, but holds only those events. A last line with no
// newline yet is left out. A nil r detects the format from the file's
// first lines.
func ReadFile(path string, r Reader, minEvents int) (*File, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return nil, err
	}
	size := info.Size()
	if r == nil {
		head := make([]byte, min(headChunk, size))
		n, err := f.ReadAt(head, 0)
		if err != nil && err != io.EOF {
			return nil, fmt.Errorf("reading transcript: %w", err)
		}
		r = Detect(head[:n])
	}

	p := r.NewParser(FileOptions(r.Harness(), path))
	t := newTail(minEvents)
	// pending holds the bytes read but not yet parsed: a line not yet
	// whole. It starts at p.Pos().At.
	var pending []byte
	buf := make([]byte, readChunk)
	for off := int64(0); off < size; {
		n, err := f.ReadAt(buf[:min(int64(len(buf)), size-off)], off)
		off += int64(n)
		pending = append(pending, buf[:n]...)
		start := p.Pos().At
		for _, e := range p.Feed(pending) {
			t.push(e)
		}
		pending = append(pending[:0], pending[p.Pos().At-start:]...)
		if err != nil && err != io.EOF {
			return nil, fmt.Errorf("reading transcript: %w", err)
		}
		if n == 0 {
			break
		}
	}
	if p.Lines > 0 && p.Skipped == p.Lines {
		return nil, fmt.Errorf("%w: none of its %d lines is a JSON object", ErrUnparseable, p.Lines)
	}

	file := &File{Session: p.Session(), Events: t.events(), Partial: t.cut, Lines: p.Lines, Skipped: p.Skipped}
	file.Session.file(path, size)
	return file, nil
}

// tail keeps the end of a stream: the events of the last n ids it adds,
// and the updates to them.
type tail struct {
	n      int
	stream []Event
	// added lists the kept ids, oldest first; kept holds them.
	added []string
	kept  map[string]bool
	// cut is set once an id was dropped.
	cut bool
}

func newTail(n int) *tail {
	return &tail{n: max(n, 0), kept: map[string]bool{}}
}

func (t *tail) push(e Event) {
	if !t.kept[e.ID] {
		if e.Op != OpAdd {
			return // an update of a dropped event
		}
		t.kept[e.ID] = true
		t.added = append(t.added, e.ID)
	}
	t.stream = append(t.stream, e)
	if len(t.added) > t.n {
		delete(t.kept, t.added[0])
		t.added = t.added[1:]
		t.cut = true
	}
	if len(t.stream) > 2*t.n+1024 {
		t.stream = t.events()
	}
}

// events returns the kept events, in stream order.
func (t *tail) events() []Event {
	var out []Event
	for _, e := range t.stream {
		if t.kept[e.ID] {
			out = append(out, e)
		}
	}
	return out
}

// file records in s the transcript file it was read from, and what the
// file's name and neighbours say: the session id, if no record named it,
// and the call that started a Claude subagent.
func (s *Session) file(path string, size int64) {
	s.Path, s.Size = path, size
	if s.SessionID == "" {
		s.SessionID = sessionIDFromPath(path)
	}
	if s.ParentSession != nil && s.ParentSession.ToolID == "" {
		s.ParentSession.ToolID = claudeSubagentTool(path)
	}
}

// sessionIDFromPath names a session after its transcript, as agent
// sessions does: Claude Code's is <session id>.jsonl (a subagent's,
// agent-<agent id>.jsonl); agy's lives in brain/<conversation id>/ or its
// .system_generated/logs; Kimi Code's session_<uuid>/agents/main/wire.jsonl
// is <uuid>, and a subagent's agents/<agent>/wire.jsonl is <uuid>:<agent>.
func sessionIDFromPath(path string) string {
	base := filepath.Base(path)
	if agentDir := filepath.Dir(path); base == "wire.jsonl" && filepath.Base(filepath.Dir(agentDir)) == "agents" {
		session := strings.TrimPrefix(filepath.Base(filepath.Dir(filepath.Dir(agentDir))), "session_")
		if agent := filepath.Base(agentDir); agent != "main" {
			return session + ":" + agent
		}
		return session
	}
	stem := strings.TrimSuffix(base, filepath.Ext(base))
	if !strings.HasPrefix(stem, "transcript") {
		return stem
	}
	if conv := agyConversationDir(path); conv != "" {
		return filepath.Base(conv)
	}
	return ""
}

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
// For a Claude subagent, the index of its parent's turns
// (ClaudeSubagentTurns); for an agy subagent, its parent conversation and
// the call and turn that started it; for Kimi Code, the session's title
// and working directory, and a subagent's parent session.
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

// Window sizes ReadFile reads: the first tail window, and the head it
// reads for metadata when the tail doesn't reach the start. Transcripts run
// to tens of megabytes, much of them tool output and request copies.
const (
	tailChunk = 1 << 20
	headChunk = 256 << 10
)

// File is a transcript ReadFile read: all of it, or its end.
type File struct {
	Session Session
	// Events is the stream the lines read make, updates included.
	Events []Event
	// Partial is set when only the end of the file was read: events whose
	// lines came before it are missing, and so are the updates to them.
	Partial bool
	// Lines counts the non-blank lines read; Skipped, those that weren't
	// records of the format.
	Lines   int
	Skipped int
}

// ReadFile reads the transcript at path with r. It reads the end of the
// file first, doubling the window until it holds at least minEvents events
// (counted as Latest counts them) or the whole file, so the last events of
// a large transcript cost a fraction of it. A last line with no newline
// yet is left out. A nil r detects the format from the file's first lines.
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
	var head []byte
	if r == nil || size > tailChunk {
		if head, _, err = readLines(f, 0, min(headChunk, size)); err != nil {
			return nil, err
		}
	}
	if r == nil {
		r = Detect(head)
	}

	opts := FileOptions(r.Harness(), path)
	var p *Parser
	var events []Event
	var start int64
	for chunk := int64(tailChunk); ; chunk *= 2 {
		var data []byte
		if data, start, err = readLines(f, max(size-chunk, 0), size); err != nil {
			return nil, err
		}
		from := Pos{At: start}
		if start > 0 {
			if from.Line, err = linesBefore(f, start); err != nil {
				return nil, err
			}
		}
		opts.From = from
		p = r.NewParser(opts)
		events = p.Feed(data)
		if start == 0 || len(Latest(events)) >= minEvents {
			break
		}
	}
	if p.Lines > 0 && p.Skipped == p.Lines {
		return nil, fmt.Errorf("%w: none of its %d lines is a JSON object", ErrUnparseable, p.Lines)
	}

	file := &File{Session: p.Session(), Events: events, Partial: start > 0, Lines: p.Lines, Skipped: p.Skipped}
	if file.Partial {
		opts.From = Pos{}
		h := r.NewParser(opts)
		h.Feed(head)
		file.Session.fill(h.Session())
	}
	file.Session.file(path, size)
	return file, nil
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

// readLines returns f's bytes [off, end), less a partial first line when
// off is mid-file, and the offset they start at: the start of a line.
func readLines(f *os.File, off, end int64) ([]byte, int64, error) {
	// A window that starts mid-file reads one byte early: a newline there
	// means the window already starts on a line.
	start := max(off-1, 0)
	buf := make([]byte, end-start)
	n, err := f.ReadAt(buf, start)
	if err != nil && err != io.EOF {
		return nil, 0, fmt.Errorf("reading transcript: %w", err)
	}
	buf = buf[:n]
	if off == 0 {
		return buf, 0, nil
	}
	nl := bytes.IndexByte(buf, '\n')
	if nl < 0 {
		return nil, end, nil
	}
	return buf[nl+1:], start + int64(nl) + 1, nil
}

// linesBefore returns how many lines f's first n bytes hold: the index of
// the line that starts at n.
func linesBefore(f *os.File, n int64) (int, error) {
	lines := 0
	buf := make([]byte, 1<<20)
	for off := int64(0); off < n; {
		m, err := f.ReadAt(buf[:min(int64(len(buf)), n-off)], off)
		lines += bytes.Count(buf[:m], []byte{'\n'})
		off += int64(m)
		if err != nil && (err != io.EOF || off < n) {
			return 0, fmt.Errorf("reading transcript: %w", err)
		}
	}
	return lines, nil
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

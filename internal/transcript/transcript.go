// Package transcript reads the transcripts agent CLIs keep of their own
// sessions (Claude Code's session JSONL, agy's transcript_full.jsonl) into
// one harness-neutral shape: a Session of Turns, each a user prompt and the
// thinking, replies and tool calls that followed it, in the order they
// happened. Session.Events flattens it into the event list agent logs prints.
//
// Ported from glyph3d-rs native/src/agent_transcript/ (types.rs: AgentSession,
// AgentTurn, ToolCallRecord, linearize_events; claude.rs: the two-pass
// tool_use/tool_result pairing, user_prompt_text, metadata harvest;
// antigravity.rs: the step parser; agent_transcript.rs: format detection),
// which replicates glyph3d-js packages/glyph3d-core/src/collections/
// sessionAdapter.js. The tail-window read follows glyph3d-js cli/sessions.go.
// Left out: file actions, diff hunks, Kimi, staging and discovery.
//
// The formats are the CLIs' internal files, not documented APIs: a line
// that isn't a JSON object is skipped and counted (Session.Skipped), and a
// record of an unknown shape is ignored.
package transcript

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// A Reader parses one harness's transcript format.
type Reader interface {
	// Harness names the format, e.g. "claude".
	Harness() string
	// Parse reads whole lines of a transcript: all of it, or a window
	// that starts on a line boundary.
	Parse(data []byte) *Session
}

// Session is one transcript: what it says about the session, and its turns.
type Session struct {
	Harness   string    `json:"harness"`
	Path      string    `json:"path,omitempty"`
	SessionID string    `json:"session_id,omitempty"`
	CWD       string    `json:"cwd,omitempty"`
	Title     string    `json:"title,omitempty"`
	Model     string    `json:"model,omitempty"`
	Version   string    `json:"version,omitempty"`
	GitBranch string    `json:"git_branch,omitempty"`
	First     time.Time `json:"first,omitzero"`
	Last      time.Time `json:"last,omitzero"`
	// Lines counts the non-blank lines read; Skipped, those that weren't
	// JSON objects.
	Lines   int `json:"lines"`
	Skipped int `json:"skipped"`
	// Partial is set when only the end of the file was read.
	Partial bool   `json:"partial,omitempty"`
	Turns   []Turn `json:"-"`
}

// Turn is a user prompt and what the agent did about it. The first turn of
// a transcript (or of a window into one) can lack a prompt.
type Turn struct {
	Prompt string
	Time   time.Time
	Steps  []Step
}

// Step is one thing the agent did in a turn: exactly one of Thinking, Reply
// or Tool is set.
type Step struct {
	Time     time.Time
	Thinking string
	Reply    string
	Tool     *ToolCall
}

// Tool call result statuses.
const (
	StatusOK      = "ok"
	StatusError   = "error"
	StatusRunning = "running" // moved to the background (agy)
	// StatusNone: the transcript holds no result yet, because the call is
	// still running or the turn was interrupted.
	StatusNone = "none"
)

// ToolCall is a tool the agent called, with its result.
type ToolCall struct {
	ID     string          `json:"id,omitempty"`
	Name   string          `json:"name"`
	Input  json.RawMessage `json:"input,omitempty"`
	Status string          `json:"status"`
	Output string          `json:"output,omitempty"`
}

// Detail returns what the call is about, from the tool table
// (SummarizeTool): its file, else its most telling input (command,
// pattern, ...), or "". A path under cwd is made relative to it.
func (c *ToolCall) Detail(cwd string) string {
	s := SummarizeTool(c.Name, c.Input, nil, cwd)
	if s.Target != "" {
		return s.Target
	}
	return s.Detail
}

// unquote decodes a value agy sometimes stores JSON-encoded a second time
// ("\"/path/file.go\""), as glyph3d-rs clean_antigravity_string does.
func unquote(s string) string {
	if len(s) >= 2 && s[0] == '"' && s[len(s)-1] == '"' {
		var inner string
		if json.Unmarshal([]byte(s), &inner) == nil {
			return inner
		}
	}
	return s
}

// Event kinds.
const (
	KindUser     = "user"
	KindThinking = "think"
	KindReply    = "reply"
	KindTool     = "tool"
)

// Event is one step of a session, flattened: a prompt, a thought, a reply,
// or a tool call.
type Event struct {
	Turn int       `json:"turn"`
	Time time.Time `json:"time,omitzero"`
	Kind string    `json:"kind"`
	Text string    `json:"text,omitempty"`
	Tool *ToolCall `json:"tool,omitempty"`
}

// Events returns the session's events in the order they happened.
func (s *Session) Events() []Event {
	var events []Event
	for i, t := range s.Turns {
		if t.Prompt != "" {
			events = append(events, Event{Turn: i, Time: t.Time, Kind: KindUser, Text: t.Prompt})
		}
		for _, st := range t.Steps {
			e := Event{Turn: i, Time: st.Time}
			switch {
			case st.Tool != nil:
				e.Kind, e.Tool = KindTool, st.Tool
			case st.Thinking != "":
				e.Kind, e.Text = KindThinking, st.Thinking
			default:
				e.Kind, e.Text = KindReply, st.Reply
			}
			events = append(events, e)
		}
	}
	return events
}

// turns builds a session's turn list as a parser walks the transcript.
type turns struct {
	s *Session
}

// prompt starts a new turn, unless the current one is still empty.
func (b turns) prompt(text string, at time.Time) {
	if n := len(b.s.Turns); n == 0 || b.s.Turns[n-1].Prompt != "" || len(b.s.Turns[n-1].Steps) > 0 {
		b.s.Turns = append(b.s.Turns, Turn{})
	}
	t := &b.s.Turns[len(b.s.Turns)-1]
	t.Prompt, t.Time = text, at
}

// step appends a step to the current turn, starting one if there is none.
func (b turns) step(st Step) {
	if len(b.s.Turns) == 0 {
		b.s.Turns = append(b.s.Turns, Turn{Time: st.Time})
	}
	t := &b.s.Turns[len(b.s.Turns)-1]
	if t.Time.IsZero() {
		t.Time = st.Time
	}
	t.Steps = append(t.Steps, st)
}

// seen widens the session's time span to include at.
func (s *Session) seen(at time.Time) {
	if at.IsZero() {
		return
	}
	if s.First.IsZero() {
		s.First = at
	}
	s.Last = at
}

// fillMeta copies into s the metadata it lacks from other.
func (s *Session) fillMeta(other *Session) {
	for _, f := range []struct{ dst, src *string }{
		{&s.SessionID, &other.SessionID}, {&s.CWD, &other.CWD}, {&s.Title, &other.Title},
		{&s.Model, &other.Model}, {&s.Version, &other.Version}, {&s.GitBranch, &other.GitBranch},
	} {
		if *f.dst == "" {
			*f.dst = *f.src
		}
	}
	if !other.First.IsZero() {
		s.First = other.First
	}
}

// eachLine calls decode with each non-blank line of data that looks like a
// JSON object, and counts in s the lines read and those skipped: the ones
// that don't, and the ones decode returns false for.
func eachLine(s *Session, data []byte, decode func(line []byte) bool) {
	for line := range bytes.SplitSeq(data, []byte{'\n'}) {
		line = bytes.TrimSpace(line)
		if len(line) == 0 {
			continue
		}
		s.Lines++
		if line[0] != '{' || !decode(line) {
			s.Skipped++
		}
	}
}

// parseTime reads an RFC 3339 timestamp, or returns the zero time.
func parseTime(s string) time.Time {
	t, err := time.Parse(time.RFC3339Nano, s)
	if err != nil {
		return time.Time{}
	}
	return t
}

// Detect guesses a transcript's format from its first lines: agy's steps
// carry step_index and step types like PLANNER_RESPONSE; anything else is
// read as Claude Code's.
func Detect(data []byte) Reader {
	n := 0
	for line := range bytes.SplitSeq(data, []byte{'\n'}) {
		if n++; n > 30 {
			break
		}
		if bytes.Contains(line, []byte(`"step_index"`)) || bytes.Contains(line, []byte(`"PLANNER_RESPONSE"`)) || bytes.Contains(line, []byte(`"USER_INPUT"`)) {
			return Agy{}
		}
	}
	return Claude{}
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

// ReadFile reads the transcript at path with r. It reads the end of the
// file first, doubling the window until it holds at least minEvents
// events or the whole file, so the last events of a large transcript cost
// a fraction of it. A trailing line still being written is left out. A nil
// r detects the format from the file's first lines.
func ReadFile(path string, r Reader, minEvents int) (*Session, error) {
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
		head, err := readLines(f, 0, min(headChunk, size))
		if err != nil {
			return nil, err
		}
		r = Detect(head)
	}

	var s *Session
	off := size
	for chunk := int64(tailChunk); off > 0; chunk *= 2 {
		off = max(size-chunk, 0)
		data, err := readLines(f, off, size)
		if err != nil {
			return nil, err
		}
		s = r.Parse(data)
		if len(s.Events()) >= minEvents {
			break
		}
	}
	if s == nil {
		s = r.Parse(nil)
	}
	if s.Lines > 0 && s.Skipped == s.Lines {
		return nil, fmt.Errorf("%w: none of its %d lines is a JSON object", ErrUnparseable, s.Lines)
	}
	if off > 0 {
		s.Partial = true
		if head, err := readLines(f, 0, min(headChunk, size)); err == nil {
			s.fillMeta(r.Parse(head))
		}
	}
	s.Path = path
	if s.SessionID == "" {
		s.SessionID = sessionIDFromPath(path)
	}
	return s, nil
}

// readLines returns the whole lines in f's bytes [off, end): a window that
// starts mid-file drops its first, partial line, and the last line is kept
// only if complete (newline-terminated, or valid JSON).
func readLines(f *os.File, off, end int64) ([]byte, error) {
	// A window that starts mid-file reads one byte early: a newline there
	// means the window already starts on a line.
	start := max(off-1, 0)
	buf := make([]byte, end-start)
	n, err := f.ReadAt(buf, start)
	if err != nil && err != io.EOF {
		return nil, fmt.Errorf("reading transcript: %w", err)
	}
	buf = buf[:n]
	if off > 0 {
		nl := bytes.IndexByte(buf, '\n')
		if nl < 0 {
			return nil, nil
		}
		buf = buf[nl+1:]
	}
	if nl := bytes.LastIndexByte(buf, '\n'); nl < len(buf)-1 && !json.Valid(bytes.TrimSpace(buf[nl+1:])) {
		buf = buf[:nl+1]
	}
	return buf, nil
}

// sessionIDFromPath names a session after its transcript: Claude Code's is
// <session id>.jsonl; agy's lives in brain/<conversation id>/.system_generated/logs.
func sessionIDFromPath(path string) string {
	stem := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))
	if !strings.HasPrefix(stem, "transcript") {
		return stem
	}
	dir := filepath.Dir(path)
	if filepath.Base(dir) == "logs" && filepath.Base(filepath.Dir(dir)) == ".system_generated" {
		return filepath.Base(filepath.Dir(filepath.Dir(dir)))
	}
	return ""
}

package transcript

import (
	"bytes"
	"encoding/json"
	"path"
	"time"
)

// The event stream: what an agent session did, one event per thing that
// happened, in transcript order. Every reader (agent logs, agent events,
// the displays that consume them) sees these same events; none of them
// parses a transcript itself. docs/EVENTS.md is the wire contract, and
// testdata/corpus the transcripts with the events they must produce.

// Event kinds.
const (
	KindPrompt  = "prompt"  // a message into the agent
	KindThink   = "think"   // a thinking block
	KindSay     = "say"     // text the agent wrote
	KindTool    = "tool"    // a tool call; its result arrives as an update
	KindSystem  = "system"  // a harness record about the session (turn timing, compaction, errors)
	KindRequest = "request" // what one model call was sent (only with Options.Requests)
)

// Event ops: a changed event (a tool's result landed, a provisional reply
// was confirmed) is sent again, whole, with the same ID.
const (
	OpAdd    = "add"
	OpUpdate = "update"
)

// Prompt origins: where a message into the agent came from.
const (
	OriginHuman            = "human"             // typed by a person
	OriginQueued           = "queued"            // sent while the agent was busy (mid-turn)
	OriginDelivered        = "delivered"         // gomuxai's held-message delivery (Stop hook context)
	OriginTaskNotification = "task-notification" // a background task reporting back
	OriginPeer             = "peer"              // another agent
	OriginCommand          = "command"           // a slash command
	OriginSystem           = "system"            // injected by the harness
)

// Tool result statuses.
const (
	StatusPending     = "pending"     // no result yet
	StatusOK          = "ok"          // returned
	StatusError       = "error"       // failed
	StatusInterrupted = "interrupted" // the turn was interrupted before it returned
	StatusBackground  = "background"  // moved to the background; a later update completes it
)

// Event is one thing that happened in a session. Exactly the field of its
// Kind is set.
type Event struct {
	// ID is stable across reads and updates: the tool_use id for a tool;
	// otherwise the source record's id and the block's index ("<uuid>#2").
	ID string `json:"id"`
	Op string `json:"op"`
	// Seq is the event's 0-based position in its stream; At and Line
	// locate the source line (byte offset, line index), so a reader can
	// page and resume.
	Seq  int   `json:"seq"`
	At   int64 `json:"at"`
	Line int   `json:"line"`
	// TS is the source record's own timestamp, as it wrote it.
	TS string `json:"ts,omitempty"`
	// Turn is the prompt this event belongs to, so a reader may group;
	// nil before the first prompt a parse has seen.
	Turn *TurnRef `json:"turn,omitempty"`
	// Agent is the subagent id on a subagent's own events.
	Agent string `json:"agent,omitempty"`
	Kind  string `json:"kind"`

	Prompt  *PromptEvent  `json:"prompt,omitempty"`
	Think   *ThinkEvent   `json:"think,omitempty"`
	Say     *SayEvent     `json:"say,omitempty"`
	Tool    *ToolEvent    `json:"tool,omitempty"`
	System  *SystemEvent  `json:"system,omitempty"`
	Request *RequestEvent `json:"request,omitempty"`
}

// Time returns the event's timestamp, or the zero time if it has none.
func (e *Event) Time() time.Time {
	return parseTime(e.TS)
}

// TurnRef names a turn: the harness's own prompt id and position where the
// transcript records them, derived otherwise (Derived says which).
type TurnRef struct {
	ID      string `json:"id"`
	Index   int    `json:"index"`
	Derived bool   `json:"derived"`
}

// PromptEvent is a message into the agent.
type PromptEvent struct {
	Text   string `json:"text"`
	Origin string `json:"origin"`
	// Command is a slash command's "/name"; Text is then "/name args".
	Command string  `json:"command,omitempty"`
	Images  []Image `json:"images,omitempty"`
}

// Image is an image a prompt carried, its base64 data whole.
type Image struct {
	MediaType string `json:"media_type"`
	Data      string `json:"data"`
}

// ThinkEvent is a thinking block; a redacted one (signature only) is still
// an event, with no text.
type ThinkEvent struct {
	Text     string `json:"text,omitempty"`
	Redacted bool   `json:"redacted,omitempty"`
}

// SayEvent is text the agent wrote. Final marks a turn's closing reply;
// Provisional, one taken from the Stop hook before the transcript holds it
// (the transcript's own copy then arrives as an update of the same ID).
type SayEvent struct {
	Text        string `json:"text"`
	Final       bool   `json:"final,omitempty"`
	Provisional bool   `json:"provisional,omitempty"`
}

// ToolEvent is a tool call and, once it lands, its result. The embedded
// ToolSummary (action, target, detail, meta) comes from the one tool table,
// SummarizeTool.
type ToolEvent struct {
	Name   string          `json:"name"`
	Input  json.RawMessage `json:"input,omitempty"`
	Result *ToolResult     `json:"result,omitempty"`
	ToolSummary
	// SubSession is the session a subagent-spawning call started.
	SubSession string `json:"sub_session,omitempty"`
}

// ToolResult is what a tool call returned, or that it hasn't yet.
type ToolResult struct {
	Status string `json:"status"`
	Output string `json:"output,omitempty"`
	// Structured is the harness's structured response (Claude Code's
	// toolUseResult object), whole.
	Structured json.RawMessage `json:"structured,omitempty"`
}

// SystemEvent is a harness record about the session itself.
type SystemEvent struct {
	Subtype string          `json:"subtype"`
	Text    string          `json:"text,omitempty"`
	Data    json.RawMessage `json:"data,omitempty"`
}

// RequestEvent is what one model call was sent: Claude Code's api-request
// line, with the messages its tail's hashes name.
type RequestEvent struct {
	Model  string                     `json:"model,omitempty"`
	Params json.RawMessage            `json:"params,omitempty"`
	Base   string                     `json:"base,omitempty"`
	Tail   []string                   `json:"tail,omitempty"`
	Blobs  map[string]json.RawMessage `json:"blobs,omitempty"`
}

// Session is what a transcript records about its session. ReadFile adds
// the file's path and size; who owns the session and its status come from
// elsewhere (agent sessions).
type Session struct {
	SessionID string `json:"session_id"`
	Harness   string `json:"harness"`
	Path      string `json:"path,omitempty"`
	Size      int64  `json:"size,omitempty"`
	// CWD is the working directory the session recorded last; Project is
	// its last element, never the storage folder's name.
	CWD     string `json:"cwd,omitempty"`
	Project string `json:"project,omitempty"`
	// Title is the latest title the harness recorded (Claude Code: a
	// custom title wins over an AI one).
	Title     string `json:"title,omitempty"`
	AgentName string `json:"agent_name,omitempty"`
	Model     string `json:"model,omitempty"`
	Version   string `json:"version,omitempty"`
	GitBranch string `json:"git_branch,omitempty"`
	// ParentSession is set on a subagent's session.
	ParentSession *ParentSession `json:"parent_session,omitempty"`
	// ContinuedIn is the session this one continued in.
	ContinuedIn string `json:"continued_in,omitempty"`

	// customTitle is set once Title holds a custom title, which a later AI
	// title doesn't replace.
	customTitle bool
}

// ParentSession is the session that started a subagent, and the tool call
// that did, where the harness records it.
type ParentSession struct {
	SessionID string `json:"session_id"`
	ToolID    string `json:"tool_id,omitempty"`
}

// setCWD records the session's working directory, and its project.
func (s *Session) setCWD(cwd string) {
	if cwd == "" {
		return
	}
	s.CWD = cwd
	s.Project = path.Base(path.Clean(cwd))
}

// fill copies into s the metadata it lacks from other: what the start of a
// transcript said, for a parse of its end.
func (s *Session) fill(other Session) {
	for _, f := range []struct{ dst, src *string }{
		{&s.SessionID, &other.SessionID}, {&s.Title, &other.Title}, {&s.AgentName, &other.AgentName},
		{&s.Model, &other.Model}, {&s.Version, &other.Version}, {&s.GitBranch, &other.GitBranch},
		{&s.ContinuedIn, &other.ContinuedIn},
	} {
		if *f.dst == "" {
			*f.dst = *f.src
		}
	}
	if s.CWD == "" {
		s.setCWD(other.CWD)
	}
	if s.ParentSession == nil {
		s.ParentSession = other.ParentSession
	}
}

// Pos is a place in a transcript: the byte offset where a line starts,
// that line's index, and the Seq the next event gets.
type Pos struct {
	At   int64 `json:"at"`
	Line int   `json:"line"`
	Seq  int   `json:"seq"`
}

// Options choose what a parse reads and emits.
type Options struct {
	// Requests adds the request channel: Claude Code's api-request lines
	// as request events.
	Requests bool
	// From is where the data starts: the zero Pos for a whole transcript,
	// or the start of a line (an event's At and Line) to read a window of
	// one. A window's parse knows nothing of the lines before it: a
	// result whose call came earlier is dropped, and events have no turn
	// until a line names one.
	From Pos
}

// A Reader parses one harness's transcript format.
type Reader interface {
	// Harness names the format, e.g. "claude".
	Harness() string
	// NewParser returns a parser for one transcript, or a window of one.
	NewParser(opts Options) *Parser
}

// lineReader is a harness's half of a Parser: it reads one line at a time,
// keeping what later lines need (the open turn, calls waiting for their
// results).
type lineReader interface {
	// read parses one non-blank line, which starts at byte offset at and
	// is line index n, and returns its events (Seq unset), and false if
	// the line isn't a record of this format.
	read(line []byte, at int64, n int) ([]Event, bool)
	// session returns what the lines so far say about the session.
	session() Session
}

// Parser turns a transcript's lines into events. Feed it the transcript's
// bytes, in order, as they arrive.
type Parser struct {
	harness string
	lines   lineReader
	pos     Pos
	// Lines counts the non-blank lines read; Skipped, those that weren't
	// records of the format.
	Lines   int
	Skipped int
}

func newParser(harness string, lines lineReader, from Pos) *Parser {
	return &Parser{harness: harness, lines: lines, pos: from}
}

// Feed parses the whole lines at the start of data, which starts at
// Pos().At, and returns their events, numbered from Pos().Seq. A last line
// with no newline yet is left for the next Feed: that one's data starts
// again at Pos().At.
func (p *Parser) Feed(data []byte) []Event {
	var events []Event
	for {
		nl := bytes.IndexByte(data, '\n')
		if nl < 0 {
			return events
		}
		if line := bytes.TrimSpace(data[:nl]); len(line) > 0 {
			p.Lines++
			var evs []Event
			ok := false
			if line[0] == '{' {
				evs, ok = p.lines.read(line, p.pos.At, p.pos.Line)
			}
			if !ok {
				p.Skipped++
			}
			for _, e := range evs {
				e.Seq = p.pos.Seq
				p.pos.Seq++
				events = append(events, e)
			}
		}
		p.pos.At += int64(nl + 1)
		p.pos.Line++
		data = data[nl+1:]
	}
}

// Pos returns where the parse stopped: the start of the first line it
// hasn't read.
func (p *Parser) Pos() Pos {
	return p.pos
}

// Session returns what the lines read so far say about the session.
func (p *Parser) Session() Session {
	s := p.lines.session()
	s.Harness = p.harness
	return s
}

// Latest folds a stream into one event per ID, in its latest state (a tool
// with its result) but where and when it was added: what a reader that
// doesn't follow updates shows. An update whose event the stream doesn't
// hold (its add came before a window) is left out.
func Latest(events []Event) []Event {
	var out []Event
	index := make(map[string]int, len(events))
	for _, e := range events {
		if i, ok := index[e.ID]; ok {
			added := out[i]
			e.Op, e.Seq, e.At, e.Line, e.TS = added.Op, added.Seq, added.At, added.Line, added.TS
			out[i] = e
			continue
		}
		if e.Op == OpUpdate {
			continue
		}
		index[e.ID] = len(out)
		out = append(out, e)
	}
	return out
}

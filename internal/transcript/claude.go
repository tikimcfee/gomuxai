package transcript

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"
)

// Claude reads Claude Code session transcripts
// (~/.claude/projects/<project>/<session id>.jsonl, and a subagent's
// <session id>/subagents/agent-<agent id>.jsonl).
type Claude struct{}

func (Claude) Harness() string { return "claude" }

func (Claude) NewParser(opts Options) *Parser {
	return newParser("claude", &claudeLines{opts: opts, tools: map[string]*Event{}}, opts.From)
}

// claudeRecord is the part of a transcript line the parser reads.
type claudeRecord struct {
	Type          string          `json:"type"`
	UUID          string          `json:"uuid"`
	Timestamp     string          `json:"timestamp"`
	SessionID     string          `json:"sessionId"`
	AgentID       string          `json:"agentId"`
	CWD           string          `json:"cwd"`
	Version       string          `json:"version"`
	GitBranch     string          `json:"gitBranch"`
	AITitle       string          `json:"aiTitle"`
	CustomTitle   string          `json:"customTitle"`
	AgentName     string          `json:"agentName"`
	ContinuedIn   string          `json:"continuedInSessionId"`
	PromptID      string          `json:"promptId"`
	IsSidechain   bool            `json:"isSidechain"`
	IsMeta        bool            `json:"isMeta"`
	ToolUseResult json.RawMessage `json:"toolUseResult"`
	Message       *struct {
		Role       string          `json:"role"`
		Model      string          `json:"model"`
		Content    json.RawMessage `json:"content"`
		StopReason string          `json:"stop_reason"`
	} `json:"message"`
}

type claudeBlock struct {
	Type      string          `json:"type"`
	Text      string          `json:"text"`
	Thinking  string          `json:"thinking"`
	ID        string          `json:"id"`
	Name      string          `json:"name"`
	Input     json.RawMessage `json:"input"`
	ToolUseID string          `json:"tool_use_id"`
	Content   json.RawMessage `json:"content"`
	IsError   bool            `json:"is_error"`
}

// blocks returns the record's content blocks, or nil when the content is
// a plain string (a typed prompt) or absent.
func (r *claudeRecord) blocks() []claudeBlock {
	if r.Message == nil {
		return nil
	}
	var blocks []claudeBlock
	if json.Unmarshal(r.Message.Content, &blocks) != nil {
		return nil
	}
	return blocks
}

// blockID is the id of the event block i of the record on line n makes:
// "<uuid>#<i>", or for a record without a uuid, "L<n>#<i>".
func (r *claudeRecord) blockID(n, i int) string {
	if r.UUID == "" {
		return fmt.Sprintf("L%d#%d", n, i)
	}
	return fmt.Sprintf("%s#%d", r.UUID, i)
}

// claudeLines is the Claude half of a Parser.
type claudeLines struct {
	opts Options
	sess Session
	turn *TurnRef
	// derived counts the turns derived from a transcript that records no
	// prompt ids.
	derived int
	// tools holds each call's latest event, for its updates.
	tools map[string]*Event
}

func (c *claudeLines) session() Session { return c.sess }

func (c *claudeLines) read(line []byte, at int64, n int) ([]Event, bool) {
	r := &claudeRecord{}
	if json.Unmarshal(line, r) != nil {
		return nil, false
	}
	// Before subagents had their own files (agentId), their records were
	// sidechains of the main transcript: another session's.
	if r.IsSidechain && r.AgentID == "" {
		return nil, true
	}
	c.harvest(r)
	if r.Message == nil || len(r.Message.Content) == 0 {
		return nil, true
	}
	ev := func(id, kind string) Event {
		return Event{ID: id, Op: OpAdd, At: at, Line: n, TS: r.Timestamp, Turn: c.turn, Agent: r.AgentID, Kind: kind}
	}
	var events []Event
	switch r.Type {
	case "user":
		blocks := r.blocks()
		for _, b := range blocks {
			if b.Type == "tool_result" {
				if e, ok := c.result(r, b, at, n); ok {
					events = append(events, e)
				}
			}
		}
		if text, ok := claudePrompt(r); ok {
			c.derived++
			id := r.blockID(n, 0)
			c.turn = &TurnRef{ID: id, Index: c.derived, Derived: true}
			if r.PromptID != "" {
				c.turn = &TurnRef{ID: r.PromptID}
			}
			e := ev(id, KindPrompt)
			e.Prompt = &PromptEvent{Text: text, Origin: OriginHuman}
			events = append(events, e)
		}
	case "assistant":
		for i, b := range r.blocks() {
			id := r.blockID(n, i)
			switch b.Type {
			case "thinking":
				e := ev(id, KindThink)
				e.Think = &ThinkEvent{Text: b.Thinking, Redacted: b.Thinking == ""}
				events = append(events, e)
			case "text":
				if strings.TrimSpace(b.Text) == "" {
					continue
				}
				e := ev(id, KindSay)
				e.Say = &SayEvent{Text: b.Text, Final: r.Message.StopReason == "end_turn"}
				events = append(events, e)
			case "tool_use":
				e := ev(b.ID, KindTool)
				e.Tool = &ToolEvent{Name: b.Name, Input: b.Input, Result: &ToolResult{Status: StatusPending},
					ToolSummary: SummarizeTool(b.Name, b.Input, nil, c.sess.CWD)}
				c.tools[b.ID] = &e
				events = append(events, e)
			}
		}
	}
	return events, true
}

// result returns the update a tool_result block makes to its call, if the
// parse has seen the call.
func (c *claudeLines) result(r *claudeRecord, b claudeBlock, at int64, n int) (Event, bool) {
	call := c.tools[b.ToolUseID]
	if call == nil {
		return Event{}, false
	}
	res := &ToolResult{Status: StatusOK, Output: resultText(b.Content)}
	if b.IsError {
		res.Status = StatusError
	}
	var response json.RawMessage
	if len(r.ToolUseResult) > 0 && r.ToolUseResult[0] == '{' {
		response = r.ToolUseResult
		res.Structured = response
	}
	t := *call.Tool
	t.Result = res
	t.ToolSummary = SummarizeTool(t.Name, t.Input, response, c.sess.CWD)
	e := *call
	e.Op, e.At, e.Line, e.TS, e.Tool = OpUpdate, at, n, r.Timestamp, &t
	c.tools[b.ToolUseID] = &e
	return e, true
}

// harvest records what the record says about the session: the latest
// value of each field wins, except that an AI title doesn't replace a
// custom one.
func (c *claudeLines) harvest(r *claudeRecord) {
	s := &c.sess
	switch r.Type {
	case "ai-title":
		if r.AITitle != "" && !s.customTitle {
			s.Title = r.AITitle
		}
	case "custom-title":
		if r.CustomTitle != "" {
			s.Title, s.customTitle = r.CustomTitle, true
		}
	case "agent-name":
		if r.AgentName != "" {
			s.AgentName = r.AgentName
		}
	case "continued-in":
		if r.ContinuedIn != "" {
			s.ContinuedIn = r.ContinuedIn
		}
	}
	switch {
	case r.AgentID != "":
		s.SessionID = "agent-" + r.AgentID
		if r.SessionID != "" {
			if s.ParentSession == nil {
				s.ParentSession = &ParentSession{}
			}
			s.ParentSession.SessionID = r.SessionID
		}
	case r.SessionID != "":
		s.SessionID = r.SessionID
	}
	s.setCWD(r.CWD)
	if r.Version != "" {
		s.Version, s.GitBranch = r.Version, r.GitBranch
	}
	if r.Type == "assistant" && r.Message != nil && r.Message.Model != "" && r.Message.Model != "<synthetic>" {
		s.Model = r.Message.Model
	}
}

// claudeSubagentTool returns the tool call that started the subagent whose
// transcript is at path, from the meta.json beside it, or "".
func claudeSubagentTool(path string) string {
	data, err := os.ReadFile(strings.TrimSuffix(path, ".jsonl") + ".meta.json")
	if err != nil {
		return ""
	}
	var meta struct {
		ToolUseID string `json:"toolUseId"`
	}
	json.Unmarshal(data, &meta)
	return meta.ToolUseID
}

// claudePrompt returns the prompt a user record carries, if it is one the
// user (or an agent sending to this one) typed. Typed prompts are plain
// string content; text blocks hold pasted content. Not prompts: isMeta
// records (caveats, image notes, reminders), echoes of local commands
// (<local-command-stdout>, <bash-input>, ...), other tagged text such as
// task notifications, and "[Request interrupted by user]". A slash command
// is one, and reads as typed: "/name args".
func claudePrompt(r *claudeRecord) (string, bool) {
	if r.IsMeta {
		return "", false
	}
	var text string
	if json.Unmarshal(r.Message.Content, &text) != nil {
		var parts []string
		for _, b := range r.blocks() {
			if b.Type == "text" {
				parts = append(parts, b.Text)
			}
		}
		text = strings.Join(parts, "\n")
	}
	text = strings.TrimSpace(text)
	if text == "" || strings.HasPrefix(text, "[Request interrupted") {
		return "", false
	}
	if strings.HasPrefix(text, "<command-name>") {
		name, ok := tagText(text, "command-name")
		if !ok {
			return "", false
		}
		if !strings.HasPrefix(name, "/") {
			name = "/" + name
		}
		if args, _ := tagText(text, "command-args"); args != "" {
			return name + " " + args, true
		}
		return name, true
	}
	if strings.HasPrefix(text, "<") {
		return "", false
	}
	return text, true
}

// tagText returns the trimmed text between <name> and </name> in s.
func tagText(s, name string) (string, bool) {
	_, rest, ok := strings.Cut(s, "<"+name+">")
	if !ok {
		return "", false
	}
	inner, _, ok := strings.Cut(rest, "</"+name+">")
	return strings.TrimSpace(inner), ok
}

// resultText flattens a tool_result's content (a string, or a list of
// blocks) to its text.
func resultText(content json.RawMessage) string {
	var s string
	if json.Unmarshal(content, &s) == nil {
		return s
	}
	var blocks []claudeBlock
	if json.Unmarshal(content, &blocks) != nil {
		return ""
	}
	var parts []string
	for _, b := range blocks {
		if b.Type == "text" {
			parts = append(parts, b.Text)
		}
	}
	return strings.Join(parts, "\n")
}

package transcript

import (
	"encoding/json"
	"strings"
)

// Claude reads Claude Code session transcripts
// (~/.claude/projects/<project>/<session id>.jsonl). Ported from
// glyph3d-rs claude.rs (parse_claude_session, user_prompt_text).
type Claude struct{}

func (Claude) Harness() string { return "claude" }

// claudeRecord is the part of a transcript line Parse reads. Lines of other
// types (attachment, system, mode, api-request-blob, ...) carry metadata
// only; api-request-blob even repeats messages, so only user and assistant
// records are read as conversation.
type claudeRecord struct {
	Type             string          `json:"type"`
	Timestamp        string          `json:"timestamp"`
	SessionID        string          `json:"sessionId"`
	CWD              string          `json:"cwd"`
	Version          string          `json:"version"`
	GitBranch        string          `json:"gitBranch"`
	AITitle          string          `json:"aiTitle"`
	IsSidechain      bool            `json:"isSidechain"`
	IsMeta           bool            `json:"isMeta"`
	IsCompactSummary bool            `json:"isCompactSummary"`
	ToolUseResult    json.RawMessage `json:"toolUseResult"`
	Message          *struct {
		Role    string          `json:"role"`
		Model   string          `json:"model"`
		Content json.RawMessage `json:"content"`
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

// isConversation reports whether the record is a message of the main
// conversation: not a subagent's (sidechain), and not a copy.
func (r *claudeRecord) isConversation() bool {
	return (r.Type == "user" || r.Type == "assistant") && !r.IsSidechain &&
		r.Message != nil && len(r.Message.Content) > 0
}

// Parse reads the transcript in two passes: tool results land on later
// lines than their calls, so the first pass pairs them by tool_use_id
// across the whole text, and the second emits each call, with its result,
// at its own position.
func (Claude) Parse(data []byte) *Session {
	s := &Session{Harness: "claude"}
	var records []*claudeRecord
	eachLine(s, data, func(line []byte) bool {
		r := &claudeRecord{}
		if json.Unmarshal(line, r) != nil {
			return false
		}
		if !r.IsSidechain {
			records = append(records, r)
		}
		return true
	})

	results := make(map[string]*ToolCall)
	for _, r := range records {
		if !r.isConversation() || r.Message.Role != "user" {
			continue
		}
		for _, b := range r.blocks() {
			if b.Type != "tool_result" || b.ToolUseID == "" {
				continue
			}
			res := &ToolCall{Status: StatusOK, Output: resultText(b.Content)}
			if res.Output == "" {
				res.Output = structuredText(r.ToolUseResult)
			}
			if b.IsError {
				res.Status = StatusError
			}
			results[b.ToolUseID] = res
		}
	}

	t := turns{s}
	for _, r := range records {
		// Metadata: the first value seen wins.
		for _, f := range []struct {
			dst *string
			src string
		}{
			{&s.SessionID, r.SessionID}, {&s.CWD, r.CWD}, {&s.Version, r.Version}, {&s.GitBranch, r.GitBranch},
		} {
			if *f.dst == "" {
				*f.dst = f.src
			}
		}
		if s.Title == "" && r.Type == "ai-title" {
			s.Title = r.AITitle
		}
		if !r.isConversation() {
			continue
		}
		at := parseTime(r.Timestamp)
		s.seen(at)
		switch r.Message.Role {
		case "user":
			if prompt, ok := claudePrompt(r); ok {
				t.prompt(prompt, at)
			}
		case "assistant":
			if s.Model == "" {
				s.Model = r.Message.Model
			}
			for _, b := range r.blocks() {
				switch b.Type {
				case "thinking":
					if text := strings.TrimSpace(b.Thinking); text != "" {
						t.step(Step{Time: at, Thinking: text})
					}
				case "text":
					if text := strings.TrimSpace(b.Text); text != "" {
						t.step(Step{Time: at, Reply: text})
					}
				case "tool_use":
					call := &ToolCall{ID: b.ID, Name: b.Name, Input: b.Input, Status: StatusNone}
					if res := results[b.ID]; res != nil {
						call.Status, call.Output = res.Status, res.Output
					}
					t.step(Step{Time: at, Tool: call})
				}
			}
		}
	}
	return s
}

// claudePrompt returns the prompt a user record carries, if it is one the
// user (or an agent sending to this one) typed. Typed prompts are plain
// string content; text blocks hold pasted content. Not prompts: isMeta
// records (caveats, image notes, reminders), compaction summaries, echoes
// of local commands (<local-command-stdout>, <bash-input>, ...), other
// tagged text such as task notifications, and "[Request interrupted by
// user]". A slash command is one, and reads as typed: "/name args".
func claudePrompt(r *claudeRecord) (string, bool) {
	if r.IsMeta || r.IsCompactSummary {
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

// structuredText returns the text of a structured toolUseResult: the
// string itself, or its first text field, as glyph3d-js toolRegistry.js
// pickText reads it.
func structuredText(raw json.RawMessage) string {
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	var fields map[string]any
	if json.Unmarshal(raw, &fields) != nil {
		return ""
	}
	if e, ok := fields["error"].(string); ok && e != "" {
		return "error: " + e
	}
	for _, key := range []string{"stdout", "content", "result", "output"} {
		if v, ok := fields[key].(string); ok && v != "" {
			return v
		}
	}
	return ""
}

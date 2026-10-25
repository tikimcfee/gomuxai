package transcript

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// Kimi reads Kimi Code's session logs
// (~/.kimi-code/sessions/<workspace>/session_<uuid>/agents/main/wire.jsonl,
// and a subagent's agents/<agent>/wire.jsonl). Each line is one record:
// turn.prompt (a prompt) and turn.steer (a message sent mid-turn),
// llm.request (a model call), and context.append_loop_event, whose event
// is a step's thinking or text (content.part), a tool call (tool.call) or
// its result (tool.result), in the order they happened. The session's
// title and working directory are in state.json beside agents/.
//
// No Kimi session was on the machines this was written on: the shapes come
// from the format survey in glyph3d-rs kimi.rs, glyph3d-js's Kimi tests,
// and testdata/corpus/kimi.
type Kimi struct{}

func (Kimi) Harness() string { return "kimi" }

func (Kimi) NewParser(opts Options) *Parser {
	k := &kimiLines{sess: opts.Session, turn: opts.Turn, tools: map[string]*Event{}, early: map[string]kimiLoopEvent{}}
	if opts.Session.ParentSession != nil {
		k.agent = opts.Session.SessionID
	}
	return newParser("kimi", k, opts.From)
}

// kimiRecord is the part of a wire.jsonl line the parser reads.
type kimiRecord struct {
	Type string `json:"type"`
	// Time is the record's time in epoch milliseconds.
	Time  *int64          `json:"time"`
	Input []kimiPart      `json:"input"`
	Text  json.RawMessage `json:"prompt"`
	Model string          `json:"model"`
	Event *kimiLoopEvent  `json:"event"`
}

type kimiPart struct {
	Type  string `json:"type"`
	Text  string `json:"text"`
	Think string `json:"think"`
}

type kimiLoopEvent struct {
	Type       string          `json:"type"`
	Part       *kimiPart       `json:"part"`
	ToolCallID string          `json:"toolCallId"`
	Name       string          `json:"name"`
	Args       json.RawMessage `json:"args"`
	Display    *struct {
		CWD string `json:"cwd"`
	} `json:"display"`
	Result json.RawMessage `json:"result"`
}

// kimiLines is the Kimi half of a Parser.
type kimiLines struct {
	sess Session
	// agent is the Agent of every event, in a subagent's log.
	agent string
	turn  *TurnRef
	// turns counts the prompts that opened a turn.
	turns int
	// tools holds each call's latest event, for its result; early, the
	// results that came before their call.
	tools map[string]*Event
	early map[string]kimiLoopEvent
}

func (k *kimiLines) session() Session { return k.sess }

func (k *kimiLines) read(line []byte, at int64, n int) ([]Event, bool) {
	var r kimiRecord
	if json.Unmarshal(line, &r) != nil {
		return nil, false
	}
	ts := ""
	if r.Time != nil {
		ts = time.UnixMilli(*r.Time).UTC().Format("2006-01-02T15:04:05.000Z")
	}
	event := func(id, kind string) Event {
		if id == "" {
			id = fmt.Sprintf("L%d#0", n)
		}
		return Event{ID: id, Op: OpAdd, At: at, Line: n, TS: ts, Turn: k.turn, Agent: k.agent, Kind: kind}
	}
	var events []Event
	switch r.Type {
	case "llm.request":
		if r.Model != "" {
			k.sess.Model = r.Model
		}
	case "turn.prompt", "turn.steer":
		text := r.promptText()
		if text == "" {
			break
		}
		e := event("", KindPrompt)
		e.Prompt = &PromptEvent{Text: text, Origin: OriginHuman}
		if k.agent != "" {
			// A subagent's prompts are its parent's.
			e.Prompt.Origin = OriginPeer
		}
		if r.Type == "turn.steer" {
			e.Prompt.Origin = OriginQueued
		} else {
			k.turns++
			k.turn = &TurnRef{ID: e.ID, Index: k.turns, Derived: true}
			e.Turn = k.turn
		}
		events = append(events, e)
	case "context.append_loop_event":
		if r.Event != nil {
			events = k.loopEvent(*r.Event, event)
		}
	}
	return events, true
}

// promptText is a turn.prompt's text: its input's text parts, or the
// prompt string older logs record.
func (r *kimiRecord) promptText() string {
	var parts []string
	for _, p := range r.Input {
		if p.Type == "text" && p.Text != "" {
			parts = append(parts, p.Text)
		}
	}
	if len(parts) == 0 {
		var s string
		json.Unmarshal(r.Text, &s)
		return strings.TrimSpace(s)
	}
	return strings.Join(parts, "\n")
}

// loopEvent reads a context.append_loop_event's event: thinking, text, a
// call, or a call's result.
func (k *kimiLines) loopEvent(ev kimiLoopEvent, event func(id, kind string) Event) []Event {
	switch ev.Type {
	case "content.part":
		switch p := ev.Part; {
		case p == nil:
		case p.Type == "think" && strings.TrimSpace(p.Think) != "":
			e := event("", KindThink)
			e.Think = &ThinkEvent{Text: p.Think}
			return []Event{e}
		case p.Type == "text" && strings.TrimSpace(p.Text) != "":
			e := event("", KindSay)
			e.Say = &SayEvent{Text: p.Text}
			return []Event{e}
		}
	case "tool.call":
		if k.sess.CWD == "" && ev.Display != nil {
			k.sess.setCWD(ev.Display.CWD)
		}
		e := event(ev.ToolCallID, KindTool)
		e.Tool = &ToolEvent{Name: ev.Name, Input: ev.Args, Result: &ToolResult{Status: StatusPending},
			ToolSummary: SummarizeTool(ev.Name, ev.Args, nil, k.sess.CWD)}
		k.tools[e.ID] = &e
		events := []Event{e}
		if res, ok := k.early[e.ID]; ok {
			delete(k.early, e.ID)
			events = append(events, k.result(res, event))
		}
		return events
	case "tool.result":
		if k.tools[ev.ToolCallID] == nil {
			// Kimi may log a result before its call.
			if ev.ToolCallID != "" {
				k.early[ev.ToolCallID] = ev
			}
			return nil
		}
		return []Event{k.result(ev, event)}
	}
	return nil
}

// result makes the update a tool.result makes to its call.
func (k *kimiLines) result(ev kimiLoopEvent, event func(id, kind string) Event) Event {
	call := k.tools[ev.ToolCallID]
	var result struct {
		Output  json.RawMessage `json:"output"`
		IsError bool            `json:"isError"`
	}
	json.Unmarshal(ev.Result, &result)
	res := &ToolResult{Status: StatusOK, Output: kimiOutput(result.Output)}
	if result.IsError {
		res.Status = StatusError
	}
	t := *call.Tool
	t.Result = res
	t.ToolSummary = SummarizeTool(t.Name, t.Input, kimiResponse(t.Name, t.Input, ev.Result, res.Output), k.sess.CWD)
	e := *call
	u := event(call.ID, KindTool)
	e.Op, e.At, e.Line, e.TS, e.Tool = OpUpdate, u.At, u.Line, u.TS, &t
	k.tools[call.ID] = &e
	return e
}

// kimiOutput flattens a result's output (a string, or a list of text
// parts) to its text; any other value is kept as its JSON.
func kimiOutput(raw json.RawMessage) string {
	if len(raw) == 0 || string(raw) == "null" {
		return ""
	}
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	var parts []kimiPart
	if json.Unmarshal(raw, &parts) == nil {
		var texts []string
		for _, p := range parts {
			if p.Type == "text" {
				texts = append(texts, p.Text)
			}
		}
		return strings.Join(texts, "\n")
	}
	return string(raw)
}

// kimiResponse is the structured response the tool table reads from a
// Kimi result, as glyph3d-js's kimiResponse builds it: the result object,
// plus a Read's file {numLines, startLine} counted from its output (Kimi
// numbers each line "N\t") and its line_offset.
func kimiResponse(name string, input, result json.RawMessage, output string) json.RawMessage {
	r := parseObject(result)
	if r == nil || name != "Read" || r["file"] != nil {
		return result
	}
	start := 1
	if n, ok := parseObject(input).num("line_offset"); ok && n > 0 {
		start = n
	}
	r["file"] = map[string]int{"numLines": countLines(output), "startLine": start}
	out, _ := json.Marshal(r)
	return out
}

// kimiFileOptions reads what Kimi keeps beside the log at path,
// session_<uuid>/agents/<agent>/wire.jsonl: the session's title and
// working directory (state.json) and, for a subagent's log (any agent but
// main), the session it belongs to. It returns no options for a path of
// another shape.
func kimiFileOptions(path string) Options {
	agentDir := filepath.Dir(path)
	if filepath.Base(path) != "wire.jsonl" || filepath.Base(filepath.Dir(agentDir)) != "agents" {
		return Options{}
	}
	sessionDir := filepath.Dir(filepath.Dir(agentDir))
	s := Session{SessionID: sessionIDFromPath(path)}
	main := filepath.Base(agentDir) == "main"
	if !main {
		s.ParentSession = &ParentSession{SessionID: strings.TrimPrefix(filepath.Base(sessionDir), "session_")}
	}
	if data, err := os.ReadFile(filepath.Join(sessionDir, "state.json")); err == nil {
		var state struct {
			Title      string `json:"title"`
			LastPrompt string `json:"lastPrompt"`
			WorkDir    string `json:"workDir"`
			CWD        string `json:"cwd"`
		}
		if json.Unmarshal(data, &state) == nil {
			// The title is the main agent's conversation's.
			if main {
				s.Title = strings.TrimSpace(state.Title)
				if s.Title == "" {
					s.Title = strings.TrimSpace(state.LastPrompt)
				}
			}
			if cwd := strings.TrimSpace(state.WorkDir); cwd != "" {
				s.setCWD(cwd)
			} else {
				s.setCWD(strings.TrimSpace(state.CWD))
			}
		}
	}
	return Options{Session: s}
}

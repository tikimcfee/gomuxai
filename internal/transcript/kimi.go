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
// and testdata/corpus/kimi. So every field is read leniently, as those
// readers do: a field of a type it doesn't expect reads as absent, and
// never costs the rest of its line.
type Kimi struct{}

func (Kimi) Harness() string { return "kimi" }

func (Kimi) NewParser(opts Options) *Parser {
	k := &kimiLines{sess: opts.Session, turn: opts.Turn, tools: map[string]*Event{}, calls: map[string]string{}, early: map[string]kimiEvent{}}
	if opts.Session.ParentSession != nil {
		k.agent = opts.Session.SessionID
	}
	return newParser("kimi", k)
}

// kimiFields is a JSON object's fields, read one at a time.
type kimiFields map[string]json.RawMessage

func kimiObject(raw json.RawMessage) kimiFields {
	var f kimiFields
	json.Unmarshal(raw, &f)
	return f
}

// str returns the field key's string, or "" if it isn't one.
func (f kimiFields) str(key string) string {
	var s string
	json.Unmarshal(f[key], &s)
	return s
}

// kimiEvent is a context.append_loop_event's event.
type kimiEvent struct {
	typ string
	// part is a content.part's {type, think|text}.
	part kimiFields
	// callID is a call's toolCallId (else its uuid); name, args and cwd
	// (display.cwd) a tool.call's; result a tool.result's.
	callID, name, cwd string
	args, result      json.RawMessage
}

func readKimiEvent(raw json.RawMessage) kimiEvent {
	f := kimiObject(raw)
	ev := kimiEvent{typ: f.str("type"), part: kimiObject(f["part"]), callID: f.str("toolCallId"), name: f.str("name"),
		cwd: kimiObject(f["display"]).str("cwd"), args: f["args"], result: f["result"]}
	if ev.callID == "" {
		ev.callID = f.str("uuid")
	}
	return ev
}

// kimiLines is the Kimi half of a Parser.
type kimiLines struct {
	sess Session
	// agent is the Agent of every event, in a subagent's log.
	agent string
	turn  *TurnRef
	// turns counts the prompts that opened a turn.
	turns int
	// tools holds each call's latest event, by event id, and calls the
	// id of the latest call with each toolCallId (Kimi numbers calls per
	// response, functions.Read:0, so an id comes back); early holds the
	// results that came before their call.
	tools map[string]*Event
	calls map[string]string
	early map[string]kimiEvent
}

func (k *kimiLines) session() Session { return k.sess }

func (k *kimiLines) read(line []byte, at int64, n int) ([]Event, bool) {
	var r kimiFields
	if json.Unmarshal(line, &r) != nil {
		return nil, false
	}
	ts := ""
	var ms float64
	if json.Unmarshal(r["time"], &ms) == nil {
		ts = time.UnixMilli(int64(ms)).UTC().Format("2006-01-02T15:04:05.000Z")
	}
	event := func(id, kind string) Event {
		if id == "" {
			id = fmt.Sprintf("L%d#0", n)
		}
		return Event{ID: id, Op: OpAdd, At: at, Line: n, TS: ts, Turn: k.turn, Agent: k.agent, Kind: kind}
	}
	var events []Event
	switch typ := r.str("type"); typ {
	case "llm.request":
		k.sess.setModel(r.str("model"), at)
	case "turn.prompt", "turn.steer":
		text := kimiPrompt(r)
		if text == "" {
			break
		}
		e := event("", KindPrompt)
		e.Prompt = &PromptEvent{Text: text, Origin: OriginHuman}
		if k.agent != "" {
			// A subagent's prompts are its parent's.
			e.Prompt.Origin = OriginPeer
		}
		if typ == "turn.steer" {
			e.Prompt.Origin = OriginQueued
		} else {
			k.turns++
			k.turn = &TurnRef{ID: e.ID, Index: k.turns, Derived: true}
			e.Turn = k.turn
		}
		events = append(events, e)
	case "context.append_loop_event":
		events = k.loopEvent(readKimiEvent(r["event"]), at, n, event)
	}
	return events, true
}

// kimiPrompt is a turn.prompt's text, trimmed: its input's text parts
// (or input itself, a string), or the prompt string older logs record.
func kimiPrompt(r kimiFields) string {
	var parts []string
	var input []json.RawMessage
	if json.Unmarshal(r["input"], &input) == nil {
		for _, raw := range input {
			if p := kimiObject(raw); p.str("type") == "text" && p.str("text") != "" {
				parts = append(parts, p.str("text"))
			}
		}
	} else if s := r.str("input"); s != "" {
		parts = append(parts, s)
	}
	if len(parts) == 0 {
		return strings.TrimSpace(r.str("prompt"))
	}
	return strings.TrimSpace(strings.Join(parts, "\n"))
}

// loopEvent reads a context.append_loop_event's event, on line n at byte
// offset at: thinking, text, a call, or a call's result.
func (k *kimiLines) loopEvent(ev kimiEvent, at int64, n int, event func(id, kind string) Event) []Event {
	switch ev.typ {
	case "content.part":
		switch think, text := ev.part.str("think"), ev.part.str("text"); {
		case ev.part.str("type") == "think" && strings.TrimSpace(think) != "":
			e := event("", KindThink)
			e.Think = &ThinkEvent{Text: think}
			return []Event{e}
		case ev.part.str("type") == "text" && strings.TrimSpace(text) != "":
			e := event("", KindSay)
			e.Say = &SayEvent{Text: text}
			return []Event{e}
		}
	case "tool.call":
		k.sess.setCWD(ev.cwd, at)
		id := ev.callID
		if id != "" && k.tools[id] != nil {
			// A toolCallId used before: this call's event needs an id of
			// its own.
			id = fmt.Sprintf("%s@L%d", id, n)
		}
		e := event(id, KindTool)
		e.Tool = &ToolEvent{Name: ev.name, Input: ev.args, Result: &ToolResult{Status: StatusPending},
			ToolSummary: SummarizeTool(ev.name, ev.args, nil, k.sess.CWD)}
		k.tools[e.ID] = &e
		if ev.callID == "" {
			return []Event{e}
		}
		k.calls[ev.callID] = e.ID
		events := []Event{e}
		if res, ok := k.early[ev.callID]; ok {
			delete(k.early, ev.callID)
			events = append(events, k.result(&e, res, event))
		}
		return events
	case "tool.result":
		call := k.tools[k.calls[ev.callID]]
		if call == nil || call.Tool.Result.Status != StatusPending {
			// Kimi may log a result before its call.
			if ev.callID != "" {
				k.early[ev.callID] = ev
			}
			return nil
		}
		return []Event{k.result(call, ev, event)}
	}
	return nil
}

// result makes the update a tool.result makes to call.
func (k *kimiLines) result(call *Event, ev kimiEvent, event func(id, kind string) Event) Event {
	r := kimiObject(ev.result)
	res := &ToolResult{Status: StatusOK, Output: kimiOutput(r["output"])}
	var isError bool
	if json.Unmarshal(r["isError"], &isError) == nil && isError {
		res.Status = StatusError
	}
	t := *call.Tool
	t.Result = res
	t.ToolSummary = SummarizeTool(t.Name, t.Input, kimiResponse(t.Name, t.Input, ev.result, res.Output), k.sess.CWD)
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
	var parts []json.RawMessage
	if json.Unmarshal(raw, &parts) == nil {
		var texts []string
		for _, p := range parts {
			if f := kimiObject(p); f.str("type") == "text" {
				texts = append(texts, f.str("text"))
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
	s := Session{SessionID: SessionIDFromPath(path)}
	main := filepath.Base(agentDir) == "main"
	if !main {
		s.ParentSession = &ParentSession{SessionID: strings.TrimPrefix(filepath.Base(sessionDir), "session_")}
	}
	if data, err := os.ReadFile(filepath.Join(sessionDir, "state.json")); err == nil {
		state := kimiObject(data)
		// The title is the main agent's conversation's.
		if main {
			s.Title = strings.TrimSpace(state.str("title"))
			if s.Title == "" {
				s.Title = strings.TrimSpace(state.str("lastPrompt"))
			}
		}
		if cwd := strings.TrimSpace(state.str("workDir")); cwd != "" {
			s.setCWD(cwd, 0)
		} else {
			s.setCWD(strings.TrimSpace(state.str("cwd")), 0)
		}
	}
	return Options{Session: s}
}

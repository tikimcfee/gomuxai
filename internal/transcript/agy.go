package transcript

import (
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
)

// Agy reads the Antigravity CLI's conversation transcripts
// (~/.gemini/antigravity-cli/brain/<conversation id>/.system_generated/logs/transcript_full.jsonl).
// Ported from glyph3d-rs antigravity.rs (parse_antigravity_session).
//
// Each line is one step: USER_INPUT (a prompt), PLANNER_RESPONSE (the
// model's thinking, text, and tool calls), then one GENERIC step per tool
// call, in order, holding its output. SYSTEM_MESSAGE and CHECKPOINT steps
// are left out.
type Agy struct{}

func (Agy) Harness() string { return "agy" }

type agyStep struct {
	StepIndex int    `json:"step_index"`
	Type      string `json:"type"`
	Status    string `json:"status"`
	CreatedAt string `json:"created_at"`
	Content   string `json:"content"`
	Thinking  string `json:"thinking"`
	Error     string `json:"error"`
	CWD       string `json:"cwd"`
	ToolCalls []struct {
		Name string          `json:"name"`
		Args json.RawMessage `json:"args"`
	} `json:"tool_calls"`
}

func (Agy) NewParser(opts Options) *Parser {
	return newParser("agy", &agyLines{}, opts.From)
}

// agyLines is the agy half of a Parser.
type agyLines struct {
	sess Session
	turn *TurnRef
	// turns counts the prompts that opened a turn.
	turns int
	// pending holds the current turn's calls still waiting for their
	// GENERIC step, oldest first.
	pending []*Event
}

func (a *agyLines) session() Session { return a.sess }

func (a *agyLines) read(line []byte, at int64, n int) ([]Event, bool) {
	var st agyStep
	if json.Unmarshal(line, &st) != nil {
		return nil, false
	}
	if a.sess.CWD == "" {
		a.sess.setCWD(st.CWD)
	}
	part := 0
	ev := func(kind string) Event {
		e := Event{ID: fmt.Sprintf("step_%d#%d", st.StepIndex, part), Op: OpAdd, At: at, Line: n, TS: st.CreatedAt, Turn: a.turn, Kind: kind}
		part++
		return e
	}
	var events []Event
	switch st.Type {
	case "USER_INPUT":
		if prompt := agyPrompt(st.Content); prompt != "" {
			e := ev(KindPrompt)
			a.turns++
			a.turn = &TurnRef{ID: e.ID, Index: a.turns, Derived: true}
			e.Turn = a.turn
			e.Prompt = &PromptEvent{Text: prompt, Origin: OriginHuman}
			events = append(events, e)
			a.pending = nil
		}
	case "PLANNER_RESPONSE":
		if text := strings.TrimSpace(st.Thinking); text != "" {
			e := ev(KindThink)
			e.Think = &ThinkEvent{Text: text}
			events = append(events, e)
		}
		if text := strings.TrimSpace(st.Content); text != "" {
			e := ev(KindSay)
			e.Say = &SayEvent{Text: text, Final: len(st.ToolCalls) == 0}
			events = append(events, e)
		}
		for i, tc := range st.ToolCalls {
			e := ev(KindTool)
			e.ID = fmt.Sprintf("step_%d_tool_%d", st.StepIndex, i)
			input := agyArgs(tc.Args)
			e.Tool = &ToolEvent{Name: tc.Name, Input: input, Result: &ToolResult{Status: StatusPending},
				ToolSummary: SummarizeTool(tc.Name, input, nil, a.sess.CWD)}
			events = append(events, e)
			a.pending = append(a.pending, &e)
		}
	case "GENERIC":
		if len(a.pending) == 0 {
			break
		}
		call := a.pending[0]
		a.pending = a.pending[1:]
		output := agyOutput(st.Content)
		t := *call.Tool
		t.Result = &ToolResult{Status: agyStatus(st, output), Output: output}
		e := *call
		e.Op, e.At, e.Line, e.TS, e.Tool = OpUpdate, at, n, st.CreatedAt, &t
		events = append(events, e)
	}
	return events, true
}

// agyArgs undoes the encoding transcript.jsonl gives a call's args: there
// every value is a string holding its JSON ("\"ls\"", "5000", "false",
// "[...]"), while transcript_full.jsonl stores them plain. A call whose
// every arg is a string of valid JSON is taken as encoded and decoded once;
// any other is left as it is, so a plain search for "\"exact phrase\"" keeps
// its quotes.
func agyArgs(args json.RawMessage) json.RawMessage {
	var fields map[string]json.RawMessage
	if json.Unmarshal(args, &fields) != nil || len(fields) == 0 {
		return args
	}
	decoded := make(map[string]json.RawMessage, len(fields))
	for k, v := range fields {
		var s string
		if json.Unmarshal(v, &s) != nil || !json.Valid([]byte(s)) {
			return args
		}
		decoded[k] = json.RawMessage(s)
	}
	out, err := json.Marshal(decoded)
	if err != nil {
		return args
	}
	return out
}

// agyPrompt returns what the user typed: USER_INPUT content wraps it in
// <USER_REQUEST> and appends <ADDITIONAL_METADATA> (time, settings changes).
func agyPrompt(content string) string {
	if req, ok := tagText(content, "USER_REQUEST"); ok {
		return req
	}
	return strings.TrimSpace(content)
}

// agyOutput returns a tool's output without the "Created At:" and
// "Completed At:" lines agy puts first.
func agyOutput(content string) string {
	rest := content
	for {
		line, after, _ := strings.Cut(rest, "\n")
		if !strings.HasPrefix(line, "Created At:") && !strings.HasPrefix(line, "Completed At:") {
			break
		}
		rest = after
	}
	return strings.TrimSpace(rest)
}

// agyExitCode matches run_command's "The command exited with code N."
var agyExitCode = regexp.MustCompile(`(?m)^(?:The c|C)ommand exited with code (\d+)\.`)

// agyStatus is a tool step's result: agy marks failed steps ERROR (or
// INVALID, for bad arguments) and backgrounded ones RUNNING. A command that
// exits non-zero counts as failed, as Claude Code's Bash does.
func agyStatus(st agyStep, output string) string {
	switch {
	case st.Status == "ERROR" || st.Status == "INVALID" || st.Error != "":
		return StatusError
	case st.Status == "RUNNING":
		return StatusBackground
	}
	if m := agyExitCode.FindStringSubmatch(output); m != nil && m[1] != "0" {
		return StatusError
	}
	return StatusOK
}

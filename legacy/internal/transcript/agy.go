package transcript

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Agy reads the Antigravity CLI's conversation transcripts
// (~/.gemini/antigravity-cli/brain/<conversation id>/.system_generated/logs/transcript_full.jsonl).
// Each line is one step: USER_INPUT (a prompt), PLANNER_RESPONSE (the
// model's thinking, text and tool calls), then one GENERIC step per tool
// call, in order, holding its result. SYSTEM_MESSAGE steps carry messages
// from background tasks and other conversations, CHECKPOINT a compaction
// summary. testdata/corpus/README.md lists the shapes and the decisions
// this reader follows.
type Agy struct{}

func (Agy) Harness() string { return "agy" }

type agyStep struct {
	StepIndex int    `json:"step_index"`
	Source    string `json:"source"`
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

// NewParser returns a parser for an agy transcript. Which of agy's two
// logs it reads comes from opts.Session.Path (FileOptions sets it):
// transcript.jsonl encodes every call's args (agyArgs); transcript_full.jsonl,
// and a parse that names no file, has them plain.
func (Agy) NewParser(opts Options) *Parser {
	a := &agyLines{sess: opts.Session, turn: opts.Turn, tools: map[string]*Event{}, prev: -1, highest: -1,
		encoded: filepath.Base(opts.Session.Path) == "transcript.jsonl"}
	a.sess.Path = ""
	if opts.Session.ParentSession != nil {
		a.agent = opts.Session.SessionID
	}
	return newParser("agy", a)
}

// agyLines is the agy half of a Parser.
type agyLines struct {
	sess Session
	// agent is the Agent of every event, on a subagent's conversation.
	agent string
	turn  *TurnRef
	// turns counts the prompts that opened a turn.
	turns int
	// encoded is set for transcript.jsonl, whose args are encoded.
	encoded bool
	// prev and highest are the step indexes of the last step and the
	// highest so far (-1 before the first); rewinds counts the times the
	// index went back (agy rewrote steps), which the ids of the steps
	// whose index came before carry.
	prev, highest, rewinds int
	// tools holds each call's latest event, for its updates. planner is
	// the latest PLANNER_RESPONSE's step index, and calls its calls' ids
	// by position, "" once a GENERIC step answered one. running holds the
	// calls sent to the background, oldest first.
	tools   map[string]*Event
	planner int
	calls   []string
	running []agyRunning
}

// agyRunning is a call agy runs as a background task.
type agyRunning struct{ task, call string }

func (a *agyLines) session() Session { return a.sess }

// agyLine is what reading one step makes: events it adds, then the
// updates it makes to earlier ones.
type agyLine struct {
	a  *agyLines
	st *agyStep
	// step names the step in ids: its index, and after a rewind, which
	// time it is that index ("347.2").
	step    string
	at      int64
	n       int
	adds    []Event
	updates []Event
}

// event starts an event of the step's, its id "step_<step>#<part>".
func (l *agyLine) event(part int, kind string) Event {
	id := fmt.Sprintf("step_%s#%d", l.step, part)
	return Event{ID: id, Op: OpAdd, At: l.at, Line: l.n, TS: l.st.CreatedAt, Turn: l.a.turn, Agent: l.a.agent, Kind: kind}
}

func (l *agyLine) add(e Event) { l.adds = append(l.adds, e) }

func (a *agyLines) read(line []byte, at int64, n int) ([]Event, bool) {
	var st agyStep
	if json.Unmarshal(line, &st) != nil {
		return nil, false
	}
	a.sess.setCWD(st.CWD, at)
	l := &agyLine{a: a, st: &st, step: strconv.Itoa(st.StepIndex), at: at, n: n}
	if a.prev >= 0 && st.StepIndex <= a.prev {
		a.rewinds++
	}
	if st.StepIndex <= a.highest {
		l.step += "." + strconv.Itoa(a.rewinds+1)
	}
	a.prev, a.highest = st.StepIndex, max(a.highest, st.StepIndex)

	switch st.Type {
	case "USER_INPUT":
		l.userInput()
	case "PLANNER_RESPONSE":
		l.planner()
	case "GENERIC":
		l.generic()
	case "SYSTEM_MESSAGE":
		l.systemMessage()
	case "EPHEMERAL_MESSAGE":
		if text := strings.TrimSpace(st.Content); text != "" {
			e := l.event(0, KindPrompt)
			e.Prompt = &PromptEvent{Text: text, Origin: agyInjected(text)}
			l.add(e)
		}
	case "CHECKPOINT":
		if strings.TrimSpace(st.Content) != "" {
			e := l.event(0, KindSystem)
			e.System = &SystemEvent{Subtype: "compact_summary", Text: st.Content}
			l.add(e)
		}
	case "ERROR_MESSAGE":
		e := l.event(0, KindSystem)
		e.System = &SystemEvent{Subtype: "api_error", Text: st.Content}
		if st.Error != "" {
			e.System.Data, _ = json.Marshal(map[string]string{"error": st.Error})
		}
		l.add(e)
	}
	return append(l.adds, l.updates...), true
}

// agyModel matches the settings change a USER_INPUT records when the
// person picks a model.
var agyModel = regexp.MustCompile("setting `Model Selection` from .*? to (.+?)\\. ")

// userInput reads a USER_INPUT step: a typed prompt, which opens a turn
// and abandons the calls still waiting for a result, or a message a hook
// injected mid-turn (source SYSTEM_SDK: gomuxai's held delivery).
func (l *agyLine) userInput() {
	if m := agyModel.FindStringSubmatch(l.st.Content); m != nil {
		l.a.sess.setModel(m[1], l.at)
	}
	text := agyPrompt(l.st.Content)
	if text == "" {
		return
	}
	e := l.event(0, KindPrompt)
	e.Prompt = &PromptEvent{Text: text, Origin: OriginHuman}
	if l.st.Source == "SYSTEM_SDK" {
		e.Prompt.Origin = agyInjected(text)
		l.add(e)
		return
	}
	l.a.turns++
	l.a.turn = &TurnRef{ID: e.ID, Index: l.a.turns, Derived: true}
	e.Turn = l.a.turn
	l.add(e)
	l.abandon()
}

// abandon marks the latest step's calls that no GENERIC step answered
// interrupted: a new prompt or model response means they never ran to
// their end.
func (l *agyLine) abandon() {
	for _, id := range l.a.calls {
		if id != "" {
			l.update(l.a.tools[id], &ToolResult{Status: StatusInterrupted}, nil, nil)
		}
	}
	l.a.calls = nil
}

// agyInjected is the origin of a message a hook injected: gomuxai's held
// delivery, whose messages start with their sender ("[from lead@hub] ..."),
// or the harness's own.
func agyInjected(text string) string {
	if strings.HasPrefix(text, "[from ") {
		return OriginDelivered
	}
	return OriginSystem
}

// planner reads a PLANNER_RESPONSE step: thinking, then text, then the
// calls, each answered by the GENERIC step its position names.
func (l *agyLine) planner() {
	st := l.st
	part := 0
	if strings.TrimSpace(st.Thinking) != "" {
		e := l.event(part, KindThink)
		e.Think = &ThinkEvent{Text: st.Thinking}
		l.add(e)
		part++
	}
	if strings.TrimSpace(st.Content) != "" {
		e := l.event(part, KindSay)
		e.Say = &SayEvent{Text: st.Content, Final: len(st.ToolCalls) == 0}
		l.add(e)
	}
	l.abandon()
	l.a.planner = st.StepIndex
	for j, tc := range st.ToolCalls {
		input := agyArgs(tc.Args, l.a.encoded)
		if l.a.sess.CWD == "" && tc.Name == "run_command" {
			var args struct{ Cwd string }
			json.Unmarshal(input, &args)
			l.a.sess.setCWD(args.Cwd, l.at)
		}
		e := l.event(0, KindTool)
		e.ID = fmt.Sprintf("step_%s_tool_%d", l.step, j)
		e.Tool = &ToolEvent{Name: tc.Name, Input: input, Result: &ToolResult{Status: StatusPending},
			ToolSummary: SummarizeTool(tc.Name, input, nil, l.a.sess.CWD)}
		l.add(e)
		l.a.tools[e.ID] = &e
		l.a.calls = append(l.a.calls, e.ID)
	}
}

// agyTaskID matches the task id in a RUNNING step's notice.
var agyTaskID = regexp.MustCompile(`background task with task id: (\S+)`)

// agyKilled matches manage_task kill's result.
var agyKilled = regexp.MustCompile(`^Task "([^"]+)" cancelled\.`)

// generic reads a GENERIC step: the result of a call of the latest
// PLANNER_RESPONSE, the one at the step's distance from it (the GENERIC
// right after the planner answers its first call). A result whose call
// came before the window, or that no call is waiting for, is dropped.
func (l *agyLine) generic() {
	j := l.st.StepIndex - l.a.planner - 1
	if j < 0 || j >= len(l.a.calls) || l.a.calls[j] == "" {
		return
	}
	id := l.a.calls[j]
	l.a.calls[j] = ""
	call := l.a.tools[id]
	output := agyOutput(l.st.Content)
	res := &ToolResult{Status: agyStatus(l.st, output), Output: output}
	var response json.RawMessage
	switch {
	case res.Status == StatusBackground:
		// While a call runs, the table has no response to read.
		if m := agyTaskID.FindStringSubmatch(output); m != nil {
			l.a.running = append(l.a.running, agyRunning{task: m[1], call: id})
		}
	case l.st.Status == "ERROR" || l.st.Status == "INVALID" || l.st.Error != "":
		response, _ = json.Marshal(map[string]string{"output": output})
	default:
		response = agyResponse(call.Tool.Name, call.Tool.Input, output)
	}
	var subs []string
	if call.Tool.Name == "invoke_subagent" {
		subs = agySubagentIDs(output)
	}
	l.update(call, res, response, subs)
	if m := agyKilled.FindStringSubmatch(output); m != nil && call.Tool.Name == "manage_task" && res.Status == StatusOK {
		l.end(m[1], &ToolResult{Status: StatusInterrupted}, nil)
	}
}

// update makes the update of call to res, its table fields read from
// response; subs, if any, name the sessions the call started.
func (l *agyLine) update(call *Event, res *ToolResult, response json.RawMessage, subs []string) {
	t := *call.Tool
	t.Result = res
	t.ToolSummary = SummarizeTool(t.Name, t.Input, response, l.a.sess.CWD)
	if len(subs) > 0 {
		t.SubSessions = subs
	}
	e := *call
	e.Op, e.At, e.Line, e.TS, e.Tool = OpUpdate, l.at, l.n, l.st.CreatedAt, &t
	l.updates = append(l.updates, e)
	l.a.tools[call.ID] = &e
}

// running returns the background call running task, or nil.
func (a *agyLines) runningCall(task string) *Event {
	for _, r := range a.running {
		if r.task == task {
			return a.tools[r.call]
		}
	}
	return nil
}

// end ends the background call running task with res, if there is one.
func (l *agyLine) end(task string, res *ToolResult, response json.RawMessage) {
	i := slices.IndexFunc(l.a.running, func(r agyRunning) bool { return r.task == task })
	if i < 0 {
		return
	}
	call := l.a.tools[l.a.running[i].call]
	l.a.running = slices.Delete(l.a.running, i, i+1)
	l.update(call, res, response, nil)
}

// agyTaskSender matches the sender of a background task's messages, the
// task's id: "<conversation id>/task-6".
var agyTaskSender = regexp.MustCompile(`/task-\d+$`)

// agyTaskDone matches a background task's end: it finished, or someone
// canceled it.
var agyTaskDone = regexp.MustCompile(`^Task id "([^"]+)" (finished|was canceled) with result:\n?((?s).*)$`)

// agyRestart starts the notice agy sends when a restart stopped every
// background task.
const agyRestart = "[Notice] All your subagents and background tasks have been stopped"

// agyHookPreamble starts a message a hook injected as a system message.
var agyHookPreamble = regexp.MustCompile(`^The following system message was injected by an? \w+ hook:\s*`)

// systemMessage reads a SYSTEM_MESSAGE step: each [Message] entry is a
// prompt, from a background task, another conversation or agy itself
// (sender=system). A message from a background task ends its call: a
// task's end (ok, error if its command exited non-zero, interrupted if it
// was canceled), or a timer's firing (a schedule call's Prompt, ok). A
// restart notice interrupts every background call.
func (l *agyLine) systemMessage() {
	for k, m := range agySystemEntries(l.st.Content) {
		e := l.event(k, KindPrompt)
		e.Prompt = &PromptEvent{Text: m.text, Origin: OriginSystem}
		switch {
		case m.sender == "":
			if loc := agyHookPreamble.FindStringIndex(m.text); loc != nil {
				e.Prompt.Text = m.text[loc[1]:]
				e.Prompt.Origin = agyInjected(e.Prompt.Text)
			}
		case m.sender == "system":
		case agyTaskSender.MatchString(m.sender):
			e.Prompt.Origin = OriginTaskNotification
		default:
			e.Prompt.Origin = OriginPeer
		}
		l.add(e)

		if done := agyTaskDone.FindStringSubmatch(m.text); done != nil {
			call := l.a.runningCall(done[1])
			if call == nil {
				continue
			}
			output := strings.TrimSpace(done[3])
			res := &ToolResult{Status: StatusOK, Output: output}
			switch {
			case done[2] == "was canceled":
				res.Status = StatusInterrupted
			case agyFailed(output):
				res.Status = StatusError
			}
			l.end(done[1], res, agyResponse(call.Tool.Name, call.Tool.Input, output))
			continue
		}
		switch {
		case m.sender == "system" && strings.HasPrefix(m.text, agyRestart):
			for len(l.a.running) > 0 {
				l.end(l.a.running[0].task, &ToolResult{Status: StatusInterrupted}, nil)
			}
		case e.Prompt.Origin == OriginTaskNotification:
			if call := l.a.runningCall(m.sender); call != nil && call.Tool.Name == "schedule" {
				l.end(m.sender, &ToolResult{Status: StatusOK, Output: m.text}, nil)
			}
		}
	}
}

type agyMessage struct{ sender, text string }

// agyMessageHead matches a SYSTEM_MESSAGE entry's header.
var agyMessageHead = regexp.MustCompile(`^\[Message\] (?:timestamp=\S+ )?sender=(\S+) (?:priority=\S+ )?content=((?s).*)$`)

// agyMessageStart matches the start of each entry.
var agyMessageStart = regexp.MustCompile(`(?m)^\[Message\] `)

// agySystemEntries splits a SYSTEM_MESSAGE's body into its entries: each
// [Message] with its sender and content, and any text outside one with no
// sender. The preamble names the tag too ("The following is a
// <SYSTEM_MESSAGE> not actually sent by the user..."), so the body is
// after the last opening tag before the closing one.
func agySystemEntries(content string) []agyMessage {
	body := content
	if end := strings.LastIndex(content, "</SYSTEM_MESSAGE>"); end >= 0 {
		if start := strings.LastIndex(content[:end], "<SYSTEM_MESSAGE>"); start >= 0 {
			body = content[start+len("<SYSTEM_MESSAGE>") : end]
		}
	}
	var parts []string
	last := 0
	for _, loc := range agyMessageStart.FindAllStringIndex(body, -1) {
		parts = append(parts, body[last:loc[0]])
		last = loc[0]
	}
	parts = append(parts, body[last:])
	var out []agyMessage
	for _, p := range parts {
		p = strings.TrimSpace(p)
		if p == "" {
			continue
		}
		if m := agyMessageHead.FindStringSubmatch(p); m != nil {
			out = append(out, agyMessage{sender: m[1], text: strings.TrimSpace(m[2])})
		} else {
			out = append(out, agyMessage{text: p})
		}
	}
	return out
}

// agySubagentIDs returns the conversations an invoke_subagent result says
// it started, in order.
func agySubagentIDs(output string) []string {
	var ids []string
	for _, m := range agyConversationID.FindAllStringSubmatch(output, -1) {
		ids = append(ids, m[1])
	}
	return ids
}

var agyConversationID = regexp.MustCompile(`"conversationId":\s*"([^"]+)"`)

// agyArgs returns a call's args as the call's input. transcript_full.jsonl
// stores them plain, as they are; transcript.jsonl (encoded) writes every
// value as a string holding its JSON ("\"ls\"", "5000", "false", "[...]"),
// so each such value is decoded once. A value agy cut short there
// (truncated_fields) is no longer valid JSON and stays as it was written.
// transcript_full.jsonl's plain strings may hold JSON themselves (a search
// for "\"exact phrase\"") and are never decoded.
func agyArgs(args json.RawMessage, encoded bool) json.RawMessage {
	var fields map[string]json.RawMessage
	if !encoded || json.Unmarshal(args, &fields) != nil || len(fields) == 0 {
		return args
	}
	for k, v := range fields {
		var s string
		if json.Unmarshal(v, &s) == nil && json.Valid([]byte(s)) {
			fields[k] = json.RawMessage(s)
		}
	}
	out, err := json.Marshal(fields)
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

// agyFailed reports whether a command's output says it exited non-zero.
func agyFailed(output string) bool {
	m := agyExitCode.FindStringSubmatch(output)
	return m != nil && m[1] != "0"
}

// agyStatus is a tool step's result: agy marks failed steps ERROR (or
// INVALID, for bad arguments) and backgrounded ones RUNNING. A command that
// exits non-zero counts as failed, as Claude Code's Bash does.
func agyStatus(st *agyStep, output string) string {
	switch {
	case st.Status == "ERROR" || st.Status == "INVALID" || st.Error != "":
		return StatusError
	case st.Status == "RUNNING":
		return StatusBackground
	case agyFailed(output):
		return StatusError
	}
	return StatusOK
}

// agyResponse is the structured response the tool table reads from a
// result agy records only as text, as glyph3d-js's agyEnrich reconstructs
// it: the output, plus run_command's as stdout, a read's file header as
// Claude Code's file, a replacement's diff as its structuredPatch, edit_file's
// count, and a write's content.
func agyResponse(name string, input json.RawMessage, output string) json.RawMessage {
	r := map[string]any{"output": output}
	in := parseObject(input)
	switch name {
	case "run_command":
		r["stdout"] = output
	case "view_file", "read_file":
		if f := agyReadFile(output, in); f != nil {
			r["file"] = f
		}
	case "replace_file_content":
		if p := agyDiffPatch(output); p != nil {
			r["structuredPatch"] = p
		}
	case "edit_file":
		if m := agyReplaced.FindStringSubmatch(output); m != nil {
			start, _ := strconv.Atoi(m[1])
			added, _ := strconv.Atoi(m[2])
			removed, _ := strconv.Atoi(m[3])
			r["replacement"] = map[string]int{"startLine": start, "added": added, "removed": removed}
		}
	case "write_to_file", "write_file":
		r["type"] = "create"
		if in["Overwrite"] == true {
			r["type"] = "update"
		}
		if content, ok := in.text("CodeContent", "Content"); ok {
			r["content"] = content
		}
	}
	out, _ := json.Marshal(r)
	return out
}

var (
	agyShowing      = regexp.MustCompile(`(?m)^Showing lines (\d+)\s*(?:-|to)\s*(\d+)(?: of (\d+))?`)
	agyTotal        = regexp.MustCompile(`(?m)^Total Lines: (\d+)`)
	agyNumbered     = regexp.MustCompile(`(?m)^\d+: `)
	agyNumberedLine = regexp.MustCompile(`^(\d+): ?(.*)$`)
	agyReplaced     = regexp.MustCompile(`(?m)^Made 1 replacement in .* starting at line (\d+) \(\+(\d+)/-(\d+) lines\)`)
	agyHunk         = regexp.MustCompile(`^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@`)
)

// agyReadFile reads a read's header ("Showing lines A to B", "Total Lines:
// T") into Claude Code's file shape, {numLines, startLine, totalLines?,
// content?}; without a Showing line, it counts the numbered lines. content
// is the lines read, their "N: " numbers taken off, when every one of them
// is in the output.
func agyReadFile(output string, in object) map[string]any {
	f := map[string]any{}
	if m := agyTotal.FindStringSubmatch(output); m != nil {
		f["totalLines"], _ = strconv.Atoi(m[1])
	}
	var start, n int
	if m := agyShowing.FindStringSubmatch(output); m != nil {
		start, _ = strconv.Atoi(m[1])
		end, _ := strconv.Atoi(m[2])
		if m[3] != "" {
			f["totalLines"], _ = strconv.Atoi(m[3])
		}
		n = max(0, end-start+1)
	} else {
		if n = len(agyNumbered.FindAllStringIndex(output, -1)); n == 0 {
			return nil
		}
		start = 1
		if s, ok := in.num("StartLine"); ok && s > 0 {
			start = s
		}
	}
	f["startLine"], f["numLines"] = start, n
	if text, ok := numberedText(output, agyNumberedLine, start, n); ok {
		f["content"] = text
	}
	return f
}

// numberedText is the text of the n lines a tool printed numbered from
// start, each line matching line (its number, then its text), with the
// numbers taken off. Lines before the first are a header and are skipped;
// ok is false unless all n are there, in order.
func numberedText(output string, line *regexp.Regexp, start, n int) (text string, ok bool) {
	if n <= 0 {
		return "", false
	}
	var b strings.Builder
	want := start
	for l := range strings.SplitSeq(output, "\n") {
		m := line.FindStringSubmatch(l)
		if m == nil {
			if want > start {
				break
			}
			continue
		}
		if num, _ := strconv.Atoi(m[1]); num != want {
			if want > start {
				break
			}
			continue
		}
		b.WriteString(m[2])
		b.WriteByte('\n')
		if want++; want == start+n {
			return b.String(), true
		}
	}
	return "", false
}

// agyDiffPatch reads the [diff_block_start]...[diff_block_end] unified diff
// of a replacement's result into structuredPatch hunks, or nil if there is
// none or agy cut it short (a partial patch would miscount).
func agyDiffPatch(output string) []agyHunkLines {
	if strings.Contains(output, "middle of the output truncated") {
		return nil
	}
	_, rest, ok := strings.Cut(output, "[diff_block_start]\n")
	if !ok {
		return nil
	}
	diff, _, ok := strings.Cut(rest, "[diff_block_end]")
	if !ok {
		return nil
	}
	count := func(s string) int {
		if s == "" {
			return 1
		}
		n, _ := strconv.Atoi(s)
		return n
	}
	var hunks []agyHunkLines
	for _, line := range strings.Split(strings.TrimSuffix(diff, "\n"), "\n") {
		if m := agyHunk.FindStringSubmatch(line); m != nil {
			hunks = append(hunks, agyHunkLines{OldStart: count(m[1]), OldLines: count(m[2]), NewStart: count(m[3]), NewLines: count(m[4]), Lines: []string{}})
			continue
		}
		if len(hunks) == 0 {
			continue
		}
		h := &hunks[len(hunks)-1]
		switch {
		case line == "":
			// A blank context line can lose its leading space.
			h.Lines = append(h.Lines, " ")
		case line[0] == ' ' || line[0] == '+' || line[0] == '-':
			h.Lines = append(h.Lines, line)
		}
	}
	return hunks
}

// agyHunkLines is a structuredPatch hunk, as Claude Code writes one.
type agyHunkLines struct {
	OldStart int      `json:"oldStart"`
	OldLines int      `json:"oldLines"`
	NewStart int      `json:"newStart"`
	NewLines int      `json:"newLines"`
	Lines    []string `json:"lines"`
}

// agyFileOptions reads, for the agy transcript at path, which of agy's
// logs it is (its Session.Path) and what agy keeps beside it (HarvestAgy):
// its workspace (history.jsonl) and title, and for a subagent's
// conversation, the conversation that started it
// (brain/<parent>/.system_generated/subagents/<id>.json), its workspace,
// the call there that started it, and that call's turn.
func agyFileOptions(path string) Options {
	opts := Options{Session: Session{Path: path}}
	conv := AgyConversationDir(path)
	if conv == "" {
		return opts
	}
	id, brain := filepath.Base(conv), filepath.Dir(conv)
	files := AgyFiles{Home: filepath.Dir(brain), Workspace: ReadAgyHistory(filepath.Dir(brain))[id]}
	if descs, _ := filepath.Glob(filepath.Join(brain, "*", ".system_generated", "subagents", id+".json")); len(descs) > 0 {
		if sub, ok := ReadAgySubagent(descs[0]); ok {
			files.Subagent = &sub
		}
	}
	opts.Session.SessionID = id
	opts.Session.HarvestAgy(files)
	if sub := files.Subagent; sub != nil {
		if call, ok := agySpawnCalls(filepath.Join(brain, sub.Parent))[id]; ok {
			opts.Session.ParentSession.ToolID = call.ID
			opts.Turn = call.Turn
		}
	}
	return opts
}

// AgyConversationDir returns the brain/<conversation id> dir an agy
// transcript is in, for either place agy keeps one:
// <id>/.system_generated/logs/transcript*.jsonl or brain/<id>/transcript*.jsonl.
// It returns "" for a path of neither shape.
func AgyConversationDir(path string) string {
	if !strings.HasPrefix(filepath.Base(path), "transcript") {
		return ""
	}
	dir := filepath.Dir(path)
	if filepath.Base(dir) == "logs" && filepath.Base(filepath.Dir(dir)) == ".system_generated" {
		return filepath.Dir(filepath.Dir(dir))
	}
	if filepath.Base(filepath.Dir(dir)) == "brain" {
		return dir
	}
	return ""
}

// agyTranscripts are the transcripts a conversation dir may hold, the one
// gomuxai reads first.
var agyTranscripts = []string{
	filepath.Join(".system_generated", "logs", "transcript_full.jsonl"),
	filepath.Join(".system_generated", "logs", "transcript.jsonl"),
	"transcript_full.jsonl",
	"transcript.jsonl",
}

// agySpawns caches agySpawnCalls by transcript path, while the file keeps
// its size and modification time: every read of a subagent's transcript
// asks, and a parent's runs to megabytes.
var agySpawns = struct {
	sync.Mutex
	m map[string]agySpawnIndex
}{m: map[string]agySpawnIndex{}}

type agySpawnIndex struct {
	size  int64
	mtime time.Time
	calls map[string]Event
}

// agySpawnCalls returns, for the conversation at conv, the invoke_subagent
// call that started each subagent, by the subagent's conversation id: the
// call whose result names it, as the parent's parse makes it. (The
// subagent's index file names the step of that result, spawnStepIndex, but
// not the call's.)
func agySpawnCalls(conv string) map[string]Event {
	var f *os.File
	var path string
	for _, name := range agyTranscripts {
		var err error
		path = filepath.Join(conv, name)
		if f, err = os.Open(path); err == nil {
			break
		}
	}
	if f == nil {
		return nil
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return nil
	}
	agySpawns.Lock()
	defer agySpawns.Unlock()
	if c, ok := agySpawns.m[path]; ok && c.size == info.Size() && c.mtime.Equal(info.ModTime()) {
		return c.calls
	}
	calls := map[string]Event{}
	p := Agy{}.NewParser(Options{Session: Session{Path: path}})
	br := bufio.NewReader(f)
	for {
		line, err := br.ReadBytes('\n')
		if len(line) > 0 && line[len(line)-1] != '\n' {
			line = append(line, '\n')
		}
		for _, e := range p.Feed(line) {
			if e.Op == OpUpdate && e.Tool != nil && e.Tool.Name == "invoke_subagent" {
				for _, id := range agySubagentIDs(e.Tool.Result.Output) {
					calls[id] = e
				}
			}
		}
		if err != nil {
			break
		}
	}
	agySpawns.m[path] = agySpawnIndex{size: info.Size(), mtime: info.ModTime(), calls: calls}
	return calls
}

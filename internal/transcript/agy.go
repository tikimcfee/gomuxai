package transcript

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
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

func (Agy) NewParser(opts Options) *Parser {
	a := &agyLines{sess: opts.Session, turn: opts.Turn, tools: map[string]*Event{}, running: map[string]string{}}
	if opts.Session.ParentSession != nil {
		a.agent = opts.Session.SessionID
	}
	return newParser("agy", a, opts.From)
}

// agyLines is the agy half of a Parser.
type agyLines struct {
	sess Session
	// agent is the Agent of every event, on a subagent's conversation.
	agent string
	turn  *TurnRef
	// turns counts the prompts that opened a turn.
	turns int
	// tools holds each call's latest event, for its updates; pending, the
	// ids of calls still waiting for their GENERIC step, oldest first;
	// running, the calls sent to the background, by task id.
	tools   map[string]*Event
	pending []string
	running map[string]string
}

func (a *agyLines) session() Session { return a.sess }

// agyLine is what reading one step makes: events it adds, then the
// updates it makes to earlier ones.
type agyLine struct {
	a       *agyLines
	st      *agyStep
	at      int64
	n       int
	adds    []Event
	updates []Event
}

// event starts an event of the step's, its id "step_<index>#<part>".
func (l *agyLine) event(part int, kind string) Event {
	id := fmt.Sprintf("step_%d#%d", l.st.StepIndex, part)
	return Event{ID: id, Op: OpAdd, At: l.at, Line: l.n, TS: l.st.CreatedAt, Turn: l.a.turn, Agent: l.a.agent, Kind: kind}
}

func (l *agyLine) add(e Event) { l.adds = append(l.adds, e) }

func (a *agyLines) read(line []byte, at int64, n int) ([]Event, bool) {
	var st agyStep
	if json.Unmarshal(line, &st) != nil {
		return nil, false
	}
	if a.sess.CWD == "" {
		a.sess.setCWD(st.CWD)
	}
	l := &agyLine{a: a, st: &st, at: at, n: n}
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
		l.a.sess.Model = m[1]
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
	for _, id := range l.a.pending {
		l.update(l.a.tools[id], &ToolResult{Status: StatusInterrupted}, nil, "")
	}
	l.a.pending = nil
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
// calls, each waiting for its own GENERIC step.
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
	for j, tc := range st.ToolCalls {
		input := agyArgs(tc.Args)
		if l.a.sess.CWD == "" && tc.Name == "run_command" {
			var args struct{ Cwd string }
			json.Unmarshal(input, &args)
			l.a.sess.setCWD(args.Cwd)
		}
		e := l.event(0, KindTool)
		e.ID = fmt.Sprintf("step_%d_tool_%d", st.StepIndex, j)
		e.Tool = &ToolEvent{Name: tc.Name, Input: input, Result: &ToolResult{Status: StatusPending},
			ToolSummary: SummarizeTool(tc.Name, input, nil, l.a.sess.CWD)}
		l.add(e)
		l.a.tools[e.ID] = &e
		l.a.pending = append(l.a.pending, e.ID)
	}
}

// agyTaskID matches the task id in a RUNNING step's notice.
var agyTaskID = regexp.MustCompile(`background task with task id: (\S+)`)

// generic reads a GENERIC step: the result of the oldest call still
// waiting for one. A result whose call came before the window is dropped.
func (l *agyLine) generic() {
	if len(l.a.pending) == 0 {
		return
	}
	id := l.a.pending[0]
	l.a.pending = l.a.pending[1:]
	call := l.a.tools[id]
	output := agyOutput(l.st.Content)
	res := &ToolResult{Status: agyStatus(l.st, output), Output: output}
	var response json.RawMessage
	switch {
	case res.Status == StatusBackground:
		// While a call runs, the table has no response to read.
		if m := agyTaskID.FindStringSubmatch(output); m != nil {
			l.a.running[m[1]] = id
		}
	case l.st.Status == "ERROR" || l.st.Status == "INVALID" || l.st.Error != "":
		response, _ = json.Marshal(map[string]string{"output": output})
	default:
		response = agyResponse(call.Tool.Name, call.Tool.Input, output)
	}
	sub := ""
	if call.Tool.Name == "invoke_subagent" {
		if ids := agySubagentIDs(output); len(ids) > 0 {
			sub = ids[0]
		}
	}
	l.update(call, res, response, sub)
}

// update makes the update of call to res, its table fields read from
// response; sub, if set, names the session the call started.
func (l *agyLine) update(call *Event, res *ToolResult, response json.RawMessage, sub string) {
	t := *call.Tool
	t.Result = res
	t.ToolSummary = SummarizeTool(t.Name, t.Input, response, l.a.sess.CWD)
	if sub != "" {
		t.SubSession = sub
	}
	e := *call
	e.Op, e.At, e.Line, e.TS, e.Tool = OpUpdate, l.at, l.n, l.st.CreatedAt, &t
	l.updates = append(l.updates, e)
	l.a.tools[call.ID] = &e
}

// agyTaskSender matches the sender of a background task's messages, the
// task's id: "<conversation id>/task-6".
var agyTaskSender = regexp.MustCompile(`/task-\d+$`)

// agyTaskDone matches a background task's end: it finished, or someone
// canceled it.
var agyTaskDone = regexp.MustCompile(`^Task id "([^"]+)" (finished|was canceled) with result:\n?((?s).*)$`)

// systemMessage reads a SYSTEM_MESSAGE step: each [Message] entry is a
// prompt, from a background task or another conversation. A task's end
// completes the call that started it: ok, error if its command exited
// non-zero, interrupted if it was canceled.
func (l *agyLine) systemMessage() {
	for k, m := range agySystemEntries(l.st.Content) {
		e := l.event(k, KindPrompt)
		e.Prompt = &PromptEvent{Text: m.text, Origin: OriginSystem}
		switch {
		case agyTaskSender.MatchString(m.sender):
			e.Prompt.Origin = OriginTaskNotification
		case m.sender != "":
			e.Prompt.Origin = OriginPeer
		}
		l.add(e)
		done := agyTaskDone.FindStringSubmatch(m.text)
		if done == nil {
			continue
		}
		id, ok := l.a.running[done[1]]
		if !ok {
			continue
		}
		delete(l.a.running, done[1])
		call := l.a.tools[id]
		output := strings.TrimSpace(done[3])
		res := &ToolResult{Status: StatusOK, Output: output}
		switch {
		case done[2] == "was canceled":
			res.Status = StatusInterrupted
		case agyFailed(output):
			res.Status = StatusError
		}
		l.update(call, res, agyResponse(call.Tool.Name, call.Tool.Input, output), "")
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
	agyShowing  = regexp.MustCompile(`(?m)^Showing lines (\d+)\s*(?:-|to)\s*(\d+)(?: of (\d+))?`)
	agyTotal    = regexp.MustCompile(`(?m)^Total Lines: (\d+)`)
	agyNumbered = regexp.MustCompile(`(?m)^\d+: `)
	agyReplaced = regexp.MustCompile(`(?m)^Made 1 replacement in .* starting at line (\d+) \(\+(\d+)/-(\d+) lines\)`)
	agyHunk     = regexp.MustCompile(`^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@`)
)

// agyReadFile reads a read's header ("Showing lines A to B", "Total Lines:
// T") into Claude Code's file shape, {numLines, startLine, totalLines?};
// without a Showing line, it counts the numbered lines.
func agyReadFile(output string, in object) map[string]int {
	f := map[string]int{}
	if m := agyTotal.FindStringSubmatch(output); m != nil {
		f["totalLines"], _ = strconv.Atoi(m[1])
	}
	if m := agyShowing.FindStringSubmatch(output); m != nil {
		start, _ := strconv.Atoi(m[1])
		end, _ := strconv.Atoi(m[2])
		if m[3] != "" {
			f["totalLines"], _ = strconv.Atoi(m[3])
		}
		f["startLine"], f["numLines"] = start, max(0, end-start+1)
		return f
	}
	n := len(agyNumbered.FindAllStringIndex(output, -1))
	if n == 0 {
		return nil
	}
	f["numLines"], f["startLine"] = n, 1
	if start, ok := in.num("StartLine"); ok && start > 0 {
		f["startLine"] = start
	}
	return f
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

// agyFileOptions reads, for the agy transcript at path, what agy keeps
// beside it about a subagent's conversation: the conversation that
// started it (brain/<parent>/.system_generated/subagents/<id>.json), the
// call there that did, and that call's turn. It returns no options for a
// conversation no other one started.
func agyFileOptions(path string) Options {
	id := sessionIDFromPath(path)
	conv := agyConversationDir(path)
	if id == "" || conv == "" {
		return Options{}
	}
	brain := filepath.Dir(conv)
	descs, _ := filepath.Glob(filepath.Join(brain, "*", ".system_generated", "subagents", id+".json"))
	if len(descs) == 0 {
		return Options{}
	}
	parent := filepath.Base(filepath.Dir(filepath.Dir(filepath.Dir(descs[0]))))
	opts := Options{Session: Session{SessionID: id, ParentSession: &ParentSession{SessionID: parent}}}
	if call := agySpawnCall(filepath.Join(brain, parent), id); call != nil {
		opts.Session.ParentSession.ToolID = call.ID
		opts.Turn = call.Turn
	}
	return opts
}

// agyConversationDir returns the brain/<conversation id> dir an agy
// transcript is in: <id>/.system_generated/logs/transcript*.jsonl, or
// <id>/transcript*.jsonl. It returns "" for a path of neither shape.
func agyConversationDir(path string) string {
	if !strings.HasPrefix(filepath.Base(path), "transcript") {
		return ""
	}
	dir := filepath.Dir(path)
	if filepath.Base(dir) == "logs" && filepath.Base(filepath.Dir(dir)) == ".system_generated" {
		return filepath.Dir(filepath.Dir(dir))
	}
	return dir
}

// agyTranscripts are the transcripts a conversation dir may hold, the one
// gomuxai reads first.
var agyTranscripts = []string{
	filepath.Join(".system_generated", "logs", "transcript_full.jsonl"),
	filepath.Join(".system_generated", "logs", "transcript.jsonl"),
	"transcript_full.jsonl",
	"transcript.jsonl",
}

// agySpawnCall finds, in the conversation at conv, the invoke_subagent
// call whose result names the conversation child: the call that started
// it, as the parent's parse makes it, or nil. (The subagent's index file
// names the step of that result, spawnStepIndex, but not the call's.)
func agySpawnCall(conv, child string) *Event {
	var f *os.File
	for _, name := range agyTranscripts {
		var err error
		if f, err = os.Open(filepath.Join(conv, name)); err == nil {
			break
		}
	}
	if f == nil {
		return nil
	}
	defer f.Close()
	p := Agy{}.NewParser(Options{})
	br := bufio.NewReader(f)
	for {
		line, err := br.ReadBytes('\n')
		if len(line) > 0 && line[len(line)-1] != '\n' {
			line = append(line, '\n')
		}
		for _, e := range p.Feed(line) {
			if e.Op != OpUpdate || e.Tool == nil || e.Tool.Name != "invoke_subagent" {
				continue
			}
			for _, id := range agySubagentIDs(e.Tool.Result.Output) {
				if id == child {
					return &e
				}
			}
		}
		if err != nil {
			return nil
		}
	}
}

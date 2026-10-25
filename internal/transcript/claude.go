package transcript

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

// Claude reads Claude Code session transcripts
// (~/.claude/projects/<project>/<session id>.jsonl, and a subagent's
// <session id>/subagents/agent-<agent id>.jsonl). Each record is one
// line: user (a prompt, tool results, or a harness note), assistant (one
// content block per record, records of one message sharing its id),
// attachment, system, and bookkeeping (titles, modes, queue operations,
// api-request copies) that carries metadata at most. testdata/corpus/README.md
// lists the shapes and the decisions this reader follows.
type Claude struct{}

func (Claude) Harness() string { return "claude" }

func (Claude) NewParser(opts Options) *Parser {
	c := &claudeLines{opts: opts, tools: map[string]*Event{}}
	if opts.Requests {
		c.blobs = map[string]json.RawMessage{}
	}
	return newParser("claude", c)
}

// claudeRecord is the part of a transcript line the parser reads.
type claudeRecord struct {
	// The session's metadata: type, sessionId, agentId, cwd, version, ...
	claudeMeta
	Subtype   string `json:"subtype"`
	UUID      string `json:"uuid"`
	Timestamp string `json:"timestamp"`

	PromptID     string `json:"promptId"`
	TurnPosition *struct {
		PromptIndex *int `json:"promptIndex"`
	} `json:"turnPosition"`
	Origin       *claudeOrigin `json:"origin"`
	PromptSource string        `json:"promptSource"`

	IsMeta            bool `json:"isMeta"`
	IsCompactSummary  bool `json:"isCompactSummary"`
	IsAPIErrorMessage bool `json:"isApiErrorMessage"`

	// Content is a system record's text.
	Content       json.RawMessage `json:"content"`
	ToolUseResult json.RawMessage `json:"toolUseResult"`
	Attachment    *struct {
		Type        string          `json:"type"`
		Prompt      json.RawMessage `json:"prompt"`
		CommandMode string          `json:"commandMode"`
		Origin      *claudeOrigin   `json:"origin"`
		Content     json.RawMessage `json:"content"`
		HookEvent   string          `json:"hookEvent"`
	} `json:"attachment"`
	Message *struct {
		Role       string          `json:"role"`
		Model      string          `json:"model"`
		Content    json.RawMessage `json:"content"`
		StopReason string          `json:"stop_reason"`
	} `json:"message"`

	// api-request and api-request-blob lines.
	ID     string          `json:"id"`
	Params json.RawMessage `json:"params"`
	Base   string          `json:"base"`
	Tail   []string        `json:"tail"`
	Hash   string          `json:"hash"`
}

type claudeOrigin struct {
	Kind string `json:"kind"`
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
	// A model fallback's models.
	From   json.RawMessage `json:"from"`
	To     json.RawMessage `json:"to"`
	Source *struct {
		MediaType string `json:"media_type"`
		Data      string `json:"data"`
	} `json:"source"`
}

// claudeContent is a message's content read as a prompt: its text (a
// string, or its text blocks joined with newlines), its images, and the
// index of the block the prompt starts at.
type claudeContent struct {
	text   string
	images []Image
	block  int
	// found is set when the content holds any text or image.
	found bool
}

// readClaudeContent reads raw, a message's content: a string or blocks.
// It returns the blocks too, nil for a string.
func readClaudeContent(raw json.RawMessage) (claudeContent, []claudeBlock) {
	var c claudeContent
	if json.Unmarshal(raw, &c.text) == nil {
		c.found = true
		return c, nil
	}
	var blocks []claudeBlock
	if json.Unmarshal(raw, &blocks) != nil {
		return c, nil
	}
	var parts []string
	for i, b := range blocks {
		switch {
		case b.Type == "text":
			parts = append(parts, b.Text)
		case b.Type == "image" && b.Source != nil:
			c.images = append(c.images, Image{MediaType: b.Source.MediaType, Data: b.Source.Data})
		default:
			continue
		}
		if !c.found {
			c.block, c.found = i, true
		}
	}
	c.text = strings.Join(parts, "\n")
	return c, blocks
}

// blockID is the id of the event block i of a record makes: "<uuid>#<i>",
// or for a record without a uuid, "L<line>#<i>".
func (r *claudeRecord) blockID(line, i int) string {
	if r.UUID == "" {
		return fmt.Sprintf("L%d#%d", line, i)
	}
	return fmt.Sprintf("%s#%d", r.UUID, i)
}

// claudeLines is the Claude half of a Parser.
type claudeLines struct {
	opts Options
	sess Session
	turn *TurnRef
	// index is the last turn index known (from a promptIndex, or
	// Options.Turns); numbered is set once the file has given a
	// promptIndex. prompts counts the human prompts of a file that gives
	// none: its turns' indexes.
	index    int
	numbered bool
	prompts  int
	// tools holds each call's latest event, for its updates; pending,
	// the ids of calls with no result yet, oldest first.
	tools   map[string]*Event
	pending []string
	// blobs holds the messages api-request-blob lines define, by hash
	// (with Options.Requests).
	blobs map[string]json.RawMessage
	// closing is a reply that ends its message without a stop reason
	// (older versions): final if its turn ends with nothing more from the
	// model.
	closing *Event
}

func (c *claudeLines) session() Session { return c.sess }

// claudeLine is what reading one record makes: events it adds, then the
// updates it makes to earlier ones.
type claudeLine struct {
	c       *claudeLines
	r       *claudeRecord
	raw     []byte
	at      int64
	n       int
	adds    []Event
	updates []Event
}

// event starts an event of the record's.
func (l *claudeLine) event(id, kind string) Event {
	return Event{ID: id, Op: OpAdd, At: l.at, Line: l.n, TS: l.r.Timestamp, Turn: l.c.turn, Agent: l.r.AgentID, Kind: kind}
}

func (l *claudeLine) add(e Event) { l.adds = append(l.adds, e) }

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
	model := ""
	if r.Type == "assistant" && r.Message != nil {
		model = r.Message.Model
	}
	harvestClaude(&c.sess, &r.claudeMeta, model, at)
	if r.PromptID != "" {
		c.enter(r)
	}

	l := &claudeLine{c: c, r: r, raw: line, at: at, n: n}
	closing := c.closing
	switch r.Type {
	case "user":
		if r.Message != nil {
			l.user()
		}
	case "assistant":
		if r.Message != nil {
			l.assistant()
		}
	case "attachment":
		if r.Attachment != nil {
			l.attachment()
		}
	case "system":
		l.system()
	case "api-request-blob":
		// The message whole, as it was sent: not the fields claudeRecord reads.
		var blob struct {
			Message json.RawMessage `json:"message"`
		}
		if c.blobs != nil && r.Hash != "" && json.Unmarshal(line, &blob) == nil && len(blob.Message) > 0 {
			c.blobs[r.Hash] = blob.Message
		}
	case "api-request":
		if c.opts.Requests {
			l.request()
		}
	}
	// A new turn closes the last one.
	if closing != nil && c.closing == closing && c.turn != nil && (closing.Turn == nil || c.turn.ID != closing.Turn.ID) {
		l.close()
	}
	return append(l.adds, l.updates...), true
}

// close marks the reply that ended its turn's last message without a stop
// reason final, now that the turn has ended: an update of it.
func (l *claudeLine) close() {
	e := *l.c.closing
	l.c.closing = nil
	e.Op, e.At, e.Line, e.TS = OpUpdate, l.at, l.n, l.r.Timestamp
	e.Say = &SayEvent{Text: e.Say.Text, Final: true}
	l.updates = append(l.updates, e)
}

// enter makes the turn a record with a prompt id names the current one.
// Its index is the record's promptIndex; for a record without one, the
// index Options.Turns gives (a subagent's records name the parent's
// turn), else the last index known; in a file that gives no promptIndex
// at all, the count of human prompts so far (prompt bumps it).
func (c *claudeLines) enter(r *claudeRecord) {
	index := c.index
	switch {
	case r.TurnPosition != nil && r.TurnPosition.PromptIndex != nil:
		index = *r.TurnPosition.PromptIndex
		c.index, c.numbered = index, true
	case c.opts.Turns != nil:
		if i, ok := c.opts.Turns(r.PromptID); ok {
			index = i
			c.index = i
		}
	case !c.numbered:
		index = c.prompts
	}
	c.setTurn(r.PromptID, index, false)
}

// setTurn makes the turn id, index the current one.
func (c *claudeLines) setTurn(id string, index int, derived bool) {
	if c.turn == nil || c.turn.ID != id || c.turn.Index != index || c.turn.Derived != derived {
		c.turn = &TurnRef{ID: id, Index: index, Derived: derived}
	}
}

// counted reports whether r's turn index is the count of human prompts:
// the file gives no promptIndex, and nothing else numbers its turns.
func (c *claudeLines) counted(r *claudeRecord) bool {
	return r.PromptID == "" || !c.numbered && c.opts.Turns == nil && (r.TurnPosition == nil || r.TurnPosition.PromptIndex == nil)
}

// claudeInterrupted is how the record Claude Code writes when a person
// interrupts a turn starts: "[Request interrupted by user]", or "... for
// tool use]" when calls were running.
const claudeInterrupted = "[Request interrupted by user"

// claudeEchoTags start the user records older versions wrote for a local
// command's output and a person's shell escape (! cmd) output; newer ones
// write a local_command system record.
var claudeEchoTags = []string{"<local-command-stdout>", "<local-command-stderr>", "<bash-stdout>", "<bash-stderr>"}

// user reads a user record: tool results, a prompt, or a harness note.
// Its tool results update their calls after the note or prompt is added,
// and an interrupt stops the calls still running after that.
func (l *claudeLine) user() {
	content, blocks := readClaudeContent(l.r.Message.Content)
	interrupted := false
	if content.found {
		interrupted = l.userContent(content)
	}
	var results []claudeBlock
	for _, b := range blocks {
		if b.Type == "tool_result" && b.ToolUseID != "" {
			results = append(results, b)
		}
	}
	for _, b := range results {
		l.result(b, len(results) == 1)
	}
	if interrupted {
		l.interrupt()
	}
}

// userContent reads a user record's text and images: a prompt, or a
// harness note. It reports whether the record is an interrupt.
func (l *claudeLine) userContent(content claudeContent) bool {
	r := l.r
	id := r.blockID(l.n, content.block)
	text := strings.TrimSpace(content.text)
	if text == "" && len(content.images) == 0 {
		return false
	}
	system := func(subtype string) {
		e := l.event(id, KindSystem)
		e.System = &SystemEvent{Subtype: subtype, Text: content.text}
		l.add(e)
	}
	switch {
	case r.IsCompactSummary:
		system("compact_summary")
		return false
	case strings.HasPrefix(text, claudeInterrupted):
		system("interrupt")
		l.c.closing = nil // the turn didn't end with that reply
		return true
	case r.IsMeta && r.Origin == nil && r.PromptSource != "system":
		// A caveat, a reminder, a skill's expanded body: the harness
		// talking to the model, not a message into the agent. One the
		// harness sends as a prompt (promptSource system: a /loop wakeup,
		// an idle notice, Stop hook feedback) is one.
		return false
	case strings.HasPrefix(text, "<local-command-caveat>"):
		return false
	}
	for _, tag := range claudeEchoTags {
		if strings.HasPrefix(text, tag) {
			system("local_command")
			return false
		}
	}

	p := &PromptEvent{Text: content.text, Images: content.images, Origin: claudeOrigins(r)}
	if name, args, ok := slashCommand(text); ok {
		p.Origin, p.Command, p.Text = OriginCommand, name, name
		if args != "" {
			p.Text += " " + args
		}
	} else if IsTaskNotification(text) {
		p.Origin = OriginTaskNotification
	} else if m := claudePlainCommand.FindStringSubmatch(text); m != nil && p.Origin == OriginHuman && r.Origin == nil {
		// Older versions wrote some commands (/compact) as typed.
		p.Origin, p.Command = OriginCommand, m[1]
	}
	l.number(id, p.Origin)
	e := l.event(id, KindPrompt)
	e.Prompt = p
	l.add(e)
	if p.Origin == OriginTaskNotification {
		l.notification(text)
	}
	return false
}

// number counts a prompt the transcript doesn't number: a human prompt
// (typed, a command, or queued until the last turn ended) is the next
// turn. A prompt with no prompt id opens a derived turn, its id the
// prompt event's id, as does a task notification there (with the index
// of the human prompt before it); a message that arrives mid-turn stays
// in the current turn. A prompt with a prompt id is in that turn; numbered
// by count when the file gives no promptIndex.
func (l *claudeLine) number(id, origin string) {
	c, r := l.c, l.r
	if !c.counted(r) {
		return
	}
	human := origin == OriginHuman || origin == OriginCommand || origin == OriginQueued
	switch {
	case human:
		c.prompts++
	case r.PromptID != "" || origin != OriginTaskNotification:
		return
	case c.prompts == 0:
		c.prompts = 1
	}
	if r.PromptID != "" {
		c.setTurn(r.PromptID, c.prompts, false)
	} else {
		c.setTurn(id, c.prompts, true)
	}
}

// claudeOrigins says where a user record's prompt came from, by the
// origin and prompt source the record carries.
func claudeOrigins(r *claudeRecord) string {
	if r.PromptSource == "queued" {
		return OriginQueued
	}
	if r.PromptSource == "system" && r.Origin == nil {
		return OriginSystem
	}
	kind := ""
	if r.Origin != nil {
		kind = r.Origin.Kind
	}
	switch kind {
	case "human":
		return OriginHuman
	case "task-notification":
		return OriginTaskNotification
	case "peer", "channel", "coordinator":
		return OriginPeer
	case "":
		// A subagent's task comes from the agent that started it; a
		// record from before origins were recorded was typed.
		if r.AgentID != "" {
			return OriginPeer
		}
		return OriginHuman
	}
	return OriginSystem
}

// claudePlainCommand matches a slash command typed as text, with no
// markup: "/compact keep the plan". A path ("/etc/hosts is gone") has a
// second slash, and isn't one.
var claudePlainCommand = regexp.MustCompile(`^(/[a-z][a-z0-9_:-]*)(?:\s|$)`)

// slashCommand reads a slash command's markup, in either tag order:
// <command-name>/review</command-name> <command-message>review</command-message>
// <command-args>internal/server</command-args>.
func slashCommand(text string) (name, args string, ok bool) {
	if !strings.HasPrefix(text, "<command-name>") && !strings.HasPrefix(text, "<command-message>") {
		return "", "", false
	}
	name, ok = tagText(text, "command-name")
	if !ok || name == "" {
		return "", "", false
	}
	if !strings.HasPrefix(name, "/") {
		name = "/" + name
	}
	args, _ = tagText(text, "command-args")
	return name, args, true
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

// result makes the update a tool_result block makes to its call, if the
// parse has seen the call. The record's toolUseResult is the call's
// structured response when the record holds no other result.
func (l *claudeLine) result(b claudeBlock, own bool) {
	call := l.c.tools[b.ToolUseID]
	if call == nil {
		return
	}
	res := &ToolResult{Status: StatusOK, Output: resultText(b.Content)}
	var structured struct {
		Status           string `json:"status"`
		BackgroundTaskID string `json:"backgroundTaskId"`
		AgentID          string `json:"agentId"`
	}
	if raw := bytes.TrimSpace(l.r.ToolUseResult); own && len(raw) > 0 && raw[0] == '{' {
		res.Structured = l.r.ToolUseResult
		json.Unmarshal(raw, &structured)
	}
	switch {
	case b.IsError:
		res.Status = StatusError
	case structured.Status == "async_launched" || structured.BackgroundTaskID != "":
		res.Status = StatusBackground
	}
	// While a call runs, the table has no response to read.
	response := res.Structured
	if res.Status == StatusBackground {
		response = nil
	}
	var subs []string
	if structured.AgentID != "" {
		subs = []string{"agent-" + structured.AgentID}
	}
	l.update(call, res, response, subs)
}

// update makes the update of call to res, its table fields read from
// response; subs, if any, name the sessions the call started.
func (l *claudeLine) update(call *Event, res *ToolResult, response json.RawMessage, subs []string) {
	t := *call.Tool
	t.Result = res
	t.ToolSummary = SummarizeTool(t.Name, t.Input, response, l.c.sess.CWD)
	if len(subs) > 0 {
		t.SubSessions = subs
	}
	e := *call
	e.Op, e.At, e.Line, e.TS, e.Tool = OpUpdate, l.at, l.n, l.r.Timestamp, &t
	l.updates = append(l.updates, e)
	l.c.tools[call.ID] = &e
	l.c.settled(call.ID, res.Status)
}

// settled records a call's new status: one that isn't pending any more
// leaves the pending list, and one that is done for good, the parser's
// memory (only a running call has updates to come).
func (c *claudeLines) settled(id, status string) {
	if status == StatusPending {
		return
	}
	if status != StatusBackground {
		delete(c.tools, id)
	}
	for i, p := range c.pending {
		if p == id {
			c.pending = append(c.pending[:i], c.pending[i+1:]...)
			return
		}
	}
}

// interrupt marks every call still pending interrupted: an interrupted
// turn's running calls never return.
func (l *claudeLine) interrupt() {
	for _, id := range append([]string(nil), l.c.pending...) {
		if call := l.c.tools[id]; call != nil && call.Tool.Result.Status == StatusPending {
			l.update(call, &ToolResult{Status: StatusInterrupted}, nil, nil)
		}
	}
}

// claudeTaskStatus maps a task notification's <status> to the result of
// the call that started the task.
var claudeTaskStatus = map[string]string{
	"completed": StatusOK,
	"failed":    StatusError,
	"killed":    StatusInterrupted,
	"stopped":   StatusInterrupted,
}

// TaskNotification is one <task-notification> block of a Claude Code
// message: a background task (a command, a subagent) reporting back.
type TaskNotification struct {
	TaskID    string
	ToolUseID string
	Status    string
	Summary   string
	Result    string
	// Usage is the <usage> block's inner markup, if any.
	Usage string
}

// IsTaskNotification reports whether text, a message's trimmed text, is
// task notifications: one or more <task-notification> blocks, alone or
// after a "[SYSTEM NOTIFICATION ...]" preamble.
func IsTaskNotification(text string) bool {
	return strings.HasPrefix(text, "<task-notification>") ||
		strings.HasPrefix(text, "[SYSTEM NOTIFICATION") && strings.Contains(text, "<task-notification>")
}

// TaskNotifications returns every <task-notification> block in text, in
// order. Claude Code puts several in one message when several tasks
// finish while the agent works.
func TaskNotifications(text string) []TaskNotification {
	var out []TaskNotification
	for {
		_, rest, ok := strings.Cut(text, "<task-notification>")
		if !ok {
			return out
		}
		block, after, _ := strings.Cut(rest, "</task-notification>")
		var n TaskNotification
		for _, f := range []struct {
			dst *string
			tag string
		}{
			{&n.TaskID, "task-id"}, {&n.ToolUseID, "tool-use-id"}, {&n.Status, "status"},
			{&n.Summary, "summary"}, {&n.Result, "result"}, {&n.Usage, "usage"},
		} {
			*f.dst, _ = tagText(block, f.tag)
		}
		out = append(out, n)
		text = after
	}
}

// notification completes the background calls the task notifications in
// text report on: each one's <result> (or <summary>) is its call's output,
// and a subagent's <usage> what the table reads its numbers from.
func (l *claudeLine) notification(text string) {
	for _, n := range TaskNotifications(text) {
		call := l.c.tools[n.ToolUseID]
		if call == nil || call.Tool.Result.Status != StatusBackground {
			continue
		}
		res := &ToolResult{Status: claudeTaskStatus[n.Status], Output: n.Result}
		if res.Status == "" {
			continue
		}
		if res.Output == "" {
			res.Output = n.Summary
		}
		var response json.RawMessage
		if n.Usage != "" {
			m := map[string]int{}
			for tag, key := range map[string]string{"tool_uses": "totalToolUseCount", "subagent_tokens": "totalTokens", "duration_ms": "totalDurationMs"} {
				var v int
				if s, ok := tagText(n.Usage, tag); ok && json.Unmarshal([]byte(s), &v) == nil {
					m[key] = v
				}
			}
			response, _ = json.Marshal(m)
		}
		l.update(call, res, response, nil)
	}
}

// assistant reads an assistant record: thinking, text and tool calls, or
// an API error the harness wrote in the model's place.
func (l *claudeLine) assistant() {
	r := l.r
	content, blocks := readClaudeContent(r.Message.Content)
	// More from the model: the last reply didn't close its turn.
	l.c.closing = nil
	if r.IsAPIErrorMessage {
		e := l.event(r.blockID(l.n, 0), KindSystem)
		data := objectFields(l.raw, func(key string) bool { return key == "error" || key == "apiErrorStatus" })
		e.System = &SystemEvent{Subtype: "api_error", Text: content.text, Data: data}
		l.add(e)
		return
	}
	lastText, calls := -1, false
	for i, b := range blocks {
		switch {
		case b.Type == "text" && strings.TrimSpace(b.Text) != "":
			lastText = i
		case b.Type == "tool_use":
			calls = true
		}
	}
	for i, b := range blocks {
		switch b.Type {
		case "thinking", "redacted_thinking":
			e := l.event(r.blockID(l.n, i), KindThink)
			e.Think = &ThinkEvent{Text: b.Thinking, Redacted: b.Thinking == ""}
			l.add(e)
		case "text":
			if strings.TrimSpace(b.Text) == "" {
				continue
			}
			// A message that ends the turn ends with end_turn or a stop
			// sequence; older versions wrote no stop reason, so their last
			// reply is final once the turn ends with nothing more.
			e := l.event(r.blockID(l.n, i), KindSay)
			stop := r.Message.StopReason
			e.Say = &SayEvent{Text: b.Text, Final: i == lastText && (stop == "end_turn" || stop == "stop_sequence")}
			l.add(e)
			if i == lastText && stop == "" && !calls {
				l.c.closing = &e
			}
		case "fallback":
			// The harness switched models for this message.
			e := l.event(r.blockID(l.n, i), KindSystem)
			e.System = &SystemEvent{Subtype: "model_fallback", Data: modelFallback(b)}
			l.add(e)
		case "tool_use":
			e := l.event(b.ID, KindTool)
			e.Tool = &ToolEvent{Name: b.Name, Input: b.Input, Result: &ToolResult{Status: StatusPending},
				ToolSummary: SummarizeTool(b.Name, b.Input, nil, l.c.sess.CWD)}
			l.add(e)
			l.c.tools[b.ID] = &e
			l.c.pending = append(l.c.pending, b.ID)
		}
	}
}

// modelFallback is a fallback block's data: the models it switched from
// and to, as the block holds them.
func modelFallback(b claudeBlock) json.RawMessage {
	var out bytes.Buffer
	out.WriteByte('{')
	for _, f := range []struct {
		key string
		v   json.RawMessage
	}{{"from", b.From}, {"to", b.To}} {
		if len(f.v) == 0 {
			continue
		}
		if out.Len() > 1 {
			out.WriteByte(',')
		}
		fmt.Fprintf(&out, "%q:", f.key)
		out.Write(f.v)
	}
	out.WriteByte('}')
	return out.Bytes()
}

// claudeDelivered matches the head line of gomuxai's held-message
// delivery, the Stop hook context that hands an agent the messages sent
// while it worked (namedagents.ClaudeBackend.DeliveryOutput).
var claudeDelivered = regexp.MustCompile(`^(A message arrived|\d+ messages arrived) while you were working`)

// attachment reads an attachment record: a message that arrived mid-turn
// (queued_command) or hook context (hook_additional_context). Other
// attachments (reminders, listings, file snapshots) aren't events.
func (l *claudeLine) attachment() {
	a := l.r.Attachment
	id := l.r.blockID(l.n, 0)
	p := &PromptEvent{}
	switch a.Type {
	case "queued_command":
		content, _ := readClaudeContent(a.Prompt)
		if !content.found {
			return
		}
		p.Text, p.Images, p.Origin = content.text, content.images, OriginQueued
		kind := ""
		if a.Origin != nil {
			kind = a.Origin.Kind
		}
		switch {
		case a.CommandMode == "task-notification" || kind == "task-notification" || IsTaskNotification(strings.TrimSpace(p.Text)):
			p.Origin = OriginTaskNotification
		case kind == "peer" || kind == "channel" || kind == "coordinator":
			p.Origin = OriginPeer
		}
	case "hook_additional_context":
		var parts []string
		if json.Unmarshal(a.Content, &parts) != nil || len(parts) == 0 {
			return
		}
		p.Text, p.Origin = strings.Join(parts, "\n"), OriginSystem
		if a.HookEvent == "Stop" && claudeDelivered.MatchString(p.Text) {
			p.Origin = OriginDelivered
		}
	default:
		return
	}
	e := l.event(id, KindPrompt)
	e.Prompt = p
	l.add(e)
	if p.Origin == OriginTaskNotification {
		l.notification(strings.TrimSpace(p.Text))
	}
}

// claudeEnvelope lists the fields every record carries about where it
// sits: a system event's data leaves them out.
var claudeEnvelope = map[string]bool{
	"type": true, "subtype": true, "content": true,
	"parentUuid": true, "logicalParentUuid": true, "isSidechain": true, "uuid": true, "timestamp": true,
	"userType": true, "entrypoint": true, "cwd": true, "sessionId": true, "session_id": true,
	"version": true, "gitBranch": true, "slug": true, "isMeta": true, "level": true, "sessionKind": true,
	"agentId": true, "promptId": true,
}

// system reads a system record (turn timing, compaction, hook summaries,
// local command output): its subtype, its content as text, and every
// other field but the envelope as data.
func (l *claudeLine) system() {
	e := l.event(l.r.blockID(l.n, 0), KindSystem)
	e.System = &SystemEvent{Subtype: l.r.Subtype, Data: objectFields(l.raw, func(key string) bool { return !claudeEnvelope[key] })}
	json.Unmarshal(l.r.Content, &e.System.Text)
	l.add(e)
	if s := l.r.Subtype; l.c.closing != nil && (s == "turn_duration" || s == "stop_hook_summary") {
		l.close() // the turn ended
	}
}

// request reads an api-request line: what one model call was sent, with
// the messages its tail names.
func (l *claudeLine) request() {
	r := l.r
	e := l.event(r.ID, KindRequest)
	req := &RequestEvent{Params: r.Params, Base: r.Base, Tail: r.Tail}
	var params struct {
		Model string `json:"model"`
	}
	json.Unmarshal(r.Params, &params)
	req.Model = params.Model
	for _, hash := range r.Tail {
		if blob, ok := l.c.blobs[hash]; ok {
			if req.Blobs == nil {
				req.Blobs = map[string]json.RawMessage{}
			}
			req.Blobs[hash] = blob
		}
	}
	e.Request = req
	l.add(e)
}

// objectFields returns the fields of the JSON object line that keep
// accepts, in its own order, or nil if there are none.
func objectFields(line []byte, keep func(key string) bool) json.RawMessage {
	dec := json.NewDecoder(bytes.NewReader(line))
	if t, err := dec.Token(); err != nil || t != json.Delim('{') {
		return nil
	}
	var out bytes.Buffer
	for dec.More() {
		t, err := dec.Token()
		if err != nil {
			return nil
		}
		var v json.RawMessage
		if err := dec.Decode(&v); err != nil {
			return nil
		}
		key, _ := t.(string)
		if !keep(key) {
			continue
		}
		if out.Len() == 0 {
			out.WriteByte('{')
		} else {
			out.WriteByte(',')
		}
		k, _ := json.Marshal(key)
		out.Write(k)
		out.WriteByte(':')
		out.Write(v)
	}
	if out.Len() == 0 {
		return nil
	}
	out.WriteByte('}')
	return out.Bytes()
}

// ClaudeSubagentTurns returns, for the Claude subagent transcript at path
// (<session id>/subagents/agent-<agent id>.jsonl), the lookup of its
// parent's turn indexes by prompt id that Options.Turns takes. A
// subagent's records name the parent's current turn by its prompt id only;
// its index is what the parent's transcript (<session id>.jsonl beside
// that folder) gives it, by the same rules. The lookup reads the parent as
// far as it has been written; a prompt id it doesn't know yet makes it
// read what the parent has added since. It returns nil for any other
// path.
func ClaudeSubagentTurns(path string) func(promptID string) (int, bool) {
	dir := filepath.Dir(path)
	if filepath.Base(dir) != "subagents" || !strings.HasPrefix(filepath.Base(path), "agent-") {
		return nil
	}
	t := &parentTurns{path: filepath.Dir(dir) + ".jsonl", p: Claude{}.NewParser(Options{}), turns: map[string]int{}}
	return t.lookup
}

// parentTurns reads a parent transcript's turns as it grows.
type parentTurns struct {
	path    string
	p       *Parser
	pending []byte // read, but not a whole line yet
	turns   map[string]int
}

func (t *parentTurns) lookup(promptID string) (int, bool) {
	if i, ok := t.turns[promptID]; ok {
		return i, true
	}
	t.read()
	i, ok := t.turns[promptID]
	return i, ok
}

// read parses what the parent's transcript holds past what was read.
func (t *parentTurns) read() {
	f, err := os.Open(t.path)
	if err != nil {
		return
	}
	defer f.Close()
	off := t.p.Pos().At + int64(len(t.pending))
	buf := make([]byte, readChunk)
	for {
		n, err := f.ReadAt(buf, off)
		off += int64(n)
		t.pending = append(t.pending, buf[:n]...)
		start := t.p.Pos().At
		for _, e := range t.p.Feed(t.pending) {
			if e.Turn != nil && !e.Turn.Derived {
				t.turns[e.Turn.ID] = e.Turn.Index
			}
		}
		t.pending = append(t.pending[:0], t.pending[t.p.Pos().At-start:]...)
		if n == 0 || err != nil {
			return
		}
	}
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

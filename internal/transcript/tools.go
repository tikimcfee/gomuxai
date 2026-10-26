package transcript

import (
	"encoding/json"
	"strings"
)

// The one tool table: it turns a raw tool call into what a reader shows.
// Ported from glyph3d-js packages/glyph3d-core/src/collections/
// toolRegistry.js (TOOLS, normalizeToolCall, the meta readers, askDetail,
// fallbackDetail, relativize, the noise list). Its presentation half
// (highlight decoration, hues, kind buckets) stays with the readers, and so
// does the result text: a tool event carries its result whole.

// ToolSummary is how a reader shows one tool call.
type ToolSummary struct {
	// Action is what kind of thing the call did: read, edit, write, bash,
	// grep, glob, task, ask, fetch, search, skill, send, or ActionNoise.
	// A tool the table doesn't know keeps its own name, lowercased.
	Action string `json:"action"`
	// Target is the file the call is about, relative to the session's
	// working directory when it's under it.
	Target string `json:"target,omitempty"`
	// Detail is the call's most telling input, whole: its command,
	// pattern, URL, question or message.
	Detail string `json:"detail,omitempty"`
	// Meta holds the numbers worth showing, by action:
	//   read   {lines, range?: [first, last]} (range on a partial read), or {bytes} for an image
	//   edit   {added, removed, ranges?: [[first, last], ...], patch?}
	//   write  {kind?, lines}
	//   bash   {lines, interrupted?}
	//   task   {tools?, tokens?, ms?}
	// Line numbers are 1-based and inclusive; ranges are the added lines'
	// runs, and patch is the result's structuredPatch hunks, whole. A
	// result that holds the numbers wins; without one (the call is still
	// running, or its harness's result doesn't carry them), an edit's and
	// a write's numbers come from the input: what the call asked for.
	Meta json.RawMessage `json:"meta,omitempty"`
	// File is the target's text as the call saw it (a read: the lines it
	// read) or left it (a write: what it wrote; an edit: the file after
	// it), when the result holds it. A reader shows it instead of reading
	// the file, which has moved on since, or lies where it can't reach.
	File *FileText `json:"file,omitempty"`
}

// FileText is part or all of a file's text: its lines from Start
// (1-based), of the file's Total lines when the result says.
type FileText struct {
	Text  string `json:"text"`
	Start int    `json:"start"`
	Total int    `json:"total,omitempty"`
}

// ActionNoise is the action of the bookkeeping tools (TodoWrite,
// ToolSearch, ...). Their events are kept; readers may hide them.
const ActionNoise = "noise"

// A toolSpec is what the table knows about one tool. Every field is
// optional.
type toolSpec struct {
	action string
	// target lists the input keys that may name the file the call is
	// about; the first one set wins.
	target []string
	detail func(in object) string
	meta   func(in, out object) any
	// file reads the target's text from the call's result.
	file func(in, out object) *FileText
}

// fileKeys name a file tool's file: Claude Code's key, then Kimi Code's.
var fileKeys = []string{"file_path", "path"}

// tools maps a raw tool name to what it does: Claude Code's and Kimi Code's
// names (Kimi shares most of Claude's), then agy's. Add a tool here, and a
// case to tools_test.go.
var tools = map[string]toolSpec{
	"Read":         {action: "read", target: fileKeys, meta: readMeta, file: readFile},
	"Edit":         {action: "edit", target: fileKeys, meta: editMeta, file: editedFile},
	"MultiEdit":    {action: "edit", target: fileKeys, meta: editMeta, file: editedFile},
	"NotebookEdit": {action: "edit", target: []string{"notebook_path", "file_path", "path"}, meta: notebookMeta},
	"Write":        {action: "write", target: fileKeys, meta: writeMeta, file: writtenFile},
	"Bash":         {action: "bash", detail: field("command"), meta: bashMeta},
	// The matches are the result, so a search's path (often a directory)
	// isn't its target.
	"Grep":            {action: "grep", detail: field("pattern")},
	"Glob":            {action: "glob", detail: field("pattern")},
	"Task":            {action: "task", detail: field("subagent_type", "description"), meta: taskMeta},
	"Agent":           {action: "task", detail: field("subagent_type", "description"), meta: taskMeta},
	"AgentSwarm":      {action: "task", detail: field("description"), meta: taskMeta}, // Kimi's parallel Task
	"Workflow":        {action: "task", detail: labeled("workflow", "name")},
	"AskUserQuestion": {action: "ask", detail: askDetail},
	"WebFetch":        {action: "fetch", detail: field("url")},
	"FetchURL":        {action: "fetch", detail: field("url")}, // Kimi's WebFetch
	"WebSearch":       {action: "search", detail: field("query")},
	"Skill":           {action: "skill", detail: field("skill")},
	"TodoWrite":       {action: ActionNoise},
	"TodoList":        {action: ActionNoise}, // Kimi's TodoWrite
	"EnterPlanMode":   {action: ActionNoise},
	"ToolSearch":      {action: ActionNoise},
	"TaskGet":         {action: ActionNoise},
	"TaskOutput":      {action: ActionNoise},

	// agy: its own names and argument keys. view_file is read_file's older
	// name.
	"view_file":            {action: "read", target: []string{"AbsolutePath"}, meta: readMeta, file: readFile},
	"read_file":            {action: "read", target: []string{"Path"}, meta: readMeta, file: readFile},
	"replace_file_content": {action: "edit", target: []string{"TargetFile"}, meta: editMeta},
	"edit_file":            {action: "edit", target: []string{"Path"}, meta: replaceMeta},
	"write_to_file":        {action: "write", target: []string{"TargetFile"}, meta: writeMeta},
	"write_file":           {action: "write", target: []string{"Path"}, meta: writeMeta},
	"run_command":          {action: "bash", detail: field("CommandLine"), meta: bashMeta},
	"invoke_subagent":      {action: "task", detail: subagentDetail},
	"manage_subagents":     {action: "task", detail: labeled("subagents", "Action")},
	"manage_task":          {action: "task", detail: joined(" ", "Action", "TaskId")},
	"schedule":             {action: "task", detail: labeled("timer", "Prompt")},
	"send_message":         {action: "send", detail: messageDetail},
	"search_web":           {action: "search", detail: field("query")},
	"read_url_content":     {action: "fetch", detail: field("Url")},
	"ask_question":         {action: "ask", detail: askDetail},
}

// fallbackKeys are the input keys tried, in order, for the detail of a tool
// the table doesn't know: glyph3d-js's, then agy's own in the same order,
// ending with toolSummary, agy's one-line description of any call.
var fallbackKeys = []string{
	"command", "pattern", "query", "url", "description", "prompt", "path", "file_path", "name",
	"CommandLine", "Pattern", "Query", "Url", "AbsolutePath", "TargetFile", "Path", "SearchPath",
	"ToolName", "toolSummary",
}

// SummarizeTool returns how a reader shows a call of the tool name with
// input, given its result's structured response (Claude Code's
// toolUseResult, or a harness's equivalent in its shape) or nil while the
// call has none. cwd is the session's working directory, or "" to keep
// paths absolute.
func SummarizeTool(name string, input, response json.RawMessage, cwd string) ToolSummary {
	in := parseObject(input)
	s := describe(name, in, cwd)
	spec := tools[name]
	if spec.meta == nil && spec.file == nil {
		return s
	}
	out := parseObject(response)
	if spec.meta != nil {
		if m := spec.meta(in, out); m != nil {
			s.Meta, _ = json.Marshal(m)
		}
	}
	if spec.file != nil && out != nil {
		s.File = spec.file(in, out)
	}
	return s
}

// DescribeTool is SummarizeTool without Meta, for a caller that shows only
// what a call is about (a status line, a log line): it reads no result
// and counts nothing.
func DescribeTool(name string, input json.RawMessage, cwd string) ToolSummary {
	return describe(name, parseObject(input), cwd)
}

func describe(name string, in object, cwd string) ToolSummary {
	spec, known := tools[name]
	if !known {
		action := strings.ToLower(name)
		if action == "" {
			action = "act"
		}
		return ToolSummary{Action: action, Detail: relativize(in.str(fallbackKeys...), cwd)}
	}
	s := ToolSummary{Action: spec.action, Target: relativize(in.str(spec.target...), cwd)}
	switch {
	case spec.detail != nil:
		s.Detail = spec.detail(in)
	case spec.action == ActionNoise:
		// Noise still says what it was about (ToolSearch's query, say).
		s.Detail = in.str(fallbackKeys...)
	}
	return s
}

// relativize makes a path under cwd relative to it; any other stays as it is.
func relativize(p, cwd string) string {
	if cwd == "" {
		return p
	}
	if rel, ok := strings.CutPrefix(p, strings.TrimSuffix(cwd, "/")+"/"); ok && rel != "" {
		return rel
	}
	return p
}

// --- details ----------------------------------------------------------------

// field returns a detail that is the first of keys set.
func field(keys ...string) func(object) string {
	return func(in object) string { return in.str(keys...) }
}

// labeled returns a detail "label: <key's value>", or just label.
func labeled(label, key string) func(object) string {
	return func(in object) string {
		if v := in.str(key); v != "" {
			return label + ": " + v
		}
		return label
	}
}

// joined returns a detail that is the keys' values, those set, joined by sep.
func joined(sep string, keys ...string) func(object) string {
	return func(in object) string {
		var parts []string
		for _, k := range keys {
			if v := in.str(k); v != "" {
				parts = append(parts, v)
			}
		}
		return strings.Join(parts, sep)
	}
}

// askDetail is the questions an AskUserQuestion (or agy ask_question) asks,
// one per line.
func askDetail(in object) string {
	var qs []string
	for _, q := range in.objects("questions") {
		if v := q.str("question"); v != "" {
			qs = append(qs, v)
		}
	}
	return strings.Join(qs, "\n")
}

// subagentDetail is the roles an agy invoke_subagent starts, one per line:
// from its Subagents list, or its own top-level spec.
func subagentDetail(in object) string {
	specs := in.objects("Subagents")
	if in.list("Subagents") == nil {
		specs = []object{in}
	}
	var roles []string
	for _, s := range specs {
		if v := s.str("Role", "TypeName"); v != "" {
			roles = append(roles, v)
		}
	}
	return strings.Join(roles, "\n")
}

// messageDetail is an agy send_message's recipient ("→ <recipient>") and,
// on the lines after, the whole message: a subagent's final report rides
// here.
func messageDetail(in object) string {
	var parts []string
	if r := in.str("Recipient"); r != "" {
		parts = append(parts, "→ "+r)
	}
	if m := in.str("Message"); m != "" {
		parts = append(parts, m)
	}
	return strings.Join(parts, "\n")
}

// --- meta -------------------------------------------------------------------

type readStats struct {
	Lines int     `json:"lines"`
	Range *[2]int `json:"range,omitempty"`
}

type sizeStats struct {
	Bytes int `json:"bytes"`
}

type editStats struct {
	Added   int             `json:"added"`
	Removed int             `json:"removed"`
	Ranges  [][2]int        `json:"ranges,omitempty"`
	Patch   json.RawMessage `json:"patch,omitempty"`
}

type writeStats struct {
	Kind  string `json:"kind,omitempty"`
	Lines int    `json:"lines"`
}

type bashStats struct {
	Lines       int  `json:"lines"`
	Interrupted bool `json:"interrupted,omitempty"`
}

type taskStats struct {
	Tools  *int `json:"tools,omitempty"`
	Tokens *int `json:"tokens,omitempty"`
	MS     *int `json:"ms,omitempty"`
}

// readMeta reads the result's file: how many lines were read and, when not
// the whole file, which; or an image's size.
func readMeta(_, out object) any {
	f := out.obj("file")
	if f == nil {
		return nil
	}
	if n, ok := f.num("numLines"); ok {
		m := readStats{Lines: n}
		start, hasStart := f.num("startLine")
		total, hasTotal := f.num("totalLines")
		if hasStart && n > 0 && (start > 1 || hasTotal && n < total) {
			m.Range = &[2]int{start, start + n - 1}
		}
		return m
	}
	if n, ok := f.num("originalSize"); ok {
		return sizeStats{Bytes: n}
	}
	return nil
}

// editMeta counts the lines an edit added and removed, from the result's
// structuredPatch, which it keeps whole, or else from the input's
// replacements.
func editMeta(in, out object) any {
	if p, ok := out["structuredPatch"].([]any); ok {
		var m editStats
		for _, h := range p {
			hunk, _ := h.(map[string]any)
			added, removed, ranges := countHunk(hunk)
			m.Added += added
			m.Removed += removed
			m.Ranges = append(m.Ranges, ranges...)
		}
		m.Patch, _ = json.Marshal(p)
		return m
	}
	added, removed, ok := countReplacements(in)
	if !ok {
		return nil
	}
	return editStats{Added: added, Removed: removed}
}

// countHunk counts a unified-diff hunk's added and removed lines, and finds
// the runs of added lines, numbered in the new file.
func countHunk(hunk object) (added, removed int, ranges [][2]int) {
	line, ok := hunk.num("newStart") // the new file's line the hunk is at
	if !ok || line == 0 {
		line = 1
	}
	run := 0 // the first line of the current run of added lines, or 0
	for _, l := range hunk.list("lines") {
		s, _ := l.(string)
		if strings.HasPrefix(s, "+") {
			added++
			if run == 0 {
				run = line
			}
			line++
			continue
		}
		if run != 0 {
			ranges = append(ranges, [2]int{run, line - 1})
			run = 0
		}
		if strings.HasPrefix(s, "-") {
			removed++ // a removed line isn't in the new file
		} else {
			line++
		}
	}
	if run != 0 {
		ranges = append(ranges, [2]int{run, line - 1})
	}
	return added, removed, ranges
}

// countReplacements counts the lines an edit's input replacements add and
// remove: a MultiEdit's every edit, Claude Code's and Kimi Code's
// old_string and new_string, or agy's TargetContent and ReplacementContent.
// It returns false when the input has none, when one replaces every
// occurrence (how many there are isn't in the input), or when they're too
// large to compare.
func countReplacements(in object) (added, removed int, ok bool) {
	edits := in.objects("edits")
	if in.list("edits") == nil {
		edits = []object{in}
	}
	budget := maxDiffCells
	for _, e := range edits {
		old, hasOld := e["old_string"].(string)
		new, hasNew := e["new_string"].(string)
		all, _ := e["replace_all"].(bool)
		if !hasOld && !hasNew {
			old, hasOld = e["TargetContent"].(string)
			new, hasNew = e["ReplacementContent"].(string)
			all, _ = e["AllowMultiple"].(bool)
		}
		if !hasOld || !hasNew || all {
			return 0, 0, false
		}
		a, r, fits := replacedLines(old, new, &budget)
		if !fits {
			return 0, 0, false
		}
		added += a
		removed += r
	}
	return added, removed, len(edits) > 0
}

// maxDiffCells bounds the work countReplacements does for one call: the
// sum, over its edits, of the product of the two sides' line counts after
// their common ends are dropped.
const maxDiffCells = 1 << 22

// replacedLines counts the file lines that replacing the text old by new
// adds and removes, taking both to be whole lines, as an edit's usually
// are: a replacement within one line counts it changed (+1 -1). It spends
// the cells it compares from budget, and returns false when they don't fit.
func replacedLines(old, new string, budget *int) (added, removed int, ok bool) {
	a, b := lineIDs(splitLines(old), splitLines(new))
	for len(a) > 0 && len(b) > 0 && a[0] == b[0] {
		a, b = a[1:], b[1:]
	}
	for len(a) > 0 && len(b) > 0 && a[len(a)-1] == b[len(b)-1] {
		a, b = a[:len(a)-1], b[:len(b)-1]
	}
	cells := len(a) * len(b)
	if cells > *budget {
		return 0, 0, false
	}
	*budget -= cells
	common := lcsLen(a, b)
	return len(b) - common, len(a) - common, true
}

// splitLines splits s into lines; "" has none, and a final newline doesn't
// start another.
func splitLines(s string) []string {
	if s == "" {
		return nil
	}
	return strings.Split(strings.TrimSuffix(s, "\n"), "\n")
}

// lineIDs numbers a's and b's lines, the same line the same number, so
// they compare as ints.
func lineIDs(a, b []string) ([]int, []int) {
	ids := map[string]int{}
	number := func(lines []string) []int {
		out := make([]int, len(lines))
		for i, l := range lines {
			id, ok := ids[l]
			if !ok {
				id = len(ids)
				ids[l] = id
			}
			out[i] = id
		}
		return out
	}
	return number(a), number(b)
}

// lcsLen returns the length of a and b's longest common subsequence.
func lcsLen(a, b []int) int {
	prev, cur := make([]int, len(b)+1), make([]int, len(b)+1)
	for i := range a {
		for j := range b {
			switch {
			case a[i] == b[j]:
				cur[j+1] = prev[j] + 1
			case prev[j+1] >= cur[j]:
				cur[j+1] = prev[j+1]
			default:
				cur[j+1] = cur[j]
			}
		}
		prev, cur = cur, prev
	}
	return prev[len(b)]
}

// notebookMeta counts a NotebookEdit's lines: from a structuredPatch if
// its result has one, else an inserted cell's.
func notebookMeta(in, out object) any {
	if _, ok := out["structuredPatch"]; ok {
		return editMeta(in, out)
	}
	src, ok := in["new_source"].(string)
	if !ok || in.str("edit_mode") != "insert" {
		return nil
	}
	return editStats{Added: countLines(src)}
}

// replaceMeta reads agy edit_file's replacement, {startLine, added,
// removed}, which its reader takes from the result's "Made 1 replacement
// ... (+A/-R lines)": edit_file's Target is free text, so there's no patch.
func replaceMeta(_, out object) any {
	x := out.obj("replacement")
	if x == nil {
		return nil
	}
	start, _ := x.num("startLine")
	added, _ := x.num("added")
	removed, _ := x.num("removed")
	m := editStats{Added: added, Removed: removed}
	if added > 0 {
		m.Ranges = [][2]int{{start, start + added - 1}}
	}
	return m
}

// writeMeta reads a write's kind (create or update) and how many lines it
// wrote: the result's content, else the input's.
func writeMeta(in, out object) any {
	content, ok := out["content"].(string)
	if !ok {
		content, ok = in.text("content", "CodeContent", "Content")
	}
	if !ok {
		return nil
	}
	m := writeStats{Lines: countLines(content)}
	m.Kind, _ = out["type"].(string)
	return m
}

// bashMeta counts a command's output lines: Claude Code's stdout, or the
// output agy and Kimi Code record.
func bashMeta(_, out object) any {
	text, ok := out.text("stdout", "output")
	if !ok {
		return nil
	}
	m := bashStats{Lines: countLines(text)}
	m.Interrupted, _ = out["interrupted"].(bool)
	return m
}

// taskMeta reads what a subagent's run took: its tool calls, tokens and
// milliseconds.
func taskMeta(_, out object) any {
	opt := func(key string) *int {
		if n, ok := out.num(key); ok {
			return &n
		}
		return nil
	}
	m := taskStats{Tools: opt("totalToolUseCount"), Tokens: opt("totalTokens"), MS: opt("totalDurationMs")}
	if m == (taskStats{}) {
		return nil
	}
	return m
}

// --- file text --------------------------------------------------------------

// readFile is the lines a read read: the result's file content (Claude
// Code's; agy's and Kimi's readers take it from the numbered output). An
// image's has no text.
func readFile(_, out object) *FileText {
	f := out.obj("file")
	text, ok := f.text("content")
	if !ok {
		return nil
	}
	start, _ := f.num("startLine")
	total, _ := f.num("totalLines")
	return &FileText{Text: text, Start: max(start, 1), Total: total}
}

// writtenFile is what a write wrote: the result's content.
func writtenFile(_, out object) *FileText {
	text, ok := out.text("content")
	if !ok {
		return nil
	}
	return &FileText{Text: text, Start: 1, Total: countLines(text)}
}

// editedFile is the file after an edit: the file before it (the result's
// originalFile, or MultiEdit's originalFileContents) with the replacements
// made, each as the harness makes it (the first match, or every one under
// replace_all). The replacements are the ones the result says it made
// (Claude Code records the strings it matched, which may differ from the
// input's), else the input's. A result without the file before it, or a
// replacement that doesn't match it, gives none.
func editedFile(in, out object) *FileText {
	text, ok := out.text("originalFile", "originalFileContents")
	if !ok {
		return nil
	}
	type repl struct {
		old, new string
		all      bool
	}
	var rs []repl
	if old, ok := out.text("oldString"); ok {
		n, _ := out.text("newString")
		rs = append(rs, repl{old, n, out["replaceAll"] == true})
	} else {
		edits := out.objects("edits")
		if len(edits) == 0 {
			edits = in.objects("edits")
		}
		if len(edits) == 0 {
			edits = []object{in}
		}
		for _, e := range edits {
			old, ok1 := e.text("old_string")
			n, ok2 := e.text("new_string")
			if !ok1 || !ok2 {
				return nil
			}
			rs = append(rs, repl{old, n, e["replace_all"] == true})
		}
	}
	for _, r := range rs {
		switch {
		case r.old == "":
			if text != "" {
				return nil
			}
			text = r.new
		case !strings.Contains(text, r.old):
			return nil
		case r.all:
			text = strings.ReplaceAll(text, r.old, r.new)
		default:
			text = strings.Replace(text, r.old, r.new, 1)
		}
	}
	return &FileText{Text: text, Start: 1, Total: countLines(text)}
}

// countLines counts s's lines; a final newline doesn't start another.
func countLines(s string) int {
	if s == "" {
		return 0
	}
	return strings.Count(strings.TrimSuffix(s, "\n"), "\n") + 1
}

// --- input and result objects -------------------------------------------------

// object is a JSON object: a tool's input, or its result's structured
// response. A nil object reads as empty.
type object map[string]any

// parseObject decodes raw as a JSON object, or returns nil if it isn't one.
func parseObject(raw json.RawMessage) object {
	var o object
	if json.Unmarshal(raw, &o) != nil {
		return nil
	}
	return o
}

// str returns the first of keys whose value is a string that isn't blank,
// trimmed, or "".
func (o object) str(keys ...string) string {
	for _, k := range keys {
		if v, ok := o[k].(string); ok && strings.TrimSpace(v) != "" {
			return strings.TrimSpace(v)
		}
	}
	return ""
}

// text returns the first of keys whose value is a string, whole, even "".
func (o object) text(keys ...string) (string, bool) {
	for _, k := range keys {
		if v, ok := o[k].(string); ok {
			return v, true
		}
	}
	return "", false
}

// num returns key's value as an int, if it's a number.
func (o object) num(key string) (int, bool) {
	v, ok := o[key].(float64)
	return int(v), ok
}

// obj returns key's value if it's an object, or nil.
func (o object) obj(key string) object {
	v, _ := o[key].(map[string]any)
	return v
}

// list returns key's value if it's an array, or nil.
func (o object) list(key string) []any {
	v, _ := o[key].([]any)
	return v
}

// objects returns the objects in key's array.
func (o object) objects(key string) []object {
	var objs []object
	for _, v := range o.list(key) {
		if m, ok := v.(map[string]any); ok {
			objs = append(objs, m)
		}
	}
	return objs
}

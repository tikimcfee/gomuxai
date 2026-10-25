package transcript

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
)

// The cases up to "out of cwd" mirror glyph3d-js tools/toolRegistry.test.mjs
// (and the patch case of tools/delta-source.test.mjs), in its order, so the
// two tables can be checked for parity. Where this table differs on
// purpose, the case says so.
func TestSummarizeTool(t *testing.T) {
	const cwd = "/home/user/proj/"
	tests := []struct {
		name, tool, input, response string
		want                        ToolSummary
		wantMeta                    string // JSON, compared by value; "" for none
	}{
		{
			name: "read: a partial slice has a range; the target is relativized",
			tool: "Read", input: `{"file_path":"/home/user/proj/a/b.js"}`,
			response: `{"type":"text","file":{"numLines":42,"startLine":40,"totalLines":100}}`,
			want:     ToolSummary{Action: "read", Target: "a/b.js"},
			wantMeta: `{"lines":42,"range":[40,81]}`,
		},
		{
			name: "read: a whole file has no range",
			tool: "Read", input: `{"file_path":"/home/user/proj/x.js"}`,
			response: `{"file":{"numLines":100,"startLine":1,"totalLines":100}}`,
			want:     ToolSummary{Action: "read", Target: "x.js"},
			wantMeta: `{"lines":100}`,
		},
		{
			name: "edit: structuredPatch gives the counts, the added runs and the hunks",
			tool: "Edit", input: `{"file_path":"/home/user/proj/a.js"}`,
			response: `{"structuredPatch":[{"newStart":10,"lines":[" a","+b","+c"," d","-e","+f"]}]}`,
			want:     ToolSummary{Action: "edit", Target: "a.js"},
			wantMeta: `{"added":3,"removed":1,"ranges":[[11,12],[14,14]],"patch":[{"newStart":10,"lines":[" a","+b","+c"," d","-e","+f"]}]}`,
		},
		{
			name: "edit: the patch wins over the input",
			tool: "Edit", input: `{"file_path":"/p/f.js","old_string":"a","new_string":"b"}`,
			response: `{"structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-a","+b"]}]}`,
			want:     ToolSummary{Action: "edit", Target: "/p/f.js"},
			wantMeta: `{"added":1,"removed":1,"ranges":[[1,1]],"patch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-a","+b"]}]}`,
		},
		{
			// glyph3d-js says {added: 0, removed: 0, ranges: []} here.
			name: "edit: no patch and no replacement in the input: no meta",
			tool: "Edit", input: `{"file_path":"/p/f.js"}`, response: `{}`,
			want: ToolSummary{Action: "edit", Target: "/p/f.js"},
		},
		{
			name: "MultiEdit is an edit",
			tool: "MultiEdit", input: `{"file_path":"/home/user/proj/m.js"}`, response: `{"structuredPatch":[]}`,
			want:     ToolSummary{Action: "edit", Target: "m.js"},
			wantMeta: `{"added":0,"removed":0,"patch":[]}`,
		},
		{
			name: "NotebookEdit is an edit of its notebook_path",
			tool: "NotebookEdit", input: `{"notebook_path":"/home/user/proj/n.ipynb"}`, response: `{"structuredPatch":[]}`,
			want:     ToolSummary{Action: "edit", Target: "n.ipynb"},
			wantMeta: `{"added":0,"removed":0,"patch":[]}`,
		},
		{
			name: "write: kind and lines",
			tool: "Write", input: `{"file_path":"/home/user/proj/w.js"}`, response: `{"type":"create","content":"a\nb\nc"}`,
			want:     ToolSummary{Action: "write", Target: "w.js"},
			wantMeta: `{"kind":"create","lines":3}`,
		},
		{
			name: "bash: the command; its output's lines, a final newline not counted",
			tool: "Bash", input: `{"command":"ls -la"}`, response: `{"stdout":"one\ntwo\n","interrupted":false}`,
			want:     ToolSummary{Action: "bash", Detail: "ls -la"},
			wantMeta: `{"lines":2}`,
		},
		{
			name: "grep: the pattern; its path is no target",
			tool: "Grep", input: `{"path":"/home/user/proj/src","pattern":"TODO"}`, response: `{"content":"src/a.js:1:TODO"}`,
			want: ToolSummary{Action: "grep", Detail: "TODO"},
		},
		{
			name: "glob: the pattern",
			tool: "Glob", input: `{"pattern":"**/*.js"}`,
			want: ToolSummary{Action: "glob", Detail: "**/*.js"},
		},
		{
			name: "task: subagent_type first; tools, tokens and ms",
			tool: "Task", input: `{"subagent_type":"Explore","description":"find x"}`,
			response: `{"totalToolUseCount":7,"totalTokens":1234,"totalDurationMs":4500,"content":"done"}`,
			want:     ToolSummary{Action: "task", Detail: "Explore"},
			wantMeta: `{"tools":7,"tokens":1234,"ms":4500}`,
		},
		{
			// glyph3d-js says {} for the meta.
			name: "Agent is a task",
			tool: "Agent", input: `{"description":"d"}`, response: `{}`,
			want: ToolSummary{Action: "task", Detail: "d"},
		},
		{
			name: "ask: the questions, one per line",
			tool: "AskUserQuestion", input: `{"questions":[{"question":"A?"},{"question":"B?"}]}`, response: `"chose A"`,
			want: ToolSummary{Action: "ask", Detail: "A?\nB?"},
		},
		{
			name: "fetch: the url",
			tool: "WebFetch", input: `{"url":"http://x"}`, response: `{"result":"summary"}`,
			want: ToolSummary{Action: "fetch", Detail: "http://x"},
		},
		{
			name: "search: the query",
			tool: "WebSearch", input: `{"query":"q"}`, response: `{"content":"hits"}`,
			want: ToolSummary{Action: "search", Detail: "q"},
		},
		{
			name: "Kimi TodoList is noise",
			tool: "TodoList", input: `{}`, response: `{}`,
			want: ToolSummary{Action: ActionNoise},
		},
		{
			name: "Kimi EnterPlanMode is noise",
			tool: "EnterPlanMode", input: `{}`, response: `{}`,
			want: ToolSummary{Action: ActionNoise},
		},
		{
			name: "Kimi AgentSwarm is a task: its description",
			tool: "AgentSwarm", input: `{"description":"review each file"}`, response: `{"output":"done"}`,
			want: ToolSummary{Action: "task", Detail: "review each file"},
		},
		{
			name: "Kimi FetchURL is a fetch",
			tool: "FetchURL", input: `{"url":"http://x"}`, response: `{"output":"page text"}`,
			want: ToolSummary{Action: "fetch", Detail: "http://x"},
		},
		{
			name: "Skill: the skill's name",
			tool: "Skill", input: `{"skill":"pdf"}`,
			want: ToolSummary{Action: "skill", Detail: "pdf"},
		},
		{
			name: "agy view_file is a read of its AbsolutePath",
			tool: "view_file", input: `{"AbsolutePath":"/home/user/proj/src/a.js"}`,
			response: `{"file":{"numLines":2,"startLine":5,"totalLines":9}}`,
			want:     ToolSummary{Action: "read", Target: "src/a.js"},
			wantMeta: `{"lines":2,"range":[5,6]}`,
		},
		{
			name: "agy read_file is a read of its Path",
			tool: "read_file", input: `{"Path":"/home/user/proj/b.md"}`,
			want: ToolSummary{Action: "read", Target: "b.md"},
		},
		{
			name: "agy edit_file: a pure deletion has no range",
			tool: "edit_file", input: `{"Path":"/home/user/proj/c.js"}`,
			response: `{"replacement":{"startLine":10,"added":0,"removed":2}}`,
			want:     ToolSummary{Action: "edit", Target: "c.js"},
			wantMeta: `{"added":0,"removed":2}`,
		},
		{
			name: "agy replace_file_content is an edit of its TargetFile",
			tool: "replace_file_content", input: `{"TargetFile":"/home/user/proj/d.js"}`, response: `{}`,
			want: ToolSummary{Action: "edit", Target: "d.js"},
		},
		{
			name: "agy write_file is a write",
			tool: "write_file", input: `{"Path":"/home/user/proj/e.txt"}`, response: `{"type":"update","content":"a\nb"}`,
			want:     ToolSummary{Action: "write", Target: "e.txt"},
			wantMeta: `{"kind":"update","lines":2}`,
		},
		{
			name: "agy run_command is a bash of its CommandLine",
			tool: "run_command", input: `{"CommandLine":"make"}`, response: `{"stdout":"ok\n","output":"ok\n"}`,
			want:     ToolSummary{Action: "bash", Detail: "make"},
			wantMeta: `{"lines":1}`,
		},
		{
			name: "agy manage_task: its action and task",
			tool: "manage_task", input: `{"Action":"kill","TaskId":"c/task-3"}`, response: `{"output":"cancelled"}`,
			want: ToolSummary{Action: "task", Detail: "kill c/task-3"},
		},
		{
			name: "agy invoke_subagent: a single top-level spec",
			tool: "invoke_subagent", input: `{"Role":"R","Prompt":"p"}`,
			want: ToolSummary{Action: "task", Detail: "R"},
		},
		{
			name: "agy search_web is a search",
			tool: "search_web", input: `{"query":"q"}`, response: `{"output":"hits"}`,
			want: ToolSummary{Action: "search", Detail: "q"},
		},
		{
			name: "agy read_url_content is a fetch of its Url",
			tool: "read_url_content", input: `{"Url":"http://x"}`,
			want: ToolSummary{Action: "fetch", Detail: "http://x"},
		},
		{
			name: "TodoWrite is noise",
			tool: "TodoWrite", input: `{"todos":[]}`, response: `{}`,
			want: ToolSummary{Action: ActionNoise},
		},
		{
			// glyph3d-js drops noise; here it's kept, and says what it was about.
			name: "ToolSearch is noise, with its query",
			tool: "ToolSearch", input: `{"query":"select:Read"}`, response: `{}`,
			want: ToolSummary{Action: ActionNoise, Detail: "select:Read"},
		},
		{
			name: "unknown tool: its name lowercased; the first known input key, command before name",
			tool: "SomeNewTool", input: `{"name":"nope","command":"do-it"}`, response: `{"output":"out"}`,
			want: ToolSummary{Action: "somenewtool", Detail: "do-it"},
		},
		{
			name: "out of cwd: the target stays absolute",
			tool: "Read", input: `{"file_path":"/var/tmp/scratch.txt"}`, response: `{"file":{"numLines":1}}`,
			want:     ToolSummary{Action: "read", Target: "/var/tmp/scratch.txt"},
			wantMeta: `{"lines":1}`,
		},

		// Beyond glyph3d-js.
		{
			name: "MultiEdit without a patch: every edit's lines, from the input",
			tool: "MultiEdit",
			input: `{"file_path":"/home/user/proj/m.go","edits":[
				{"old_string":"foo\nbar\nbaz","new_string":"foo\nBAR\nbaz"},
				{"old_string":"x := 1\n","new_string":"x := 1\ny := 2\nz := 3\n"},
				{"old_string":"a\ngone\nb","new_string":"a\nb"}]}`,
			want:     ToolSummary{Action: "edit", Target: "m.go"},
			wantMeta: `{"added":3,"removed":2}`,
		},
		{
			name: "edit from the input: a replacement within one line changes it",
			tool: "Edit", input: `{"file_path":"/home/user/proj/a.go","old_string":"foo","new_string":"bar"}`,
			want:     ToolSummary{Action: "edit", Target: "a.go"},
			wantMeta: `{"added":1,"removed":1}`,
		},
		{
			name: "edit from the input: text split off a line's end is a changed line and a new one",
			tool: "Edit", input: `{"file_path":"/home/user/proj/a.go","old_string":"x","new_string":"y\nx"}`,
			want:     ToolSummary{Action: "edit", Target: "a.go"},
			wantMeta: `{"added":2,"removed":1}`,
		},
		{
			name: "edit from the input: replace_all can't be counted",
			tool: "Edit", input: `{"file_path":"/home/user/proj/a.go","old_string":"a","new_string":"b","replace_all":true}`,
			want: ToolSummary{Action: "edit", Target: "a.go"},
		},
		{
			name: "MultiEdit from the input: one replace_all leaves the whole call uncounted",
			tool: "MultiEdit", input: `{"file_path":"/p/m.go","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d","replace_all":true}]}`,
			want: ToolSummary{Action: "edit", Target: "/p/m.go"},
		},
		{
			name:     "agy replace_file_content from the input",
			tool:     "replace_file_content",
			input:    `{"TargetFile":"/home/user/proj/d.go","TargetContent":"a\nb\n","ReplacementContent":"a\nc\nd\n","AllowMultiple":false}`,
			want:     ToolSummary{Action: "edit", Target: "d.go"},
			wantMeta: `{"added":2,"removed":1}`,
		},
		{
			name: "NotebookEdit: an inserted cell's lines",
			tool: "NotebookEdit", input: `{"notebook_path":"/home/user/proj/n.ipynb","edit_mode":"insert","new_source":"import os\nprint(1)"}`,
			response: `{"new_source":"import os\nprint(1)","edit_mode":"insert"}`,
			want:     ToolSummary{Action: "edit", Target: "n.ipynb"},
			wantMeta: `{"added":2,"removed":0}`,
		},
		{
			name: "NotebookEdit: a replaced cell can't be counted from the input",
			tool: "NotebookEdit", input: `{"notebook_path":"/home/user/proj/n.ipynb","edit_mode":"replace","new_source":"x"}`,
			want: ToolSummary{Action: "edit", Target: "n.ipynb"},
		},
		{
			name: "write while running: its lines from the input",
			tool: "Write", input: `{"file_path":"/home/user/proj/w.go","content":"package w\n\nfunc W() {}\n"}`,
			want:     ToolSummary{Action: "write", Target: "w.go"},
			wantMeta: `{"lines":3}`,
		},
		{
			name: "agy write_to_file: its CodeContent",
			tool: "write_to_file", input: `{"TargetFile":"/home/user/proj/w.md","CodeContent":"# W\n"}`,
			want:     ToolSummary{Action: "write", Target: "w.md"},
			wantMeta: `{"lines":1}`,
		},
		{
			name: "Kimi file tools name the file path",
			tool: "Read", input: `{"path":"/home/user/proj/k.rs"}`,
			want: ToolSummary{Action: "read", Target: "k.rs"},
		},
		{
			name: "Kimi Bash output counts its lines",
			tool: "Bash", input: `{"command":"ls"}`, response: `{"output":"a\nb\nc\n"}`,
			want:     ToolSummary{Action: "bash", Detail: "ls"},
			wantMeta: `{"lines":3}`,
		},
		{
			name: "bash while running: no meta",
			tool: "Bash", input: `{"command":"sleep 9"}`,
			want: ToolSummary{Action: "bash", Detail: "sleep 9"},
		},
		{
			name: "bash interrupted",
			tool: "Bash", input: `{"command":"sleep 9"}`, response: `{"stdout":"","interrupted":true}`,
			want:     ToolSummary{Action: "bash", Detail: "sleep 9"},
			wantMeta: `{"lines":0,"interrupted":true}`,
		},
		{
			name: "read of an image: its size",
			tool: "Read", input: `{"file_path":"/home/user/proj/i.png"}`, response: `{"type":"image","file":{"originalSize":2048}}`,
			want:     ToolSummary{Action: "read", Target: "i.png"},
			wantMeta: `{"bytes":2048}`,
		},
		{
			name: "agy invoke_subagent: its Subagents' roles",
			tool: "invoke_subagent", input: `{"Subagents":[{"Role":"reviewer"},{"TypeName":"tester"}]}`,
			want: ToolSummary{Action: "task", Detail: "reviewer\ntester"},
		},
		{
			name: "agy send_message: the recipient and the whole message",
			tool: "send_message", input: `{"Recipient":"main","Message":"done:\n3 files"}`,
			want: ToolSummary{Action: "send", Detail: "→ main\ndone:\n3 files"},
		},
		{
			name: "agy manage_subagents and schedule are labeled",
			tool: "schedule", input: `{"Prompt":"check the build"}`,
			want: ToolSummary{Action: "task", Detail: "timer: check the build"},
		},
		{
			name: "Workflow without a name is just labeled",
			tool: "Workflow", input: `{"script":"…"}`,
			want: ToolSummary{Action: "task", Detail: "workflow"},
		},
		{
			name: "agy ask_question: its questions",
			tool: "ask_question", input: `{"questions":[{"question":"Which?","options":["a","b"]}]}`,
			want: ToolSummary{Action: "ask", Detail: "Which?"},
		},
		{
			name: "unknown agy tool: agy's own keys",
			tool: "grep_search", input: `{"Query":"TODO","SearchPath":"/home/user/proj"}`,
			want: ToolSummary{Action: "grep_search", Detail: "TODO"},
		},
		{
			name: "unknown tool: a path under cwd is relativized",
			tool: "mcp__fs__stat", input: `{"path":"/home/user/proj/go.mod"}`,
			want: ToolSummary{Action: "mcp__fs__stat", Detail: "go.mod"},
		},
		{
			name: "agy target stored JSON-encoded twice",
			tool: "view_file", input: `{"AbsolutePath":"\"/home/user/proj/q.go\""}`,
			want: ToolSummary{Action: "read", Target: "q.go"},
		},
		{
			name: "a detail is whole, its quotes kept",
			tool: "search_web", input: `{"query":"\"exact phrase\""}`,
			want: ToolSummary{Action: "search", Detail: `"exact phrase"`},
		},
		{
			name: "input that isn't an object",
			tool: "Bash", input: `not json`,
			want: ToolSummary{Action: "bash"},
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			var response json.RawMessage
			if tt.response != "" {
				response = json.RawMessage(tt.response)
			}
			got := SummarizeTool(tt.tool, json.RawMessage(tt.input), response, cwd)
			meta := got.Meta
			got.Meta = nil
			if !reflect.DeepEqual(got, tt.want) {
				t.Errorf("summary = %+v, want %+v", got, tt.want)
			}
			if !sameJSON(meta, tt.wantMeta) {
				t.Errorf("meta = %s, want %s", meta, tt.wantMeta)
			}
		})
	}
}

// sameJSON reports whether got holds the same JSON value as want, or both
// are empty.
func sameJSON(got json.RawMessage, want string) bool {
	if len(got) == 0 || want == "" {
		return len(got) == 0 && want == ""
	}
	var g, w any
	if json.Unmarshal(got, &g) != nil || json.Unmarshal([]byte(want), &w) != nil {
		return false
	}
	return reflect.DeepEqual(g, w)
}

func TestReplacedLinesTooLarge(t *testing.T) {
	big := strings.Repeat("a\n", 3000)
	other := strings.Repeat("b\n", 3000)
	if _, _, ok := replacedLines(big, other); ok {
		t.Error("replacedLines compared 3000×3000 lines, want it to give up")
	}
	if a, r, ok := replacedLines(big+"x", big+"y"); !ok || a != 1 || r != 1 {
		t.Errorf("replacedLines(common ends) = +%d -%d %v, want +1 -1 true", a, r, ok)
	}
}

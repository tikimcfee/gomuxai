package transcript

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// summary renders an event compactly for comparisons: its turn's index,
// op, kind and what it says.
func summary(e Event) string {
	turn := "-"
	if e.Turn != nil {
		turn = fmt.Sprint(e.Turn.Index)
	}
	head := fmt.Sprintf("%s %s %s", turn, e.Op, e.Kind)
	switch {
	case e.Prompt != nil:
		return fmt.Sprintf("%s(%s) %q", head, e.Prompt.Origin, e.Prompt.Text)
	case e.Think != nil && e.Think.Redacted:
		return head + " redacted"
	case e.Think != nil:
		return fmt.Sprintf("%s %q", head, e.Think.Text)
	case e.Say != nil && e.Say.Final:
		return fmt.Sprintf("%s final %q", head, e.Say.Text)
	case e.Say != nil:
		return fmt.Sprintf("%s %q", head, e.Say.Text)
	case e.Tool != nil:
		about := e.Tool.Target
		if about == "" {
			about = e.Tool.Detail
		}
		return fmt.Sprintf("%s %s(%s) %s %q", head, e.Tool.Name, about, e.Tool.Result.Status, e.Tool.Result.Output)
	case e.System != nil:
		return fmt.Sprintf("%s %s %q", head, e.System.Subtype, e.System.Text)
	}
	return head
}

func summaries(events []Event) []string {
	var out []string
	for _, e := range events {
		out = append(out, summary(e))
	}
	return out
}

func readFixture(t *testing.T, name string) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

// parse reads data whole with r.
func parse(r Reader, data []byte) (*Parser, []Event) {
	p := r.NewParser(Options{})
	return p, p.Feed(data)
}

func TestClaudeParse(t *testing.T) {
	p, events := parse(Claude{}, readFixture(t, "claude.jsonl"))
	want := []string{
		`1 add prompt(human) "run the tests"`,
		`1 add think "go test should do it"`,
		`1 add tool Bash(go test ./...) pending ""`,
		`1 add tool Read(go.mod) pending ""`,
		`1 update tool Bash(go test ./...) ok "ok  \tpkg/a\t0.1s\nok  \tpkg/b\t0.2s"`,
		`1 update tool Read(go.mod) ok "1\tmodule example.com/proj"`,
		`1 add say "All tests pass."`,
		// The caveat is meta: not a prompt. A task notification opens a
		// turn of its own, with the index of the human prompt before it.
		`2 add prompt(command) "/review high"`,
		`2 add tool Bash(make lint) pending ""`,
		`2 update tool Bash(make lint) error "Exit code 2\nlint: 1 issue"`,
		`2 add prompt(task-notification) "<task-notification>background task finished</task-notification>"`,
		`2 add say "One lint issue.\nSee above."`,
		`2 add tool Grep(TODO) pending ""`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	for i, e := range events {
		if e.Seq != i {
			t.Errorf("event %d has seq %d", i, e.Seq)
		}
	}
	if e := events[2]; e.ID != "tu-bash" || e.TS != "2026-08-01T10:00:01.000Z" || e.Line != 7 {
		t.Errorf("tool event = %+v, want tu-bash at line 7 with its line's timestamp", e)
	}

	// Sidechain records don't leak their cwd; api-request-blob copies add nothing.
	s := p.Session()
	if s.Harness != "claude" || s.CWD != "/home/user/proj" || s.Project != "proj" || s.SessionID != "00000000-0000-0000-0000-00000000c1a0" ||
		s.Version != "2.1.400" || s.GitBranch != "main" || s.Title != "Run the tests" || s.Model != "claude-test-1" {
		t.Errorf("session = %+v", s)
	}
	if p.Lines != 20 || p.Skipped != 1 {
		t.Errorf("lines = %d, skipped = %d, want 20 and 1", p.Lines, p.Skipped)
	}
}

// The Claude part of glyph3d-rs tests.rs (test_synthetic_claude_session_parity):
// results pair by id across distance, and land as updates of their calls.
func TestClaudeParse_Pairing(t *testing.T) {
	lines := []string{
		`{"type":"assistant","uuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Edit","input":{"file_path":"/r/a.js"}}]}}`,
		`{"type":"assistant","uuid":"u2","message":{"role":"assistant","content":[{"type":"tool_use","id":"b","name":"Bash","input":{"command":"ls"}}]}}`,
		`{"type":"user","uuid":"u3","toolUseResult":{"stdout":"one\ntwo\n"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}]}}`,
		`{"type":"user","uuid":"u4","toolUseResult":{"structuredPatch":[]},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"The file has been updated"}]}}`,
		`{"type":"user","uuid":"u5","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"unseen","content":"its call came before"}]}}`,
	}
	_, events := parse(Claude{}, []byte(strings.Join(lines, "\n")+"\n"))
	want := []string{
		`- add tool Edit(/r/a.js) pending ""`,
		`- add tool Bash(ls) pending ""`,
		`- update tool Bash(ls) ok "one\ntwo"`,
		`- update tool Edit(/r/a.js) ok "The file has been updated"`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if string(events[2].Tool.Result.Structured) != `{"stdout":"one\ntwo\n"}` || events[2].Line != 2 {
		t.Errorf("update = %+v, want the result's toolUseResult and line", events[2])
	}
	// The add the parser handed out earlier is left as it was.
	if events[1].Tool.Result.Status != StatusPending {
		t.Errorf("an update changed its add: %+v", events[1].Tool.Result)
	}
}

// User records of shapes the corpus doesn't hold, seen in real transcripts
// (after glyph3d-rs tests.rs, user_prompt_text_classifies_real_record_shapes):
// what each one is, on its own.
func TestClaudeUserRecords(t *testing.T) {
	for _, tt := range []struct{ fields, content, want string }{
		{``, `"  Fix the fog  "`, `1 add prompt(human) "  Fix the fog  "`},
		{``, `[{"type":"text","text":"pasted"}]`, `1 add prompt(human) "pasted"`},
		{`"promptSource":"sdk",`, `"from the SDK"`, `1 add prompt(human) "from the SDK"`},
		{``, `"<command-name>/model</command-name>\n<command-args></command-args>"`, `1 add prompt(command) "/model"`},
		{``, `"<command-name>review</command-name>\n<command-args>high</command-args>"`, `1 add prompt(command) "/review high"`},
		{`"origin":{"kind":"human"},`, `"<pasted_content id=\"a1\">log</pasted_content>"`, `1 add prompt(human) "<pasted_content id=\"a1\">log</pasted_content>"`},
		{``, `"<bash-input>ls</bash-input>"`, `1 add prompt(human) "<bash-input>ls</bash-input>"`},
		{`"isMeta":true,"origin":{"kind":"peer"},`, `"<agent-message from=\"x\">hi</agent-message>"`, `- add prompt(peer) "<agent-message from=\"x\">hi</agent-message>"`},
		{`"isMeta":true,"origin":{"kind":"channel"},`, `"<channel source=\"chat\">hi</channel>"`, `- add prompt(peer) "<channel source=\"chat\">hi</channel>"`},
		{`"isMeta":true,"origin":{"kind":"coordinator"},`, `"next step"`, `- add prompt(peer) "next step"`},
		{`"isMeta":true,"origin":{"kind":"auto-continuation"},`, `"continue"`, `- add prompt(system) "continue"`},
		// The harness's own prompts: a /loop wakeup or an idle notice opens
		// its turn; Stop hook feedback continues the current one.
		{`"isMeta":true,"promptSource":"system","promptId":"w1","turnPosition":{"promptIndex":4},`, `"[Cross-session idle notice] nothing new"`, `4 add prompt(system) "[Cross-session idle notice] nothing new"`},
		{`"isMeta":true,"promptSource":"system",`, `"Stop hook feedback:\nrun the linter"`, `- add prompt(system) "Stop hook feedback:\nrun the linter"`},
		// With no prompt id, a task notification opens a (derived) turn.
		{`"isMeta":true,"origin":{"kind":"task-notification"},`, `"<task-notification>done</task-notification>"`, `1 add prompt(task-notification) "<task-notification>done</task-notification>"`},
		{``, `"<local-command-stdout>Set model</local-command-stdout>"`, `- add system local_command "<local-command-stdout>Set model</local-command-stdout>"`},
		{``, `[{"type":"text","text":"[Request interrupted by user]"}]`, `- add system interrupt "[Request interrupted by user]"`},
		{`"isMeta":true,`, `"<local-command-caveat>Caveat</local-command-caveat>"`, ``},
		{`"isMeta":true,`, `"[Image: original 100x100]"`, ``},
		{``, `[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]`, ``},
		{``, `"   "`, ``},
	} {
		line := `{"type":"user","uuid":"u",` + tt.fields + `"message":{"role":"user","content":` + tt.content + `}}` + "\n"
		_, events := parse(Claude{}, []byte(line))
		if got := strings.Join(summaries(events), "\n"); got != tt.want {
			t.Errorf("%s\n got %q\nwant %q", line, got, tt.want)
		}
	}
}

// Records the corpus doesn't hold: a task notification taken mid-turn (an
// attachment) completes its call in the current turn; hook context from a
// hook other than Stop is a system prompt; a record that holds a result and
// an interrupt updates the call it answers, then interrupts the rest.
func TestClaudeMidTurn(t *testing.T) {
	lines := []string{
		`{"type":"user","uuid":"u1","promptId":"p1","turnPosition":{"promptIndex":3},"message":{"role":"user","content":"start it and lint"}}`,
		`{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"tool_use","id":"bg","name":"Bash","input":{"command":"make serve","run_in_background":true}}],"stop_reason":"tool_use"}}`,
		`{"type":"user","uuid":"u2","promptId":"p1","toolUseResult":{"stdout":"","backgroundTaskId":"t9"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"bg","content":"Command running in background with ID: t9."}]}}`,
		`{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"tool_use","id":"lint","name":"Bash","input":{"command":"make lint"}}],"stop_reason":"tool_use"}}`,
		`{"type":"attachment","uuid":"at1","attachment":{"type":"queued_command","prompt":"<task-notification>\n<tool-use-id>bg</tool-use-id>\n<status>failed</status>\n<summary>exit 2</summary>\n</task-notification>","commandMode":"task-notification"}}`,
		`{"type":"attachment","uuid":"at2","attachment":{"type":"hook_additional_context","content":["lint is slow today"],"hookEvent":"PostToolUse"}}`,
		`{"type":"assistant","uuid":"a3","message":{"role":"assistant","content":[{"type":"tool_use","id":"vet","name":"Bash","input":{"command":"go vet"}}],"stop_reason":"tool_use"}}`,
		`{"type":"user","uuid":"u3","promptId":"p1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"lint","content":"ok"},{"type":"text","text":"[Request interrupted by user for tool use]"}]}}`,
	}
	_, events := parse(Claude{}, []byte(strings.Join(lines, "\n")+"\n"))
	want := []string{
		`3 add prompt(human) "start it and lint"`,
		`3 add tool Bash(make serve) pending ""`,
		`3 update tool Bash(make serve) background "Command running in background with ID: t9."`,
		`3 add tool Bash(make lint) pending ""`,
		`3 add prompt(task-notification) "<task-notification>\n<tool-use-id>bg</tool-use-id>\n<status>failed</status>\n<summary>exit 2</summary>\n</task-notification>"`,
		`3 update tool Bash(make serve) error "exit 2"`,
		`3 add prompt(system) "lint is slow today"`,
		`3 add tool Bash(go vet) pending ""`,
		`3 add system interrupt "[Request interrupted by user for tool use]"`,
		`3 update tool Bash(make lint) ok "ok"`,
		`3 update tool Bash(go vet) interrupted ""`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if e := events[8]; e.ID != "u3#1" {
		t.Errorf("interrupt id %q, want its block's, u3#1", e.ID)
	}
}

// One message can hold several task notifications, after a preamble: each
// completes its own call.
func TestClaudeTaskNotifications(t *testing.T) {
	bg := func(n int) string {
		return fmt.Sprintf(`{"type":"assistant","uuid":"a%d","message":{"role":"assistant","content":[{"type":"tool_use","id":"bg%d","name":"Bash","input":{"command":"job %d"}}],"stop_reason":"tool_use"}}`+"\n"+
			`{"type":"user","uuid":"r%d","promptId":"p1","toolUseResult":{"backgroundTaskId":"t%d"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"bg%d","content":"started"}]}}`, n, n, n, n, n, n)
	}
	notes := `[SYSTEM NOTIFICATION - two tasks finished]\n` +
		`<task-notification>\n<tool-use-id>bg1</tool-use-id>\n<status>completed</status>\n<summary>job 1 done</summary>\n</task-notification>\n` +
		`<task-notification>\n<tool-use-id>bg2</tool-use-id>\n<status>killed</status>\n<summary>job 2 stopped</summary>\n</task-notification>`
	lines := []string{
		`{"type":"user","uuid":"u1","promptId":"p1","turnPosition":{"promptIndex":1},"message":{"role":"user","content":"run both"}}`,
		bg(1), bg(2),
		`{"type":"user","uuid":"u2","promptId":"p2","origin":{"kind":"task-notification"},"message":{"role":"user","content":"` + notes + `"}}`,
	}
	_, events := parse(Claude{}, []byte(strings.Join(lines, "\n")+"\n"))
	got := summaries(events)
	want := []string{
		`1 update tool Bash(job 1) ok "job 1 done"`,
		`1 update tool Bash(job 2) interrupted "job 2 stopped"`,
	}
	if len(got) < 2 || !slices.Equal(got[len(got)-2:], want) || !strings.HasPrefix(got[len(got)-3], "1 add prompt(task-notification)") {
		t.Errorf("events:\n%s\nwant to end with a notification, then:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if n := TaskNotifications(strings.ReplaceAll(notes, `\n`, "\n")); len(n) != 2 || n[1].ToolUseID != "bg2" || n[1].Status != "killed" {
		t.Errorf("TaskNotifications = %+v", n)
	}
}

// HarvestClaude reads the session's metadata alone, line by line, as the
// parser does.
func TestHarvestClaude(t *testing.T) {
	var s Session
	for line := range bytes.SplitSeq(readFixture(t, "claude.jsonl"), []byte{'\n'}) {
		HarvestClaude(&s, line)
	}
	p, _ := parse(Claude{}, readFixture(t, "claude.jsonl"))
	if want := p.Session(); !reflect.DeepEqual(s, want) {
		t.Errorf("harvested %+v\nparsed    %+v", s, want)
	}
	for _, line := range []string{
		`{"type":"custom-title","customTitle":"mine"}`,
		`{"type":"ai-title","aiTitle":"later AI title"}`,
		`{"type":"agent-name","agentName":"fixer"}`,
		`{"type":"continued-in","continuedInSessionId":"next"}`,
		`{"type":"assistant","message":{"role":"assistant","model":"<synthetic>","content":[]}}`,
	} {
		HarvestClaude(&s, []byte(line))
	}
	if s.Title != "mine" || s.AgentName != "fixer" || s.ContinuedIn != "next" || s.Model != "claude-test-1" {
		t.Errorf("session = %+v", s)
	}
}

func TestAgyParse(t *testing.T) {
	p, events := parse(Agy{}, readFixture(t, "agy.jsonl"))
	want := []string{
		`1 add prompt(human) "list the files"`,
		`1 add think "I should list them and read the readme."`,
		`1 add tool run_command(ls) pending ""`,
		`1 add tool view_file(/home/user/proj/README) pending ""`,
		// Two calls in one step: each GENERIC step answers the next one.
		`1 update tool run_command(ls) ok "The command exited with code 0.\nOutput:\nREADME\nmain.go"`,
		`1 update tool view_file(/home/user/proj/README) ok "File Path: ` + "`file:///home/user/proj/README`" + `\nTotal Lines: 2\n1: hello\n2: world"`,
		`1 add say final "Two files: README and main.go."`,
		`2 add prompt(human) "build it"`,
		`2 add tool run_command(go build ./...) pending ""`,
		`2 update tool run_command(go build ./...) error "The command exited with code 1.\nOutput:\nmain.go:3: undefined: x"`,
		`2 add tool view_file(/etc/secret) pending ""`,
		`2 add tool run_command(sleep 100) pending ""`,
		`2 update tool view_file(/etc/secret) error "Encountered error in step execution: permission denied"`,
		`2 update tool run_command(sleep 100) background "Tool is running as a background task with task id: t-1"`,
		`2 add tool run_command(go vet ./...) pending ""`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if p.Lines != 14 || p.Skipped != 1 {
		t.Errorf("lines = %d, skipped = %d, want 14 and 1", p.Lines, p.Skipped)
	}
	if s := p.Session(); s.Harness != "agy" {
		t.Errorf("session = %+v", s)
	}
}

func TestAgyArgs(t *testing.T) {
	tests := []struct{ name, args, want string }{
		{
			name: "transcript.jsonl: every value is its JSON in a string",
			args: `{"CommandLine":"\"echo hi\\nls\"","WaitMsBeforeAsync":"500","AllowMultiple":"false","questions":"[{\"question\":\"Q?\"}]","toolSummary":"\"Run\""}`,
			want: `{"AllowMultiple":false,"CommandLine":"echo hi\nls","WaitMsBeforeAsync":500,"questions":[{"question":"Q?"}],"toolSummary":"Run"}`,
		},
		{
			name: "transcript_full.jsonl: plain values stay, a quoted search's quotes too",
			args: `{"query":"\"exact phrase\"","toolSummary":"Search the web"}`,
			want: `{"query":"\"exact phrase\"","toolSummary":"Search the web"}`,
		},
		{
			name: "a value that isn't a string means plain",
			args: `{"CommandLine":"\"ls\"","WaitMsBeforeAsync":500}`,
			want: `{"CommandLine":"\"ls\"","WaitMsBeforeAsync":500}`,
		},
		{name: "no args", args: `{}`, want: `{}`},
		{name: "not an object", args: `null`, want: `null`},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := agyArgs(json.RawMessage(tt.args)); string(got) != tt.want {
				t.Errorf("agyArgs = %s, want %s", got, tt.want)
			}
		})
	}

	// Decoded, an encoded replacement's lines count.
	args := agyArgs(json.RawMessage(`{"TargetFile":"\"/p/d.go\"","TargetContent":"\"a\\nb\"","ReplacementContent":"\"a\\nc\\nd\\ne\"","AllowMultiple":"false"}`))
	if s := SummarizeTool("replace_file_content", args, nil, ""); s.Target != "/p/d.go" || string(s.Meta) != `{"added":3,"removed":1}` {
		t.Errorf("encoded replace_file_content: %+v, meta %s; want /p/d.go, +3 -1", s, s.Meta)
	}
}

func TestDetect(t *testing.T) {
	if r := Detect(readFixture(t, "agy.jsonl")); r.Harness() != "agy" {
		t.Errorf("agy fixture detected as %s", r.Harness())
	}
	if r := Detect(readFixture(t, "claude.jsonl")); r.Harness() != "claude" {
		t.Errorf("claude fixture detected as %s", r.Harness())
	}
}

func TestDegenerateInputs(t *testing.T) {
	for _, r := range []Reader{Claude{}, Agy{}} {
		if p, events := parse(r, nil); len(events) != 0 || p.Lines != 0 {
			t.Errorf("%s: empty input gave %d events, %d lines", r.Harness(), len(events), p.Lines)
		}
		p, events := parse(r, []byte("null\n42\n\"str\"\n{invalid json\n[]\n"))
		if len(events) != 0 || p.Lines != 5 || p.Skipped != 5 {
			t.Errorf("%s: garbage gave %d events, %d lines, %d skipped", r.Harness(), len(events), p.Lines, p.Skipped)
		}
	}
}

// Fed in pieces, as a live transcript grows, a parser makes the events it
// makes from the whole; a line arrives once its newline does.
func TestParser_FeedInPieces(t *testing.T) {
	data := readFixture(t, "claude.jsonl")
	_, whole := parse(Claude{}, data)

	p := Claude{}.NewParser(Options{})
	var pieces []Event
	for end := 0; end <= len(data); end += 97 {
		pending := data[p.Pos().At:min(end, len(data))]
		pieces = append(pieces, p.Feed(pending)...)
	}
	pieces = append(pieces, p.Feed(data[p.Pos().At:])...)
	if !reflect.DeepEqual(pieces, whole) {
		t.Errorf("fed in pieces:\n%s\nwhole:\n%s", strings.Join(summaries(pieces), "\n"), strings.Join(summaries(whole), "\n"))
	}
	if pos := p.Pos(); pos.At != int64(len(data)) || pos.Line != 20 || pos.Seq != len(whole) {
		t.Errorf("stopped at %+v, want the end: %d bytes, 20 lines, %d events", pos, len(data), len(whole))
	}

}

// Resumed after any line, a parse sends exactly what the whole stream
// sends after that line: same events, seq, turns, and updates to calls
// made before it.
func TestResumeAfter(t *testing.T) {
	paths, err := filepath.Glob(filepath.Join(corpusDir, "claude", "*.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	paths = append(paths, filepath.Join("testdata", "claude.jsonl"))
	for _, path := range paths {
		if strings.HasSuffix(path, ".events.jsonl") {
			continue
		}
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		for _, requests := range []bool{false, true} {
			opts := Options{Requests: requests}
			whole := Claude{}.NewParser(opts).Feed(data)
			for at, line := int64(0), 0; at < int64(len(data)); line++ {
				p, err := ResumeAfter(Claude{}, opts, data, at)
				if err != nil {
					t.Fatalf("%s: %v", path, err)
				}
				got := p.Feed(data[p.Pos().At:])
				var want []Event
				for _, e := range whole {
					if e.Line > line {
						want = append(want, e)
					}
				}
				if !reflect.DeepEqual(got, want) {
					t.Errorf("%s, after line %d (requests %v):\n%s\nwant:\n%s", path, line, requests,
						strings.Join(summaries(got), "\n"), strings.Join(summaries(want), "\n"))
				}
				at += int64(bytes.IndexByte(data[at:], '\n')) + 1
			}
		}
	}

	data := readFixture(t, "claude.jsonl")
	for _, at := range []int64{1, int64(len(data)), int64(len(data)) + 5} {
		if _, err := ResumeAfter(Claude{}, Options{}, data, at); err == nil {
			t.Errorf("offset %d: no error", at)
		}
	}
	if _, err := ResumeAfter(Claude{}, Options{}, []byte(`{"type":"user"`), 0); err == nil || !strings.Contains(err.Error(), "no end yet") {
		t.Errorf("unfinished line: %v", err)
	}
}

func TestLatest(t *testing.T) {
	_, events := parse(Claude{}, readFixture(t, "claude.jsonl"))
	latest := Latest(events)
	want := []string{
		`1 add prompt(human) "run the tests"`,
		`1 add think "go test should do it"`,
		`1 add tool Bash(go test ./...) ok "ok  \tpkg/a\t0.1s\nok  \tpkg/b\t0.2s"`,
		`1 add tool Read(go.mod) ok "1\tmodule example.com/proj"`,
		`1 add say "All tests pass."`,
		`2 add prompt(command) "/review high"`,
		`2 add tool Bash(make lint) error "Exit code 2\nlint: 1 issue"`,
		`2 add prompt(task-notification) "<task-notification>background task finished</task-notification>"`,
		`2 add say "One lint issue.\nSee above."`,
		`2 add tool Grep(TODO) pending ""`,
	}
	if got := summaries(latest); !slices.Equal(got, want) {
		t.Errorf("latest:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	// A tool keeps where and when it was called.
	if latest[2].Line != 7 || latest[2].Seq != 2 {
		t.Errorf("folded tool at line %d, seq %d; want its call's, 7 and 2", latest[2].Line, latest[2].Seq)
	}
	// An update without its add (a window's) is left out.
	if got := Latest(events[4:5]); len(got) != 0 {
		t.Errorf("a lone update gave %v", summaries(got))
	}
}

func writeFile(t *testing.T, name, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestReadFile(t *testing.T) {
	path := writeFile(t, "00000000-0000-0000-0000-0000000000aa.jsonl", string(readFixture(t, "claude.jsonl")))
	f, err := ReadFile(path, Claude{}, 50)
	if err != nil {
		t.Fatal(err)
	}
	if len(f.Events) != 13 || f.Partial || f.Session.Path != path || f.Session.Size != int64(len(readFixture(t, "claude.jsonl"))) ||
		f.Session.SessionID != "00000000-0000-0000-0000-00000000c1a0" {
		t.Errorf("got %d events, partial %v, session %+v", len(f.Events), f.Partial, f.Session)
	}

	// Without a reader, the format is detected.
	path = writeFile(t, "transcript_full.jsonl", string(readFixture(t, "agy.jsonl")))
	if f, err := ReadFile(path, nil, 50); err != nil || f.Session.Harness != "agy" || len(f.Events) != 15 {
		t.Errorf("detected: %+v, %v", f, err)
	}
}

// A large transcript is read from its end, in a window that grows until it
// holds enough events; metadata still comes from its head.
func TestReadFile_TailWindow(t *testing.T) {
	var b strings.Builder
	b.WriteString(`{"type":"user","uuid":"p","cwd":"/home/user/big","sessionId":"big","timestamp":"2026-08-01T10:00:00Z","message":{"role":"user","content":"first"}}` + "\n")
	pad := strings.Repeat("x", 2000)
	for i := range 3000 {
		fmt.Fprintf(&b, `{"type":"assistant","uuid":"a%d","timestamp":"2026-08-01T10:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"reply %d"}]},"pad":"%s"}`+"\n", i, i, pad)
	}
	path := writeFile(t, "big.jsonl", b.String())

	for _, n := range []int{3, 1000} {
		f, err := ReadFile(path, Claude{}, n)
		if err != nil {
			t.Fatal(err)
		}
		events := f.Events
		if len(events) < n || !f.Partial {
			t.Fatalf("n=%d: %d events, partial %v", n, len(events), f.Partial)
		}
		if last := events[len(events)-1]; last.Say.Text != "reply 2999" || last.Line != 3000 {
			t.Errorf("n=%d: last event %q at line %d, want reply 2999 at 3000", n, last.Say.Text, last.Line)
		}
		if first := events[0]; first.Say == nil || first.Say.Text != fmt.Sprintf("reply %d", first.Line-1) {
			t.Errorf("n=%d: first event %+v: the window's partial first line must be dropped, and lines counted", n, first)
		}
		if s := f.Session; s.CWD != "/home/user/big" || s.SessionID != "big" || f.Skipped != 0 {
			t.Errorf("n=%d: cwd %q, session %q, skipped %d", n, s.CWD, s.SessionID, f.Skipped)
		}
	}

	// Asking for more than there is reads it all.
	f, err := ReadFile(path, Claude{}, 1_000_000)
	if err != nil {
		t.Fatal(err)
	}
	if len(f.Events) != 3001 || f.Partial {
		t.Errorf("whole file: %d events, partial %v", len(f.Events), f.Partial)
	}
}

// A line still being written is left out, not counted as unparseable.
func TestReadFile_PartialLastLine(t *testing.T) {
	path := writeFile(t, "s.jsonl", `{"type":"user","uuid":"u","message":{"role":"user","content":"hi"}}`+"\n"+`{"type":"assistant","message":{"role":"assis`)
	f, err := ReadFile(path, Claude{}, 50)
	if err != nil {
		t.Fatal(err)
	}
	if len(f.Events) != 1 || f.Lines != 1 || f.Skipped != 0 {
		t.Errorf("%d events, %d lines, %d skipped", len(f.Events), f.Lines, f.Skipped)
	}
}

func TestReadFile_Errors(t *testing.T) {
	if _, err := ReadFile(filepath.Join(t.TempDir(), "none.jsonl"), Claude{}, 50); !errors.Is(err, os.ErrNotExist) {
		t.Errorf("missing file: %v", err)
	}
	path := writeFile(t, "s.jsonl", "plain text\nnot a transcript\n")
	if _, err := ReadFile(path, Claude{}, 50); !errors.Is(err, ErrUnparseable) {
		t.Errorf("text file: %v", err)
	}
	// An empty transcript (a session that hasn't written yet) is not an error.
	if f, err := ReadFile(writeFile(t, "e.jsonl", ""), Agy{}, 50); err != nil || len(f.Events) != 0 {
		t.Errorf("empty file: %v, %v", f, err)
	}
}

// A subagent's events carry the turn of the parent's call that started it,
// its index read from the parent's transcript.
func TestReadFile_SubagentTurn(t *testing.T) {
	path := filepath.Join(corpusDir, "claude", "subagent", "subagents", "agent-a3f9c2e17b4d5e608.jsonl")
	turns := ClaudeSubagentTurns(path)
	for id, want := range map[string]int{
		"b92d70b1-18b1-5ccf-96b7-bd5d1ce10135": 1, // the prompt
		"f6c51b03-1d25-579e-8362-5a8ab18cb715": 1, // the task notification, sharing its index
	} {
		if got, ok := turns(id); !ok || got != want {
			t.Errorf("turn %s = %d, %v; want %d", id, got, ok, want)
		}
	}
	if _, ok := turns("unknown"); ok {
		t.Error("a turn the parent doesn't have")
	}
	f, err := ReadFile(path, Claude{}, 50)
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range f.Events {
		if e.Turn == nil || e.Turn.ID != "b92d70b1-18b1-5ccf-96b7-bd5d1ce10135" || e.Turn.Index != 1 || e.Agent != "a3f9c2e17b4d5e608" {
			t.Errorf("event %s: turn %+v, agent %q; want the parent's turn 1", e.ID, e.Turn, e.Agent)
		}
	}
	if ClaudeSubagentTurns(filepath.Join(corpusDir, "claude", "subagent.jsonl")) != nil {
		t.Error("turns for a transcript that isn't a subagent's")
	}
}

// A background subagent outlives the parent's turn that started it: its
// later records name the parent's current turn, which the lookup finds
// in what the parent has written since. A turn the parent doesn't number
// keeps the last index known.
func TestClaudeSubagentTurns_Grow(t *testing.T) {
	dir := t.TempDir()
	parent := filepath.Join(dir, "s1.jsonl")
	prompt := func(id string, index int) string {
		return fmt.Sprintf(`{"type":"user","uuid":"%s","promptId":"%s","turnPosition":{"promptIndex":%d},"sessionId":"s1","message":{"role":"user","content":"go"}}`+"\n", id, id, index)
	}
	writeTestFile := func(path, content string) {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	writeTestFile(parent, prompt("p1", 1))
	sub := filepath.Join(dir, "s1", "subagents", "agent-x.jsonl")
	say := func(uuid, promptID string) string {
		return fmt.Sprintf(`{"type":"user","uuid":"%s","promptId":"%s","agentId":"x","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"none","content":""}]}}`+"\n", uuid, promptID) +
			fmt.Sprintf(`{"type":"assistant","uuid":"%s-a","agentId":"x","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}`+"\n", uuid)
	}
	p := Claude{}.NewParser(Options{Turns: ClaudeSubagentTurns(sub)})
	var events []Event
	events = append(events, p.Feed([]byte(say("r1", "p1")))...)
	// The parent moves on to its second turn; the subagent follows.
	writeTestFile(parent, prompt("p1", 1)+prompt("p2", 2))
	events = append(events, p.Feed([]byte(say("r2", "p2")))...)
	events = append(events, p.Feed([]byte(say("r3", "p9")))...)
	var got []string
	for _, e := range events {
		got = append(got, fmt.Sprintf("%s %d", e.Turn.ID, e.Turn.Index))
	}
	if want := []string{"p1 1", "p2 2", "p9 2"}; !slices.Equal(got, want) {
		t.Errorf("turns %q, want %q", got, want)
	}
}

// Claude Code versions that wrote promptId but no turnPosition: a turn's
// index counts the human prompts, as a derived turn's does.
func TestClaudeCountedTurns(t *testing.T) {
	lines := []string{
		`{"type":"user","uuid":"u1","promptId":"p1","message":{"role":"user","content":"first"}}`,
		`{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"one"}],"stop_reason":"end_turn"}}`,
		`{"type":"user","uuid":"u2","promptId":"p2","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<summary>done</summary>\n</task-notification>"}}`,
		`{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"text","text":"noted"}],"stop_reason":"end_turn"}}`,
		`{"type":"user","uuid":"u3","promptId":"p3","message":{"role":"user","content":"<command-name>/review</command-name>"}}`,
		`{"type":"assistant","uuid":"a3","message":{"role":"assistant","content":[{"type":"text","text":"reviewed"}],"stop_reason":"end_turn"}}`,
	}
	_, events := parse(Claude{}, []byte(strings.Join(lines, "\n")+"\n"))
	var got []string
	for _, e := range events {
		got = append(got, fmt.Sprintf("%s %d %v", e.Turn.ID, e.Turn.Index, e.Turn.Derived))
	}
	if want := []string{"p1 1 false", "p1 1 false", "p2 1 false", "p2 1 false", "p3 2 false", "p3 2 false"}; !slices.Equal(got, want) {
		t.Errorf("turns %q, want %q", got, want)
	}
}

func TestSessionIDFromPath(t *testing.T) {
	for path, want := range map[string]string{
		"/home/user/.claude/projects/-p/0000-aa.jsonl":                                              "0000-aa",
		"/home/user/.claude/projects/-p/0000-aa/subagents/agent-a1b2.jsonl":                         "agent-a1b2",
		"/home/user/.gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript_full.jsonl": "c-1",
		"/tmp/transcript.jsonl": "",
	} {
		if got := sessionIDFromPath(path); got != want {
			t.Errorf("%s: %q, want %q", path, got, want)
		}
	}
}

// Opt-in smoke test against real transcripts on this machine, as in
// glyph3d-rs tests.rs: GOMUX_REAL_TRANSCRIPTS=<path>[:<path>...] go test.
// Unset, it checks nothing, so it never depends on whose machine runs it.
func TestRealTranscriptsIfPresent(t *testing.T) {
	paths := os.Getenv("GOMUX_REAL_TRANSCRIPTS")
	if paths == "" {
		t.Skip("GOMUX_REAL_TRANSCRIPTS not set")
	}
	for _, path := range filepath.SplitList(paths) {
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		r := Detect(data)
		f, err := ReadFile(path, r, 1<<30)
		if err != nil {
			t.Fatalf("%s: %v", path, err)
		}
		kinds := map[string]int{}
		for _, e := range Latest(f.Events) {
			kinds[e.Kind]++
			if e.Tool != nil {
				kinds[e.Tool.Result.Status]++
			}
			if e.Turn != nil && e.Turn.Index == 0 {
				kinds["turn index 0"]++
			}
		}
		t.Logf("%s: %s, %d events, %d lines, %d skipped, %v", filepath.Base(path), r.Harness(), len(f.Events), f.Lines, f.Skipped, kinds)
		if f.Skipped > 0 {
			t.Errorf("%s: %d lines skipped", path, f.Skipped)
		}
	}
}

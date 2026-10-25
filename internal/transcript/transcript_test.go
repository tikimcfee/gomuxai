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
		// The caveat is meta: not a prompt. The next prompt ends the turn,
		// so its last reply, which recorded no stop reason, is final. A
		// task notification opens a turn of its own, with the index of the
		// human prompt before it.
		`2 add prompt(command) "/review high"`,
		`1 update say final "All tests pass."`,
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

// Older versions' replies carry no stop reason: the last one of a turn
// becomes final once the turn ends (its timing record, or the next
// prompt), not when the model goes on or the turn is interrupted. A stop
// sequence ends a turn too. A model fallback is a system event.
func TestClaudeClosingReplies(t *testing.T) {
	say := func(uuid, text, stop string) string {
		return fmt.Sprintf(`{"type":"assistant","uuid":"%s","message":{"role":"assistant","content":[{"type":"text","text":"%s"}],"stop_reason":%s}}`, uuid, text, stop)
	}
	prompt := func(uuid, text string) string {
		return fmt.Sprintf(`{"type":"user","uuid":"%s","message":{"role":"user","content":"%s"}}`, uuid, text)
	}
	lines := []string{
		prompt("u1", "one"),
		say("a1", "thinking aloud", "null"),
		say("a2", "done with one", "null"),
		`{"type":"system","subtype":"turn_duration","uuid":"s1","durationMs":5}`,
		prompt("u2", "two"),
		say("a3", "done with two", "null"),
		prompt("u3", "/compact keep the plan"),
		say("a4", "partial", "null"),
		prompt("u4", "[Request interrupted by user]"),
		prompt("u5", "/etc/hosts is empty"),
		`{"type":"assistant","uuid":"a5","message":{"role":"assistant","content":[{"type":"fallback","from":"model-a","to":"model-b"},{"type":"text","text":"checked"}],"stop_reason":"stop_sequence"}}`,
	}
	_, events := parse(Claude{}, []byte(strings.Join(lines, "\n")+"\n"))
	want := []string{
		`1 add prompt(human) "one"`,
		`1 add say "thinking aloud"`,
		`1 add say "done with one"`,
		`1 add system turn_duration ""`,
		`1 update say final "done with one"`,
		`2 add prompt(human) "two"`,
		`2 add say "done with two"`,
		`3 add prompt(command) "/compact keep the plan"`,
		`2 update say final "done with two"`,
		`3 add say "partial"`,
		`3 add system interrupt "[Request interrupted by user]"`,
		`4 add prompt(human) "/etc/hosts is empty"`,
		`4 add system model_fallback ""`,
		`4 add say final "checked"`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if e := events[7]; e.Prompt.Command != "/compact" {
		t.Errorf("command %q, want /compact", e.Prompt.Command)
	}
	if e := events[8]; e.Line != 6 || e.ID != "a3#0" {
		t.Errorf("closing update at line %d, id %s; want the next prompt's line 6, a3#0", e.Line, e.ID)
	}
	if e := events[12]; string(e.System.Data) != `{"from":"model-a","to":"model-b"}` {
		t.Errorf("fallback data %s", e.System.Data)
	}
}

// A request's blobs are the messages as the blob lines hold them, whole:
// no field the reader doesn't know is lost, none is added.
func TestClaudeRequestBlobs(t *testing.T) {
	blob := `{"role":"assistant","content":[{"type":"text","text":"hi"}],"output_config":{"effort":"high"}}`
	lines := []string{
		`{"type":"api-request-blob","hash":"h1","message":` + blob + `}`,
		`{"type":"api-request","id":"q1","params":{"model":"m"},"tail":["h1"]}`,
	}
	events := Claude{}.NewParser(Options{Requests: true}).Feed([]byte(strings.Join(lines, "\n") + "\n"))
	if len(events) != 1 || string(events[0].Request.Blobs["h1"]) != blob {
		t.Errorf("events %+v, want one request whose blob is %s", events, blob)
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
		// The first run_command's Cwd is the session's.
		`1 add tool view_file(README) pending ""`,
		// Two calls in one step: each GENERIC step answers the next one.
		`1 update tool run_command(ls) ok "The command exited with code 0.\nOutput:\nREADME\nmain.go"`,
		`1 update tool view_file(README) ok "File Path: ` + "`file:///home/user/proj/README`" + `\nTotal Lines: 2\n1: hello\n2: world"`,
		`1 add say final "Two files: README and main.go."`,
		// A SYSTEM_MESSAGE with no [Message] entry is the harness's.
		`1 add prompt(system) "The following is a <SYSTEM_MESSAGE> not actually sent by the user."`,
		`2 add prompt(human) "build it"`,
		`2 add tool run_command(go build ./...) pending ""`,
		`2 update tool run_command(go build ./...) error "The command exited with code 1.\nOutput:\nmain.go:3: undefined: x"`,
		`2 add tool view_file(/etc/secret) pending ""`,
		`2 add tool run_command(sleep 100) pending ""`,
		`2 update tool view_file(/etc/secret) error "Encountered error in step execution: permission denied"`,
		`2 update tool run_command(sleep 100) background "Tool is running as a background task with task id: c-1/task-3"`,
		`2 add tool run_command(go vet ./...) pending ""`,
		// gomuxai's held delivery, injected mid-turn, stays in the turn.
		`2 add prompt(delivered) "[from lead@hub] also run the linter"`,
		`2 add system api_error "Error: response cut off"`,
		// A typed prompt abandons the calls still waiting for a result.
		`3 add prompt(human) "stop, summarize"`,
		`2 update tool run_command(go vet ./...) interrupted ""`,
		`3 add system compact_summary "# Summary\nThe user asked to build and vet."`,
		// A background task's completion completes its call; it exited
		// non-zero, so the call failed.
		`3 add prompt(task-notification) "Task id \"c-1/task-3\" finished with result:\nThe command exited with code 2.\nOutput:\nfail"`,
		`3 add prompt(peer) "hello from c-2"`,
		`2 update tool run_command(sleep 100) error "The command exited with code 2.\nOutput:\nfail"`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if p.Lines != 19 || p.Skipped != 1 {
		t.Errorf("lines = %d, skipped = %d, want 19 and 1", p.Lines, p.Skipped)
	}
	if s := p.Session(); s.Harness != "agy" || s.CWD != "/home/user/proj" || s.Model != "Gemini 3.8 Pro" {
		t.Errorf("session = %+v", s)
	}
	for _, e := range events {
		if e.ID == "step_14#0" && string(e.System.Data) != `{"error":"response cut off"}` {
			t.Errorf("api_error data = %s", e.System.Data)
		}
		if e.ID == "step_1_tool_1" && e.Op == OpUpdate && string(e.Tool.Meta) != `{"lines":2}` {
			t.Errorf("view_file meta = %s, want 2 lines read off its numbered lines", e.Tool.Meta)
		}
	}
}

// A canceled background task interrupts its call.
func TestAgyTaskCanceled(t *testing.T) {
	_, events := parse(Agy{}, []byte(strings.Join([]string{
		`{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nserve\n</USER_REQUEST>"}`,
		`{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"make serve"}}]}`,
		`{"step_index":2,"type":"GENERIC","status":"RUNNING","content":"Tool is running as a background task with task id: c-1/task-2"}`,
		`{"step_index":3,"type":"SYSTEM_MESSAGE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-04T12:00:00Z sender=c-1/task-2 priority=MESSAGE_PRIORITY_HIGH content=Task id \"c-1/task-2\" was canceled with result:\nserving on :8080\n</SYSTEM_MESSAGE>"}`,
	}, "\n")+"\n"))
	if got := summary(events[len(events)-1]); got != `1 update tool run_command(make serve) interrupted "serving on :8080"` {
		t.Errorf("last event: %s", got)
	}
}

// A GENERIC step answers the call its distance from the PLANNER_RESPONSE
// names, so a GENERIC agy never wrote leaves its call unanswered rather
// than shifting every later result onto the wrong call.
func TestAgyMissingResult(t *testing.T) {
	_, events := parse(Agy{}, []byte(strings.Join([]string{
		`{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\ngo\n</USER_REQUEST>"}`,
		`{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"pkill srv"}},{"name":"run_command","args":{"CommandLine":"ps"}}]}`,
		// Step 2, pkill's result, was never written.
		`{"step_index":3,"type":"GENERIC","status":"DONE","content":"srv not running"}`,
		`{"step_index":4,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"make"}}]}`,
		`{"step_index":5,"type":"GENERIC","status":"DONE","content":"built"}`,
	}, "\n")+"\n"))
	want := []string{
		`1 add prompt(human) "go"`,
		`1 add tool run_command(pkill srv) pending ""`,
		`1 add tool run_command(ps) pending ""`,
		`1 update tool run_command(ps) ok "srv not running"`,
		// The next model response abandons the call no step answered.
		`1 add tool run_command(make) pending ""`,
		`1 update tool run_command(pkill srv) interrupted ""`,
		`1 update tool run_command(make) ok "built"`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}

// agy can rewrite steps it already wrote: the steps whose index went back
// carry which time it is in their ids, so no two events share an id.
func TestAgyRewind(t *testing.T) {
	_, events := parse(Agy{}, []byte(strings.Join([]string{
		`{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nfirst\n</USER_REQUEST>"}`,
		`{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls"}}]}`,
		`{"step_index":2,"type":"GENERIC","status":"DONE","content":"a"}`,
		`{"step_index":1,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nagain\n</USER_REQUEST>"}`,
		`{"step_index":2,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls -a"}}]}`,
		`{"step_index":3,"type":"GENERIC","status":"DONE","content":"b"}`,
	}, "\n")+"\n"))
	var got []string
	for _, e := range events {
		got = append(got, fmt.Sprintf("%s %s turn %s", e.Op, e.ID, e.Turn.ID))
	}
	want := []string{
		"add step_0#0 turn step_0#0",
		"add step_1_tool_0 turn step_0#0",
		"update step_1_tool_0 turn step_0#0",
		"add step_1.2#0 turn step_1.2#0",
		"add step_2.2_tool_0 turn step_1.2#0",
		// Past the highest index so far, ids are plain again.
		"update step_2.2_tool_0 turn step_1.2#0",
	}
	if !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if n := len(Latest(events)); n != 4 {
		t.Errorf("Latest folds %d events, want 4: both prompts and both calls", n)
	}
}

// Background calls end however agy records it; messages from agy itself
// and from a hook say so.
func TestAgyBackgroundEnds(t *testing.T) {
	msg := func(step int, sender, content string) string {
		body, _ := json.Marshal("<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-04T12:00:00Z sender=" + sender + " priority=MESSAGE_PRIORITY_HIGH content=" + content + "\n</SYSTEM_MESSAGE>")
		return fmt.Sprintf(`{"step_index":%d,"type":"SYSTEM_MESSAGE","source":"SYSTEM","content":%s}`, step, body)
	}
	running := func(step int, task string) string {
		return fmt.Sprintf(`{"step_index":%d,"type":"GENERIC","status":"RUNNING","content":"Tool is running as a background task with task id: %s"}`, step, task)
	}
	_, events := parse(Agy{}, []byte(strings.Join([]string{
		`{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\ngo\n</USER_REQUEST>"}`,
		`{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"schedule","args":{"DurationSeconds":60,"Prompt":"Check the build"}},{"name":"run_command","args":{"CommandLine":"make serve"}},{"name":"run_command","args":{"CommandLine":"make watch"}},{"name":"run_command","args":{"CommandLine":"make docs"}}]}`,
		running(2, "c/task-2"), running(3, "c/task-3"), running(4, "c/task-4"), running(5, "c/task-5"),
		// A timer fires: its message is the schedule's Prompt.
		msg(6, "c/task-2", "Check the build"),
		`{"step_index":7,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"manage_task","args":{"Action":"kill","TaskId":"c/task-3"}}]}`,
		`{"step_index":8,"type":"GENERIC","status":"DONE","content":"Task \"c/task-3\" cancelled."}`,
		msg(9, "system", "Subagent c-2 has gone idle."),
		msg(10, "system", "[Notice] All your subagents and background tasks have been stopped due to server restart."),
		`{"step_index":11,"type":"SYSTEM_MESSAGE","source":"SYSTEM_SDK","content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\nThe following system message was injected by a PreInvocation hook:\n[from lead@hub] also lint\n</SYSTEM_MESSAGE>"}`,
	}, "\n")+"\n"))
	want := []string{
		`1 add prompt(task-notification) "Check the build"`,
		`1 update tool schedule(timer: Check the build) ok "Check the build"`,
		`1 add tool manage_task(kill c/task-3) pending ""`,
		`1 update tool manage_task(kill c/task-3) ok "Task \"c/task-3\" cancelled."`,
		`1 update tool run_command(make serve) interrupted ""`,
		`1 add prompt(system) "Subagent c-2 has gone idle."`,
		`1 add prompt(system) "[Notice] All your subagents and background tasks have been stopped due to server restart."`,
		`1 update tool run_command(make watch) interrupted ""`,
		`1 update tool run_command(make docs) interrupted ""`,
		`1 add prompt(delivered) "[from lead@hub] also lint"`,
	}
	if got := summaries(events[9:]); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}

// agyResponse rebuilds the structured response the tool table reads from
// a result agy keeps only as text.
func TestAgyResponse(t *testing.T) {
	tests := []struct{ name, tool, input, output, meta string }{
		{
			name: "a partial read: its Showing header",
			tool: "view_file", input: `{"AbsolutePath":"/p/a.go"}`,
			output: "File Path: `file:///p/a.go`\nTotal Lines: 40\nShowing lines 11 to 13\n11: a\n12: b\n13: c",
			meta:   `{"lines":3,"range":[11,13]}`,
		},
		{
			name: "a replacement: its diff block",
			tool: "replace_file_content", input: `{"TargetFile":"/p/a.go"}`,
			output: "The following changes were made:\n[diff_block_start]\n@@ -3,2 +3,3 @@\n a\n-b\n+c\n+d\n\n[diff_block_end]",
			meta:   `{"added":2,"removed":1,"ranges":[[4,5]],"patch":[{"lines":[" a","-b","+c","+d"," "],"newLines":3,"newStart":3,"oldLines":2,"oldStart":3}]}`,
		},
		{
			name: "edit_file: its replacement count",
			tool: "edit_file", input: `{"Path":"/p/a.go"}`,
			output: "Made 1 replacement in file:///p/a.go starting at line 7 (+2/-1 lines)",
			meta:   `{"added":2,"removed":1,"ranges":[[7,8]]}`,
		},
		{
			name: "write_to_file: its content",
			tool: "write_to_file", input: `{"TargetFile":"/p/n.go","CodeContent":"a\nb\n","Overwrite":true}`,
			output: "Created file",
			meta:   `{"kind":"update","lines":2}`,
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			in := json.RawMessage(tt.input)
			if s := SummarizeTool(tt.tool, in, agyResponse(tt.tool, in, tt.output), ""); string(s.Meta) != tt.meta {
				t.Errorf("meta = %s, want %s", s.Meta, tt.meta)
			}
		})
	}
}

// An agy subagent's conversation: agy's index beside the parent's names
// it, and the parent's call names it back.
func TestAgySubagent(t *testing.T) {
	brain := t.TempDir()
	write := func(path, content string) {
		t.Helper()
		path = filepath.Join(brain, path)
		if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	logs := filepath.Join(".system_generated", "logs", "transcript_full.jsonl")
	write(filepath.Join("parent", logs), strings.Join([]string{
		`{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-10-04T12:00:00Z","content":"<USER_REQUEST>\nfirst\n</USER_REQUEST>"}`,
		`{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-10-04T12:01:00Z","content":"<USER_REQUEST>\nsplit the review\n</USER_REQUEST>"}`,
		`{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-10-04T12:01:01Z","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"/p/a"}},{"name":"invoke_subagent","args":{"Subagents":[{"Role":"api"},{"Role":"db"}]}}]}`,
		`{"step_index":3,"source":"MODEL","type":"GENERIC","created_at":"2026-10-04T12:01:02Z","content":"1: a"}`,
		`{"step_index":4,"source":"MODEL","type":"GENERIC","created_at":"2026-10-04T12:01:03Z","content":"Started the subagents:\n{\n  \"conversationId\":  \"child-api\"\n}\n{\n  \"conversationId\":  \"child-db\"\n}"}`,
	}, "\n")+"\n")
	write(filepath.Join("parent", ".system_generated", "subagents", "child-db.json"), `{"conversationId":"child-db","spawnStepIndex":4,"state":"SUBAGENT_STATE_ALIVE"}`)
	child := filepath.Join(brain, "child-db", logs)
	write(filepath.Join("child-db", logs), strings.Join([]string{
		`{"step_index":0,"source":"SYSTEM","type":"SYSTEM_MESSAGE","created_at":"2026-10-04T12:01:03Z","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-04T12:01:03Z sender=parent priority=MESSAGE_PRIORITY_HIGH content=Review the schema.\n</SYSTEM_MESSAGE>"}`,
		`{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-10-04T12:01:05Z","content":"Schema looks fine."}`,
	}, "\n")+"\n")

	parent, err := ReadFile(filepath.Join(brain, "parent", logs), Agy{}, 50)
	if err != nil {
		t.Fatal(err)
	}
	if last := parent.Events[len(parent.Events)-1]; last.ID != "step_2_tool_1" || last.Tool.SubSession != "child-api" {
		t.Errorf("the call: %s, sub_session %q; want step_2_tool_1 naming the first subagent, child-api", last.ID, last.Tool.SubSession)
	}

	f, err := ReadFile(child, Agy{}, 50)
	if err != nil {
		t.Fatal(err)
	}
	if s := f.Session; s.SessionID != "child-db" || s.ParentSession == nil || *s.ParentSession != (ParentSession{SessionID: "parent", ToolID: "step_2_tool_1"}) {
		t.Errorf("session %q, parent %+v; want child-db, started by parent's step_2_tool_1", s.SessionID, s.ParentSession)
	}
	want := TurnRef{ID: "step_1#0", Index: 2, Derived: true}
	if got := summaries(f.Events); !slices.Equal(got, []string{`2 add prompt(peer) "Review the schema."`, `2 add say final "Schema looks fine."`}) {
		t.Errorf("events: %q", got)
	}
	for _, e := range f.Events {
		if e.Agent != "child-db" || *e.Turn != want {
			t.Errorf("event %s: agent %q, turn %+v; want child-db in the parent's turn %+v", e.ID, e.Agent, e.Turn, want)
		}
	}

	// The parent's calls are read once while its transcript is unchanged,
	// and again once it grows.
	if len(agySpawnCalls(filepath.Join(brain, "parent"))) != 2 {
		t.Error("spawn calls before the parent grows")
	}
	f2, err := os.OpenFile(filepath.Join(brain, "parent", logs), os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	f2.WriteString(`{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"invoke_subagent","args":{"Subagents":[{"Role":"ui"}]}}]}` + "\n" +
		`{"step_index":6,"source":"MODEL","type":"GENERIC","content":"{\"conversationId\": \"child-ui\"}"}` + "\n")
	f2.Close()
	if call, ok := agySpawnCalls(filepath.Join(brain, "parent"))["child-ui"]; !ok || call.ID != "step_5_tool_0" {
		t.Errorf("after the parent grew: %+v, %v", call, ok)
	}

	// A conversation no other started is no subagent.
	if opts := FileOptions("agy", filepath.Join(brain, "parent", logs)); opts.Session.ParentSession != nil || opts.Turn != nil {
		t.Errorf("parent's options: %+v", opts)
	}
}

func TestKimiParse(t *testing.T) {
	wire := strings.Join([]string{
		`{"type":"metadata","protocol_version":"1.4","created_at":1792490400000}`,
		`{"type":"turn.prompt","time":1792490400000,"prompt":"older prompt shape"}`,
		`{"type":"context.append_loop_event","time":1792490401000,"event":{"type":"tool.result","toolCallId":"tc-1","result":{"output":"early\n"}}}`,
		`{"type":"context.append_loop_event","time":1792490402000,"event":{"type":"tool.call","toolCallId":"tc-1","name":"Bash","args":{"command":"echo early"},"display":{"kind":"command","cwd":"/home/user/w"}}}`,
		`{"type":"turn.steer","time":1792490403000,"input":[{"type":"text","text":"also check b"}]}`,
		`{"type":"context.append_loop_event","time":1792490404000,"event":{"type":"tool.call","toolCallId":"tc-2","name":"Read","args":{"path":"/home/user/w/b.go","line_offset":10}}}`,
		`{"type":"context.append_loop_event","time":1792490405000,"event":{"type":"tool.result","toolCallId":"tc-2","result":{"output":"10\tx\n11\ty\n"}}}`,
		`{"type":"context.append_loop_event","time":1792490406000,"event":{"type":"tool.call","toolCallId":"tc-3","name":"Bash","args":{"command":"false"}}}`,
		`{"type":"context.append_loop_event","time":1792490407000,"event":{"type":"tool.result","toolCallId":"tc-3","result":{"output":[{"type":"text","text":"exit 1"}],"isError":true}}}`,
		`{"type":"context.append_loop_event","time":1792490408000,"event":{"type":"content.part","part":{"type":"think","think":"   "}}}`,
		`{"type":"turn.prompt","time":1792490409000,"input":[{"type":"text","text":"next"},{"type":"text","text":"line two"}]}`,
	}, "\n") + "\n"
	p, events := parse(Kimi{}, []byte(wire))
	want := []string{
		`1 add prompt(human) "older prompt shape"`,
		// A result logged before its call completes it as it's added.
		`1 add tool Bash(echo early) pending ""`,
		`1 update tool Bash(echo early) ok "early\n"`,
		`1 add prompt(queued) "also check b"`,
		`1 add tool Read(b.go) pending ""`,
		`1 update tool Read(b.go) ok "10\tx\n11\ty\n"`,
		`1 add tool Bash(false) pending ""`,
		`1 update tool Bash(false) error "exit 1"`,
		`2 add prompt(human) "next\nline two"`,
	}
	if got := summaries(events); !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if events[5].TS != "2026-10-20T10:00:05.000Z" || string(events[5].Tool.Meta) != `{"lines":2,"range":[10,11]}` {
		t.Errorf("Read update: ts %s, meta %s", events[5].TS, events[5].Tool.Meta)
	}
	if s := p.Session(); s.Harness != "kimi" || s.CWD != "/home/user/w" {
		t.Errorf("session = %+v", s)
	}
}

// Kimi's fields are read one by one: one of an unexpected type reads as
// absent and costs nothing else. A call reuses a toolCallId (Kimi numbers
// calls per response); a call with none is named by its uuid.
func TestKimiLenient(t *testing.T) {
	wire := strings.Join([]string{
		`{"type":"turn.prompt","time":1792490400000.5,"input":"  as a string  "}`,
		`{"type":"llm.request","time":1792490401000,"model":{"name":"k3"}}`,
		`{"type":"context.append_loop_event","time":"soon","event":{"type":"tool.call","toolCallId":"functions.Bash:0","name":"Bash","args":{"command":"ls"},"display":"inline"}}`,
		`{"type":"context.append_loop_event","time":1792490403000,"event":{"type":"tool.result","toolCallId":"functions.Bash:0","result":{"output":"a\n","isError":"no"}}}`,
		`{"type":"context.append_loop_event","time":1792490404000,"event":{"type":"tool.call","toolCallId":"functions.Bash:0","name":"Bash","args":{"command":"pwd"}}}`,
		`{"type":"context.append_loop_event","time":1792490405000,"event":{"type":"tool.result","toolCallId":"functions.Bash:0","result":{"output":"/w\n"}}}`,
		`{"type":"context.append_loop_event","time":1792490406000,"event":{"type":"tool.call","uuid":"u-7","name":"Glob","args":{"pattern":"*.go"}}}`,
		`{"type":"context.append_loop_event","time":1792490407000,"event":{"type":"tool.result","uuid":"r-1","toolCallId":"u-7","result":{"output":"a.go\n"}}}`,
	}, "\n") + "\n"
	p, events := parse(Kimi{}, []byte(wire))
	var got []string
	for _, e := range events {
		got = append(got, e.ID+" "+e.TS+" "+summary(e))
	}
	want := []string{
		`L0#0 2026-10-20T10:00:00.000Z 1 add prompt(human) "as a string"`,
		`functions.Bash:0  1 add tool Bash(ls) pending ""`,
		`functions.Bash:0 2026-10-20T10:00:03.000Z 1 update tool Bash(ls) ok "a\n"`,
		`functions.Bash:0@L4 2026-10-20T10:00:04.000Z 1 add tool Bash(pwd) pending ""`,
		`functions.Bash:0@L4 2026-10-20T10:00:05.000Z 1 update tool Bash(pwd) ok "/w\n"`,
		`u-7 2026-10-20T10:00:06.000Z 1 add tool Glob(*.go) pending ""`,
		`u-7 2026-10-20T10:00:07.000Z 1 update tool Glob(*.go) ok "a.go\n"`,
	}
	if !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if p.Skipped != 0 || p.Session().Model != "" {
		t.Errorf("skipped %d, model %q", p.Skipped, p.Session().Model)
	}
}

// A Kimi subagent's log is its own session, named as agent sessions names
// it, and its events say whose they are.
func TestKimiSubagent(t *testing.T) {
	session := filepath.Join(t.TempDir(), "session_f0aa")
	wire := filepath.Join(session, "agents", "agent-1", "wire.jsonl")
	if err := os.MkdirAll(filepath.Dir(wire), 0o700); err != nil {
		t.Fatal(err)
	}
	os.WriteFile(filepath.Join(session, "state.json"), []byte(`{"title":"The main conversation","workDir":"/home/user/w"}`), 0o600)
	os.WriteFile(wire, []byte(`{"type":"turn.prompt","time":1792490400000,"input":[{"type":"text","text":"review a.go"}]}`+"\n"), 0o600)
	f, err := ReadFile(wire, nil, 50)
	if err != nil {
		t.Fatal(err)
	}
	if s := f.Session; s.Harness != "kimi" || s.SessionID != "f0aa:agent-1" || s.ParentSession == nil || s.ParentSession.SessionID != "f0aa" ||
		s.CWD != "/home/user/w" || s.Title != "" {
		t.Errorf("session = %+v", s)
	}
	if len(f.Events) != 1 || f.Events[0].Agent != "f0aa:agent-1" || f.Events[0].Prompt.Origin != OriginPeer {
		t.Errorf("events = %+v", f.Events)
	}
}

func TestAgyArgs(t *testing.T) {
	tests := []struct {
		name, args, want string
		encoded          bool
	}{
		{
			name: "transcript.jsonl: every value is its JSON in a string", encoded: true,
			args: `{"CommandLine":"\"echo hi\\nls\"","WaitMsBeforeAsync":"500","AllowMultiple":"false","questions":"[{\"question\":\"Q?\"}]","toolSummary":"\"Run\""}`,
			want: `{"AllowMultiple":false,"CommandLine":"echo hi\nls","WaitMsBeforeAsync":500,"questions":[{"question":"Q?"}],"toolSummary":"Run"}`,
		},
		{
			name: "transcript.jsonl: a value agy cut short stays as written", encoded: true,
			args: `{"CommandLine":"\"make bui","Cwd":"\"/p\"","toolSummary":"\"Build\""}`,
			want: `{"CommandLine":"\"make bui","Cwd":"/p","toolSummary":"Build"}`,
		},
		{
			name: "transcript_full.jsonl: a quoted search keeps its quotes",
			args: `{"query":"\"exact phrase\""}`,
			want: `{"query":"\"exact phrase\""}`,
		},
		{
			name: "transcript_full.jsonl: plain values stay",
			args: `{"CommandLine":"\"ls\"","WaitMsBeforeAsync":500}`,
			want: `{"CommandLine":"\"ls\"","WaitMsBeforeAsync":500}`,
		},
		{name: "no args", args: `{}`, want: `{}`, encoded: true},
		{name: "not an object", args: `null`, want: `null`, encoded: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := agyArgs(json.RawMessage(tt.args), tt.encoded); string(got) != tt.want {
				t.Errorf("agyArgs = %s, want %s", got, tt.want)
			}
		})
	}

	// The parser takes the encoding from the file's name.
	line := `{"step_index":0,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"search_web","args":{"query":"\"exact phrase\""}}]}` + "\n"
	for file, want := range map[string]string{"transcript.jsonl": "exact phrase", "transcript_full.jsonl": `"exact phrase"`, "": `"exact phrase"`} {
		events := Agy{}.NewParser(Options{Session: Session{Path: filepath.Join("/c", file)}}).Feed([]byte(line))
		if got := events[0].Tool.Detail; got != want {
			t.Errorf("%q: detail %s, want %s", file, got, want)
		}
	}

	// Decoded, an encoded replacement's lines count.
	args := agyArgs(json.RawMessage(`{"TargetFile":"\"/p/d.go\"","TargetContent":"\"a\\nb\"","ReplacementContent":"\"a\\nc\\nd\\ne\"","AllowMultiple":"false"}`), true)
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
	kimi, err := os.ReadFile(filepath.Join(corpusDir, "kimi", "interleaved", "session_f0aa1fb8-41b1-5485-a887-ecd00a0bf053", "agents", "main", "wire.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if r := Detect(kimi); r.Harness() != "kimi" {
		t.Errorf("kimi wire detected as %s", r.Harness())
	}
}

func TestDegenerateInputs(t *testing.T) {
	for _, r := range []Reader{Claude{}, Agy{}, Kimi{}} {
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
	claude, err := filepath.Glob(filepath.Join(corpusDir, "claude", "*.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	transcripts := map[Reader][]string{
		Claude{}: append(claude, filepath.Join("testdata", "claude.jsonl")),
		Agy{}: {
			filepath.Join(corpusDir, "agy", "conversation", "b51e7553-7a08-532d-a2c7-2b5cc6769489", ".system_generated", "logs", "transcript_full.jsonl"),
			filepath.Join("testdata", "agy.jsonl"),
		},
		Kimi{}: {filepath.Join(corpusDir, "kimi", "interleaved", "session_f0aa1fb8-41b1-5485-a887-ecd00a0bf053", "agents", "main", "wire.jsonl")},
	}
	for r, paths := range transcripts {
		for _, path := range paths {
			if strings.HasSuffix(path, ".events.jsonl") {
				continue
			}
			testResumeAfter(t, r, path)
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

// testResumeAfter resumes r's parse of the transcript at path after each of
// its lines, with the options ReadFile gives it, and compares what follows
// to the whole stream's.
func testResumeAfter(t *testing.T, r Reader, path string) {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, requests := range []bool{false, true} {
		opts := FileOptions(r.Harness(), path)
		opts.Requests = requests
		whole := r.NewParser(opts).Feed(data)
		for at, line := int64(0), 0; at < int64(len(data)); line++ {
			p, err := ResumeAfter(r, opts, data, at)
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

func TestLatest(t *testing.T) {
	_, events := parse(Claude{}, readFixture(t, "claude.jsonl"))
	latest := Latest(events)
	want := []string{
		`1 add prompt(human) "run the tests"`,
		`1 add think "go test should do it"`,
		`1 add tool Bash(go test ./...) ok "ok  \tpkg/a\t0.1s\nok  \tpkg/b\t0.2s"`,
		`1 add tool Read(go.mod) ok "1\tmodule example.com/proj"`,
		`1 add say final "All tests pass."`,
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
	if len(f.Events) != 14 || f.Partial || f.Session.Path != path || f.Session.Size != int64(len(readFixture(t, "claude.jsonl"))) ||
		f.Session.SessionID != "00000000-0000-0000-0000-00000000c1a0" {
		t.Errorf("got %d events, partial %v, session %+v", len(f.Events), f.Partial, f.Session)
	}

	// Without a reader, the format is detected.
	path = writeFile(t, "transcript_full.jsonl", string(readFixture(t, "agy.jsonl")))
	if f, err := ReadFile(path, nil, 50); err != nil || f.Session.Harness != "agy" || len(f.Events) != 24 {
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
			fmt.Sprintf(`{"type":"assistant","uuid":"%s-a","agentId":"x","message":{"role":"assistant","content":[{"type":"text","text":"working"}],"stop_reason":"tool_use"}}`+"\n", uuid)
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
		"/home/user/.gemini/antigravity-cli/brain/c-2/transcript_full.jsonl":                        "c-2",
		"/home/user/.kimi-code/sessions/wd_w_1/session_f0aa/agents/main/wire.jsonl":                 "f0aa",
		"/home/user/.kimi-code/sessions/wd_w_1/session_f0aa/agents/agent-1/wire.jsonl":              "f0aa:agent-1",
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

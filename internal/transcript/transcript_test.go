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
		// The caveat is meta and the task notification is tagged text: neither is a prompt.
		`2 add prompt(human) "/review high"`,
		`2 add tool Bash(make lint) pending ""`,
		`2 update tool Bash(make lint) error "Exit code 2\nlint: 1 issue"`,
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

// From glyph3d-rs tests.rs (user_prompt_text_classifies_real_record_shapes).
func TestClaudePrompt(t *testing.T) {
	rec := func(content string, extra string) *claudeRecord {
		var r claudeRecord
		line := `{"type":"user"` + extra + `,"message":{"role":"user","content":` + content + `}}`
		if err := json.Unmarshal([]byte(line), &r); err != nil {
			t.Fatalf("%s: %v", line, err)
		}
		return &r
	}
	prompts := map[*claudeRecord]string{
		rec(`"  Fix the fog  "`, ""):                                                        "Fix the fog",
		rec(`[{"type":"text","text":"pasted"}]`, ""):                                        "pasted",
		rec(`"<command-name>/model</command-name>\n<command-args></command-args>"`, ""):     "/model",
		rec(`"<command-name>review</command-name>\n<command-args>high</command-args>"`, ""): "/review high",
	}
	for r, want := range prompts {
		if got, ok := claudePrompt(r); !ok || got != want {
			t.Errorf("prompt = %q, %v; want %q", got, ok, want)
		}
	}
	for _, r := range []*claudeRecord{
		rec(`"<local-command-caveat>Caveat</local-command-caveat>"`, `,"isMeta":true`),
		rec(`"[Image: original 100x100]"`, `,"isMeta":true`),
		rec(`"<local-command-stdout>Set model</local-command-stdout>"`, ""),
		rec(`"<bash-input>ls</bash-input>"`, ""),
		rec(`[{"type":"text","text":"[Request interrupted by user]"}]`, ""),
		rec(`[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]`, ""),
		rec(`"   "`, ""),
	} {
		if got, ok := claudePrompt(r); ok {
			t.Errorf("%s: prompt %q, want none", r.Message.Content, got)
		}
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

	// A window that starts on a later line numbers its events from there.
	from := Pos{At: int64(bytes.Index(data, []byte(`{"type":"user","isMeta":true`))), Line: 14, Seq: 100}
	w := Claude{}.NewParser(Options{From: from})
	events := w.Feed(data[from.At:])
	if len(events) == 0 || events[0].Seq != 100 || events[0].Line != 15 || events[0].At != from.At+int64(bytes.IndexByte(data[from.At:], '\n'))+1 {
		t.Errorf("window: first event %+v", events[0])
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
		`2 add prompt(human) "/review high"`,
		`2 add tool Bash(make lint) error "Exit code 2\nlint: 1 issue"`,
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
	if len(f.Events) != 12 || f.Partial || f.Session.Path != path || f.Session.Size != int64(len(readFixture(t, "claude.jsonl"))) ||
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
		}
		t.Logf("%s: %s, %d events, %d lines, %d skipped, %v", filepath.Base(path), r.Harness(), len(f.Events), f.Lines, f.Skipped, kinds)
		if f.Skipped > 0 {
			t.Errorf("%s: %d lines skipped", path, f.Skipped)
		}
	}
}

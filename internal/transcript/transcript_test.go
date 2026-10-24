package transcript

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
)

// eventSummary renders an event compactly for comparisons.
func eventSummary(e Event) string {
	if e.Tool != nil {
		return fmt.Sprintf("%d tool %s(%s) %s %q", e.Turn, e.Tool.Name, e.Tool.Detail(""), e.Tool.Status, e.Tool.Output)
	}
	return fmt.Sprintf("%d %s %q", e.Turn, e.Kind, e.Text)
}

func readFixture(t *testing.T, name string) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func TestClaudeParse(t *testing.T) {
	s := Claude{}.Parse(readFixture(t, "claude.jsonl"))

	var got []string
	for _, e := range s.Events() {
		got = append(got, eventSummary(e))
	}
	want := []string{
		`0 user "run the tests"`,
		`0 think "go test should do it"`,
		`0 tool Bash(go test ./...) ok "ok  \tpkg/a\t0.1s\nok  \tpkg/b\t0.2s"`,
		`0 tool Read(/home/user/proj/go.mod) ok "1\tmodule example.com/proj"`,
		`0 reply "All tests pass."`,
		// The caveat is meta and the task notification is tagged text: neither is a prompt.
		`1 user "/review high"`,
		`1 tool Bash(make lint) error "Exit code 2\nlint: 1 issue"`,
		`1 reply "One lint issue.\nSee above."`,
		`1 tool Grep(TODO) none ""`,
	}
	if !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}

	// Sidechain records don't leak their cwd; api-request-blob copies add nothing.
	if s.CWD != "/home/user/proj" || s.SessionID != "00000000-0000-0000-0000-00000000c1a0" ||
		s.Version != "2.1.400" || s.GitBranch != "main" || s.Title != "Run the tests" || s.Model != "claude-test-1" {
		t.Errorf("metadata = %+v", s)
	}
	if s.Lines != 20 || s.Skipped != 1 {
		t.Errorf("lines = %d, skipped = %d, want 20 and 1", s.Lines, s.Skipped)
	}
	if want := time.Date(2026, 8, 1, 10, 0, 0, 0, time.UTC); !s.First.Equal(want) {
		t.Errorf("first = %v, want %v", s.First, want)
	}
	if want := time.Date(2026, 8, 1, 10, 1, 5, 0, time.UTC); !s.Last.Equal(want) {
		t.Errorf("last = %v, want %v", s.Last, want)
	}
	if e := s.Events()[2]; !e.Time.Equal(time.Date(2026, 8, 1, 10, 0, 1, 0, time.UTC)) {
		t.Errorf("tool time = %v, want its line's timestamp", e.Time)
	}
}

// The Claude part of glyph3d-rs tests.rs (test_synthetic_claude_session_parity):
// results pair by id across distance, a result's text wins over the
// structured record, and the structured text is the fallback.
func TestClaudeParse_Pairing(t *testing.T) {
	lines := []string{
		`{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Edit","input":{"file_path":"/r/a.js"}}]}}`,
		`{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"b","name":"Bash","input":{"command":"ls"}}]}}`,
		`{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"Bash","input":{"command":"true"}}]}}`,
		`{"type":"user","toolUseResult":{"stdout":"one\ntwo\n"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}]}}`,
		`{"type":"user","toolUseResult":{"stdout":"from structured"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c"}]}}`,
		`{"type":"user","toolUseResult":{"structuredPatch":[]},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"The file has been updated"}]}}`,
	}
	s := Claude{}.Parse([]byte(strings.Join(lines, "\n")))
	var got []string
	for _, e := range s.Events() {
		got = append(got, eventSummary(e))
	}
	want := []string{
		`0 tool Edit(/r/a.js) ok "The file has been updated"`,
		`0 tool Bash(ls) ok "one\ntwo"`,
		`0 tool Bash(true) ok "from structured"`,
	}
	if !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
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
		rec(`"This session is being continued from a previous conversation"`, `,"isCompactSummary":true`),
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
	s := Agy{}.Parse(readFixture(t, "agy.jsonl"))

	var got []string
	for _, e := range s.Events() {
		got = append(got, eventSummary(e))
	}
	want := []string{
		`0 user "list the files"`,
		`0 think "I should list them and read the readme."`,
		// Two calls in one step: each GENERIC step answers the next one.
		`0 tool run_command(ls) ok "The command exited with code 0.\nOutput:\nREADME\nmain.go"`,
		`0 tool view_file(/home/user/proj/README) ok "File Path: ` + "`file:///home/user/proj/README`" + `\nTotal Lines: 2\n1: hello\n2: world"`,
		`0 reply "Two files: README and main.go."`,
		`1 user "build it"`,
		`1 tool run_command(go build ./...) error "The command exited with code 1.\nOutput:\nmain.go:3: undefined: x"`,
		`1 tool view_file(/etc/secret) error "Encountered error in step execution: permission denied"`,
		`1 tool run_command(sleep 100) running "Tool is running as a background task with task id: t-1"`,
		`1 tool run_command(go vet ./...) none ""`,
	}
	if !slices.Equal(got, want) {
		t.Errorf("events:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
	if s.Lines != 14 || s.Skipped != 1 {
		t.Errorf("lines = %d, skipped = %d, want 14 and 1", s.Lines, s.Skipped)
	}
	if want := time.Date(2026, 10, 4, 12, 1, 6, 0, time.UTC); !s.Last.Equal(want) {
		t.Errorf("last = %v, want %v", s.Last, want)
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
		if s := r.Parse(nil); len(s.Turns) != 0 || s.Lines != 0 {
			t.Errorf("%s: empty input gave %+v", r.Harness(), s)
		}
		s := r.Parse([]byte("null\n42\n\"str\"\n{invalid json\n[]"))
		if len(s.Turns) != 0 || s.Lines != 5 || s.Skipped != 5 {
			t.Errorf("%s: garbage gave %d turns, %d lines, %d skipped", r.Harness(), len(s.Turns), s.Lines, s.Skipped)
		}
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
	s, err := ReadFile(path, Claude{}, 50)
	if err != nil {
		t.Fatal(err)
	}
	if len(s.Events()) != 9 || s.Partial || s.Path != path || s.SessionID != "00000000-0000-0000-0000-00000000c1a0" {
		t.Errorf("got %d events, partial %v, path %q, session %q", len(s.Events()), s.Partial, s.Path, s.SessionID)
	}

	// Without a reader, the format is detected.
	path = writeFile(t, "transcript_full.jsonl", string(readFixture(t, "agy.jsonl")))
	if s, err := ReadFile(path, nil, 50); err != nil || s.Harness != "agy" || len(s.Events()) != 10 {
		t.Errorf("detected: %+v, %v", s, err)
	}
}

// A large transcript is read from its end, in a window that grows until it
// holds enough events; metadata still comes from its head.
func TestReadFile_TailWindow(t *testing.T) {
	var b strings.Builder
	b.WriteString(`{"type":"user","cwd":"/home/user/big","sessionId":"big","timestamp":"2026-08-01T10:00:00Z","message":{"role":"user","content":"first"}}` + "\n")
	pad := strings.Repeat("x", 2000)
	for i := range 3000 {
		fmt.Fprintf(&b, `{"type":"assistant","timestamp":"2026-08-01T10:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"reply %d"}]},"pad":"%s"}`+"\n", i, pad)
	}
	path := writeFile(t, "big.jsonl", b.String())

	for _, n := range []int{3, 1000} {
		s, err := ReadFile(path, Claude{}, n)
		if err != nil {
			t.Fatal(err)
		}
		events := s.Events()
		if len(events) < n || !s.Partial {
			t.Fatalf("n=%d: %d events, partial %v", n, len(events), s.Partial)
		}
		if last := events[len(events)-1].Text; last != "reply 2999" {
			t.Errorf("n=%d: last event %q", n, last)
		}
		if first := events[0].Text; !strings.HasPrefix(first, "reply ") {
			t.Errorf("n=%d: first event %q: the window's partial first line must be dropped", n, first)
		}
		if s.CWD != "/home/user/big" || s.SessionID != "big" || s.Skipped != 0 {
			t.Errorf("n=%d: cwd %q, session %q, skipped %d", n, s.CWD, s.SessionID, s.Skipped)
		}
	}

	// Asking for more than there is reads it all.
	s, err := ReadFile(path, Claude{}, 1_000_000)
	if err != nil {
		t.Fatal(err)
	}
	if len(s.Events()) != 3001 || s.Partial {
		t.Errorf("whole file: %d events, partial %v", len(s.Events()), s.Partial)
	}
}

// A line still being written is left out, not counted as unparseable.
func TestReadFile_PartialLastLine(t *testing.T) {
	path := writeFile(t, "s.jsonl", `{"type":"user","message":{"role":"user","content":"hi"}}`+"\n"+`{"type":"assistant","message":{"role":"assis`)
	s, err := ReadFile(path, Claude{}, 50)
	if err != nil {
		t.Fatal(err)
	}
	if len(s.Events()) != 1 || s.Lines != 1 || s.Skipped != 0 {
		t.Errorf("%d events, %d lines, %d skipped", len(s.Events()), s.Lines, s.Skipped)
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
	if s, err := ReadFile(writeFile(t, "e.jsonl", ""), Agy{}, 50); err != nil || len(s.Events()) != 0 {
		t.Errorf("empty file: %v, %v", s, err)
	}
}

func TestSessionIDFromPath(t *testing.T) {
	for path, want := range map[string]string{
		"/home/user/.claude/projects/-p/0000-aa.jsonl":                                              "0000-aa",
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
		s, err := ReadFile(path, r, 1<<30)
		if err != nil {
			t.Fatalf("%s: %v", path, err)
		}
		kinds := map[string]int{}
		for _, e := range s.Events() {
			kinds[e.Kind]++
			if e.Tool != nil {
				kinds[e.Tool.Status]++
			}
		}
		t.Logf("%s: %s, %d turns, %d lines, %d skipped, %v", filepath.Base(path), r.Harness(), len(s.Turns), s.Lines, s.Skipped, kinds)
		if s.Skipped > 0 {
			t.Errorf("%s: %d lines skipped", path, s.Skipped)
		}
	}
}

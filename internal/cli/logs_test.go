package cli

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/transcript"
)

func TestTailLogLines(t *testing.T) {
	tests := []struct {
		name     string
		padding  int // bytes of escape sequences after each line's text
		lines    int
		n        int
		expected []string
	}{
		{name: "small file", lines: 3, n: 50, expected: []string{"line 0", "line 1", "line 2"}},
		{name: "last n of a small file", lines: 10, n: 2, expected: []string{"line 8", "line 9"}},
		// Larger than the first window: the line cut off at its start must
		// not show up as a line of its own.
		{name: "tail of a large file", lines: 100_000, n: 3, expected: []string{"line 99997", "line 99998", "line 99999"}},
		// So much padding that the first window holds fewer than n lines.
		{name: "window grows", padding: 10_000, lines: 200, n: 40, expected: lineRange(160, 200)},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			var b strings.Builder
			pad := strings.Repeat("\x1b[K", tt.padding/3)
			for i := range tt.lines {
				fmt.Fprintf(&b, "\x1b[38;5;246mline %d\x1b[39m%s\r\n", i, pad)
			}
			path := filepath.Join(t.TempDir(), "agent.log")
			if err := os.WriteFile(path, []byte(b.String()), 0o600); err != nil {
				t.Fatal(err)
			}

			got, err := tailLogLines(path, tt.n)
			if err != nil {
				t.Fatalf("tailLogLines: %v", err)
			}
			if !slices.Equal(got, tt.expected) {
				t.Errorf("tailLogLines(n=%d) = %q, want %q", tt.n, got, tt.expected)
			}
		})
	}
}

func TestTailLogLinesMissingFile(t *testing.T) {
	if _, err := tailLogLines(filepath.Join(t.TempDir(), "none.log"), 10); !os.IsNotExist(err) {
		t.Errorf("err = %v, want a not-exist error", err)
	}
}

func lineRange(from, to int) []string {
	var lines []string
	for i := from; i < to; i++ {
		lines = append(lines, fmt.Sprintf("line %d", i))
	}
	return lines
}

// A synthetic Claude Code transcript: one prompt, a tool call, a reply.
const testTranscript = `{"type":"user","sessionId":"sess-1","cwd":"/home/user/proj","timestamp":"2026-08-01T12:04:31Z","message":{"role":"user","content":"run the tests"}}
{"type":"assistant","timestamp":"2026-08-01T12:04:33Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"go test ./... && go vet ./..."}}]}}
{"type":"user","timestamp":"2026-08-01T12:04:40Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok  a\nok  b\nok  c"}]}}
{"type":"assistant","timestamp":"2026-08-01T12:05:02Z","message":{"role":"assistant","content":[{"type":"text","text":"All tests pass.\nNothing else to do."}]}}
`

// setupLogs is setupCLI with logs' flags reset and times printed in UTC.
func setupLogs(t *testing.T) {
	t.Helper()
	setupCLI(t)
	forbidBus(t)
	if err := os.MkdirAll(cfg.LogDir, 0o755); err != nil {
		t.Fatal(err)
	}
	oldLocal := time.Local
	time.Local = time.UTC
	t.Cleanup(func() {
		time.Local = oldLocal
		logsLines, logsFull, logsJSON, logsTerminal = 50, false, false, false
	})
	logsLines, logsFull, logsJSON, logsTerminal = 50, false, false, false
}

// hookTranscript reports a hook naming path as worker's transcript.
func hookTranscript(t *testing.T, path string) {
	t.Helper()
	payload := `{"hook_event_name":"UserPromptSubmit","transcript_path":"` + path + `"}`
	if err := agentService.ReportHook(context.Background(), "worker", "", "", []byte(payload)); err != nil {
		t.Fatal(err)
	}
}

func writeTestFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestLogs_Transcript(t *testing.T) {
	setupLogs(t)
	path := filepath.Join(t.TempDir(), "sess-1.jsonl")
	writeTestFile(t, path, testTranscript)
	hookTranscript(t, path)

	out, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	if err != nil {
		t.Fatal(err)
	}
	want := `12:04:31 user  run the tests
12:04:33 tool  Bash: go test ./... && go vet ./... → ok (3 lines)
12:05:02 reply All tests pass.…
`
	if out != want {
		t.Errorf("logs:\n%s\nwant:\n%s", out, want)
	}
	if hints.Len() > 0 {
		t.Errorf("hint for a readable transcript: %s", hints)
	}

	logsLines, logsFull = 1, true
	out, _ = stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	if want := "12:05:02 reply All tests pass.\n               Nothing else to do.\n"; out != want {
		t.Errorf("--lines 1 --full:\n%q\nwant:\n%q", out, want)
	}

	logsLines, logsFull, logsJSON = 50, false, true
	out, _ = stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	var sessions []struct {
		Harness, Path, CWD string
		Events             []transcript.Event
	}
	if err := json.Unmarshal([]byte(out), &sessions); err != nil {
		t.Fatalf("--json: %v\n%s", err, out)
	}
	if len(sessions) != 1 || sessions[0].Harness != "claude" || sessions[0].Path != path || sessions[0].CWD != "/home/user/proj" ||
		len(sessions[0].Events) != 3 || sessions[0].Events[1].Tool.Output != "ok  a\nok  b\nok  c" {
		t.Errorf("--json:\n%s", out)
	}
	if !strings.Contains(out, `"go test ./... && go vet ./..."`) {
		t.Errorf("--json escapes the command:\n%s", out)
	}
}

// The list of transcripts outlives the agent record, and spans sessions.
func TestLogs_AfterCleanupAndAcrossSessions(t *testing.T) {
	setupLogs(t)
	dir := t.TempDir()
	first, second := filepath.Join(dir, "sess-1.jsonl"), filepath.Join(dir, "sess-2.jsonl")
	writeTestFile(t, first, testTranscript)
	writeTestFile(t, second, `{"type":"user","sessionId":"sess-2","timestamp":"2026-08-01T13:00:00Z","message":{"role":"user","content":"again"}}`+"\n")
	hookTranscript(t, first)
	hookTranscript(t, first)
	hookTranscript(t, second)
	if err := store.DeleteAgent(context.Background(), "worker"); err != nil {
		t.Fatal(err)
	}

	logsLines = 2
	out, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	if err != nil {
		t.Fatal(err)
	}
	want := `12:05:02 reply All tests pass.…
--- session sess-2 (claude)
13:00:00 user  again
`
	if out != want {
		t.Errorf("logs:\n%s\nwant:\n%s", out, want)
	}
}

func TestLogs_FallsBackToTerminalLog(t *testing.T) {
	setupLogs(t)
	writeTestFile(t, cfg.LogPath("worker"), "$ claude\r\nhello from the terminal\r\n")

	out, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	if err != nil || !strings.Contains(out, "hello from the terminal") {
		t.Errorf("no transcript: %q, %v", out, err)
	}
	if want := "hint: printing worker's terminal log: no transcript recorded for it\n"; hints.String() != want {
		t.Errorf("hint = %q, want %q", hints, want)
	}

	gone := filepath.Join(t.TempDir(), "gone.jsonl")
	hookTranscript(t, gone)
	hints.Reset()
	hinted = false
	out, _ = stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	if !strings.Contains(out, "hello from the terminal") || !strings.Contains(hints.String(), "its transcript "+gone+" is gone") {
		t.Errorf("missing transcript: %q, hint %q", out, hints)
	}

	logsJSON = true
	if _, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) }); err == nil || !strings.Contains(err.Error(), "--terminal") {
		t.Errorf("--json without a transcript: %v", err)
	}

	logsJSON, logsTerminal = false, true
	hints.Reset()
	hinted = false
	out, _ = stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	if !strings.Contains(out, "hello from the terminal") || hints.Len() > 0 {
		t.Errorf("--terminal: %q, hint %q", out, hints)
	}
}

func TestLogs_Unparseable(t *testing.T) {
	setupLogs(t)
	writeTestFile(t, cfg.LogPath("worker"), "terminal text\n")
	path := filepath.Join(t.TempDir(), "s.jsonl")
	writeTestFile(t, path, "not a transcript\n")
	hookTranscript(t, path)

	out, _ := stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
	if !strings.Contains(out, "terminal text") || !strings.Contains(hints.String(), "can't be read: unparseable transcript") {
		t.Errorf("unparseable: %q, hint %q", out, hints)
	}
}

func TestLogs_RefusesOtherHosts(t *testing.T) {
	setupLogs(t)
	err := runLogs(logsCmd, []string{"worker@hub"})
	if err == nil || !strings.Contains(err.Error(), "agent capture worker@hub") {
		t.Errorf("remote: %v", err)
	}
}

func TestFormatEvent(t *testing.T) {
	old := time.Local
	time.Local = time.UTC
	defer func() { time.Local = old }()
	at := time.Date(2026, 8, 1, 9, 5, 7, 0, time.UTC)
	tool := func(status, output string) transcript.Event {
		return transcript.Event{Time: at, Kind: transcript.KindTool, Tool: &transcript.ToolCall{
			Name: "Bash", Input: json.RawMessage(`{"command":"make\nlint"}`), Status: status, Output: output,
		}}
	}
	long := strings.Repeat("word ", 40)
	for _, tt := range []struct {
		e    transcript.Event
		full bool
		want string
	}{
		{transcript.Event{Kind: transcript.KindUser, Text: "hi"}, false, "--:--:-- user  hi"},
		{transcript.Event{Time: at, Kind: transcript.KindReply, Text: long}, false, "09:05:07 reply " + strings.TrimSpace(long[:119]) + "…"},
		{tool("ok", ""), false, "09:05:07 tool  Bash: make… → ok"},
		{tool("ok", "1 issue\n"), false, "09:05:07 tool  Bash: make… → ok: 1 issue"},
		{tool("error", "Exit code 2\nlint: 1 issue"), false, "09:05:07 tool  Bash: make… → error: Exit code 2… (2 lines)"},
		{tool("running", "started"), false, "09:05:07 tool  Bash: make… → running in the background"},
		{tool("none", ""), false, "09:05:07 tool  Bash: make… → no result yet"},
		{tool("ok", "a\nb"), true, "09:05:07 tool  Bash: make\n               lint → ok (2 lines)\n               a\n               b"},
	} {
		if got := formatEvent(tt.e, "", tt.full); got != tt.want {
			t.Errorf("formatEvent(full=%v):\n%q\nwant:\n%q", tt.full, got, tt.want)
		}
	}

	// A path under the session's working directory prints relative to it.
	read := transcript.Event{Time: at, Kind: transcript.KindTool, Tool: &transcript.ToolCall{
		Name: "Read", Input: json.RawMessage(`{"file_path":"/home/user/proj/src/a.go"}`), Status: "ok",
	}}
	for cwd, want := range map[string]string{
		"/home/user/proj":  "09:05:07 tool  Read: src/a.go → ok",
		"/home/user/proj/": "09:05:07 tool  Read: src/a.go → ok",
		"/home/user/pro":   "09:05:07 tool  Read: /home/user/proj/src/a.go → ok",
	} {
		if got := formatEvent(read, cwd, false); got != want {
			t.Errorf("cwd %s: %q, want %q", cwd, got, want)
		}
	}
}

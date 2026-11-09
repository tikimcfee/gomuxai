package cli

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// agent logs name@host prints what agent logs prints on that host, with
// the same flags, and its hint.
func TestRemoteLogsMatchLocal(t *testing.T) {
	setupLogs(t)
	serveAsH1(t)
	path := filepath.Join(t.TempDir(), "sess-1.jsonl")
	writeTestFile(t, path, testTranscript)
	hookTranscript(t, path)

	for _, flags := range []struct {
		lines          int
		full, js, term bool
	}{{50, false, false, false}, {1, true, false, false}, {50, false, true, false}, {2, false, false, true}} {
		name := fmt.Sprintf("--lines %d --full=%v --json=%v --terminal=%v", flags.lines, flags.full, flags.js, flags.term)
		logsLines, logsFull, logsJSON, logsTerminal = flags.lines, flags.full, flags.js, flags.term
		hints.Reset()
		local, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker"}) })
		if err != nil {
			t.Fatalf("%s: local: %v", name, err)
		}
		localHints := hints.String()
		hints.Reset()
		hinted = false
		remote, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker@h1"}) })
		if err != nil {
			t.Fatalf("%s: remote: %v", name, err)
		}
		if remote != local || hints.String() != localHints {
			t.Errorf("%s differs:\nlocal:\n%s%s\nremote:\n%s%s", name, local, localHints, remote, hints)
		}
		hinted = false
	}
}

// With no transcript, logs falls back to the terminal log and says why,
// on either host.
func TestRemoteLogsHint(t *testing.T) {
	setupLogs(t)
	serveAsH1(t)
	writeTestFile(t, cfg.LogPath("worker"), "line one\r\nline two\r\n")

	out, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker@h1"}) })
	if err != nil {
		t.Fatal(err)
	}
	if out != "line one\nline two\n" || !strings.Contains(hints.String(), "printing worker's terminal log: no transcript recorded for it") {
		t.Errorf("remote logs = %q, hint %q", out, hints)
	}
}

// A log too big for a bus message keeps its newest lines and says how
// much was left out.
func TestRemoteLogsTruncated(t *testing.T) {
	setupLogs(t)
	serveAsH1(t)
	var b strings.Builder
	for i := range 40_000 {
		fmt.Fprintf(&b, "terminal line %06d %s\r\n", i, strings.Repeat("x", 20))
	}
	writeTestFile(t, cfg.LogPath("worker"), b.String())
	logsTerminal, logsLines = true, 40_000

	out, err := stdout(t, func() error { return runLogs(logsCmd, []string{"worker@h1"}) })
	if err != nil {
		t.Fatal(err)
	}
	if len(out) > natsbus.MaxLogsReply || !strings.HasSuffix(out, "terminal line 039999 "+strings.Repeat("x", 20)+"\n") {
		t.Errorf("remote logs: %d bytes ending %q, want at most %d ending with the last line", len(out), out[max(len(out)-60, 0):], natsbus.MaxLogsReply)
	}
	if !strings.HasPrefix(out, "terminal line ") {
		t.Errorf("remote logs start mid-line: %q", out[:min(len(out), 40)])
	}
	if !strings.Contains(hints.String(), "were cut to fit a bus message: the oldest") {
		t.Errorf("no word of the cut: %q", hints)
	}
}

func TestFitLogs(t *testing.T) {
	chunks := []logsChunk{
		{text: "S1\n", session: true}, {text: "a1\n"}, {text: "a2\n"},
		{text: "S2\n", session: true}, {text: "b1\n"}, {text: "b2\n"}, {text: "b3\n"},
	}
	for _, tt := range []struct {
		budget  int
		want    string
		dropped int
	}{
		// Each chunk takes 4 bytes encoded: "\n" escapes to two.
		{100, "S1\na1\na2\nS2\nb1\nb2\nb3\n", 0},
		{12, "S2\nb2\nb3\n", 12}, // b2's session line is put back
		{16, "S2\nb1\nb2\nb3\n", 9},
		{20, "S2\nb1\nb2\nb3\n", 9}, // a2 would need S1 too
		{24, "S1\na2\nS2\nb1\nb2\nb3\n", 3},
	} {
		got, dropped := fitLogs(chunks, tt.budget, false)
		if got != tt.want || dropped != tt.dropped {
			t.Errorf("budget %d: %q, %d dropped; want %q, %d", tt.budget, got, dropped, tt.want, tt.dropped)
		}
	}

	// One chunk too big alone: text keeps its end, JSON drops it.
	big := []logsChunk{{text: strings.Repeat("y", 100) + "end\n"}}
	if got, dropped := fitLogs(big, 60, true); got == "" || !strings.HasSuffix(got, "end\n") || dropped != 104-len(got) {
		t.Errorf("text too big: %q, %d dropped", got, dropped)
	}
	if got, dropped := fitLogs(big, 60, false); got != "" || dropped != 104 {
		t.Errorf("JSON line too big: %q, %d dropped; want it left out", got, dropped)
	}
	// Sizes count JSON escaping: quotes take two bytes each, 8 in all.
	quoted := []logsChunk{{text: `"""` + "\n"}, {text: "ok\n"}}
	if got, _ := fitLogs(quoted, 8, false); got != "ok\n" {
		t.Errorf("escaped size: %q, want only ok", got)
	}
}

// fakeClaude launches nothing, and records the session it was asked to
// resume.
type fakeClaude struct {
	*namedagents.ClaudeBackend
}

func (fakeClaude) LaunchCommand() string { return "true" }

// agent resume name@host runs that host's resume: its refusals, and a
// resume it starts there.
func TestRemoteResume(t *testing.T) {
	home, dir, _ := setupContinue(t, nil)
	var resumed []string
	backends.RegisterFactory("claude", func(c backend.ProviderConfig) backend.Provider {
		if c.Resume != "" {
			resumed = append(resumed, c.Resume)
		}
		return fakeClaude{namedagents.NewClaudeBackendWithConfig(namedagents.ClaudeConfig{Resume: c.Resume})}
	})
	t.Cleanup(func() { resumeAttach, resumeSkipPermissions, resumeTrustWorkspace = false, false, false })

	// Not served: resume is off for other hosts by default.
	serveAsH1With(t, natsbus.DefaultRemoteOps)
	err := runResume(resumeCmd, []string{"worker@h1"})
	if err == nil || !strings.Contains(err.Error(), `does it allow "resume"`) || !strings.Contains(hints.String(), "--remote-ops listing resume") {
		t.Errorf("resume not served: err %v, hint %q", err, hints)
	}

	serveAsH1With(t, natsbus.AllOps)
	// Refusals are the serving host's resume's own.
	for _, name := range []string{"worker", "ghost"} {
		local := runResume(resumeCmd, []string{name})
		remote := runResume(resumeCmd, []string{name + "@h1"})
		want := "h1 couldn't resume " + name + ": " + strings.Replace(fmt.Sprint(local), "(1 agent there: agent list)", "(1 agent there: agent list --host local)", 1)
		if local == nil || remote == nil || remote.Error() != want {
			t.Errorf("resume %s: local %v\nremote %v\nwant   %s", name, local, remote, want)
		}
	}
	resumeAttach = true
	if err := runResume(resumeCmd, []string{"worker@h1"}); err == nil || !strings.Contains(err.Error(), "--attach") {
		t.Errorf("resume --attach of another host's agent: %v", err)
	}
	resumeAttach = false

	ctx := context.Background()
	path := claudeTranscript(t, home, "11111111-2222-3333-4444-555555555555", dir, "", 0)
	if err := store.CreateAgent(ctx, &storage.Agent{Name: "done", SessionName: "rpc-cli-done", Backend: "claude", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { tmuxClient.KillSession("rpc-cli-done") })
	if err := agentService.Report(ctx, "done", service.StatusReport{Status: service.AgentStatusEnded, EndReason: backend.EndQuit, Transcript: path}); err != nil {
		t.Fatal(err)
	}
	out, err := stdout(t, func() error { return runResume(resumeCmd, []string{"done@h1"}) })
	if err != nil {
		t.Fatal(err)
	}
	if want := "Resumed agent done@h1 (session rpc-cli-done, backend claude) on session 11111111-2222-3333-4444-555555555555, in " + dir + ".\n"; !strings.HasPrefix(out, want) {
		t.Errorf("remote resume printed:\n%s\nwant it to start:\n%s", out, want)
	}
	if len(resumed) != 1 || resumed[0] != "11111111-2222-3333-4444-555555555555" {
		t.Errorf("resumed sessions %v, want the transcript's", resumed)
	}
	if a, err := store.GetAgent(ctx, "done"); err != nil || a == nil || a.Cwd != dir {
		t.Errorf("resumed agent = %+v, %v; want it started in %s, where its session ran", a, err, dir)
	}
	if wd, _ := os.Getwd(); wd == dir {
		t.Error("resume changed this process's directory: the bus serves requests concurrently")
	}
}

// A remote resume says which agent asks, as name@host; the serving host
// lets an agent resume only its own helpers, as locally, and refuses a
// caller that names the serving host itself.
func TestRemoteResume_OnlyYourHelpers(t *testing.T) {
	setupCLI(t)
	serveAsH1With(t, natsbus.AllOps)
	// h1 is this test's own service, whose host is "local": the request
	// says lead@local, which h1 takes for one of its own agents.
	t.Setenv("AGENT_NAME", "lead")
	err := runResume(resumeCmd, []string{"worker@h1"})
	if err == nil || !strings.Contains(err.Error(), "resumed_by lead@local names an agent on this host (local)") {
		t.Errorf("err = %v, want lead@local refused by the host it names", err)
	}

	_, err = serveResume(context.Background(), natsbus.ResumeRequest{Agent: "worker", ResumedBy: "lead@laptop"})
	if err == nil || !strings.Contains(err.Error(), `it was spawned by a person, not by you ("lead@laptop") or your helpers`) {
		t.Errorf("lead@laptop resuming worker: %v, want refused", err)
	}
}

// The JSON lines a remote logs --json prints are whole: a cut never
// splits one.
func TestRemoteLogsJSONLinesWhole(t *testing.T) {
	chunks := []logsChunk{{text: `{"type":"session"}` + "\n", session: true}}
	for i := range 50 {
		chunks = append(chunks, logsChunk{text: fmt.Sprintf(`{"type":"event","n":%d}`+"\n", i)})
	}
	out, dropped := fitLogs(chunks, 200, false)
	if dropped == 0 {
		t.Fatal("nothing was cut")
	}
	lines := strings.Split(strings.TrimSuffix(out, "\n"), "\n")
	if lines[0] != `{"type":"session"}` {
		t.Errorf("first line %q, want the session line put back", lines[0])
	}
	for _, l := range lines {
		if !json.Valid([]byte(l)) {
			t.Errorf("line %q isn't JSON", l)
		}
	}
}

package cli

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

func TestHintOncePerCommandAndQuiet(t *testing.T) {
	setupCLI(t)
	hint("first %d", 1)
	hint("second")
	if got := hints.String(); got != "hint: first 1\n" {
		t.Errorf("hints = %q, want only the first", got)
	}

	hints.Reset()
	hinted, quiet = false, true
	t.Cleanup(func() { quiet = false })
	hint("dropped")
	notice("dropped too")
	if hints.Len() != 0 {
		t.Errorf("--quiet printed %q", hints.String())
	}
}

func TestArgErrorsShowUsageAndExample(t *testing.T) {
	explainArgErrors(rootCmd)
	err := sendCmd.ValidateArgs([]string{"helper"})
	want := "requires at least 2 arg(s), only received 1\nUsage: agent send <name>[@host] <message> [flags]\nExample: agent send researcher"
	if err == nil || !strings.HasPrefix(err.Error(), want) {
		t.Errorf("err = %v, want it to start with %q", err, want)
	}
	// Subcommands' arguments too.
	if err := stateGetCmd.ValidateArgs(nil); err == nil || !strings.Contains(err.Error(), "Usage: agent state get <name> <key>") {
		t.Errorf("state get: err = %v", err)
	}
}

func TestUnknownFlagSuggestions(t *testing.T) {
	err := flagError(listCmd, errors.New("unknown flag: --jsn"))
	if err == nil || err.Error() != "unknown flag: --jsn; did you mean --json?" {
		t.Errorf("err = %v", err)
	}
	err = flagError(listCmd, errors.New("unknown flag: --zzzzzz"))
	if err == nil || !strings.Contains(err.Error(), "see agent list --help") {
		t.Errorf("err = %v", err)
	}
}

func TestUnknownSubcommandSuggestions(t *testing.T) {
	err := runGroup(stateCmd, []string{"sett"})
	if err == nil || err.Error() != `unknown command "sett" for "agent state"; did you mean set? (see agent help state)` {
		t.Errorf("err = %v", err)
	}
}

// The root command suggests by the same rule as groups, not cobra's, which
// took any command within two edits ("bogus": logs, bus).
func TestUnknownCommandSuggestions(t *testing.T) {
	tests := map[string]string{
		"bogus": `unknown command "bogus" for "agent" (see agent help)`,
		"lsit":  `unknown command "lsit" for "agent"; did you mean list? (see agent help)`,
	}
	rootCmd.InitDefaultHelpCmd() // Execute adds it
	for word, want := range tests {
		if _, err := runAgent(t, word); err == nil || err.Error() != want {
			t.Errorf("agent %s: err = %v\nwant %s", word, err, want)
		}
		if _, err := runAgent(t, "help", word); err == nil || err.Error() != want {
			t.Errorf("agent help %s: err = %v\nwant %s", word, err, want)
		}
	}
}

func TestAgentNotFoundSuggests(t *testing.T) {
	setupCLI(t)
	addTeam(t, map[string]string{"helper": ""})
	tests := map[string]string{
		"helpr":          `agent "helpr" not found on host local; did you mean helper?`,
		"rpc-cli-helper": `agent "rpc-cli-helper" not found on host local: that is the tmux session of agent helper, so use the agent name helper`,
		"zzz":            `agent "zzz" not found on host local (2 agents there: agent list)`,
	}
	for name, want := range tests {
		_, err := stdout(t, func() error { return runStatus(statusCmd, []string{name}) })
		if err == nil || err.Error() != want {
			t.Errorf("status %s: err = %v\nwant %s", name, err, want)
		}
	}
}

// queue's hint about a missing watcher comes from the watch lock.
func TestQueueHintsOnlyWithoutAWatcher(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	t.Setenv("AGENT_NAME", "")

	if _, err := stdout(t, func() error { return runQueue(queueCmd, []string{"worker", "hi"}) }); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(hints.String(), "no watcher is running") {
		t.Errorf("hints = %q, want the missing-watcher hint", hints.String())
	}

	release, err := holdWatchLock()
	if err != nil {
		t.Fatal(err)
	}
	defer release()
	hints.Reset()
	hinted = false
	if _, err := stdout(t, func() error { return runQueue(queueCmd, []string{"worker", "hi"}) }); err != nil {
		t.Fatal(err)
	}
	if hints.Len() != 0 {
		t.Errorf("with a watcher running, hints = %q", hints.String())
	}
}

// queue tags a message from "user" as send does: a person, or an AI session
// a person started.
func TestQueueTagsUnrecordedSenders(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("AI_AGENT", "")
	for claudecode, want := range map[string]string{
		"":  "[from a person on local] hi",
		"1": "[from a Claude session on local] hi",
	} {
		t.Setenv("CLAUDECODE", claudecode)
		if _, err := stdout(t, func() error { return runQueue(queueCmd, []string{"worker", "hi"}) }); err != nil {
			t.Fatal(err)
		}
		pending, err := store.GetPendingMessages(context.Background(), "worker", 100)
		if err != nil || len(pending) == 0 {
			t.Fatalf("pending = %v, err %v", pending, err)
		}
		if got := pending[len(pending)-1].Content; got != want {
			t.Errorf("CLAUDECODE=%q: queued %q, want %q", claudecode, got, want)
		}
	}
}

func TestSendRefusesExitedAgent(t *testing.T) {
	setupCLI(t)
	if err := agentService.Report(context.Background(), "worker", service.StatusReport{Status: service.AgentStatusExited}); err != nil {
		t.Fatal(err)
	}
	err := runSend(sendCmd, []string{"worker", "hi"})
	if err == nil || !strings.Contains(err.Error(), "agent worker has exited, so the message would be typed into the shell left behind") {
		t.Errorf("err = %v", err)
	}
}

func TestSendRefusesWaitingAgentSayingWhatToDo(t *testing.T) {
	setupCLI(t)
	err := runSend(sendCmd, []string{"worker", "hi"})
	want := "agent worker is waiting on a prompt (permission: Bash: ls), where the message's Enter would answer it: see it with agent status worker -v, then agent approve worker or agent deny worker"
	if err == nil || !strings.HasPrefix(err.Error(), want) {
		t.Errorf("err = %v", err)
	}
}

func TestSessionCommandsNameTheAgentsSession(t *testing.T) {
	setupCLI(t)
	want := `no tmux session "worker", but there is an agent worker: this command takes session names, and its session is rpc-cli-worker`
	for name, run := range map[string]func() error{
		"keys":  func() error { return runKeys(keysCmd, []string{"worker", "hi"}) },
		"key":   func() error { return runKey(keyCmd, []string{"worker", "Enter"}) },
		"clear": func() error { return runClear(clearCmd, []string{"worker"}) },
		"kill":  func() error { return runSessionKill(sessionKillCmd, []string{"worker"}) },
	} {
		if err := run(); err == nil || err.Error() != want {
			t.Errorf("%s: err = %v", name, err)
		}
	}
}

func TestStateErrorsAndClear(t *testing.T) {
	setupCLI(t)
	t.Setenv("AGENT_NAME", "")

	err := runStateSet(stateSetCmd, []string{"coordinator", "k", "v"})
	if err == nil || !strings.Contains(err.Error(), "state belongs to an agent record") || !strings.Contains(err.Error(), "can't have state: pass results with agent send") {
		t.Errorf("state set for a non-agent: err = %v", err)
	}

	if _, err := stdout(t, func() error { return runStateSet(stateSetCmd, []string{"worker", "progress", "3/5"}) }); err != nil {
		t.Fatal(err)
	}
	out, err := stdout(t, func() error { return runStateGet(stateGetCmd, []string{"worker", "progres"}) })
	if err == nil || err.Error() != `agent worker has no state "progres"; did you mean progress?` || out != "" {
		t.Errorf("state get of a missing key = %q, %v", out, err)
	}

	out, err = stdout(t, func() error { return runStateClear(stateClearCmd, []string{"worker"}) })
	if err != nil || out != "Cleared 1 key for agent worker (kept 5 \"_\" keys that status reports write).\n" {
		t.Errorf("state clear = %q, %v", out, err)
	}
	if st, _ := agentService.StatusInfo(context.Background(), "worker"); st == nil || st.Status != service.AgentStatusWaiting {
		t.Errorf("state clear removed the reported status: %+v", st)
	}
}

func TestTaskGetMissing(t *testing.T) {
	setupCLI(t)
	if err := store.SetTask(context.Background(), &storage.Task{Name: "review-auth", Content: "x", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	out, err := stdout(t, func() error { return runTaskGet(taskGetCmd, []string{"review-ath"}) })
	if err == nil || err.Error() != `no task "review-ath"; did you mean review-auth?` || out != "" {
		t.Errorf("task get = %q, %v", out, err)
	}
	if err := runTaskClear(taskClearCmd, []string{"nope"}); err == nil {
		t.Error("task clear of a missing task: no error")
	}
}

func TestReportSaysWhatItRecorded(t *testing.T) {
	setupCLI(t)
	t.Cleanup(func() { reportAs = "" })
	reportAs = "worker"
	out, err := stdout(t, func() error { return runReport(reportCmd, []string{"processing", "running tests"}) })
	if err != nil || out != "Reported worker processing (running tests).\n" {
		t.Errorf("report = %q, %v", out, err)
	}
	reportAs = "ghost"
	if err := runReport(reportCmd, []string{"idle"}); err == nil || !strings.Contains(err.Error(), `agent "ghost" not found`) {
		t.Errorf("report for a non-agent: err = %v", err)
	}
}

func TestJSONOutput(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	t.Cleanup(func() { listJSON, statusJSON = false, false })

	listJSON = true
	out, err := stdout(t, func() error { return runList(listCmd, nil) })
	if err != nil || !strings.Contains(out, `"name": "worker"`) || !strings.Contains(out, `"host": "local"`) || !strings.Contains(out, `"session": "rpc-cli-worker"`) {
		t.Errorf("list --json = %s, %v", out, err)
	}

	statusJSON = true
	out, err = stdout(t, func() error { return runStatus(statusCmd, []string{"worker"}) })
	for _, field := range []string{`"agent": "worker"`, `"status": "waiting"`, `"detail": "permission: Bash: ls"`, `"tool": "Bash"`} {
		if err != nil || !strings.Contains(out, field) {
			t.Errorf("status --json lacks %s: %s, %v", field, out, err)
		}
	}
	if hints.Len() != 0 {
		t.Errorf("--json printed hints: %q", hints.String())
	}
}

// status -v and --json show the whole tool call a prompt asks about, here
// and over the bus.
func TestStatusShowsWholePermissionInput(t *testing.T) {
	setupCLI(t)
	serveAsH1(t)
	t.Cleanup(func() { statusJSON = false })
	command := "cat TASK.md; python3 -c \"\nimport wordfreq\nprint('हिन्दी Straße')\"" + strings.Repeat(" x", 80)
	input, _ := json.Marshal(map[string]string{"command": command, "description": "check it"})
	if err := agentService.Report(context.Background(), "worker", service.StatusReport{
		Status: service.AgentStatusWaiting, Event: "PermissionRequest", Tool: "Bash",
		Detail: "permission: Bash: " + command, ToolInput: string(input),
	}); err != nil {
		t.Fatal(err)
	}

	statusVerbose = true
	for _, target := range []string{"worker", "worker@h1"} {
		out, err := stdout(t, func() error { return runStatus(statusCmd, []string{target}) })
		if err != nil {
			t.Fatal(err)
		}
		for _, want := range []string{
			"  input:  command: cat TASK.md; python3 -c \"\n",
			"\n            import wordfreq\n            print('हिन्दी Straße')\" x x",
			"\n          description: check it\n",
		} {
			if !strings.Contains(out, want) {
				t.Errorf("status -v %s lacks %q:\n%s", target, want, out)
			}
		}
		xs := 0
		for _, f := range strings.Fields(out[strings.Index(out, "input:"):]) {
			if f == "x" {
				xs++
			}
		}
		if xs != 80 {
			t.Errorf("status -v %s cut the command:\n%s", target, out)
		}
	}

	statusVerbose, statusJSON = false, true
	out, err := stdout(t, func() error { return runStatus(statusCmd, []string{"worker@h1"}) })
	var got struct {
		ToolInput map[string]string `json:"tool_input"`
	}
	if err != nil || json.Unmarshal([]byte(out), &got) != nil || got.ToolInput["command"] != command {
		t.Errorf("status --json tool_input = %q, %v\n%s", got.ToolInput["command"], err, out)
	}
}

// wait for idle says, once, that the agent is blocked on a prompt.
func TestWaitHintsWhenAgentWaits(t *testing.T) {
	setupCLI(t)
	noBus(t)
	t.Cleanup(func() { waitFor, waitInterval = "", 0 })
	waitFor, waitInterval = "idle", 20*time.Millisecond
	go func() {
		time.Sleep(100 * time.Millisecond)
		agentService.Report(context.Background(), "worker", service.StatusReport{Status: service.AgentStatusIdle})
	}()
	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"worker"}) })
	if err != nil || out != "worker: idle\n" {
		t.Fatalf("wait = %q, %v", out, err)
	}
	want := "hint: worker is waiting on a person (a permission prompt or question), so it won't be idle until someone answers: agent status worker -v shows what it's asking (--for idle,waiting returns on prompts)\n"
	if hints.String() != want {
		t.Errorf("hints = %q, want %q", hints.String(), want)
	}
}

func TestWrapIndented(t *testing.T) {
	got := wrapIndented("permission: Bash: echo one two three\nsecond line", 20, "  ")
	want := "permission: Bash:\n  echo one two three\n  second line"
	if got != want {
		t.Errorf("wrapIndented =\n%s\nwant:\n%s", got, want)
	}
}

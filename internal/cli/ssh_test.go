package cli

import (
	"context"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// execCall is what execInto would have replaced this process with.
type execCall struct {
	path string
	argv []string
}

// stubExec records execs instead of making them, finds every program named
// without a path at /usr/bin/<name> (one with a path is itself, as for
// exec.LookPath), and says stdin is a terminal. It returns the calls.
func stubExec(t *testing.T) *[]execCall {
	t.Helper()
	calls := new([]execCall)
	oldExec, oldLook, oldTerm, oldRecorded := execProcess, lookPath, attachTerminal, runRecorded
	t.Cleanup(func() { execProcess, lookPath, attachTerminal, runRecorded = oldExec, oldLook, oldTerm, oldRecorded })
	execProcess = func(path string, argv []string, env []string) error {
		*calls = append(*calls, execCall{path, slices.Clone(argv)})
		return nil
	}
	lookPath = func(name string) (string, error) {
		if strings.Contains(name, "/") {
			return name, nil
		}
		return "/usr/bin/" + name, nil
	}
	attachTerminal = func() bool { return true }
	return calls
}

// agent attach name@host runs ssh to the host's destination, running the
// binary the host names in its list reply, quoted, with attach <name>.
func TestAttachRemote_RunsSSH(t *testing.T) {
	setupCLI(t)
	serveAsH1(t)
	calls := stubExec(t)
	cfg.AgentBinary = "/home/me/.local/bin/agent"

	if err := runAttach(attachCmd, []string{"worker@h1"}); err != nil {
		t.Fatal(err)
	}
	want := []execCall{{"/usr/bin/ssh", []string{"ssh", "-t", "--", "h1", "'/home/me/.local/bin/agent' attach 'worker'"}}}
	if !slices.EqualFunc(*calls, want, sameCall) {
		t.Errorf("exec = %+v, want %+v", *calls, want)
	}
	if !strings.Contains(hints.String(), `it needs "ssh h1" to work from here`) {
		t.Errorf("no word that this is ssh, not the bus: %q", hints)
	}

	// A destination set for the host replaces its name.
	if _, err := stdout(t, func() error { return runBusSSH(busSSHCmd, []string{"h1", "me@h1.lan"}) }); err != nil {
		t.Fatal(err)
	}
	*calls = nil
	if err := runAttach(attachCmd, []string{"worker@h1"}); err != nil {
		t.Fatal(err)
	}
	if len(*calls) != 1 || (*calls)[0].argv[3] != "me@h1.lan" {
		t.Errorf("exec = %+v, want ssh to me@h1.lan", *calls)
	}

	// resume --attach runs that host's own resume --attach.
	*calls = nil
	resumeAttach = true
	t.Cleanup(func() { resumeAttach = false })
	if err := runResume(resumeCmd, []string{"worker@h1"}); err != nil {
		t.Fatal(err)
	}
	if len(*calls) != 1 || (*calls)[0].argv[4] != "'/home/me/.local/bin/agent' resume 'worker' --attach" {
		t.Errorf("exec = %+v, want ssh running resume 'worker' --attach", *calls)
	}
	// The permission flags are still refused for another host's agent.
	resumeSkipPermissions = true
	t.Cleanup(func() { resumeSkipPermissions = false })
	if err := runResume(resumeCmd, []string{"worker@h1"}); err == nil || !strings.Contains(err.Error(), "--skip-permissions works only for an agent on this host") {
		t.Errorf("resume --attach --skip-permissions: %v", err)
	}
}

func sameCall(a, b execCall) bool { return a.path == b.path && slices.Equal(a.argv, b.argv) }

// Attach over ssh refuses, without running ssh: no terminal to hand over,
// an agent the host doesn't have, a host that doesn't say where its
// binary is (older), or a path that isn't safe to put on a command line.
func TestAttachRemote_Refusals(t *testing.T) {
	setupCLI(t)
	serveAsH1(t)
	calls := stubExec(t)

	for _, tt := range []struct {
		name, binary, agent, want string
		terminal                  bool
	}{
		{"no terminal", "/opt/agent", "worker", "needs one: run it in a terminal (agent capture worker@h1", false},
		{"no such agent", "/opt/agent", "ghost", `agent "ghost" not found on host h1 (1 agent there: agent list --host h1)`, true},
		{"older host", "", "worker", "h1's agent predates attach over ssh and doesn't say where its binary is", true},
		{"unsafe path", "/opt/my agent;rm -rf ~", "worker", "won't run h1's agent over ssh", true},
		{"relative path", "bin/agent", "worker", "isn't an absolute path", true},
	} {
		cfg.AgentBinary = tt.binary
		attachTerminal = func() bool { return tt.terminal }
		err := runAttach(attachCmd, []string{tt.agent + "@h1"})
		if err == nil || !strings.Contains(err.Error(), tt.want) {
			t.Errorf("%s: err = %v, want %q", tt.name, err, tt.want)
		}
	}
	if len(*calls) != 0 {
		t.Errorf("ran %+v despite refusing", *calls)
	}
}

// Local attach becomes tmux rather than running it as a child, and the
// command-log row is written first, since nothing runs after.
func TestAttachLocal_ExecsTmux(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	calls := stubExec(t)
	t.Setenv("TMUX", "")
	oldCmd, oldStart := runCmd, runStart
	t.Cleanup(func() { runCmd, runStart = oldCmd, oldStart })
	runCmd, runStart, runRecorded = attachCmd, time.Now(), false

	if err := runAttach(attachCmd, []string{"worker"}); err != nil {
		t.Fatal(err)
	}
	// The tmux it execs is the one internal/tmux runs: on PATH, or where
	// a package manager put it.
	bin, err := tmux.Binary()
	if err != nil {
		t.Skip(err)
	}
	want := []execCall{{bin, []string{"tmux", "attach-session", "-t", "=rpc-cli-worker"}}}
	if !slices.EqualFunc(*calls, want, sameCall) {
		t.Errorf("exec = %+v, want %+v", *calls, want)
	}
	rows, err := store.ListCommands(context.Background(), storage.CommandFilter{})
	if err != nil {
		t.Fatal(err)
	}
	if !runRecorded || len(rows) == 0 {
		t.Errorf("recorded %v, %d command-log rows: want the run written before the exec", runRecorded, len(rows))
	}
}

func TestBusSSH(t *testing.T) {
	setupCLI(t)
	show := func() string {
		t.Helper()
		out, err := stdout(t, func() error { return runBusSSH(busSSHCmd, []string{"hub"}) })
		if err != nil {
			t.Fatal(err)
		}
		return out
	}
	if got := show(); got != "hub: ssh to hub (the default: its host name)\n" {
		t.Errorf("default: %q", got)
	}
	if _, err := stdout(t, func() error { return runBusSSH(busSSHCmd, []string{"hub", "me@hub.lan"}) }); err != nil {
		t.Fatal(err)
	}
	if got := show(); got != "hub: ssh to me@hub.lan\n" || sshDestFor("hub") != "me@hub.lan" {
		t.Errorf("set: %q, sshDestFor %q", got, sshDestFor("hub"))
	}
	busSSHClear = true
	t.Cleanup(func() { busSSHClear = false })
	if _, err := stdout(t, func() error { return runBusSSH(busSSHCmd, []string{"hub"}) }); err != nil {
		t.Fatal(err)
	}
	busSSHClear = false
	if sshDestFor("hub") != "hub" {
		t.Errorf("after --clear: %q, want the default", sshDestFor("hub"))
	}

	// Never something ssh would read as an option, or a second word.
	for _, bad := range []string{"-oProxyCommand=sh", "me@hub lan", "hub;ls", "", "hub\n"} {
		if err := runBusSSH(busSSHCmd, []string{"hub", bad}); err == nil {
			t.Errorf("destination %q accepted", bad)
		}
	}
	if err := runBusSSH(busSSHCmd, []string{"local", "x"}); err == nil || !strings.Contains(err.Error(), "is this host") {
		t.Errorf("this host: %v", err)
	}
}

func TestValidateAgentBinary(t *testing.T) {
	for path, ok := range map[string]bool{
		"/home/me/.local/bin/agent":          true,
		"/Users/me/src/gomuxai/bin/agent":    true,
		"/opt/gomux-1.2_rc+3/agent":          true,
		"bin/agent":                          false,
		"":                                   false,
		"/opt/my agent":                      false,
		"/opt/agent';rm -rf ~;'":             false,
		"/opt/$(id)/agent":                   false,
		"/opt/`id`":                          false,
		"/opt/agent\nrm":                     false,
		"/opt/agent\x1b]0;x\x07":             false,
		"/home/me/agent\"":                   false,
		"/home/é/agent":                      false,
		"/home/me/../me/.local/bin/agent":    true, // quoted; the shell doesn't treat .. specially
		"/home/me/.local/bin/agent --no-bus": false,
	} {
		if err := natsbus.ValidateAgentBinary(path); (err == nil) != ok {
			t.Errorf("ValidateAgentBinary(%q) = %v, want ok %v", path, err, ok)
		}
	}
}

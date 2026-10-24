package cli

import (
	"context"
	"errors"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// setupCLI points the CLI's globals at a temp database holding one agent,
// "worker", waiting on a Bash permission prompt, on host "local". The agent
// has no tmux session: status comes from its report alone.
func setupCLI(t *testing.T) {
	t.Helper()
	if _, err := exec.LookPath("tmux"); err != nil {
		t.Skip("tmux not available")
	}
	dir := t.TempDir()
	testCfg := &config.Config{
		DatabasePath:      filepath.Join(dir, "gomuxai.db"),
		LogDir:            filepath.Join(dir, "logs"),
		StateDir:          filepath.Join(dir, "state"),
		SessionPrefix:     "rpc-cli-",
		DefaultScrollback: 100,
		Host:              "local",
	}
	testStore, err := storage.NewSQLiteStorage(testCfg.DatabasePath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { testStore.Close() })
	testTmux, err := tmux.NewClient()
	if err != nil {
		t.Fatal(err)
	}

	oldCfg, oldStore, oldTmux, oldBackends, oldService, oldConnect := cfg, store, tmuxClient, backends, agentService, busConnect
	t.Cleanup(func() {
		cfg, store, tmuxClient, backends, agentService, busConnect = oldCfg, oldStore, oldTmux, oldBackends, oldService, oldConnect
		statusVerbose, listHost, permissionExpect, keysNoEnter = false, "", "", false
	})
	cfg, store, tmuxClient, backends = testCfg, testStore, testTmux, backend.NewRegistry()
	agentService = service.NewAgentService(cfg, store, tmuxClient, backends)

	ctx := context.Background()
	created := time.Date(2026, 10, 1, 9, 30, 0, 0, time.UTC)
	if err := store.CreateAgent(ctx, &storage.Agent{Name: "worker", SessionName: "rpc-cli-worker", Backend: "claude", CreatedAt: created}); err != nil {
		t.Fatal(err)
	}
	if err := agentService.Report(ctx, "worker", service.StatusReport{
		Status: service.AgentStatusWaiting, Detail: "permission: Bash: ls", Tool: "Bash",
	}); err != nil {
		t.Fatal(err)
	}
}

// serveAsH1 serves the CLI's own service as host "h1" and makes busConnect
// reach it, so "worker@h1" is the same agent as local "worker". It returns a
// counter of bus connections.
func serveAsH1(t *testing.T) *int {
	t.Helper()
	nc := natstest.Start(t)
	ops := append([]string{natsbus.OpKeys}, natsbus.DefaultRemoteOps...)
	stop, err := natsbus.ServeRPC(nc, "h1", service.NewRPCHandler(agentService), ops)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(stop)
	dials := new(int)
	busConnect = func() (*nats.Conn, error) {
		*dials++
		return natstest.Connect(t, nc.ConnectedUrl()), nil
	}
	return dials
}

// forbidBus fails the test if a command dials the bus.
func forbidBus(t *testing.T) {
	busConnect = func() (*nats.Conn, error) {
		t.Error("local command dialed the bus")
		return nil, natsbus.ErrNoBus
	}
}

// stdout runs fn and returns what it printed.
func stdout(t *testing.T, fn func() error) (string, error) {
	t.Helper()
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	old := os.Stdout
	os.Stdout = w
	runErr := fn()
	w.Close()
	os.Stdout = old
	out, _ := io.ReadAll(r)
	return string(out), runErr
}

func TestParseTarget(t *testing.T) {
	old := cfg
	defer func() { cfg = old }()
	cfg = &config.Config{Host: "local"}

	tests := []struct {
		arg        string
		want       natsbus.AgentRef
		wantRemote bool
		wantErr    bool
	}{
		{arg: "worker", want: natsbus.AgentRef{Name: "worker"}},
		{arg: "my.session", want: natsbus.AgentRef{Name: "my.session"}}, // no "@": passed through untouched
		{arg: "worker@local", want: natsbus.AgentRef{Name: "worker"}},
		{arg: "worker@h1", want: natsbus.AgentRef{Name: "worker", Host: "h1"}, wantRemote: true},
		{arg: "worker@", wantErr: true},
		{arg: "worker@h.1", wantErr: true},
		{arg: "a.b@h1", wantErr: true},
	}
	for _, tt := range tests {
		ref, remote, err := parseTarget(tt.arg)
		if (err != nil) != tt.wantErr {
			t.Errorf("parseTarget(%q) err = %v, wantErr %v", tt.arg, err, tt.wantErr)
			continue
		}
		if !tt.wantErr && (ref != tt.want || remote != tt.wantRemote) {
			t.Errorf("parseTarget(%q) = %+v, %v; want %+v, %v", tt.arg, ref, remote, tt.want, tt.wantRemote)
		}
	}

	for host, want := range map[string]bool{"": false, "local": false, "h1": true} {
		if got := isRemoteHost(host); got != want {
			t.Errorf("isRemoteHost(%q) = %v, want %v", host, got, want)
		}
	}
}

func TestLocalTargetsDontDialTheBus(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	statusVerbose = true

	for _, target := range []string{"worker", "worker@local"} {
		out, err := stdout(t, func() error { return runStatus(statusCmd, []string{target}) })
		if err != nil || !strings.HasPrefix(out, target+": waiting\n") {
			t.Errorf("status %s = %q, %v", target, out, err)
		}
	}
	for _, host := range []string{"", "local"} {
		listHost = host
		out, err := stdout(t, func() error { return runList(listCmd, nil) })
		if err != nil || !strings.Contains(out, "rpc-cli-worker") {
			t.Errorf("list --host %q = %q, %v", host, out, err)
		}
	}
	if err := runSend(sendCmd, []string{"ghost@local", "hi"}); err == nil || err.Error() != `agent "ghost" not found` {
		t.Errorf("send ghost@local: err = %v", err)
	}
	if err := runKeys(keysCmd, []string{"ghost@local", "hi"}); err == nil || err.Error() != `agent "ghost" not found` {
		t.Errorf("keys ghost@local: err = %v", err)
	}
}

func TestRemoteOutputMatchesLocal(t *testing.T) {
	setupCLI(t)
	dials := serveAsH1(t)

	run := func(fn func() error) string {
		t.Helper()
		out, err := stdout(t, fn)
		if err != nil {
			t.Fatalf("command failed: %v", err)
		}
		return out
	}

	statusVerbose = true
	local := run(func() error { return runStatus(statusCmd, []string{"worker"}) })
	remote := run(func() error { return runStatus(statusCmd, []string{"worker@h1"}) })
	if want := strings.Replace(local, "worker:", "worker@h1:", 1); remote != want {
		t.Errorf("status -v differs:\nlocal:\n%s\nremote:\n%s", local, remote)
	}
	if !strings.Contains(remote, "tool:   Bash") {
		t.Errorf("remote status lost detail:\n%s", remote)
	}

	listHost = ""
	local = run(func() error { return runList(listCmd, nil) })
	listHost = "h1"
	remote = run(func() error { return runList(listCmd, nil) })
	if remote != local {
		t.Errorf("list differs:\nlocal:\n%s\nremote:\n%s", local, remote)
	}

	if *dials != 2 {
		t.Errorf("bus dialed %d times, want 2 (once per remote command)", *dials)
	}
}

func TestRemoteErrorsMatchLocal(t *testing.T) {
	setupCLI(t)
	serveAsH1(t)

	pairs := []struct {
		name          string
		local, remote func() error
	}{
		{"status", func() error { return runStatus(statusCmd, []string{"ghost"}) },
			func() error { return runStatus(statusCmd, []string{"ghost@h1"}) }},
		{"capture", func() error { return runCapture(captureCmd, []string{"ghost"}) },
			func() error { return runCapture(captureCmd, []string{"ghost@h1"}) }},
		{"send", func() error { return runSend(sendCmd, []string{"ghost", "hi"}) },
			func() error { return runSend(sendCmd, []string{"ghost@h1", "hi"}) }},
		{"keys", func() error { return runKeys(keysCmd, []string{"ghost@local", "hi"}) },
			func() error { return runKeys(keysCmd, []string{"ghost@h1", "hi"}) }},
		{"approve --expect", func() error {
			permissionExpect = "Write"
			return respondPermission("worker", backend.PermissionApprove, "approved")
		}, func() error {
			permissionExpect = "Write"
			return respondPermission("worker@h1", backend.PermissionApprove, "approved")
		}},
	}
	for _, p := range pairs {
		_, localErr := stdout(t, p.local)
		_, remoteErr := stdout(t, p.remote)
		if localErr == nil || remoteErr == nil || localErr.Error() != remoteErr.Error() {
			t.Errorf("%s: local err %v, remote err %v", p.name, localErr, remoteErr)
		}
	}
}

func TestRemoteNeedsBus(t *testing.T) {
	setupCLI(t)
	busConnect = func() (*nats.Conn, error) { return nil, natsbus.ErrNoBus }

	for name, fn := range map[string]func() error{
		"status":  func() error { return runStatus(statusCmd, []string{"worker@h2"}) },
		"capture": func() error { return runCapture(captureCmd, []string{"worker@h2"}) },
		"send":    func() error { return runSend(sendCmd, []string{"worker@h2", "hi"}) },
		"keys":    func() error { return runKeys(keysCmd, []string{"worker@h2", "hi"}) },
		"deny":    func() error { return respondPermission("worker@h2", backend.PermissionDeny, "denied") },
		"list":    func() error { listHost = "h2"; return runList(listCmd, nil) },
	} {
		_, err := stdout(t, fn)
		if !errors.Is(err, natsbus.ErrNoBus) || !strings.Contains(err.Error(), "needs a running agent bus") {
			t.Errorf("%s: err = %v", name, err)
		}
	}
}

func TestRemoteHostNotAnswering(t *testing.T) {
	setupCLI(t)
	serveAsH1(t)

	_, err := stdout(t, func() error { return runStatus(statusCmd, []string{"worker@h2"}) })
	if err == nil || !strings.Contains(err.Error(), `no bus answering for host "h2"`) {
		t.Errorf("err = %v", err)
	}
}

// Messages an agent sends carry its address so the recipient can reply;
// messages typed by a user don't.
func TestFromAgent(t *testing.T) {
	old := cfg
	t.Cleanup(func() { cfg = old })
	cfg = &config.Config{Host: "h1"}
	t.Setenv("AGENT_NAME", "")
	if got := fromAgent("hi"); got != "hi" {
		t.Errorf("user message changed: %q", got)
	}
	t.Setenv("AGENT_NAME", "lead")
	if got := fromAgent("hi"); got != "[from lead@h1] hi" {
		t.Errorf("agent message = %q", got)
	}
}

// --force skips send's dialog check, so it's refused inside agents (and to
// anything without a terminal, as agents' commands are).
func TestSendForce_RefusedForAgents(t *testing.T) {
	old := cfg
	t.Cleanup(func() { cfg = old; sendForce = false })
	cfg = &config.Config{Host: "h1"}
	sendForce = true
	t.Setenv("AGENT_NAME", "lead")
	err := runSend(sendCmd, []string{"worker", "y"})
	if err == nil || !strings.Contains(err.Error(), "needs a person") {
		t.Errorf("err = %v, want refusal", err)
	}
	t.Setenv("AGENT_NAME", "") // tests run without a terminal on stdin
	err = runSend(sendCmd, []string{"worker", "y"})
	if err == nil || !strings.Contains(err.Error(), "needs a person") {
		t.Errorf("without a terminal: err = %v, want refusal", err)
	}
}

// Queueing isn't served over the bus: name@<this host> queues locally, and
// another host is refused without dialing it.
func TestQueueTargets(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	t.Setenv("AGENT_NAME", "")

	for _, target := range []string{"worker", "worker@local"} {
		out, err := stdout(t, func() error { return runQueue(queueCmd, []string{target, "hi"}) })
		if err != nil || out != "Queued message from \"user\" to \"worker\"\n" {
			t.Errorf("queue %s = %q, %v", target, out, err)
		}
	}
	err := runQueue(queueCmd, []string{"worker@h1", "hi"})
	if err == nil || !strings.Contains(err.Error(), "agent send worker@h1") {
		t.Errorf("queue worker@h1: err = %v, want a refusal suggesting agent send", err)
	}
	if err := runQueue(queueCmd, []string{"worker@", "hi"}); err == nil {
		t.Error("queue worker@: want an error")
	}

	pending, err := store.GetPendingMessages(context.Background(), "worker", 10)
	if err != nil {
		t.Fatal(err)
	}
	if len(pending) != 2 {
		t.Errorf("pending messages = %d, want 2 (one per local target)", len(pending))
	}
}

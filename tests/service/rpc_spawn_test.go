package service_test

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// serveSpawn serves env's service as host "h1" with every op, spawn
// included, and returns a client on a separate connection. Spawns use a
// custom backend, "catty", that runs cat: no real CLI starts.
func serveSpawn(t *testing.T, env *testEnv) *natsbus.RPCClient {
	t.Helper()
	cfgFile := filepath.Join(t.TempDir(), "catty.json")
	if err := os.WriteFile(cfgFile, []byte(`{"name": "catty", "launch_command": "cat", "init_message_format": "read %s"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := env.backends.LoadFromFile(cfgFile); err != nil {
		t.Fatal(err)
	}
	nc := natstest.Start(t)
	stop, err := natsbus.ServeRPC(nc, "h1", service.NewRPCHandler(env.service), natsbus.AllOps)
	if err != nil {
		t.Fatalf("ServeRPC: %v", err)
	}
	t.Cleanup(stop)
	return natsbus.NewRPCClient(natstest.Connect(t, nc.ConnectedUrl()), 5*time.Second)
}

// panePath is the working directory of session's pane, as tmux sees it.
func panePath(t *testing.T, session string) string {
	t.Helper()
	out, err := exec.Command("tmux", "display-message", "-p", "-t", session, "#{pane_current_path}").Output()
	if err != nil {
		t.Fatalf("reading %s's pane path: %v", session, err)
	}
	return strings.TrimSpace(string(out))
}

func TestRPC_SpawnStartsAgentWhereAsked(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	client := serveSpawn(t, env)
	dir := t.TempDir()

	resp, err := client.Spawn(ctx, natsbus.AgentRef{Name: "builder", Host: "h1"}, natsbus.SpawnRequest{
		Backend:   "catty",
		Cwd:       dir,
		TaskFile:  "tasks/build.md", // relative: to Cwd, on the serving host
		SpawnedBy: "lead@laptop",
	})
	if err != nil {
		t.Fatalf("Spawn: %v", err)
	}
	session := env.cfg.SessionName("builder")
	defer env.tmux.KillSession(session)
	want := natsbus.SpawnResponse{
		Name: "builder", Session: session, Backend: "catty", SpawnedBy: "lead@laptop",
		Cwd: dir, TaskFile: filepath.Join(dir, "tasks/build.md"), Status: resp.Status,
	}
	if *resp != want {
		t.Errorf("response = %+v\nwant       %+v", *resp, want)
	}
	if resp.Status == "" || resp.Status == string(service.AgentStatusEnded) {
		t.Errorf("status = %q, want the live agent's", resp.Status)
	}

	stored, err := env.store.GetAgent(ctx, "builder")
	if err != nil || stored == nil {
		t.Fatalf("stored agent = %+v, %v", stored, err)
	}
	if stored.Cwd != dir || stored.SpawnedBy != "lead@laptop" || stored.TaskFile != want.TaskFile {
		t.Errorf("stored cwd %q, parent %q, task %q", stored.Cwd, stored.SpawnedBy, stored.TaskFile)
	}
	if got := panePath(t, session); got != dir {
		t.Errorf("pane starts in %q, want %q", got, dir)
	}

	// The remote parent shows in list, for agent list --host.
	agents, err := client.List(ctx, "h1")
	if err != nil || len(agents) != 1 || agents[0].SpawnedBy != "lead@laptop" {
		t.Errorf("List = %+v, %v", agents, err)
	}
}

func TestRPC_SpawnWithoutCwdStartsInHome(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	home := t.TempDir()
	t.Setenv("HOME", home)
	client := serveSpawn(t, env)

	resp, err := client.Spawn(context.Background(), natsbus.AgentRef{Name: "homebody", Host: "h1"}, natsbus.SpawnRequest{Backend: "catty"})
	if err != nil {
		t.Fatalf("Spawn: %v", err)
	}
	defer env.tmux.KillSession(resp.Session)
	if resp.Cwd != home || resp.SpawnedBy != "" {
		t.Errorf("cwd %q, parent %q; want %q and none (a person spawned it)", resp.Cwd, resp.SpawnedBy, home)
	}
	if got := panePath(t, resp.Session); got != home {
		t.Errorf("pane starts in %q, want %q", got, home)
	}
}

func TestRPC_SpawnRefusals(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	client := serveSpawn(t, env)
	file := filepath.Join(t.TempDir(), "plain")
	if err := os.WriteFile(file, nil, 0o600); err != nil {
		t.Fatal(err)
	}

	tests := []struct {
		name string
		req  natsbus.SpawnRequest
		want string
	}{
		{"relative cwd", natsbus.SpawnRequest{Backend: "catty", Cwd: "src/repo"}, `working directory "src/repo" is not an absolute path`},
		{"missing cwd", natsbus.SpawnRequest{Backend: "catty", Cwd: "/no/such/dir"}, "no such file or directory"},
		{"cwd is a file", natsbus.SpawnRequest{Backend: "catty", Cwd: file}, "is not a directory"},
		{"unknown backend", natsbus.SpawnRequest{Backend: "nope", Cwd: t.TempDir()}, `backend "nope" not found`},
		{"no backend", natsbus.SpawnRequest{Cwd: t.TempDir()}, "names no backend"},
	}
	for _, tt := range tests {
		_, err := client.Spawn(ctx, natsbus.AgentRef{Name: "refused", Host: "h1"}, tt.req)
		if err == nil || !strings.Contains(err.Error(), tt.want) {
			t.Errorf("%s: err = %v, want it to say %q", tt.name, err, tt.want)
		}
	}
	if a, _ := env.store.GetAgent(ctx, "refused"); a != nil {
		t.Errorf("a refused spawn left a record: %+v", a)
	}
	if ok, _ := env.tmux.SessionExists(env.cfg.SessionName("refused")); ok {
		env.tmux.KillSession(env.cfg.SessionName("refused"))
		t.Error("a refused spawn left a tmux session")
	}

	// A name in use is refused the way a local spawn refuses it.
	resp, err := client.Spawn(ctx, natsbus.AgentRef{Name: "taken", Host: "h1"}, natsbus.SpawnRequest{Backend: "catty", Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer env.tmux.KillSession(resp.Session)
	if _, err := client.Spawn(ctx, natsbus.AgentRef{Name: "taken", Host: "h1"}, natsbus.SpawnRequest{Backend: "catty", Cwd: t.TempDir()}); err == nil || !strings.Contains(err.Error(), "agent taken already exists") {
		t.Errorf("second spawn of taken: err = %v", err)
	}
}

// A local spawn with Cwd starts there too.
func TestSpawn_Cwd(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	dir := t.TempDir()
	spawnFakeWith(t, env, "placed", service.SpawnOptions{Cwd: dir})
	defer env.tmux.KillSession(env.cfg.SessionName("placed"))
	if a, _ := env.store.GetAgent(context.Background(), "placed"); a == nil || a.Cwd != dir {
		t.Errorf("stored agent = %+v, want cwd %q", a, dir)
	}
	if got := panePath(t, env.cfg.SessionName("placed")); got != dir {
		t.Errorf("pane starts in %q, want %q", got, dir)
	}
}

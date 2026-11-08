package service_test

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
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

// realPath is path with its symlinks resolved, as tmux reports a pane's
// directory: on macOS, t.TempDir() is under /var, a link to /private/var.
func realPath(t *testing.T, path string) string {
	t.Helper()
	real, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatalf("resolving %s: %v", path, err)
	}
	return real
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
	if got, want := panePath(t, session), realPath(t, dir); got != want {
		t.Errorf("pane starts in %q, want %q", got, want)
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
	if got, want := panePath(t, resp.Session), realPath(t, home); got != want {
		t.Errorf("pane starts in %q, want %q", got, want)
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
	if got, want := panePath(t, env.cfg.SessionName("placed")), realPath(t, dir); got != want {
		t.Errorf("pane starts in %q, want %q", got, want)
	}
}

// A peer can't name an agent on the serving host as the parent: the spawn
// came over the bus, so that agent didn't ask for it.
func TestRPC_SpawnRefusesParentOnServingHost(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.Host = "h1"
	client := serveSpawn(t, env)

	// Host names match ignoring case, so "H1" is this host too.
	for _, by := range []string{"victim@h1", "victim@H1"} {
		_, err := client.Spawn(context.Background(), natsbus.AgentRef{Name: "imposter", Host: "h1"},
			natsbus.SpawnRequest{Backend: "catty", Cwd: t.TempDir(), SpawnedBy: by})
		if err == nil || !strings.Contains(err.Error(), "names an agent on this host (h1)") {
			t.Errorf("spawned_by %s: err = %v, want a refusal", by, err)
		}
		if a, _ := env.store.GetAgent(context.Background(), "imposter"); a != nil {
			t.Errorf("spawned_by %s: spawned anyway: %+v", by, a)
		}
	}
}

// A spawn whose context ends mid-startup (a bus stopping cancels the
// requests it is serving) leaves neither its session nor its record.
func TestSpawn_CanceledDuringStartupLeavesNothing(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.StartupDelay = 2 * time.Second
	fake := &fakeHookBackend{ClaudeBackend: namedagents.NewClaudeBackend()}
	env.backends.Register(fake)
	env.backends.RegisterFactory("fakehooks", func(backend.ProviderConfig) backend.Provider { return fake })

	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	_, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "stopped", Backend: "fakehooks"})
	if err == nil {
		env.tmux.KillSession(env.cfg.SessionName("stopped"))
		t.Fatal("spawn succeeded after its context ended")
	}
	if a, _ := env.store.GetAgent(context.Background(), "stopped"); a != nil {
		t.Errorf("a canceled spawn left its record: %+v", a)
	}
	if ok, _ := env.tmux.SessionExists(env.cfg.SessionName("stopped")); ok {
		env.tmux.KillSession(env.cfg.SessionName("stopped"))
		t.Error("a canceled spawn left its tmux session")
	}
}

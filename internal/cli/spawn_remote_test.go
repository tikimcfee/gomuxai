package cli

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

func TestSpawnTarget(t *testing.T) {
	old := cfg
	defer func() { cfg = old }()
	cfg = &config.Config{Host: "local"}

	tests := []struct {
		arg, host  string
		want       natsbus.AgentRef
		wantRemote bool
		wantErr    string
	}{
		{arg: "w", want: natsbus.AgentRef{Name: "w"}},
		{arg: "w@local", want: natsbus.AgentRef{Name: "w"}},
		{arg: "w", host: "local", want: natsbus.AgentRef{Name: "w"}},
		{arg: "w@local", host: "local", want: natsbus.AgentRef{Name: "w"}},
		{arg: "w@hub", want: natsbus.AgentRef{Name: "w", Host: "hub"}, wantRemote: true},
		{arg: "w", host: "hub", want: natsbus.AgentRef{Name: "w", Host: "hub"}, wantRemote: true},
		{arg: "w@hub", host: "hub", want: natsbus.AgentRef{Name: "w", Host: "hub"}, wantRemote: true},
		{arg: "w@hub", host: "other", wantErr: "w@hub names host hub, but --host says other: give one"},
		{arg: "w@local", host: "hub", wantErr: "w@local names host local, but --host says hub: give one"},
		{arg: "w", host: "bad.host", wantErr: `invalid host name "bad.host"`},
		{arg: "w@", wantErr: "invalid host name"},
	}
	for _, tt := range tests {
		ref, remote, err := spawnTarget(tt.arg, tt.host)
		if tt.wantErr != "" {
			if err == nil || !strings.Contains(err.Error(), tt.wantErr) {
				t.Errorf("spawnTarget(%q, %q) err = %v, want %q", tt.arg, tt.host, err, tt.wantErr)
			}
			continue
		}
		if err != nil || ref != tt.want || remote != tt.wantRemote {
			t.Errorf("spawnTarget(%q, %q) = %+v, %v, %v; want %+v, %v", tt.arg, tt.host, ref, remote, err, tt.want, tt.wantRemote)
		}
	}
}

// setupSpawnCLI is setupCLI with a custom backend, "catty", that runs cat,
// as spawn's backend, so spawns start no real CLI.
func setupSpawnCLI(t *testing.T) {
	t.Helper()
	setupCLI(t)
	path := filepath.Join(t.TempDir(), "catty.json")
	if err := os.WriteFile(path, []byte(`{"name": "catty", "launch_command": "cat", "init_message_format": "read %s"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := backends.LoadFromFile(path); err != nil {
		t.Fatal(err)
	}
	cfg.Backend = "catty"
}

// killAfter kills agent name's session when the test ends.
func killAfter(t *testing.T, name string) {
	t.Cleanup(func() { tmuxClient.KillSession(cfg.SessionName(name)) })
}

func TestRemoteSpawn(t *testing.T) {
	setupSpawnCLI(t)
	serveAsH1With(t, natsbus.AllOps)
	t.Setenv("AGENT_NAME", "lead")
	dir := t.TempDir()
	killAfter(t, "builder")

	spawnCwd = dir
	out, err := stdout(t, func() error { return runSpawn(spawnCmd, []string{"builder@h1", "tasks/build.md"}) })
	if err != nil {
		t.Fatalf("spawn builder@h1: %v", err)
	}
	task := filepath.Join(dir, "tasks/build.md")
	want := "Spawned agent builder@h1 (session rpc-cli-builder, backend catty) on h1 in " + dir + ", a helper of lead@local.\n" +
		"Asked it to read " + task + ".\n"
	if !strings.HasPrefix(out, want) {
		t.Errorf("spawn printed:\n%s\nwant it to start:\n%s", out, want)
	}
	if !strings.Contains(out, "builder@h1") || strings.Count(out, "\n") != 3 {
		t.Errorf("want a readiness line naming builder@h1, got:\n%s", out)
	}

	// h1 is this host's own service, so the record is here to check.
	a, err := store.GetAgent(context.Background(), "builder")
	if err != nil || a == nil {
		t.Fatalf("builder's record: %+v, %v", a, err)
	}
	if a.SpawnedBy != "lead@local" || a.Cwd != dir || a.TaskFile != task {
		t.Errorf("recorded parent %q, cwd %q, task %q", a.SpawnedBy, a.Cwd, a.TaskFile)
	}

	// --host, a stored task, and no AGENT_NAME: a person's spawn, no parent.
	t.Setenv("AGENT_NAME", "")
	if err := store.SetTask(context.Background(), &storage.Task{Name: "build", Content: "build it", SourceFile: "/here/only.md", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	killAfter(t, "byperson")
	spawnHost, spawnTask = "h1", "build"
	out, err = stdout(t, func() error { return runSpawn(spawnCmd, []string{"byperson"}) })
	if err != nil {
		t.Fatalf("spawn byperson --host h1: %v", err)
	}
	if !strings.HasPrefix(out, "Spawned agent byperson@h1 (session rpc-cli-byperson, backend catty) on h1 in "+dir+".\nSent it task build.\n") {
		t.Errorf("spawn --host printed:\n%s", out)
	}
	if a, _ := store.GetAgent(context.Background(), "byperson"); a == nil || a.SpawnedBy != "" || a.TaskFile != "" {
		t.Errorf("byperson's record = %+v, want no parent and no task file (its source is a path on the caller)", a)
	}

	// Spawn's errors come back from h1, saying it was h1.
	err = runSpawn(spawnCmd, []string{"builder@h1"})
	if err == nil || !strings.HasPrefix(err.Error(), "h1 couldn't spawn builder: agent builder already exists") {
		t.Errorf("respawn builder@h1: err = %v", err)
	}
}

func TestRemoteSpawn_RefusesLocalOnlyFlags(t *testing.T) {
	setupSpawnCLI(t)
	forbidBus(t)
	t.Cleanup(func() { backendConfig = "" })

	tests := []struct {
		flag string
		set  func()
	}{
		{"--resume", func() { spawnResume = "3f34adff" }},
		{"--attach", func() { spawnAttach = true }},
		{"--skip-permissions", func() { spawnSkipPermissions = true }},
		{"--backend-config", func() { backendConfig = "my-cli.json" }},
	}
	for _, tt := range tests {
		spawnResume, spawnAttach, spawnSkipPermissions, backendConfig = "", false, false, ""
		tt.set()
		err := runSpawn(spawnCmd, []string{"x@hub"})
		if want := tt.flag + " works only for an agent on this host: to spawn x with it, run agent spawn on hub"; err == nil || err.Error() != want {
			t.Errorf("%s: err = %v, want %q", tt.flag, err, want)
		}
	}
	spawnResume, spawnAttach, spawnSkipPermissions, backendConfig = "", false, false, ""

	spawnCwd = "src/repo"
	if err := runSpawn(spawnCmd, []string{"x@hub"}); err == nil || !strings.Contains(err.Error(), "--cwd src/repo: give an absolute path on hub") {
		t.Errorf("relative --cwd: err = %v", err)
	}
}

func TestRemoteSpawn_HostThatDoesntServeSpawn(t *testing.T) {
	setupSpawnCLI(t)
	serveAsH1(t) // the default remote ops: no spawn

	err := runSpawn(spawnCmd, []string{"x@h1"})
	if err == nil || !strings.Contains(err.Error(), `no bus answering for host "h1"`) {
		t.Errorf("err = %v, want no bus answering", err)
	}
	if !strings.Contains(hints.String(), "h1 serves spawn to other hosts only if its bus runs with --remote-ops listing spawn") {
		t.Errorf("hint = %q, want how to allow spawn", hints.String())
	}
	if a, _ := store.GetAgent(context.Background(), "x"); a != nil {
		t.Errorf("x was spawned: %+v", a)
	}
}

func TestLocalSpawn_Cwd(t *testing.T) {
	setupSpawnCLI(t)
	forbidBus(t)
	dir := t.TempDir()
	killAfter(t, "here")

	spawnCwd = dir
	out, err := stdout(t, func() error { return runSpawn(spawnCmd, []string{"here@local", "task.md"}) })
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	wd, _ := os.Getwd()
	task := filepath.Join(wd, "task.md") // relative to spawn's directory, not --cwd
	if want := "Spawned agent here (session rpc-cli-here, backend catty) in " + dir + ".\nAsked it to read " + task + ".\n"; !strings.HasPrefix(out, want) {
		t.Errorf("spawn printed:\n%s\nwant it to start:\n%s", out, want)
	}
	if a, _ := store.GetAgent(context.Background(), "here"); a == nil || a.Cwd != dir || a.TaskFile != task {
		t.Errorf("record = %+v, want cwd %q and task %q", a, dir, task)
	}

	spawnResume = "3f34adff"
	if err := runSpawn(spawnCmd, []string{"there"}); err == nil || !strings.Contains(err.Error(), "--cwd and --resume don't go together") {
		t.Errorf("--cwd with --resume: err = %v", err)
	}
}

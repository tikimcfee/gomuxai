package service_test

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// spawn --model goes on the CLI's launch line, ahead of the exit report agy's
// line ends with, and resume relaunches on the same model.
func TestSpawn_ModelIsPassedAndKeptOnResume(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.AgentBinary = "/opt/gomuxai/agent"
	ctx := context.Background()
	resumedFake := registerFakeAgy(env)
	spawned := spawnFakeAgy(t, env, service.SpawnOptions{Name: "m", Backend: "fakeagy", Model: "gemini-3.8-flash-high"})

	launch := spawned.launchedCmd
	model := strings.Index(launch, " --model 'gemini-3.8-flash-high'")
	exit := strings.Index(launch, "; ")
	if model < 0 || exit < 0 || model > exit {
		t.Errorf("launch line %q: want --model before the exit report", launch)
	}
	if got := stateValue(t, env, "m", service.StateKeyModel); got != "gemini-3.8-flash-high" {
		t.Errorf("%s = %q", service.StateKeyModel, got)
	}

	// The same through resume, on the real agy registration.
	if _, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "r", Backend: "agy", Model: "claude-opus-5-5-high"}); err != nil {
		t.Fatal(err)
	}
	if info, err := env.service.StatusInfo(ctx, "r"); err != nil || info.Model != "claude-opus-5-5-high" {
		t.Errorf("StatusInfo = %+v, %v; want its model", info, err)
	}
	if err := env.tmux.KillSession(env.cfg.SessionName("r")); err != nil {
		t.Fatal(err)
	}
	if _, err := env.service.Resume(ctx, service.ResumeOptions{Name: "r", SessionID: "conv-9"}); err != nil {
		t.Fatalf("Resume: %v", err)
	}
	if f := resumedFake(); f == nil || !strings.Contains(f.launchedCmd, "--model 'claude-opus-5-5-high'") {
		t.Errorf("resumed with %v, want --model claude-opus-5-5-high", f)
	}
	if got := stateValue(t, env, "r", service.StateKeyModel); got != "claude-opus-5-5-high" {
		t.Errorf("%s after resume = %q", service.StateKeyModel, got)
	}
}

// A backend that takes no model refuses --model rather than start on its
// default, as does a name that doesn't read as a model.
func TestSpawn_ModelRefusals(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	cfgPath := filepath.Join(t.TempDir(), "backend.json")
	if err := os.WriteFile(cfgPath, []byte(`{"name": "custom", "launch_command": "echo custom"}`), 0644); err != nil {
		t.Fatal(err)
	}
	if _, err := env.backends.LoadFromFile(cfgPath); err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		backend, model, want string
	}{
		{"shell", "opus", "backend shell takes no --model"},
		{"claude", "opus; rm -rf ~", "invalid model"},
		{"claude", "-p", "invalid model"},
		{"custom", "opus", "backend custom takes no --model"},
	} {
		_, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "x", Backend: tc.backend, Model: tc.model})
		if err == nil || !strings.Contains(err.Error(), tc.want) {
			t.Errorf("spawn --backend %s --model %q: %v, want %q", tc.backend, tc.model, err, tc.want)
		}
		if exists, _ := env.tmux.SessionExists(env.cfg.SessionName("x")); exists {
			t.Errorf("--model %q left a session behind", tc.model)
		}
	}
}

func TestValidateModel(t *testing.T) {
	for _, ok := range []string{"opus", "sonnet", "claude-fable-5-1", "claude-opus-5-5[1m]", "gemini-3.8-flash-high", "openrouter/x-ai/grok-4", "gpt-5@2026-01"} {
		if err := service.ValidateModel(ok); err != nil {
			t.Errorf("ValidateModel(%q) = %v", ok, err)
		}
	}
	for _, bad := range []string{"", "-p", "--dangerously-skip-permissions", "a b", "$(id)", "x'y", "a\nb", strings.Repeat("a", 129)} {
		if err := service.ValidateModel(bad); err == nil {
			t.Errorf("ValidateModel(%q) passed", bad)
		}
	}
}

// A spawn over the bus carries --model; the serving host checks it as a
// local spawn does, and says in its response which model the agent runs on.
func TestRPC_SpawnModel(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	client := serveSpawn(t, env)
	registerFakeAgy(env)

	resp, err := client.Spawn(ctx, natsbus.AgentRef{Name: "m", Host: "h1"}, natsbus.SpawnRequest{Backend: "agy", Cwd: t.TempDir(), Model: "gemini-3.8-flash-high"})
	if err != nil {
		t.Fatalf("Spawn: %v", err)
	}
	if resp.Model != "gemini-3.8-flash-high" {
		t.Errorf("response model = %q", resp.Model)
	}
	if got := stateValue(t, env, "m", service.StateKeyModel); got != "gemini-3.8-flash-high" {
		t.Errorf("%s = %q", service.StateKeyModel, got)
	}
	st, err := client.Status(ctx, natsbus.AgentRef{Name: "m", Host: "h1"})
	if err != nil || st.Model != "gemini-3.8-flash-high" {
		t.Errorf("Status = %+v, %v; want its model", st, err)
	}

	for model, want := range map[string]string{
		"x; touch /tmp/pwned": "invalid model",
		"opus":                "backend catty takes no --model",
	} {
		_, err := client.Spawn(ctx, natsbus.AgentRef{Name: "bad", Host: "h1"}, natsbus.SpawnRequest{Backend: "catty", Cwd: t.TempDir(), Model: model})
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("spawn --model %q over the bus: %v, want %q", model, err, want)
		}
	}
}

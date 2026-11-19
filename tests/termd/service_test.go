package termd_test

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

func TestConfigTermdPrecedenceAndValidation(t *testing.T) {
	t.Setenv("GOMUXAI_TERMD_NS", "")

	def := config.DefaultConfig()
	if def.TermdNamespace != config.DefaultTermdNamespace {
		t.Fatalf("DefaultConfig TermdNamespace = %q, want %q",
			def.TermdNamespace, config.DefaultTermdNamespace)
	}
	if err := def.ValidateTermd(); err != nil {
		t.Fatalf("default ValidateTermd: %v", err)
	}

	// File config sets termd_namespace.
	dataDir := t.TempDir()
	appCfgPath := filepath.Join(t.TempDir(), "app.json")
	dbPath := filepath.Join(dataDir, "gomuxai.db")
	rawCfg := `{"database_path": "` + filepath.ToSlash(dbPath) + `", "termd_namespace": "agents-file"}`
	if err := os.WriteFile(appCfgPath, []byte(rawCfg), 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err := config.LoadFromFile(appCfgPath)
	if err != nil {
		t.Fatalf("LoadFromFile: %v", err)
	}
	if loaded.TermdNamespace != "agents-file" {
		t.Errorf("from file = %q, want agents-file", loaded.TermdNamespace)
	}

	// Env overrides file.
	t.Setenv("GOMUXAI_TERMD_NS", "agents-env")
	envCfg, err := config.LoadFromFile(appCfgPath)
	if err != nil {
		t.Fatal(err)
	}
	if envCfg.TermdNamespace != "agents-env" {
		t.Errorf("from env = %q, want agents-env", envCfg.TermdNamespace)
	}
	if err := envCfg.ValidateTermd(); err != nil {
		t.Errorf("ValidateTermd(agents-env) = %v, want nil", err)
	}

	// Reserved and invalid namespaces fail loud.
	for _, reserved := range []string{"default", "wall"} {
		bad := &config.Config{TermdNamespace: reserved}
		if err := bad.ValidateTermd(); err == nil || !strings.Contains(err.Error(), "reserved") {
			t.Errorf("ValidateTermd(%q) = %v, want reserved refusal", reserved, err)
		}
	}
	invalid := &config.Config{TermdNamespace: "bad/ns"}
	if err := invalid.ValidateTermd(); err == nil {
		t.Error("ValidateTermd(bad/ns) = nil, want error")
	}
}

func TestAgentServiceEndToEndOnTermd(t *testing.T) {
	tc := newTestClient(t, "agents-svc")
	ctx := context.Background()

	root := t.TempDir()
	binDir := filepath.Join(root, "bin")
	if err := os.MkdirAll(binDir, 0o700); err != nil {
		t.Fatal(err)
	}
	fakeAgentBin := filepath.Join(binDir, "agent")
	if err := os.WriteFile(fakeAgentBin, []byte("#!/bin/sh\nexit 0\n"), 0o700); err != nil {
		t.Fatal(err)
	}

	cfg := &config.Config{
		Backend:           "shell",
		TermdNamespace:    "agents-svc",
		DatabasePath:      filepath.Join(root, ".gomuxai", "gomuxai.db"),
		LogDir:            filepath.Join(root, ".gomuxai", "logs"),
		StateDir:          filepath.Join(root, ".gomuxai", "state"),
		SessionPrefix:     "agent-",
		DefaultScrollback: 200,
		StartupDelay:      0,
		SubmitWait:        0,
		AgentBinary:       fakeAgentBin,
		Host:              "test-host",
	}
	if err := cfg.EnsureDirs(); err != nil {
		t.Fatal(err)
	}
	store, err := storage.NewSQLiteStorage(cfg.DatabasePath)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	svc := service.NewAgentService(cfg, store, tc, backend.NewRegistry())

	workDir := t.TempDir()
	agent, err := svc.Spawn(ctx, service.SpawnOptions{
		Name:    "helper",
		Backend: "shell",
		Cwd:     workDir,
	})
	if err != nil {
		t.Fatalf("Spawn on termd: %v", err)
	}

	// Verify session exists in termd and carries AGENT_NAME, AGENT_HOST, GOMUXAI_TERMD_NS.
	if got := tc.SessionEnv(agent.SessionName, "AGENT_NAME"); got != "helper" {
		t.Errorf("AGENT_NAME = %q, want helper", got)
	}
	if got := tc.SessionEnv(agent.SessionName, "GOMUXAI_TERMD_NS"); got != "agents-svc" {
		t.Errorf("GOMUXAI_TERMD_NS = %q, want agents-svc", got)
	}

	// CallerAgent walks process ancestors up to the termd terminal's root PID.
	pane, err := tc.FirstPane(agent.SessionName)
	if err != nil {
		t.Fatalf("FirstPane: %v", err)
	}
	caller, known, err := svc.CallerAgent(ctx, []int{999999, pane.PID, 1})
	if err != nil || !known || caller != "helper" {
		t.Errorf("CallerAgent = (%q, %v, %v), want (helper, true, nil)", caller, known, err)
	}

	// Send runs through PasteText + Enter into the shell; Capture reads it back.
	if _, err := svc.Send(ctx, "helper", "echo termd-svc-$((40+2))", service.SendOptions{}); err != nil {
		t.Fatalf("Send: %v", err)
	}
	await.Until(t, "termd-svc-42 in Capture", func() (string, bool) {
		out, err := svc.Capture(ctx, "helper", 50, false)
		if err != nil {
			return err.Error(), false
		}
		return out, strings.Contains(out, "termd-svc-42")
	})

	// Adopt an existing termd terminal via CurrentPane ($G3D_TERMD_ID), and verify
	// Cleanup leaves the adopted terminal running while removing the spawned one.
	userTerm := "dev-shell"
	if err := tc.CreateSessionIn(userTerm, workDir, nil); err != nil {
		t.Fatalf("CreateSessionIn(dev-shell): %v", err)
	}
	adopted, err := svc.Adopt(ctx, service.AdoptOptions{
		Name:        "adopted-dev",
		CurrentPane: userTerm,
	})
	if err != nil {
		t.Fatalf("Adopt on termd: %v", err)
	}
	if !adopted.Agent.Adopted || strings.HasPrefix(adopted.Agent.Cwd, "file://") {
		t.Errorf("Adopt result = %+v", adopted.Agent)
	}
	if err := svc.Cleanup(ctx, "adopted-dev"); err != nil {
		t.Fatalf("Cleanup(adopted-dev): %v", err)
	}
	if exists, _ := tc.SessionExists(userTerm); !exists {
		t.Error("Cleanup killed an adopted termd terminal; must leave it running")
	}
	_ = tc.KillSession(userTerm)

	// Cleanup of spawned agent snapshots terminal scrollback to LogPath and kills the terminal.
	if err := svc.Cleanup(ctx, "helper"); err != nil {
		t.Fatalf("Cleanup(helper): %v", err)
	}
	if exists, _ := tc.SessionExists(agent.SessionName); exists {
		t.Error("spawned termd terminal still exists after Cleanup")
	}
	logBytes, err := os.ReadFile(cfg.LogPath("helper"))
	if err != nil || !strings.Contains(string(logBytes), "termd-svc-42") {
		t.Errorf("post-cleanup terminal log (%v) = %q, want termd-svc-42", err, logBytes)
	}
}

package cli

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// writeBackendConfig writes a --backend-config file and points the flag at it.
func writeBackendConfig(t *testing.T, json string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "backend.json")
	if err := os.WriteFile(path, []byte(json), 0o600); err != nil {
		t.Fatal(err)
	}
	old := backendConfig
	t.Cleanup(func() { backendConfig, backendConfigErr = old, nil })
	backendConfig = path
	return path
}

func TestSetupBackends(t *testing.T) {
	oldCfg, oldBackends := cfg, backends
	t.Cleanup(func() { cfg, backends = oldCfg, oldBackends })

	// A custom backend config names spawn's backend unless something else did.
	writeBackendConfig(t, `{"name": "custom", "launch_command": "cat"}`)
	cfg = &config.Config{}
	setupBackends()
	if backendConfigErr != nil || cfg.Backend != "custom" {
		t.Errorf("backend = %q, err = %v; want custom, nil", cfg.Backend, backendConfigErr)
	}
	if _, err := backends.Get("custom"); err != nil {
		t.Errorf("custom backend not registered: %v", err)
	}
	cfg = &config.Config{Backend: "claude"}
	setupBackends()
	if cfg.Backend != "claude" {
		t.Errorf("backend = %q, want the app config's claude", cfg.Backend)
	}

	// An app config isn't a backend config: no error yet, but the reason is kept.
	path := writeBackendConfig(t, `{"backend": "claude"}`)
	cfg = &config.Config{Backend: "claude"}
	setupBackends()
	if backendConfigErr == nil {
		t.Fatal("backendConfigErr = nil for a file with no backend name")
	}
	err := withBackendConfigErr(os.ErrNotExist)
	if !strings.Contains(err.Error(), path+" didn't load as a backend config: backend config must have a name") {
		t.Errorf("err = %v, want the reason the file didn't load", err)
	}
}

// Repro: a custom backend config missing launch_command was skipped
// silently, and spawn failed only with "backend not found".
func TestSpawnSaysWhyBackendConfigDidntLoad(t *testing.T) {
	setupCLI(t)
	writeBackendConfig(t, `{"name": "custom"}`)
	setupBackends()
	agentService = service.NewAgentService(cfg, store, tmuxClient, backends)
	cfg.Backend = "custom"

	err := runSpawn(spawnCmd, []string{"new-worker"})
	if err == nil || !strings.Contains(err.Error(), `backend "custom" not found`) ||
		!strings.Contains(err.Error(), "must have a launch_command") {
		t.Errorf("err = %v, want not found, with why the backend config didn't load", err)
	}
}

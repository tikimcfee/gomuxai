// Package config_test provides comprehensive tests for configuration management.
package config_test

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/config"
)

// ===========================================================================
// File Loading Tests
// ===========================================================================

func TestLoadFromFile_Success(t *testing.T) {
	// Create a temp file with valid JSON config
	tmpFile, err := os.CreateTemp("", "config-*.json")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	defer os.Remove(tmpFile.Name())

	validConfig := map[string]interface{}{
		"backend":           "claude",
		"database_path":     "/tmp/test.db",
		"log_dir":           "/tmp/logs",
		"state_dir":         "/tmp/state",
		"session_prefix":    "test-",
		"default_scrollback": 1000,
	}

	data, _ := json.Marshal(validConfig)
	if _, err := tmpFile.Write(data); err != nil {
		t.Fatalf("writing config: %v", err)
	}
	tmpFile.Close()

	cfg, err := config.LoadFromFile(tmpFile.Name())
	if err != nil {
		t.Fatalf("LoadFromFile failed: %v", err)
	}

	if cfg.Backend != "claude" {
		t.Errorf("expected backend 'claude', got %q", cfg.Backend)
	}
	if cfg.DatabasePath != "/tmp/test.db" {
		t.Errorf("expected database_path '/tmp/test.db', got %q", cfg.DatabasePath)
	}
	if cfg.LogDir != "/tmp/logs" {
		t.Errorf("expected log_dir '/tmp/logs', got %q", cfg.LogDir)
	}
	if cfg.StateDir != "/tmp/state" {
		t.Errorf("expected state_dir '/tmp/state', got %q", cfg.StateDir)
	}
	if cfg.SessionPrefix != "test-" {
		t.Errorf("expected session_prefix 'test-', got %q", cfg.SessionPrefix)
	}
	if cfg.DefaultScrollback != 1000 {
		t.Errorf("expected default_scrollback 1000, got %d", cfg.DefaultScrollback)
	}
}

func TestLoadFromFile_MissingFile(t *testing.T) {
	_, err := config.LoadFromFile("/nonexistent/path/config.json")
	if err == nil {
		t.Fatal("expected error for missing file, got nil")
	}

	// Should contain helpful message about reading the file
	if !containsSubstring(err.Error(), "reading config file") {
		t.Errorf("error should mention 'reading config file', got: %v", err)
	}
}

func TestLoadFromFile_InvalidJSON(t *testing.T) {
	tmpFile, err := os.CreateTemp("", "config-invalid-*.json")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	defer os.Remove(tmpFile.Name())

	// Write invalid JSON
	if _, err := tmpFile.WriteString(`{"backend": "claude", invalid json`); err != nil {
		t.Fatalf("writing invalid config: %v", err)
	}
	tmpFile.Close()

	_, err = config.LoadFromFile(tmpFile.Name())
	if err == nil {
		t.Fatal("expected error for invalid JSON, got nil")
	}

	if !containsSubstring(err.Error(), "parsing config file") {
		t.Errorf("error should mention 'parsing config file', got: %v", err)
	}
}

func TestLoadFromFile_EmptyFile(t *testing.T) {
	tmpFile, err := os.CreateTemp("", "config-empty-*.json")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	defer os.Remove(tmpFile.Name())
	tmpFile.Close()

	// Empty file is invalid JSON
	_, err = config.LoadFromFile(tmpFile.Name())
	if err == nil {
		t.Fatal("expected error for empty file, got nil")
	}
}

func TestLoadFromFile_PartialConfig(t *testing.T) {
	// Test that loading partial config merges with defaults
	tmpFile, err := os.CreateTemp("", "config-partial-*.json")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	defer os.Remove(tmpFile.Name())

	// Only set backend, rest should be defaults
	partialConfig := map[string]interface{}{
		"backend": "aider",
	}

	data, _ := json.Marshal(partialConfig)
	if _, err := tmpFile.Write(data); err != nil {
		t.Fatalf("writing config: %v", err)
	}
	tmpFile.Close()

	cfg, err := config.LoadFromFile(tmpFile.Name())
	if err != nil {
		t.Fatalf("LoadFromFile failed: %v", err)
	}

	if cfg.Backend != "aider" {
		t.Errorf("expected backend 'aider', got %q", cfg.Backend)
	}

	// Should have default values for other fields
	defaults := config.DefaultConfig()
	if cfg.SessionPrefix != defaults.SessionPrefix {
		t.Errorf("expected default session_prefix %q, got %q", defaults.SessionPrefix, cfg.SessionPrefix)
	}
	if cfg.DefaultScrollback != defaults.DefaultScrollback {
		t.Errorf("expected default scrollback %d, got %d", defaults.DefaultScrollback, cfg.DefaultScrollback)
	}
}

// ===========================================================================
// Validation Tests
// ===========================================================================

func TestValidate_BackendRequired(t *testing.T) {
	cfg := config.DefaultConfig()
	// Backend is empty by default

	err := cfg.Validate()
	if err == nil {
		t.Fatal("expected validation error for empty backend, got nil")
	}

	// Error should mention backend and provide helpful guidance
	if !containsSubstring(err.Error(), "backend") {
		t.Errorf("error should mention 'backend', got: %v", err)
	}
	if !containsSubstring(err.Error(), "--backend") {
		t.Errorf("error should suggest '--backend' flag, got: %v", err)
	}
	if !containsSubstring(err.Error(), "--backend-config") {
		t.Errorf("error should suggest '--backend-config' flag, got: %v", err)
	}
}

func TestValidate_ValidBackend(t *testing.T) {
	testCases := []struct {
		name    string
		backend string
	}{
		{"claude backend", "claude"},
		{"aider backend", "aider"},
		{"custom backend", "custom-backend"},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := config.DefaultConfig()
			cfg.Backend = tc.backend

			err := cfg.Validate()
			if err != nil {
				t.Errorf("validation should pass for backend %q, got error: %v", tc.backend, err)
			}
		})
	}
}

// ===========================================================================
// Directory Setup Tests
// ===========================================================================

func TestEnsureDirs_CreatesAllDirectories(t *testing.T) {
	// Create temp base directory
	tmpDir, err := os.MkdirTemp("", "config-dirs-*")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

	cfg := &config.Config{
		DatabasePath: filepath.Join(tmpDir, "db", "gomuxai.db"),
		LogDir:       filepath.Join(tmpDir, "logs"),
		StateDir:     filepath.Join(tmpDir, "state"),
	}

	err = cfg.EnsureDirs()
	if err != nil {
		t.Fatalf("EnsureDirs failed: %v", err)
	}

	// Check directories exist
	dirs := []string{
		filepath.Join(tmpDir, "db"),    // Parent of database
		filepath.Join(tmpDir, "logs"),
		filepath.Join(tmpDir, "state"),
	}

	for _, dir := range dirs {
		info, err := os.Stat(dir)
		if err != nil {
			t.Errorf("directory %q should exist: %v", dir, err)
			continue
		}
		if !info.IsDir() {
			t.Errorf("%q should be a directory", dir)
		}
	}
}

func TestEnsureDirs_ExistingDirectories(t *testing.T) {
	// Create temp directories that already exist
	tmpDir, err := os.MkdirTemp("", "config-existing-*")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

	// Create directories beforehand
	logDir := filepath.Join(tmpDir, "logs")
	stateDir := filepath.Join(tmpDir, "state")
	os.MkdirAll(logDir, 0755)
	os.MkdirAll(stateDir, 0755)

	cfg := &config.Config{
		DatabasePath: filepath.Join(tmpDir, "gomuxai.db"),
		LogDir:       logDir,
		StateDir:     stateDir,
	}

	// Should not error on existing directories
	err = cfg.EnsureDirs()
	if err != nil {
		t.Fatalf("EnsureDirs should handle existing directories: %v", err)
	}
}

func TestEnsureDirs_NestedDirectories(t *testing.T) {
	tmpDir, err := os.MkdirTemp("", "config-nested-*")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

	cfg := &config.Config{
		DatabasePath: filepath.Join(tmpDir, "deep", "nested", "path", "gomuxai.db"),
		LogDir:       filepath.Join(tmpDir, "a", "b", "c", "logs"),
		StateDir:     filepath.Join(tmpDir, "x", "y", "z", "state"),
	}

	err = cfg.EnsureDirs()
	if err != nil {
		t.Fatalf("EnsureDirs should create nested directories: %v", err)
	}

	// Verify nested paths exist
	nestedDirs := []string{
		filepath.Join(tmpDir, "deep", "nested", "path"),
		filepath.Join(tmpDir, "a", "b", "c", "logs"),
		filepath.Join(tmpDir, "x", "y", "z", "state"),
	}

	for _, dir := range nestedDirs {
		if _, err := os.Stat(dir); err != nil {
			t.Errorf("nested directory %q should exist: %v", dir, err)
		}
	}
}

// ===========================================================================
// Path Construction Tests
// ===========================================================================

func TestSessionName(t *testing.T) {
	testCases := []struct {
		name          string
		sessionPrefix string
		agentName     string
		expected      string
	}{
		{
			name:          "default prefix",
			sessionPrefix: "agent-",
			agentName:     "researcher",
			expected:      "agent-researcher",
		},
		{
			name:          "custom prefix",
			sessionPrefix: "myproject-",
			agentName:     "builder",
			expected:      "myproject-builder",
		},
		{
			name:          "empty prefix",
			sessionPrefix: "",
			agentName:     "worker",
			expected:      "worker",
		},
		{
			name:          "agent name with dashes",
			sessionPrefix: "test-",
			agentName:     "sub-agent-1",
			expected:      "test-sub-agent-1",
		},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := &config.Config{SessionPrefix: tc.sessionPrefix}
			result := cfg.SessionName(tc.agentName)
			if result != tc.expected {
				t.Errorf("SessionName(%q) = %q, want %q", tc.agentName, result, tc.expected)
			}
		})
	}
}

func TestLogPath(t *testing.T) {
	testCases := []struct {
		name      string
		logDir    string
		agentName string
		expected  string
	}{
		{
			name:      "simple path",
			logDir:    "/var/log/gomuxai",
			agentName: "researcher",
			expected:  "/var/log/gomuxai/researcher.log",
		},
		{
			name:      "relative path",
			logDir:    "logs",
			agentName: "builder",
			expected:  "logs/builder.log",
		},
		{
			name:      "agent name with special chars",
			logDir:    "/tmp/logs",
			agentName: "agent-v2",
			expected:  "/tmp/logs/agent-v2.log",
		},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := &config.Config{LogDir: tc.logDir}
			result := cfg.LogPath(tc.agentName)
			if result != tc.expected {
				t.Errorf("LogPath(%q) = %q, want %q", tc.agentName, result, tc.expected)
			}
		})
	}
}

func TestStatePath(t *testing.T) {
	testCases := []struct {
		name      string
		stateDir  string
		agentName string
		expected  string
	}{
		{
			name:      "simple path",
			stateDir:  "/var/lib/gomuxai/state",
			agentName: "researcher",
			expected:  "/var/lib/gomuxai/state/researcher",
		},
		{
			name:      "relative path",
			stateDir:  ".state",
			agentName: "builder",
			expected:  ".state/builder",
		},
		{
			name:      "agent name with dashes",
			stateDir:  "/tmp/state",
			agentName: "sub-agent-1",
			expected:  "/tmp/state/sub-agent-1",
		},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := &config.Config{StateDir: tc.stateDir}
			result := cfg.StatePath(tc.agentName)
			if result != tc.expected {
				t.Errorf("StatePath(%q) = %q, want %q", tc.agentName, result, tc.expected)
			}
		})
	}
}

// ===========================================================================
// DefaultConfig Tests
// ===========================================================================

func TestDefaultConfig(t *testing.T) {
	cfg := config.DefaultConfig()

	// Backend should be empty (required to be explicitly set)
	if cfg.Backend != "" {
		t.Errorf("default Backend should be empty, got %q", cfg.Backend)
	}

	// Session prefix should have a sensible default
	if cfg.SessionPrefix == "" {
		t.Error("default SessionPrefix should not be empty")
	}

	// Scrollback should be positive
	if cfg.DefaultScrollback <= 0 {
		t.Errorf("default scrollback should be positive, got %d", cfg.DefaultScrollback)
	}

	// Paths should be set
	if cfg.DatabasePath == "" {
		t.Error("default DatabasePath should not be empty")
	}
	if cfg.LogDir == "" {
		t.Error("default LogDir should not be empty")
	}
	if cfg.StateDir == "" {
		t.Error("default StateDir should not be empty")
	}
}

func TestDefaultConfig_PortableStorage(t *testing.T) {
	cfg := config.DefaultConfig()

	// CRITICAL: Storage must NOT be in home directory
	// This ensures the tool is portable
	home, err := os.UserHomeDir()
	if err != nil {
		t.Skip("cannot determine home directory")
	}

	// DatabasePath should NOT start with home directory
	if len(cfg.DatabasePath) > len(home) && cfg.DatabasePath[:len(home)] == home {
		t.Errorf("DatabasePath should NOT be in home directory for portability, got %q", cfg.DatabasePath)
	}

	// LogDir should NOT start with home directory
	if len(cfg.LogDir) > len(home) && cfg.LogDir[:len(home)] == home {
		t.Errorf("LogDir should NOT be in home directory for portability, got %q", cfg.LogDir)
	}

	// StateDir should NOT start with home directory
	if len(cfg.StateDir) > len(home) && cfg.StateDir[:len(home)] == home {
		t.Errorf("StateDir should NOT be in home directory for portability, got %q", cfg.StateDir)
	}

	// All paths should contain the storage dir name
	if !containsSubstring(cfg.DatabasePath, config.StorageDirName) {
		t.Errorf("DatabasePath should contain %q, got %q", config.StorageDirName, cfg.DatabasePath)
	}
}

// ===========================================================================
// Timeout Configuration Tests
// ===========================================================================

func TestDefaultTimeouts(t *testing.T) {
	cfg := config.DefaultConfig()

	// SpawnTimeout should be set to default (30s)
	if cfg.SpawnTimeout != config.DefaultSpawnTimeout {
		t.Errorf("expected SpawnTimeout %v, got %v", config.DefaultSpawnTimeout, cfg.SpawnTimeout)
	}
	if cfg.SpawnTimeout != 30*time.Second {
		t.Errorf("SpawnTimeout should be 30s, got %v", cfg.SpawnTimeout)
	}

	// CaptureTimeout should be set to default (10s)
	if cfg.CaptureTimeout != config.DefaultCaptureTimeout {
		t.Errorf("expected CaptureTimeout %v, got %v", config.DefaultCaptureTimeout, cfg.CaptureTimeout)
	}
	if cfg.CaptureTimeout != 10*time.Second {
		t.Errorf("CaptureTimeout should be 10s, got %v", cfg.CaptureTimeout)
	}

	// StatusTimeout should be set to default (5s)
	if cfg.StatusTimeout != config.DefaultStatusTimeout {
		t.Errorf("expected StatusTimeout %v, got %v", config.DefaultStatusTimeout, cfg.StatusTimeout)
	}
	if cfg.StatusTimeout != 5*time.Second {
		t.Errorf("StatusTimeout should be 5s, got %v", cfg.StatusTimeout)
	}
}

func TestTimeoutConstants(t *testing.T) {
	// Verify timeout constants have sensible values
	if config.DefaultSpawnTimeout <= 0 {
		t.Error("DefaultSpawnTimeout should be positive")
	}
	if config.DefaultCaptureTimeout <= 0 {
		t.Error("DefaultCaptureTimeout should be positive")
	}
	if config.DefaultStatusTimeout <= 0 {
		t.Error("DefaultStatusTimeout should be positive")
	}

	// SpawnTimeout should be the longest (operations can take time)
	if config.DefaultSpawnTimeout < config.DefaultCaptureTimeout {
		t.Error("SpawnTimeout should be >= CaptureTimeout")
	}
	if config.DefaultSpawnTimeout < config.DefaultStatusTimeout {
		t.Error("SpawnTimeout should be >= StatusTimeout")
	}
}

func TestLoadFromFile_WithTimeouts(t *testing.T) {
	tmpFile, err := os.CreateTemp("", "config-timeouts-*.json")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	defer os.Remove(tmpFile.Name())

	// Create config with custom timeouts (as nanoseconds in JSON)
	configData := map[string]interface{}{
		"backend":         "claude",
		"spawn_timeout":   60000000000,  // 60s in nanoseconds
		"capture_timeout": 20000000000,  // 20s in nanoseconds
		"status_timeout":  15000000000,  // 15s in nanoseconds
	}

	data, _ := json.Marshal(configData)
	if _, err := tmpFile.Write(data); err != nil {
		t.Fatalf("writing config: %v", err)
	}
	tmpFile.Close()

	cfg, err := config.LoadFromFile(tmpFile.Name())
	if err != nil {
		t.Fatalf("LoadFromFile failed: %v", err)
	}

	if cfg.SpawnTimeout != 60*time.Second {
		t.Errorf("expected SpawnTimeout 60s, got %v", cfg.SpawnTimeout)
	}
	if cfg.CaptureTimeout != 20*time.Second {
		t.Errorf("expected CaptureTimeout 20s, got %v", cfg.CaptureTimeout)
	}
	if cfg.StatusTimeout != 15*time.Second {
		t.Errorf("expected StatusTimeout 15s, got %v", cfg.StatusTimeout)
	}
}

func TestLoadFromFile_DefaultTimeoutsWhenNotSpecified(t *testing.T) {
	tmpFile, err := os.CreateTemp("", "config-no-timeouts-*.json")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	defer os.Remove(tmpFile.Name())

	// Config without timeouts - should use defaults
	configData := map[string]interface{}{
		"backend": "claude",
	}

	data, _ := json.Marshal(configData)
	if _, err := tmpFile.Write(data); err != nil {
		t.Fatalf("writing config: %v", err)
	}
	tmpFile.Close()

	cfg, err := config.LoadFromFile(tmpFile.Name())
	if err != nil {
		t.Fatalf("LoadFromFile failed: %v", err)
	}

	// Should have default timeout values
	if cfg.SpawnTimeout != config.DefaultSpawnTimeout {
		t.Errorf("expected default SpawnTimeout, got %v", cfg.SpawnTimeout)
	}
	if cfg.CaptureTimeout != config.DefaultCaptureTimeout {
		t.Errorf("expected default CaptureTimeout, got %v", cfg.CaptureTimeout)
	}
	if cfg.StatusTimeout != config.DefaultStatusTimeout {
		t.Errorf("expected default StatusTimeout, got %v", cfg.StatusTimeout)
	}
}

// ===========================================================================
// Helpers
// ===========================================================================

func containsSubstring(s, substr string) bool {
	return len(s) >= len(substr) && (s == substr || len(s) > 0 && containsSubstringImpl(s, substr))
}

func containsSubstringImpl(s, substr string) bool {
	for i := 0; i <= len(s)-len(substr); i++ {
		if s[i:i+len(substr)] == substr {
			return true
		}
	}
	return false
}

// Package config provides configuration management for the agent tool.
package config

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

// Config holds runtime configuration for the agent tool.
type Config struct {
	// Backend is the name of the backend to use (e.g., "claude", "aider").
	Backend string `json:"backend"`

	// DatabasePath is the path to the SQLite database file.
	DatabasePath string `json:"database_path"`

	// LogDir is the directory for agent log files.
	LogDir string `json:"log_dir"`

	// StateDir is created by EnsureDirs, but agent state lives in the
	// database; AgentDir takes the data directory from its parent.
	StateDir string `json:"state_dir"`

	// SessionPrefix is the prefix for tmux session names.
	SessionPrefix string `json:"session_prefix"`

	// DefaultScrollback is the default number of lines to capture.
	DefaultScrollback int `json:"default_scrollback"`

	// SpawnTimeout, CaptureTimeout, and StatusTimeout are read from config
	// files and defaulted, but no operation applies them yet.
	SpawnTimeout   time.Duration `json:"spawn_timeout"`
	CaptureTimeout time.Duration `json:"capture_timeout"`
	StatusTimeout  time.Duration `json:"status_timeout"`

	// StartupDelay is the time to wait after launching a CLI before sending init message.
	// Default: 2s (can be set to 0 for testing)
	StartupDelay time.Duration `json:"startup_delay"`

	// AgentBinary is the absolute path of the agent binary that backend hooks
	// call to report status. Empty disables hook-based status reporting.
	// Default: the running executable.
	AgentBinary string `json:"agent_binary"`

	// Host names this machine on the agent bus ("name@host" addressing and
	// bus subjects). Default: the short hostname, made subject-safe.
	Host string `json:"host"`
}

// Default timeout values.
const (
	DefaultSpawnTimeout   = 30 * time.Second
	DefaultCaptureTimeout = 10 * time.Second
	DefaultStatusTimeout  = 5 * time.Second
	DefaultStartupDelay   = 2 * time.Second
)

// StorageDirName is the name of the data directory.
const StorageDirName = ".gomuxai"

// getStorageDir returns the base directory for all gomuxai data: next to the
// binary, so the tool keeps nothing in ~/, or in the working directory if
// the binary's path is unknown.
func getStorageDir() string {
	if exe := agentBinaryPath(); exe != "" {
		return filepath.Join(filepath.Dir(exe), StorageDirName)
	}
	return StorageDirName
}

// defaultHost returns $AGENT_HOST if set, else the short hostname with
// characters that can't appear in a bus subject token replaced, e.g.
// "Ivans-Air.local" -> "Ivans-Air".
func defaultHost() string {
	if host := os.Getenv("AGENT_HOST"); host != "" {
		return host
	}
	name, err := os.Hostname()
	if err != nil || name == "" {
		return "localhost"
	}
	name, _, _ = strings.Cut(name, ".")
	return hostUnsafe.ReplaceAllString(name, "-")
}

var hostUnsafe = regexp.MustCompile(`[^A-Za-z0-9_-]`)

// agentBinaryPath returns the resolved path of the running executable, or "".
func agentBinaryPath() string {
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	if resolved, err := filepath.EvalSymlinks(exe); err == nil {
		return resolved
	}
	return exe
}

// DefaultConfig returns a Config with sensible defaults.
// Note: Backend is intentionally empty - it must be explicitly provided.
// Storage is relative to the binary location for portability.
func DefaultConfig() *Config {
	baseDir := getStorageDir()

	return &Config{
		Backend:           "", // Must be explicitly set
		DatabasePath:      filepath.Join(baseDir, "gomuxai.db"),
		LogDir:            filepath.Join(baseDir, "logs"),
		StateDir:          filepath.Join(baseDir, "state"),
		SessionPrefix:     "agent-",
		DefaultScrollback: 500,
		SpawnTimeout:      DefaultSpawnTimeout,
		CaptureTimeout:    DefaultCaptureTimeout,
		StatusTimeout:     DefaultStatusTimeout,
		StartupDelay:      DefaultStartupDelay,
		AgentBinary:       agentBinaryPath(),
		Host:              defaultHost(),
	}
}

// LoadFromFile loads configuration from a JSON file.
func LoadFromFile(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading config file: %w", err)
	}

	cfg := DefaultConfig()
	if err := json.Unmarshal(data, cfg); err != nil {
		return nil, fmt.Errorf("parsing config file: %w", err)
	}

	return cfg, nil
}

// Validate checks that required configuration is present.
func (c *Config) Validate() error {
	if c.Backend == "" {
		return fmt.Errorf("backend is required: use --backend <name> or --backend-config <file>")
	}
	return nil
}

// EnsureDirs creates necessary directories if they don't exist.
func (c *Config) EnsureDirs() error {
	dirs := []string{
		filepath.Dir(c.DatabasePath),
		c.LogDir,
		c.StateDir,
	}

	// Private: logs and the database hold terminal output and agent state.
	for _, dir := range dirs {
		if err := os.MkdirAll(dir, 0700); err != nil {
			return fmt.Errorf("creating directory %s: %w", dir, err)
		}
	}

	return nil
}

// SessionName returns the full tmux session name for an agent.
func (c *Config) SessionName(agentName string) string {
	return c.SessionPrefix + agentName
}

// LogPath returns the log file path for an agent.
func (c *Config) LogPath(agentName string) string {
	return filepath.Join(c.LogDir, agentName+".log")
}

// BusDir returns the agent bus directory (identity, trusted peers, runtime
// files), alongside the database.
func (c *Config) BusDir() string {
	return filepath.Join(filepath.Dir(c.DatabasePath), "bus")
}

// AgentDir returns the per-agent bundle directory under the gomuxai data dir
// (e.g. .gomuxai/agents/<name>).
func (c *Config) AgentDir(agentName string) string {
	baseDir := StorageDirName
	if c.StateDir != "" {
		baseDir = filepath.Dir(c.StateDir)
	} else if c.DatabasePath != "" {
		baseDir = filepath.Dir(c.DatabasePath)
	}
	return filepath.Join(baseDir, "agents", agentName)
}

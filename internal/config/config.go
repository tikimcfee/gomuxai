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

	// StartupDelay is the time to wait after launching a CLI before sending init message.
	// Default: 2s (can be set to 0 for testing)
	StartupDelay time.Duration `json:"startup_delay"`

	// SubmitWait bounds each wait while typing a message into a CLI that
	// can take its Enter for a newline (Claude): for the input box to show
	// the typed text before Enter, and for it to empty after each Enter.
	// Default: 3s; 0 types and presses Enter without looking.
	SubmitWait time.Duration `json:"submit_wait"`

	// AgentBinary is the absolute path of the agent binary that backend hooks
	// call to report status. Empty disables hook-based status reporting.
	// Default: the running executable.
	AgentBinary string `json:"agent_binary"`

	// Host names this machine on the agent bus ("name@host" addressing and
	// bus subjects). Default: $AGENT_HOST, else the host the bus identity
	// was created for, else the system's name for this machine (defaultHost).
	Host string `json:"host"`

	// ClaudeProjectsDir, AgyBrainDir and KimiSessionsDir are where agent
	// sessions looks for each harness's transcripts. Unset (nil) means the
	// app's default locations that exist; a path is the only place looked;
	// an empty string turns that harness off. A leading ~ is the home dir.
	ClaudeProjectsDir *string `json:"claude_projects_dir,omitempty"`
	AgyBrainDir       *string `json:"agy_brain_dir,omitempty"`
	KimiSessionsDir   *string `json:"kimi_sessions_dir,omitempty"`
}

// DefaultStartupDelay is the default StartupDelay.
const DefaultStartupDelay = 2 * time.Second

// DefaultSubmitWait is the default SubmitWait.
const DefaultSubmitWait = 3 * time.Second

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

// defaultHost returns $AGENT_HOST if set, else the host name the bus
// identity in busDir was created for, else systemHost's: this machine's
// name on the bus stays the one its peers trust, whatever the network
// later calls it (macOS renames itself "Mac" on some networks).
func defaultHost(busDir string) string {
	if host := os.Getenv("AGENT_HOST"); host != "" {
		return host
	}
	// The identity's own record (natsbus's hostNameFile): written by
	// agent bus init, and checked against the host on every load.
	if b, err := os.ReadFile(filepath.Join(busDir, "host")); err == nil {
		if host := strings.TrimSpace(string(b)); host != "" {
			return host
		}
	}
	return systemHost()
}

// systemHost returns this machine's name with characters that can't
// appear in a bus subject token replaced, e.g. "Studio-Mac.local" ->
// "Studio-Mac": on macOS the LocalHostName set in Sharing settings, which
// the network doesn't change, else the short hostname, else "localhost".
func systemHost() string {
	name := localHostName()
	if name == "" {
		if h, err := os.Hostname(); err == nil {
			name = h
		}
	}
	name, _, _ = strings.Cut(name, ".")
	if name == "" {
		return "localhost"
	}
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
		StartupDelay:      DefaultStartupDelay,
		SubmitWait:        DefaultSubmitWait,
		AgentBinary:       agentBinaryPath(),
		Host:              defaultHost(filepath.Join(baseDir, "bus")),
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
	// A database_path elsewhere has its own bus dir, whose identity names
	// the host, unless the file names one itself.
	var set struct {
		Host *string `json:"host"`
	}
	if json.Unmarshal(data, &set) == nil && set.Host == nil {
		cfg.Host = defaultHost(cfg.BusDir())
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

// LogPath returns the log file path for an agent: the pane's pipe-pane
// output. External viewers (glyph3d-js's screens) tail this path, so
// changing it breaks them.
func (c *Config) LogPath(agentName string) string {
	return filepath.Join(c.LogDir, agentName+".log")
}

// TranscriptsPath returns the file, beside the agent's log, listing the
// CLI transcripts its sessions were recorded in.
func (c *Config) TranscriptsPath(agentName string) string {
	return filepath.Join(c.LogDir, agentName+".transcripts")
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

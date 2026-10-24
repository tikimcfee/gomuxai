// Package cli provides the command-line interface for the agent tool.
package cli

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

var (
	// Global flags
	backendName   string
	backendConfig string

	// Shared state
	cfg          *config.Config
	store        storage.Storage
	tmuxClient   tmux.TmuxOps
	backends     *backend.Registry
	agentService *service.AgentService

	// backendConfigErr is why --backend-config didn't load as a custom
	// backend, for spawn's errors: the file may be an app config instead.
	backendConfigErr error
)

// rootCmd is the base command.
var rootCmd = &cobra.Command{
	Use:   "agent",
	Short: "Multi-agent orchestration tool",
	Long: `agent is a CLI tool for orchestrating AI agents in tmux sessions.

Agents can use this tool to spawn sub-agents, send messages, capture output,
and coordinate with other agents. The same binary serves both as the
orchestrator and as the tool agents use for inter-agent communication.`,
	PersistentPreRunE: setup,
	SilenceUsage:      true,
}

// Execute runs the CLI and records the run in the command log. Its error's
// exit code is ExitCode(err).
func Execute() error {
	defer closeBusPublisher()
	start := time.Now()
	cmd, err := rootCmd.ExecuteC()
	recordCommand(cmd, os.Args[1:], start, err)
	return err
}

func init() {
	rootCmd.PersistentFlags().StringVar(&backendName, "backend", "", "Backend to use (claude, aider, agy)")
	rootCmd.PersistentFlags().StringVar(&backendConfig, "backend-config", "", "Path to backend config file")

	// Add all subcommands
	rootCmd.AddCommand(spawnCmd)
	rootCmd.AddCommand(sendCmd)
	rootCmd.AddCommand(queueCmd)
	rootCmd.AddCommand(captureCmd)
	rootCmd.AddCommand(listCmd)
	rootCmd.AddCommand(statusCmd)
	rootCmd.AddCommand(cleanupCmd)
	rootCmd.AddCommand(watchCmd)
	rootCmd.AddCommand(sessionCmd)
	rootCmd.AddCommand(keysCmd)
	rootCmd.AddCommand(keyCmd)
	rootCmd.AddCommand(clearCmd)
	rootCmd.AddCommand(taskCmd)
	rootCmd.AddCommand(stateCmd)
	rootCmd.AddCommand(logsCmd)
	rootCmd.AddCommand(reportCmd)
	rootCmd.AddCommand(busCmd)
	rootCmd.AddCommand(approveCmd)
	rootCmd.AddCommand(denyCmd)
	rootCmd.AddCommand(waitCmd)
	rootCmd.AddCommand(eventsCmd)
	rootCmd.AddCommand(versionCmd)
	rootCmd.AddCommand(historyCmd)
	rootCmd.AddCommand(frictionCmd)
}

func setup(cmd *cobra.Command, args []string) error {
	// Skip setup for commands that don't need it
	if cmd.Name() == "help" {
		return nil
	}

	// Initialize config
	cfg = config.DefaultConfig()

	// Handle backend configuration
	if backendConfig != "" {
		// Load from file
		fileCfg, err := config.LoadFromFile(backendConfig)
		if err != nil {
			return fmt.Errorf("loading config: %w", err)
		}
		cfg = fileCfg
	}

	if backendName != "" {
		cfg.Backend = backendName
	}
	setupBackends()

	// Only spawn needs a backend
	if cmd.Name() == "spawn" {
		if err := cfg.Validate(); err != nil {
			return withBackendConfigErr(err)
		}
	}

	// Ensure directories exist
	if err := cfg.EnsureDirs(); err != nil {
		return fmt.Errorf("creating directories: %w", err)
	}

	// Initialize storage
	var err error
	store, err = storage.NewSQLiteStorage(cfg.DatabasePath)
	if err != nil {
		return fmt.Errorf("initializing storage: %w", err)
	}

	// Initialize tmux client
	tmuxClient, err = tmux.NewClient()
	if err != nil {
		return fmt.Errorf("initializing tmux: %w", err)
	}

	// Initialize agent service
	agentService = service.NewAgentService(cfg, store, tmuxClient, backends)
	agentService.SetEventPublisher(warnPublisher{busPublisher})

	return nil
}

// setupBackends creates the backend registry, with the custom backend
// --backend-config configures, if it does. The file may be an app config
// instead (or both), so one that isn't a backend config is no error here; a
// custom backend is the default for spawn when neither the app config nor
// --backend names one.
func setupBackends() {
	backends = backend.NewRegistry()
	backendConfigErr = nil
	if backendConfig == "" {
		return
	}
	b, err := backends.LoadFromFile(backendConfig)
	if err != nil {
		backendConfigErr = err
		return
	}
	if cfg.Backend == "" {
		cfg.Backend = b.Name()
	}
}

// withBackendConfigErr adds why --backend-config didn't load as a custom
// backend, if it didn't, to err, an error about spawn's backend.
func withBackendConfigErr(err error) error {
	if backendConfigErr == nil {
		return err
	}
	return fmt.Errorf("%w (%s didn't load as a backend config: %v)", err, backendConfig, backendConfigErr)
}

func getContext() context.Context {
	return context.Background()
}

// Package cli provides the command-line interface for the agent tool.
package cli

import (
	"context"
	"fmt"

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

// Execute runs the CLI.
func Execute() error {
	defer closeBusPublisher()
	return rootCmd.Execute()
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

	// Only spawn needs a backend
	if cmd.Name() == "spawn" {
		if err := cfg.Validate(); err != nil {
			return err
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

	// Initialize backend registry
	backends = backend.NewRegistry()

	// Load custom backend if provided. The file may be an app config
	// instead, so an invalid backend config isn't an error.
	if backendConfig != "" {
		_ = backends.LoadFromFile(backendConfig)
	}

	// Initialize agent service
	agentService = service.NewAgentService(cfg, store, tmuxClient, backends)
	agentService.SetEventPublisher(warnPublisher{busPublisher})

	return nil
}

func getContext() context.Context {
	return context.Background()
}

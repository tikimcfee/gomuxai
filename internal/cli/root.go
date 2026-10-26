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
	Short: "Coordinate AI agents running in tmux sessions",
	Long: `agent spawns AI agents (claude, aider, agy) in tmux sessions and coordinates
them: messages, status, permission prompts, and shared state, on this host or
across hosts linked by the agent bus.

Run "agent" alone for a short orientation (who you are, your agents, the bus);
"agent help <command>" has each command's details and examples.

Results go to stdout; a hint about what to do next may follow on stderr
(-q drops it). Exit codes: 0 done, 1 failed; wait exits 2 when it times out,
and friction add exits 3 when it finds a similar entry instead of filing.

If agent gets in your way, "agent friction add" tells the people who fix it.`,
	Example: `  agent spawn helper --backend claude
  agent send helper "summarize README.md" && agent wait helper --for idle --next
  agent state get helper _result`,
	Args:              rootArgs,
	RunE:              runOrient,
	PersistentPreRunE: setup,
	SilenceUsage:      true,
}

// helpCmd replaces cobra's help command, which takes a word the root's Args
// accept for an unknown command and prints the root's help for it.
var helpCmd = &cobra.Command{
	Use:   "help [command]",
	Short: "Help about any command",
	Long:  `Help prints a command's details and examples, or with no command, the list of commands.`,
	Example: `  agent help
  agent help wait
  agent help friction add`,
	RunE: func(c *cobra.Command, args []string) error {
		cmd, rest, err := c.Root().Find(args)
		if err != nil {
			return err
		}
		if len(rest) > 0 {
			return unknownCommand(cmd, rest[0])
		}
		cmd.InitDefaultHelpFlag()
		cmd.InitDefaultVersionFlag()
		return cmd.Help()
	},
}

// Command groups for "agent help".
var commandGroups = []struct {
	group *cobra.Group
	cmds  []*cobra.Command
}{
	{&cobra.Group{ID: "agents", Title: "Agents:"}, []*cobra.Command{spawnCmd, attachCmd, listCmd, cleanupCmd, exportCmd, sessionsCmd}},
	{&cobra.Group{ID: "messages", Title: "Messages:"}, []*cobra.Command{sendCmd, queueCmd, deliverCmd, watchCmd}},
	{&cobra.Group{ID: "status", Title: "Status and waiting:"}, []*cobra.Command{statusCmd, waitCmd, approveCmd, denyCmd, captureCmd, logsCmd, transcriptCmd, eventsCmd, reportCmd}},
	{&cobra.Group{ID: "state", Title: "State and tasks:"}, []*cobra.Command{stateCmd, taskCmd}},
	{&cobra.Group{ID: "bus", Title: "Machines (bus):"}, []*cobra.Command{busCmd}},
	{&cobra.Group{ID: "tool", Title: "About this tool:"}, []*cobra.Command{frictionCmd, historyCmd, versionCmd}},
	{&cobra.Group{ID: "tmux", Title: "Low-level tmux (session names, not agent names):"}, []*cobra.Command{sessionCmd, keysCmd, keyCmd, clearCmd}},
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
	rootCmd.PersistentFlags().StringVar(&backendName, "backend", "", "Backend for spawn: claude, aider, or agy (spawn requires it or --backend-config; no default)")
	rootCmd.PersistentFlags().StringVar(&backendConfig, "backend-config", "", "JSON file defining a custom backend (or app settings); pass it again to status/wait/watch for agents spawned with it")
	rootCmd.PersistentFlags().BoolVarP(&quiet, "quiet", "q", false, "Don't print hints (results and errors still print)")

	for _, g := range commandGroups {
		rootCmd.AddGroup(g.group)
		for _, c := range g.cmds {
			c.GroupID = g.group.ID
			rootCmd.AddCommand(c)
		}
	}
	rootCmd.CompletionOptions.HiddenDefaultCmd = true
	cobra.EnableCommandSorting = false // keep each group in task order
	rootCmd.SetFlagErrorFunc(flagError)
	rootCmd.SetHelpCommand(helpCmd)
	// Subcommands of subcommands are added by later init functions, so the
	// whole tree is only there once the CLI runs.
	cobra.OnInitialize(func() { explainArgErrors(rootCmd) })
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

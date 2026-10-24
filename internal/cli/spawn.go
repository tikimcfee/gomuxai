package cli

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var (
	skipPermissions bool
	trustWorkspace  bool
	taskName        string
)

var spawnCmd = &cobra.Command{
	Use:   "spawn <name> [task-file]",
	Short: "Spawn a new agent",
	Long: `Spawn creates a new agent in a tmux session.

The agent will run the specified backend CLI tool. You can provide initial work via:
  - --task <name>  : Reference a task from the task repository (DB)
  - [task-file]    : Path to a task file (read directly)

Examples:
  agent spawn researcher --backend claude
  agent spawn researcher --backend claude --task research-auth
  agent spawn builder --backend claude tasks/build.md
  agent spawn helper --backend aider
  agent spawn auto-worker --backend claude --skip-permissions`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runSpawn,
}

func init() {
	spawnCmd.Flags().BoolVar(&skipPermissions, "skip-permissions", false,
		"Pass --dangerously-skip-permissions to Claude or Agy (use with caution)")
	spawnCmd.Flags().BoolVar(&trustWorkspace, "trust-workspace", false,
		"Confirm the CLI's workspace trust prompt on launch (use with caution)")
	spawnCmd.Flags().StringVar(&taskName, "task", "",
		"Reference a task from the repository by name")
}

func runSpawn(cmd *cobra.Command, args []string) error {
	name := args[0]
	var taskFile string
	var taskContent string

	// Priority: --task flag > positional task-file argument
	if taskName != "" {
		// Fetch task from DB
		task, err := store.GetTask(getContext(), taskName)
		if err != nil {
			return fmt.Errorf("fetching task: %w", err)
		}
		if task == nil {
			return fmt.Errorf("task %q not found", taskName)
		}
		taskContent = task.Content
		// Use source file if available for reference
		if task.SourceFile != "" {
			taskFile = task.SourceFile
		}
	} else if len(args) > 1 {
		taskFile = args[1]
	}

	opts := service.SpawnOptions{
		Name:            name,
		Backend:         cfg.Backend,
		TaskFile:        taskFile,
		TaskContent:     taskContent,
		SkipPermissions: skipPermissions,
		TrustWorkspace:  trustWorkspace,
		SpawnedBy:       os.Getenv("AGENT_NAME"),
	}

	agent, err := agentService.Spawn(getContext(), opts)
	if err != nil {
		return err
	}

	fmt.Printf("Spawned agent %q (session: %s, backend: %s)\n",
		agent.Name, agent.SessionName, agent.Backend)

	return nil
}

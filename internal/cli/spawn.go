package cli

import (
	"errors"
	"fmt"
	"os"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var (
	spawnSkipPermissions bool
	spawnTrustWorkspace  bool
	spawnTask            string
)

var spawnCmd = &cobra.Command{
	Use:   "spawn <name> [task-file]",
	Short: "Spawn a new agent",
	Long: `Spawn creates a new agent in a tmux session, running the backend's CLI
(built in: claude, aider, agy).

Initial work, if given, is sent as the agent's first message:
  - --task <name>  : the task's content, from the task repository (DB)
  - [task-file]    : a request to read that file (claude/agy: "Please read and
                     execute the task in <file>", aider: "/read <file>")
Without either, send the first message yourself with "agent send".

Examples:
  agent spawn researcher --backend claude
  agent spawn researcher --backend claude --task research-auth
  agent spawn builder --backend claude tasks/build.md
  agent spawn helper --backend aider
  agent spawn auto-worker --backend claude --skip-permissions
  agent spawn reviewer --backend agy --trust-workspace`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runSpawn,
}

func init() {
	spawnCmd.Flags().BoolVar(&spawnSkipPermissions, "skip-permissions", false,
		"Pass --dangerously-skip-permissions to Claude or Agy (use with caution)")
	spawnCmd.Flags().BoolVar(&spawnTrustWorkspace, "trust-workspace", false,
		"Confirm the CLI's workspace trust prompt on launch (use with caution)")
	spawnCmd.Flags().StringVar(&spawnTask, "task", "",
		"Reference a task from the repository by name")
}

func runSpawn(cmd *cobra.Command, args []string) error {
	name := args[0]
	var taskFile string
	var taskContent string

	// Priority: --task flag > positional task-file argument
	if spawnTask != "" {
		// Fetch task from DB
		task, err := store.GetTask(getContext(), spawnTask)
		if err != nil {
			return fmt.Errorf("fetching task: %w", err)
		}
		if task == nil {
			return fmt.Errorf("task %q not found", spawnTask)
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
		SkipPermissions: spawnSkipPermissions,
		TrustWorkspace:  spawnTrustWorkspace,
		SpawnedBy:       os.Getenv("AGENT_NAME"),
	}

	agent, err := agentService.Spawn(getContext(), opts)
	if errors.Is(err, backend.ErrNotFound) {
		return withBackendConfigErr(err)
	}
	if err != nil {
		return err
	}

	fmt.Printf("Spawned agent %q (session: %s, backend: %s)\n",
		agent.Name, agent.SessionName, agent.Backend)

	return nil
}

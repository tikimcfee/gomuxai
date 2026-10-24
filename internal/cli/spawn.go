package cli

import (
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var (
	spawnSkipPermissions bool
	spawnTrustWorkspace  bool
	spawnTask            string
)

var spawnCmd = &cobra.Command{
	Use:   "spawn <name> [task-file]",
	Short: "Start a new agent in its own tmux session",
	Long: `Spawn creates a new agent in a tmux session, running the backend's CLI
(built in: claude, aider, agy).

Initial work, if given, is sent as the agent's first message:
  - --task <name>  : the task's content, from the task repository (DB)
  - [task-file]    : a request to read that file (claude/agy: "Please read and
                     execute the task in <file>", aider: "/read <file>")
Without either, send the first message yourself with "agent send".

When an agent runs spawn (AGENT_NAME set), the new agent is recorded as its
helper: "agent list" shows it under PARENT, and only that agent (or its own
parent, and so on up) or a person can clean it up. It starts in the
directory spawn runs from, so it gets that project's permission settings
(for Claude, its .claude/ settings on top of the user's); --skip-permissions
bypasses them.

Spawn returns once the CLI has had its startup time, and says the status it
found the agent in: "idle" means it is ready for a message.

A custom backend (--backend-config <file>) is only known through that file:
pass it again to status, wait, and watch for that agent.`,
	Example: `  agent spawn researcher --backend claude
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
		task, err := getTask(spawnTask)
		if err != nil {
			return err
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

	fmt.Print(spawnSummary(agent, spawnTask, taskFile))
	if info, err := agentService.StatusInfo(getContext(), name); err == nil {
		fmt.Println(spawnReadiness(name, info.Status))
	}
	return nil
}

// spawnReadiness says what the status spawn found the new agent in means
// for sending to it.
func spawnReadiness(name string, status service.AgentStatus) string {
	switch status {
	case service.AgentStatusIdle:
		return "It is idle: ready for a message."
	case service.AgentStatusProcessing:
		return "It is processing (still starting, or working on its task)."
	case service.AgentStatusWaiting:
		return fmt.Sprintf("It is already waiting on a prompt: agent status %s -v shows it.", name)
	case service.AgentStatusExited:
		return fmt.Sprintf("It has already exited: agent logs %s shows why.", name)
	}
	return fmt.Sprintf("Its status is %s so far: agent wait %s --for idle --timeout 1m waits until it's ready.", status, name)
}

// spawnSummary says what spawn did: the new agent, its parent if an agent
// spawned it, and what it was sent first.
func spawnSummary(agent *storage.Agent, task, taskFile string) string {
	var b strings.Builder
	fmt.Fprintf(&b, "Spawned agent %s (session %s, backend %s)", agent.Name, agent.SessionName, agent.Backend)
	if agent.SpawnedBy != "" {
		fmt.Fprintf(&b, ", a helper of %s", agent.SpawnedBy)
	}
	b.WriteString(".\n")
	switch {
	case task != "":
		fmt.Fprintf(&b, "Sent it task %s.\n", task)
	case taskFile != "":
		fmt.Fprintf(&b, "Asked it to read %s.\n", taskFile)
	default:
		fmt.Fprintf(&b, "It has no task yet: agent send %s \"...\"\n", agent.Name)
	}
	return b.String()
}

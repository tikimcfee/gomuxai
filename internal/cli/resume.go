package cli

import (
	"fmt"
	"os"
	"slices"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var (
	resumeSkipPermissions bool
	resumeTrustWorkspace  bool
	resumeAttach          bool
)

var resumeCmd = &cobra.Command{
	Use:   "resume <name>",
	Short: "Start an ended agent again, continuing its last session",
	Long: `Resume starts an agent whose session has ended again, on that same
session: after a reboot, a killed tmux server, or a CLI that quit. It keeps
the agent's name, backend and parent (so its helpers stay its helpers),
its own state keys and its last _result, and delivers the messages held
for it when it ended. It starts in the directory the session ran in.

Only an ended claude or agy agent can be resumed ("agent list" shows
"ended (...)"). A failed resume leaves it ended, to resume again or clean
up.

An adopted agent (agent adopt) is refused: its tmux session is your own,
so resume neither types a launch into your shell nor creates a session to
replace it. Start the CLI there yourself (claude -c continues its last
conversation) and adopt keeps tracking it. If that tmux session is gone,
agent cleanup <name>, then agent spawn <name> --continue (or --resume
<session>) starts it in a session of gomuxai's own.

Inside an agent (AGENT_NAME set), you can resume only your own helpers and
theirs.

To continue a session that no agent owns (one you ran in a terminal, or a
cleaned-up agent's), use agent spawn <name> --resume <session>.`,
	Example: `  agent resume lead
  agent resume lead --attach`,
	Args: cobra.ExactArgs(1),
	RunE: runResume,
}

func init() {
	resumeCmd.Flags().BoolVar(&resumeSkipPermissions, "skip-permissions", false,
		"Pass --dangerously-skip-permissions to Claude or Agy (use with caution)")
	resumeCmd.Flags().BoolVar(&resumeTrustWorkspace, "trust-workspace", false,
		"Confirm the CLI's workspace trust prompt on launch (use with caution)")
	resumeCmd.Flags().BoolVar(&resumeAttach, "attach", false,
		"Open the agent's tmux session in this terminal once it starts")
}

func runResume(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	name := args[0]
	agent, err := agentService.Agent(ctx, name)
	if err != nil {
		return err
	}
	if caller := nonInteractiveCallerAgent(); caller != "" {
		list, err := agentService.List(ctx)
		if err != nil {
			return err
		}
		if !slices.Contains(newTeams(list).ancestors(name), caller) {
			by := "a person"
			if agent.SpawnedBy != "" {
				by = fmt.Sprintf("%q", agent.SpawnedBy)
			}
			return fmt.Errorf("can't resume %s: it was spawned by %s, not by you (%q) or your helpers", name, by, caller)
		}
	}

	if agent.Adopted {
		// Its tmux session is the person's: nothing is typed or created.
		return agentService.AdoptedResumeError(agent)
	}

	t, err := resolveTranscript(ctx, name)
	if err != nil {
		return fmt.Errorf("finding agent %s's last session: %w", name, err)
	}
	s := t.session
	if err := checkResumable(s, agent.Backend); err != nil {
		return err
	}
	// The agent starts where the session ran: its tmux session opens in
	// this directory, and the CLI finds the session from there.
	if err := os.Chdir(s.Cwd); err != nil {
		return fmt.Errorf("entering %s, where session %s ran: %w", s.Cwd, s.SessionID, err)
	}

	resumed, err := agentService.Resume(ctx, service.ResumeOptions{
		Name:            name,
		SessionID:       s.SessionID,
		SkipPermissions: resumeSkipPermissions,
		TrustWorkspace:  resumeTrustWorkspace,
	})
	if err != nil {
		return err
	}

	fmt.Printf("Resumed agent %s (session %s, backend %s) on session %s, in %s.\n",
		resumed.Name, resumed.SessionName, resumed.Backend, s.SessionID, s.Cwd)
	if n := agentService.HeldCount(ctx, name); n > 0 {
		fmt.Printf("%d held message(s) go to it once it is at its prompt.\n", n)
	}
	if info, err := agentService.StatusInfo(ctx, name); err == nil {
		fmt.Println(spawnReadiness(name, resumed.Backend, info.Status, info.Detail, readiness{}))
	}
	if resumeAttach {
		return attachSession(resumed.SessionName)
	}
	return nil
}

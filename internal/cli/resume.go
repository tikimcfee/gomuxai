package cli

import (
	"context"
	"fmt"
	"slices"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
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
cleaned-up agent's), use agent spawn <name> --resume <session>.

agent resume name@host resumes an agent on another host through the agent
bus: that host runs its own resume, so the refusals are its own (not
claude or agy, no session, not ended, adopted). Its bus must list resume
in --remote-ops: resume is off for other hosts by default, since it starts
a process as that host's user. Inside an agent, the other host lets you
resume only your own helpers there. --skip-permissions and
--trust-workspace are refused for another host's agent, as for agent spawn
name@host: run agent resume on that host for those. --attach isn't
supported for another host's agent yet.`,
	Example: `  agent resume lead
  agent resume lead --attach
  agent resume lead@hub`,
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
	ref, remote, err := parseTarget(args[0])
	if err != nil {
		return err
	}
	if remote {
		return runRemoteResume(ref)
	}
	ctx := getContext()
	resp, err := resumeAgent(ctx, ref.Name, nonInteractiveCallerAgent(), resumeSkipPermissions, resumeTrustWorkspace)
	if err != nil {
		return err
	}
	printResumed(ref.Name, resp)
	if resumeAttach {
		return attachSession(resp.Session)
	}
	return nil
}

// printResumed says what a resume did, for the agent named name (name@host
// for another host's).
func printResumed(name string, resp *natsbus.ResumeResponse) {
	fmt.Printf("Resumed agent %s (session %s, backend %s) on session %s, in %s.\n",
		name, resp.Session, resp.Backend, resp.SessionID, resp.Cwd)
	if resp.Held > 0 {
		fmt.Printf("%d held message(s) go to it once it is at its prompt.\n", resp.Held)
	}
	if resp.Status != "" {
		fmt.Println(spawnReadiness(name, service.AgentStatus(resp.Status)))
	}
}

// resumeAgent resumes the agent name on this host, as agent resume does,
// for agent resume and the bus's resume op. caller is the agent asking:
// "" for a person, a name here, or name@host for another host's agent,
// which may resume only its own helpers and theirs.
func resumeAgent(ctx context.Context, name, caller string, skipPermissions, trustWorkspace bool) (*natsbus.ResumeResponse, error) {
	agent, err := agentService.Agent(ctx, name)
	if err != nil {
		return nil, err
	}
	if caller != "" {
		list, err := agentService.List(ctx)
		if err != nil {
			return nil, err
		}
		if !slices.Contains(newTeams(list).ancestors(name), caller) {
			by := "a person"
			if agent.SpawnedBy != "" {
				by = fmt.Sprintf("%q", agent.SpawnedBy)
			}
			return nil, fmt.Errorf("can't resume %s: it was spawned by %s, not by you (%q) or your helpers", name, by, caller)
		}
	}

	if agent.Adopted {
		// Its tmux session is the person's: nothing is typed or created.
		return nil, agentService.AdoptedResumeError(agent)
	}

	t, err := resolveTranscript(ctx, name)
	if err != nil {
		return nil, fmt.Errorf("finding agent %s's last session: %w", name, err)
	}
	s := t.session
	if err := checkResumable(s, agent.Backend); err != nil {
		return nil, err
	}

	// The agent starts where the session ran: its tmux session opens in
	// that directory, and the CLI finds the session from there.
	resumed, err := agentService.Resume(ctx, service.ResumeOptions{
		Name:            name,
		SessionID:       s.SessionID,
		Cwd:             s.Cwd,
		SkipPermissions: skipPermissions,
		TrustWorkspace:  trustWorkspace,
	})
	if err != nil {
		return nil, err
	}
	resp := &natsbus.ResumeResponse{
		Name: resumed.Name, Session: resumed.SessionName, Backend: resumed.Backend,
		SessionID: s.SessionID, Cwd: s.Cwd, Held: agentService.HeldCount(ctx, name),
	}
	if info, err := agentService.StatusInfo(ctx, name); err == nil {
		resp.Status, resp.EndReason = string(info.Status), string(info.EndReason)
	}
	return resp, nil
}

package cli

import (
	"context"
	"fmt"
	"slices"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var (
	resumeSkipPermissions bool
	resumeTrustWorkspace  bool
	resumeAttach          bool
	resumeAll             bool
	resumeDryRun          bool
)

var resumeCmd = &cobra.Command{
	Use:   "resume <name> | --all [--dry-run]",
	Short: "Start an ended agent again, continuing its last session; --all: every one a reboot ended",
	Long: `Resume starts an agent whose session has ended again, on that same
session: after a reboot, a killed tmux server, or a CLI that quit. It keeps
the agent's name, backend and parent (so its helpers stay its helpers),
the model spawn --model started it on, its own state keys and its last
_result, and delivers the messages held
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

--all resumes every agent on this host that a reboot or a killed tmux
server ended: status ended (gone), as agent status resolves it, and
resumable (claude or agy, not adopted, with a session). Agents that ended
any other way (quit, finished, closed, replaced, parent) were ended on
purpose, so --all leaves them, listing each as skipped with how it ended;
agent resume <name> still resumes one. Gone agents it can't resume (aider,
adopted, no session) are listed as skipped with why. Parents come back
before their helpers: one depth of the agent tree at a time, up to 4 at
once within it, each launch half a second after the one before. It prints
one line per ended agent (resumed, skipped, failed), parents first and
then by name, then a total, and exits 1 if any resume failed. --dry-run
says what it would resume (and checks each session as resume would) and
skip, and why, and changes nothing. Inside an agent, --all covers only
your own helpers and theirs. --skip-permissions and --trust-workspace
apply to every agent it resumes; --attach, which opens one session, is
refused. --all is this host's: run agent resume --all on another host to
resume its agents.

To continue a session that no agent owns (one you ran in a terminal, or a
cleaned-up agent's), use agent spawn <name> --resume <session>.

agent resume name@host resumes an agent on another host through the agent
bus: that host runs its own resume, so the refusals are its own (not
claude or agy, no session, not ended, adopted). Its bus must list resume
in --remote-ops: resume is off for other hosts by default, since it starts
a process as that host's user. Inside an agent, the other host lets you
resume only your own helpers there. --skip-permissions and
--trust-workspace are refused for another host's agent, as for agent spawn
name@host: run agent resume on that host for those.

agent resume name@host --attach runs ssh to that host instead, and its own
agent resume <name> --attach there, as agent attach name@host does (see
agent help attach): as you on that host, not through its resume op, and
it needs ssh to work from here.`,
	Example: `  agent resume lead
  agent resume --all --dry-run   # after a reboot: what --all would resume and skip
  agent resume --all
  agent resume lead --attach
  agent resume lead@hub
  agent resume lead@hub --attach`,
	Args: resumeArgs,
	RunE: runResume,
}

func init() {
	resumeCmd.Flags().BoolVar(&resumeSkipPermissions, "skip-permissions", false,
		"Pass --dangerously-skip-permissions to Claude or Agy (use with caution)")
	resumeCmd.Flags().BoolVar(&resumeTrustWorkspace, "trust-workspace", false,
		"Confirm the CLI's workspace trust prompt on launch (use with caution)")
	resumeCmd.Flags().BoolVar(&resumeAttach, "attach", false,
		"Open the agent's tmux session in this terminal once it starts")
	resumeCmd.Flags().BoolVar(&resumeAll, "all", false,
		"Resume every agent here that a reboot or killed tmux server ended (yours only, inside an agent)")
	resumeCmd.Flags().BoolVar(&resumeDryRun, "dry-run", false,
		"With --all: list what it would resume and skip, and why, changing nothing")
}

// resumeArgs checks resume's arguments: one name, or --all and none.
func resumeArgs(cmd *cobra.Command, args []string) error {
	if !resumeAll {
		if resumeDryRun {
			return fmt.Errorf("--dry-run goes with --all: agent resume --all --dry-run")
		}
		return cobra.ExactArgs(1)(cmd, args)
	}
	if len(args) > 0 {
		// Checked by shape: arguments are checked before the config that
		// names this host is loaded.
		if _, host, remote := strings.Cut(args[0], "@"); remote {
			return fmt.Errorf("resume --all resumes this host's agents: run agent resume --all on %s to resume its own, or agent resume %s for that one", host, args[0])
		}
		return fmt.Errorf("--all picks the agents itself: name one agent, or pass --all, not both")
	}
	if resumeAttach {
		return fmt.Errorf("--attach opens one agent's session, and --all resumes several: agent attach <name> opens one once they're back")
	}
	return nil
}

func runResume(cmd *cobra.Command, args []string) error {
	if resumeAll {
		return runResumeAll(getContext(), resumeDryRun, nonInteractiveCallerAgent(), resumeSkipPermissions, resumeTrustWorkspace)
	}
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
		fmt.Println(spawnReadiness(name, resp.Backend, service.AgentStatus(resp.Status), resp.Detail, readiness{}))
	}
}

// resumeSession finds the session resume would continue agent on, and
// checks that it can.
func resumeSession(ctx context.Context, agent *storage.Agent) (discovery.Session, error) {
	t, err := resolveTranscript(ctx, agent.Name)
	if err != nil {
		return discovery.Session{}, fmt.Errorf("finding agent %s's last session: %w", agent.Name, err)
	}
	s := t.session
	if s.Cwd == "" && agent.Cwd != "" {
		// agy ties a conversation to its workspace in history.jsonl only
		// in some lines (its prompts' carry no conversation id), so a
		// conversation can have none recorded yet. The agent's record has
		// where its session ran: where its tmux session started.
		s.Cwd = agent.Cwd
	}
	if err := checkResumable(s, agent.Backend); err != nil {
		return discovery.Session{}, err
	}
	return s, nil
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

	s, err := resumeSession(ctx, agent)
	if err != nil {
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
		resp.Status, resp.EndReason, resp.Detail = string(info.Status), string(info.EndReason), info.Detail
	}
	return resp, nil
}

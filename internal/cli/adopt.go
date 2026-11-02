package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

var (
	adoptSession         string
	adoptResume          string
	adoptContinue        bool
	adoptSkipPermissions bool
	adoptTrustWorkspace  bool
	adoptNoAttach        bool
)

var adoptCmd = &cobra.Command{
	Use:   "adopt <name>",
	Short: "Create a bus entry for an existing running terminal or tmux session",
	Long: `Adopt gives an existing terminal or tmux session a seat on the agent bus under
<name>, so other agents and hosts can reach it with agent send, status, capture,
and wait.

When an agent CLI is already up in your terminal, pause it with Ctrl-Z first,
run "agent adopt <name>", and adopt brings it back up on the bus:
  - Inside tmux (or targeting another session with --session <tmux-session>):
    - A paused (Ctrl-Z) Claude, aider, or shell job is registered in place and
      resumed in the foreground (fg); Claude's global hook reports immediately.
    - A paused (Ctrl-Z) unhooked agy session is cleanly closed and relaunched in
      place on the same conversation with gomuxai's hooks and instructions.
    - A live process in --session <tmux-session> (or an empty shell) is adopted
      in place without interrupting it.
  - Outside tmux (a plain terminal window):
    - A paused (Ctrl-Z) Claude or agy session is cleanly closed, resumed in a
      new tmux session (agent-<name>) with full hooks, and attached here
      (--no-attach skips attaching).
    - With no paused AI CLI, a new tmux session (agent-<name>) is created in
      this directory and attached here.

If you exited the CLI instead of pausing with Ctrl-Z, pass -c / --continue (for
the most recent ended Claude or agy session in the directory) or --resume <id>.

Cleaning up an adopted tmux session ("agent cleanup <name>") unregisters it from
the bus and stops log piping while leaving the pre-existing tmux session running.`,
	Example: `  agent adopt helper
  agent adopt helper --session dev
  agent adopt helper --continue
  agent adopt helper --resume 3f34adff --backend claude
  agent adopt scratch --backend shell`,
	Args: cobra.ExactArgs(1),
	RunE: runAdopt,
}

func init() {
	adoptCmd.Flags().StringVar(&adoptSession, "session", "",
		"Existing tmux session name to adopt (defaults to current tmux session, or this terminal)")
	adoptCmd.Flags().StringVar(&adoptResume, "resume", "",
		"Continue this Claude or agy session (an id, the start of one, or a transcript's path)")
	adoptCmd.Flags().BoolVarP(&adoptContinue, "continue", "c", false,
		"Continue the most recent ended Claude or agy session in the target directory")
	adoptCmd.Flags().BoolVar(&adoptSkipPermissions, "skip-permissions", false,
		"Pass --dangerously-skip-permissions when launching Claude or Agy (use with caution)")
	adoptCmd.Flags().BoolVar(&adoptTrustWorkspace, "trust-workspace", false,
		"Confirm the CLI's workspace trust prompt when launching (use with caution)")
	adoptCmd.Flags().BoolVar(&adoptNoAttach, "no-attach", false,
		"Don't attach this terminal when adopt creates a new tmux session outside tmux")
}

func runAdopt(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	name := args[0]

	if adoptResume != "" && adoptContinue {
		return errors.New("pass either --resume or --continue, not both")
	}

	// Only pass an explicit --backend (or --backend-config) override; ignore any
	// default backend from an app config unless the user asked for one, so
	// adopt auto-detects what is actually running in the session.
	explicitBackend := backendName
	if explicitBackend == "" && backendConfig != "" {
		explicitBackend = cfg.Backend
	}

	currentPane := os.Getenv("TMUX_PANE")
	targetCwd, _ := os.Getwd()
	if c, ok := tmuxClient.(*tmux.Client); ok {
		sess := adoptSession
		if sess == "" && currentPane != "" {
			sess, _ = c.SessionOfPane(currentPane)
		}
		if sess != "" {
			if p, err := c.FirstPane(sess); err == nil && p.Cwd != "" {
				targetCwd = p.Cwd
			}
		}
	}

	var (
		resumeID            string
		resumeHarness       string
		resumeCwd           string
		resumeMustBeStopped bool
	)
	if adoptResume != "" {
		s, err := adoptResumableSession(ctx, adoptResume, explicitBackend)
		if err != nil {
			return err
		}
		resumeID = s.SessionID
		resumeHarness = s.Harness
		resumeCwd = s.Cwd
		resumeMustBeStopped = s.Status != discovery.StatusEnded
	} else if adoptContinue {
		s, err := latestUnownedSessionInDir(ctx, targetCwd, explicitBackend)
		if err != nil {
			return err
		}
		resumeID = s.SessionID
		resumeHarness = s.Harness
		resumeCwd = s.Cwd
	}

	// If adopting outside tmux and resuming a session in another directory,
	// enter that directory so the new tmux session opens there.
	if adoptSession == "" && currentPane == "" && resumeCwd != "" {
		if err := os.Chdir(resumeCwd); err != nil {
			return fmt.Errorf("entering %s, where session %s ran: %w", resumeCwd, resumeID, err)
		}
	}

	res, err := agentService.Adopt(ctx, service.AdoptOptions{
		Name:                name,
		SessionName:         adoptSession,
		CurrentPane:         currentPane,
		CallerShellPID:      os.Getppid(),
		SelfPID:             os.Getpid(),
		Backend:             explicitBackend,
		Resume:              resumeID,
		ResumeHarness:       resumeHarness,
		ResumeCwd:           resumeCwd,
		ResumeMustBeStopped: resumeMustBeStopped,
		SkipPermissions:     adoptSkipPermissions,
		TrustWorkspace:      adoptTrustWorkspace,
		SpawnedBy:           nonInteractiveCallerAgent(),
	})
	if errors.Is(err, backend.ErrNotFound) {
		return withBackendConfigErr(err)
	}
	if err != nil {
		return err
	}

	fmt.Print(adoptSummary(res))

	if res.SendCommandOnExit != "" {
		if err := agentService.SendDeferredCommand(res.Agent.SessionName, res.SendCommandOnExit); err != nil {
			return fmt.Errorf("resuming in %s: %w", res.Agent.SessionName, err)
		}
	} else if res.SendFGOnExit {
		if err := agentService.ResumeStoppedJob(res.Agent.SessionName); err != nil {
			return fmt.Errorf("resuming stopped process in %s: %w", res.Agent.SessionName, err)
		}
	}

	if res.CreatedTmux && !adoptNoAttach && stdinIsTerminal() {
		return attachSession(res.Agent.SessionName)
	}
	return nil
}

func adoptResumableSession(ctx context.Context, arg, explicitBackend string) (discovery.Session, error) {
	t, err := resolveTranscript(ctx, arg)
	if err != nil {
		return discovery.Session{}, err
	}
	s := t.session
	switch {
	case s.Harness != discovery.HarnessClaude && s.Harness != discovery.HarnessAgy:
		return discovery.Session{}, fmt.Errorf("session %s is a %s session: --resume continues claude and agy sessions", s.SessionID, s.Harness)
	case explicitBackend != "" && explicitBackend != s.Harness:
		return discovery.Session{}, fmt.Errorf("session %s is a %s session: adopt it with --backend %s", s.SessionID, s.Harness, s.Harness)
	case s.ParentSession != nil:
		return discovery.Session{}, fmt.Errorf("session %s is a subagent's: resume the session that started it, %s", s.SessionID, s.ParentSession.SessionID)
	case s.Workgroup != nil:
		return discovery.Session{}, fmt.Errorf("session %s is already owned by agent %s", s.SessionID, s.Workgroup.Name)
	case s.Cwd == "":
		return discovery.Session{}, fmt.Errorf("session %s recorded no working directory to resume it in", s.SessionID)
	}
	if info, err := os.Stat(s.Cwd); err != nil || !info.IsDir() {
		return discovery.Session{}, fmt.Errorf("session %s ran in %s, which is no longer a directory", s.SessionID, s.Cwd)
	}
	return s, nil
}

func latestUnownedSessionInDir(ctx context.Context, dir, explicitBackend string) (discovery.Session, error) {
	if explicitBackend != "" && explicitBackend != discovery.HarnessClaude && explicitBackend != discovery.HarnessAgy {
		return discovery.Session{}, fmt.Errorf("--continue continues a claude or agy session, not %s", explicitBackend)
	}
	cleanDir := filepath.Clean(dir)
	if eval, err := filepath.EvalSymlinks(cleanDir); err == nil {
		cleanDir = eval
	}
	agents, err := workgroupAgents(ctx)
	if err != nil {
		return discovery.Session{}, err
	}
	live, _ := liveNow(ctx)
	sessions := discovery.Resolve(discovery.Scan(sessionDirs()), agents, live)
	var best *discovery.Session
	for i := range sessions {
		s := &sessions[i]
		if s.Harness != discovery.HarnessClaude && s.Harness != discovery.HarnessAgy {
			continue
		}
		if explicitBackend != "" && s.Harness != explicitBackend {
			continue
		}
		if s.ParentSession != nil || s.Workgroup != nil || s.Status != discovery.StatusEnded {
			continue
		}
		sDir := filepath.Clean(s.Cwd)
		if eval, err := filepath.EvalSymlinks(sDir); err == nil {
			sDir = eval
		}
		if sDir != cleanDir {
			continue
		}
		if best == nil || s.Mtime.After(best.Mtime) {
			best = s
		}
	}
	if best == nil {
		return discovery.Session{}, fmt.Errorf("no ended Claude or agy session found in %s to continue (pass --resume <session> to name one)", dir)
	}
	return *best, nil
}

func adoptSummary(res *service.AdoptResult) string {
	var b strings.Builder
	a := res.Agent
	switch res.Action {
	case service.AdoptActionSpawned:
		fmt.Fprintf(&b, "Adopted agent %s in new tmux session %s (backend %s).\n", a.Name, a.SessionName, a.Backend)
	default:
		fmt.Fprintf(&b, "Adopted agent %s (tmux session %s, backend %s).\n", a.Name, a.SessionName, a.Backend)
	}
	switch res.Action {
	case service.AdoptActionResumed:
		if res.SessionID != "" {
			fmt.Fprintf(&b, "Resuming paused %s session %s in the foreground.\n", a.Backend, res.SessionID)
		} else if res.ProcessCmd != "" {
			fmt.Fprintf(&b, "Resuming paused %s in the foreground.\n", res.ProcessCmd)
		}
	case service.AdoptActionUpgraded:
		fmt.Fprintf(&b, "Relaunching paused %s conversation %s with gomuxai hooks.\n", a.Backend, res.SessionID)
	case service.AdoptActionLaunched:
		if res.SessionID != "" {
			fmt.Fprintf(&b, "Launching %s continuing session %s.\n", a.Backend, res.SessionID)
		} else {
			fmt.Fprintf(&b, "Launching %s in session %s.\n", a.Backend, a.SessionName)
		}
	case service.AdoptActionSpawned:
		if res.SessionID != "" {
			fmt.Fprintf(&b, "Continuing %s session %s.\n", a.Backend, res.SessionID)
		}
	case service.AdoptActionLive:
		if res.SessionID != "" {
			fmt.Fprintf(&b, "Tracking running %s session %s.\n", a.Backend, res.SessionID)
		}
	}
	if a.Adopted {
		fmt.Fprintf(&b, "agent cleanup %s unregisters it without killing tmux session %s.\n", a.Name, a.SessionName)
	}
	return b.String()
}

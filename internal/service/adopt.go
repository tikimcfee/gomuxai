package service

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// AdoptAction describes how Adopt registered or started the agent.
type AdoptAction string

const (
	// AdoptActionLive registered an existing tmux session with its running process or shell.
	AdoptActionLive AdoptAction = "live"
	// AdoptActionResumed registered an existing tmux session and resumed its stopped (Ctrl+Z) job via fg.
	AdoptActionResumed AdoptAction = "resumed"
	// AdoptActionUpgraded replaced a stopped (Ctrl+Z) unhooked CLI in an existing tmux session with a hooked launch on the same conversation.
	AdoptActionUpgraded AdoptAction = "upgraded"
	// AdoptActionLaunched launched a CLI into an existing bare-shell tmux session.
	AdoptActionLaunched AdoptAction = "launched"
	// AdoptActionSpawned created a new agent-<name> tmux session (when called outside tmux).
	AdoptActionSpawned AdoptAction = "spawned"
)

// AdoptOptions holds options for adopting a terminal or tmux session onto the bus.
type AdoptOptions struct {
	Name                string
	SessionName         string // explicit target tmux session (--session); "" uses CurrentPane or creates a new session outside tmux
	CurrentPane         string // caller's $TMUX_PANE (e.g. "%0"), "" when outside tmux
	CallerShellPID      int    // caller's parent shell PID (os.Getppid()) when outside tmux
	SelfPID             int    // current process PID (os.Getpid()), excluded when scanning descendants
	Backend             string // explicit --backend override; "" auto-detects
	Resume              string // resolved session/conversation ID to continue, if any
	ResumeHarness       string // harness of Resume ("claude" or "agy"), if resolved
	ResumeCwd           string // working directory of Resume, if resolved
	ResumeMustBeStopped bool   // true when Resume was not ended on disk and is only safe if held by the stopped process in the target pane/terminal
	SkipPermissions     bool
	TrustWorkspace      bool
	SpawnedBy           string
}

// AdoptResult describes the outcome of Adopt.
type AdoptResult struct {
	Agent             *storage.Agent
	Action            AdoptAction
	SessionID         string // detected or continued claude/agy session ID, if any
	ProcessCmd        string // detected foreground/stopped command name, if any
	SendFGOnExit      bool   // true when caller should send "fg" to Agent.SessionName after printing output
	SendCommandOnExit string // command line caller should send to Agent.SessionName after printing output ("fg" or launch script)
	CreatedTmux       bool   // true when Adopt created a new agent-<name> tmux session
}

type tmuxPaneInspector interface {
	FirstPane(sessionName string) (tmux.PaneInfo, error)
	SessionOfPane(pane string) (string, error)
}

// ResumeStoppedJob sends "fg" + Enter to sessionName so its shell brings a
// stopped job back to the foreground.
func (s *AgentService) ResumeStoppedJob(sessionName string) error {
	return s.sendLine(sessionName, "fg")
}

// SendDeferredCommand sends a deferred command line (such as "fg" or a launch
// script) to sessionName after the CLI has printed its output.
func (s *AgentService) SendDeferredCommand(sessionName, cmd string) error {
	return s.sendLine(sessionName, cmd)
}

// Adopt creates a bus entry for an existing tmux session or terminal,
// detecting any running or Ctrl+Z-stopped agent CLI and resuming it onto the bus.
func (s *AgentService) Adopt(ctx context.Context, opts AdoptOptions) (*AdoptResult, error) {
	if err := ValidateAgentName(opts.Name); err != nil {
		return nil, err
	}
	if err := s.checkAgentNameFree(ctx, opts.Name); err != nil {
		return nil, err
	}

	targetSession := strings.TrimSpace(opts.SessionName)
	if targetSession == "" && opts.CurrentPane != "" {
		sess, err := s.sessionOfPane(opts.CurrentPane)
		if err != nil {
			return nil, err
		}
		targetSession = sess
	}

	if targetSession != "" {
		return s.adoptTmuxSession(ctx, opts, targetSession)
	}
	return s.adoptOutsideTmux(ctx, opts)
}

func (s *AgentService) sessionOfPane(pane string) (string, error) {
	if pi, ok := s.tmux.(tmuxPaneInspector); ok {
		return pi.SessionOfPane(pane)
	}
	return "", nil
}

func (s *AgentService) firstPane(sessionName string) (tmux.PaneInfo, error) {
	if pi, ok := s.tmux.(tmuxPaneInspector); ok {
		return pi.FirstPane(sessionName)
	}
	pid, err := s.panePID(sessionName)
	if err != nil {
		return tmux.PaneInfo{}, err
	}
	cwd, _ := os.Getwd()
	return tmux.PaneInfo{PID: pid, Cwd: cwd}, nil
}

type detectedProcess struct {
	pid         int
	backendName string // "claude", "agy", "aider", or "shell"
	cmdName     string // e.g. "claude", "agy", "vim"
	sessionID   string // claude session ID or agy conversation ID
	hooked      bool   // for agy: launched with gomuxai's --add-dir
	stopped     bool   // ps stat starts with T/t (Ctrl+Z)
	paneSession *backend.PaneSession
}

func (d *detectedProcess) rank() int {
	switch {
	case d.stopped && d.backendName != "shell":
		return 4
	case !d.stopped && d.backendName != "shell":
		return 3
	case d.stopped:
		return 2
	default:
		return 1
	}
}

// inspectSubtree inspects processes descending from rootPID (excluding selfPID's branch).
func (s *AgentService) inspectSubtree(ctx context.Context, rootPID, selfPID int) (*detectedProcess, error) {
	if rootPID <= 0 {
		return nil, nil
	}
	procs, tree, err := util.ReadProcs(ctx)
	if err != nil {
		return nil, err
	}
	descendants := tree.DescendantsExcluding(rootPID, selfPID)
	descSet := make(map[int]bool, len(descendants))
	for _, pid := range descendants {
		descSet[pid] = true
	}

	// Check Claude's session registry via PaneSessionProvider first.
	if p, err := s.backends.CreateProvider("claude", backend.ProviderConfig{}); err == nil {
		if pp, ok := p.(backend.PaneSessionProvider); ok {
			if ps, found, _ := pp.PaneSession(ctx, rootPID); found && ps.ID != "" {
				if ps.PID == 0 || len(descSet) == 0 || descSet[ps.PID] {
					stopped := procs[ps.PID].Stopped()
					return &detectedProcess{
						pid:         ps.PID,
						backendName: "claude",
						cmdName:     "claude",
						sessionID:   ps.ID,
						stopped:     stopped,
						paneSession: &ps,
					}, nil
				}
			}
		}
	}

	var best *detectedProcess
	for _, pid := range descendants {
		info := procs[pid]
		if info.Zombie() {
			continue
		}
		args, ok := util.ProcArgs(pid)
		if !ok || len(args) == 0 {
			continue
		}
		beName, convID, hooked, cmdName := detectCLIInArgs(args)
		if cmdName == "" {
			continue
		}
		stopped := info.Stopped()
		// Ignore background shell helper daemons (e.g. gitstatusd, ssh-agent, sleep &)
		// that are neither stopped nor in the terminal's foreground process group.
		if beName == "shell" && !stopped && !info.Foreground() {
			continue
		}
		if beName == "agy" && convID == "" {
			var subPIDs []int
			subPIDs = append(subPIDs, pid)
			for _, d := range descendants {
				if _, under := tree.Depth(d, pid); under && d != pid {
					subPIDs = append(subPIDs, d)
				}
			}
			convID = agyOpenConversationID(ctx, subPIDs)
		}
		cand := &detectedProcess{
			pid:         pid,
			backendName: beName,
			cmdName:     cmdName,
			sessionID:   convID,
			hooked:      hooked,
			stopped:     stopped,
		}
		if best == nil || cand.rank() > best.rank() {
			best = cand
		}
	}
	return best, nil
}

var scriptRuntimes = map[string]bool{
	"node":    true,
	"nodejs":  true,
	"bun":     true,
	"deno":    true,
	"npx":     true,
	"python":  true,
	"python3": true,
	"sh":      true,
	"bash":    true,
	"zsh":     true,
}

// detectCLIInArgs classifies a process's command line from its executable
// (and script argument when invoked via a runtime like node or sh), without
// misclassifying file arguments passed to editors or grep.
func detectCLIInArgs(args []string) (backendName, sessionID string, hooked bool, cmdName string) {
	i := 0
	for i < len(args) {
		base := filepath.Base(args[i])
		if base == "env" {
			i++
			for i < len(args) {
				tok := args[i]
				if tok == "-u" && i+1 < len(args) {
					i += 2
					continue
				}
				if strings.HasPrefix(tok, "-") || strings.Contains(tok, "=") {
					i++
					continue
				}
				break
			}
			continue
		}
		if strings.Contains(args[i], "=") && !strings.HasPrefix(args[i], "-") && !strings.Contains(args[i], "/") {
			i++
			continue
		}
		break
	}
	if i >= len(args) {
		return "", "", false, ""
	}

	execTok := args[i]
	cmdName = filepath.Base(execTok)
	classifyToken := func(tok string) string {
		b := filepath.Base(tok)
		switch {
		case b == "agy" || b == "antigravity-cli" || strings.Contains(tok, "/antigravity-cli/"):
			return "agy"
		case b == "claude":
			return "claude"
		case b == "aider":
			return "aider"
		}
		return ""
	}

	backendName = classifyToken(execTok)
	argStart := i + 1
	if backendName == "" && scriptRuntimes[cmdName] {
		for j := i + 1; j < len(args); j++ {
			if strings.HasPrefix(args[j], "-") {
				continue
			}
			if be := classifyToken(args[j]); be != "" {
				backendName = be
				cmdName = be
				argStart = j + 1
			}
			break
		}
	}
	if backendName != "" {
		cmdName = backendName
	} else {
		backendName = "shell"
	}

	for j := argStart; j < len(args); j++ {
		arg := args[j]
		switch {
		case (arg == "--conversation" || arg == "--resume" || arg == "--session-id") && j+1 < len(args):
			sessionID = args[j+1]
			j++
		case strings.HasPrefix(arg, "--conversation="):
			sessionID = strings.TrimPrefix(arg, "--conversation=")
		case strings.HasPrefix(arg, "--resume="):
			sessionID = strings.TrimPrefix(arg, "--resume=")
		case strings.HasPrefix(arg, "--session-id="):
			sessionID = strings.TrimPrefix(arg, "--session-id=")
		case arg == "--add-dir" && j+1 < len(args):
			if strings.Contains(args[j+1], ".gomuxai/agents/") {
				hooked = true
			}
			j++
		case strings.HasPrefix(arg, "--add-dir="):
			if strings.Contains(arg, ".gomuxai/agents/") {
				hooked = true
			}
		}
	}
	return backendName, sessionID, hooked, cmdName
}

// agyOpenConversationID inspects open files of pids for a path matching
// .../presence/<conversation-id>.lock.
func agyOpenConversationID(ctx context.Context, pids []int) string {
	for _, p := range openFilesForPIDs(ctx, pids) {
		if id := extractAgyConversationFromLockPath(p); id != "" {
			return id
		}
	}
	return ""
}

func extractAgyConversationFromLockPath(path string) string {
	if !strings.HasSuffix(path, ".lock") {
		return ""
	}
	if filepath.Base(filepath.Dir(path)) != "presence" {
		return ""
	}
	id := strings.TrimSuffix(filepath.Base(path), ".lock")
	if id == "" {
		return ""
	}
	return id
}

// validateSpawnPreconditions verifies that spawning backendName will not fail
// before we terminate a user's Ctrl+Z-stopped process.
func (s *AgentService) validateSpawnPreconditions(ctx context.Context, opts AdoptOptions, backendName, existingSession string) error {
	provider, err := s.backends.CreateProvider(backendName, backend.ProviderConfig{
		SkipPermissions: opts.SkipPermissions,
		AgentDir:        s.cfg.AgentDir(opts.Name),
		Resume:          opts.Resume,
	})
	if err != nil {
		if _, bErr := s.backends.Get(backendName); bErr != nil {
			return bErr
		}
	} else if gh, ok := provider.(backend.GlobalHooker); ok && s.cfg.AgentBinary != "" {
		if err := gh.HooksInstalled(s.cfg.AgentBinary); err != nil {
			return err
		}
	}
	if existingSession != "" {
		exists, err := s.tmux.SessionExists(existingSession)
		if err != nil {
			return fmt.Errorf("checking session %q: %w", existingSession, err)
		}
		if !exists {
			return fmt.Errorf("tmux session %q not found", existingSession)
		}
		agents, err := s.storage.ListAgents(ctx)
		if err != nil {
			return fmt.Errorf("listing agents: %w", err)
		}
		for _, a := range agents {
			if a.SessionName == existingSession {
				return fmt.Errorf("tmux session %s is already registered as agent %s", existingSession, a.Name)
			}
		}
	} else {
		if exists, _ := s.tmux.SessionExists(s.cfg.SessionName(opts.Name)); exists {
			return fmt.Errorf("tmux session %s already exists, but no agent %s does (a leftover session?): pick another name, or agent session kill %s first",
				s.cfg.SessionName(opts.Name), opts.Name, s.cfg.SessionName(opts.Name))
		}
	}
	return nil
}

func (s *AgentService) adoptTmuxSession(ctx context.Context, opts AdoptOptions, sessionName string) (*AdoptResult, error) {
	pane, err := s.firstPane(sessionName)
	if err != nil {
		return nil, err
	}
	samePane := opts.CurrentPane != "" && pane.ID != "" && opts.CurrentPane == pane.ID
	if opts.SessionName == "" && opts.CurrentPane != "" && pane.ID != "" && opts.CurrentPane != pane.ID {
		return nil, fmt.Errorf("tmux session %s has multiple panes and agent targets its first pane (%s, not current pane %s): run adopt in the first pane or move this pane to its own session",
			sessionName, pane.ID, opts.CurrentPane)
	}
	if opts.SamePaneTrustUnsupported(samePane) {
		return nil, fmt.Errorf("--trust-workspace cannot confirm a prompt when adopting the current pane in place: run adopt from another terminal with --session %s, or answer the trust prompt directly", sessionName)
	}

	det, _ := s.inspectSubtree(ctx, pane.PID, opts.SelfPID)

	if opts.Resume != "" && opts.ResumeMustBeStopped {
		if det == nil || !det.stopped || (det.sessionID != "" && det.sessionID != opts.Resume) {
			return nil, fmt.Errorf("session %s is still running, not ended: pause it with Ctrl-Z in this pane or quit the CLI running it first", opts.Resume)
		}
	}

	// 1. If a stopped (Ctrl+Z) process is in the pane:
	if det != nil && det.stopped {
		if opts.Backend != "" && opts.Backend != "shell" && det.backendName != "shell" && opts.Backend != det.backendName {
			return nil, fmt.Errorf("tmux session %s has a stopped %s process (pid %d), not %s", sessionName, det.cmdName, det.pid, opts.Backend)
		}
		convID := det.sessionID
		if convID == "" && opts.Resume != "" && (opts.ResumeHarness == "" || opts.ResumeHarness == det.backendName) {
			convID = opts.Resume
		}
		det.sessionID = convID
		// Stopped unhooked agy with a known conversation ID: validate preconditions,
		// cleanly close the stopped process, and relaunch hooked agy --conversation <id> in place.
		if det.backendName == "agy" && !det.hooked && convID != "" && opts.Backend != "shell" && s.cfg.AgentBinary != "" {
			if err := s.validateSpawnPreconditions(ctx, opts, "agy", sessionName); err != nil {
				return nil, err
			}
			if err := terminateStoppedProcess(ctx, det.pid); err != nil {
				return nil, err
			}
			cwd := pane.Cwd
			if opts.ResumeCwd != "" {
				cwd = opts.ResumeCwd
			}
			var deferredCmd string
			agent, err := s.Spawn(ctx, SpawnOptions{
				Name:            opts.Name,
				Backend:         "agy",
				SkipPermissions: opts.SkipPermissions,
				TrustWorkspace:  opts.TrustWorkspace,
				SpawnedBy:       opts.SpawnedBy,
				Resume:          convID,
				existingSession: sessionName,
				sessionCwd:      cwd,
				samePane:        samePane,
				deferredCmd:     &deferredCmd,
			})
			if err != nil {
				return nil, err
			}
			return &AdoptResult{
				Agent:             agent,
				Action:            AdoptActionUpgraded,
				SessionID:         convID,
				ProcessCmd:        "agy",
				SendCommandOnExit: deferredCmd,
			}, nil
		}

		// Stopped claude, hooked agy, aider, or other CLI: register session and resume via fg.
		beName := opts.Backend
		if beName == "" {
			beName = det.backendName
		}
		agent, err := s.registerAdoptedInPlace(ctx, opts, sessionName, pane.Cwd, beName, det)
		if err != nil {
			return nil, err
		}
		deferredCmd := ""
		if samePane {
			deferredCmd = "fg"
		} else {
			if err := s.ResumeStoppedJob(sessionName); err != nil {
				return nil, fmt.Errorf("resuming stopped process in %s: %w", sessionName, err)
			}
		}
		return &AdoptResult{
			Agent:             agent,
			Action:            AdoptActionResumed,
			SessionID:         convID,
			ProcessCmd:        det.cmdName,
			SendFGOnExit:      samePane,
			SendCommandOnExit: deferredCmd,
		}, nil
	}

	// 2. If a live (non-stopped) process is running in the pane:
	if det != nil && !det.stopped {
		if opts.Resume != "" {
			return nil, fmt.Errorf("tmux session %s is already running %s (pid %d): quit or pause it before launching session %s",
				sessionName, det.cmdName, det.pid, opts.Resume)
		}
		if opts.Backend != "" && opts.Backend != "shell" && opts.Backend != det.backendName {
			return nil, fmt.Errorf("tmux session %s is running %s (pid %d), not %s: quit or pause it first", sessionName, det.cmdName, det.pid, opts.Backend)
		}
		beName := opts.Backend
		if beName == "" {
			beName = det.backendName
		}
		agent, err := s.registerAdoptedInPlace(ctx, opts, sessionName, pane.Cwd, beName, det)
		if err != nil {
			return nil, err
		}
		return &AdoptResult{
			Agent:      agent,
			Action:     AdoptActionLive,
			SessionID:  det.sessionID,
			ProcessCmd: det.cmdName,
		}, nil
	}

	// 3. Bare shell in the pane: if --resume / --continue or an explicit AI
	// backend (--backend claude|agy|aider) was requested, launch it in place.
	if opts.Resume != "" {
		beName := opts.Backend
		if beName == "" {
			beName = opts.ResumeHarness
		}
		if beName == "" {
			return nil, fmt.Errorf("backend is required to resume session %s", opts.Resume)
		}
		cwd := pane.Cwd
		if opts.ResumeCwd != "" {
			cwd = opts.ResumeCwd
		}
		var deferredCmd string
		agent, err := s.Spawn(ctx, SpawnOptions{
			Name:            opts.Name,
			Backend:         beName,
			SkipPermissions: opts.SkipPermissions,
			TrustWorkspace:  opts.TrustWorkspace,
			SpawnedBy:       opts.SpawnedBy,
			Resume:          opts.Resume,
			existingSession: sessionName,
			sessionCwd:      cwd,
			samePane:        samePane,
			deferredCmd:     &deferredCmd,
		})
		if err != nil {
			return nil, err
		}
		return &AdoptResult{
			Agent:             agent,
			Action:            AdoptActionLaunched,
			SessionID:         opts.Resume,
			ProcessCmd:        beName,
			SendCommandOnExit: deferredCmd,
		}, nil
	}

	if opts.Backend != "" && opts.Backend != "shell" {
		var deferredCmd string
		agent, err := s.Spawn(ctx, SpawnOptions{
			Name:            opts.Name,
			Backend:         opts.Backend,
			SkipPermissions: opts.SkipPermissions,
			TrustWorkspace:  opts.TrustWorkspace,
			SpawnedBy:       opts.SpawnedBy,
			existingSession: sessionName,
			sessionCwd:      pane.Cwd,
			samePane:        samePane,
			deferredCmd:     &deferredCmd,
		})
		if err != nil {
			return nil, err
		}
		return &AdoptResult{
			Agent:             agent,
			Action:            AdoptActionLaunched,
			ProcessCmd:        opts.Backend,
			SendCommandOnExit: deferredCmd,
		}, nil
	}

	// 4. Bare shell with no AI session to launch: adopt the shell session as-is.
	agent, err := s.registerAdoptedInPlace(ctx, opts, sessionName, pane.Cwd, "shell", nil)
	if err != nil {
		return nil, err
	}
	return &AdoptResult{
		Agent:  agent,
		Action: AdoptActionLive,
	}, nil
}

// SamePaneTrustUnsupported reports whether --trust-workspace was requested for
// an in-place launch inside the caller's own tmux pane.
func (opts AdoptOptions) SamePaneTrustUnsupported(samePane bool) bool {
	return samePane && opts.TrustWorkspace
}

func (s *AgentService) registerAdoptedInPlace(ctx context.Context, opts AdoptOptions, sessionName, cwd, backendName string, det *detectedProcess) (*storage.Agent, error) {
	if _, err := s.backends.Get(backendName); err != nil {
		return nil, err
	}
	if err := s.prepareExistingSession(ctx, opts.Name, sessionName); err != nil {
		return nil, err
	}
	agent, err := s.storeAgent(ctx, SpawnOptions{
		Name:            opts.Name,
		Backend:         backendName,
		SpawnedBy:       opts.SpawnedBy,
		existingSession: sessionName,
		sessionCwd:      cwd,
	}, sessionName)
	if err != nil {
		return nil, err
	}
	if det != nil && det.sessionID != "" {
		_ = s.setState(ctx, opts.Name, StateKeySessionID, det.sessionID)
	}
	if det != nil && det.paneSession != nil {
		ps := det.paneSession
		_ = s.Report(ctx, opts.Name, StatusReport{
			Status:    fromBackendStatus(ps.Status),
			Event:     EventAdopted,
			Detail:    fmt.Sprintf("adopted session %s (pid %d, %s)", ps.ID, ps.PID, ps.Detail),
			SessionID: ps.ID,
		})
	}
	return agent, nil
}

func (s *AgentService) adoptOutsideTmux(ctx context.Context, opts AdoptOptions) (*AdoptResult, error) {
	det, _ := s.inspectSubtree(ctx, opts.CallerShellPID, opts.SelfPID)
	if opts.Resume != "" && opts.ResumeMustBeStopped {
		if det == nil || !det.stopped || (det.sessionID != "" && det.sessionID != opts.Resume) {
			return nil, fmt.Errorf("session %s is still running, not ended: pause it with Ctrl-Z or quit the CLI running it first", opts.Resume)
		}
	}
	if det != nil && !det.stopped && det.backendName != "shell" {
		return nil, fmt.Errorf("%s (pid %d) is running in this terminal: pause it with Ctrl-Z first so agent adopt can move it into tmux", det.cmdName, det.pid)
	}
	if det != nil && det.stopped {
		if opts.Backend != "" && opts.Backend != "shell" && det.backendName != "shell" && opts.Backend != det.backendName {
			return nil, fmt.Errorf("this terminal has a stopped %s process (pid %d), not %s", det.cmdName, det.pid, opts.Backend)
		}
		sessionID := det.sessionID
		if sessionID == "" && opts.Resume != "" {
			sessionID = opts.Resume
		}
		if (det.backendName == "claude" || det.backendName == "agy") && sessionID != "" && opts.Backend != "shell" {
			if err := s.validateSpawnPreconditions(ctx, opts, det.backendName, ""); err != nil {
				return nil, err
			}
			if err := terminateStoppedProcess(ctx, det.pid); err != nil {
				return nil, err
			}
			agent, err := s.Spawn(ctx, SpawnOptions{
				Name:            opts.Name,
				Backend:         det.backendName,
				SkipPermissions: opts.SkipPermissions,
				TrustWorkspace:  opts.TrustWorkspace,
				SpawnedBy:       opts.SpawnedBy,
				Resume:          sessionID,
				sessionCwd:      opts.ResumeCwd,
			})
			if err != nil {
				return nil, err
			}
			return &AdoptResult{
				Agent:       agent,
				Action:      AdoptActionSpawned,
				SessionID:   sessionID,
				ProcessCmd:  det.backendName,
				CreatedTmux: true,
			}, nil
		}
		return nil, fmt.Errorf("process %d (%s) is stopped in this non-tmux terminal and cannot be moved into a new tmux session: resume it here with fg, or run inside tmux so agent adopt can resume it in place",
			det.pid, det.cmdName)
	}

	beName := opts.Backend
	if beName == "" {
		if opts.ResumeHarness != "" {
			beName = opts.ResumeHarness
		} else {
			beName = "shell"
		}
	}
	agent, err := s.Spawn(ctx, SpawnOptions{
		Name:            opts.Name,
		Backend:         beName,
		SkipPermissions: opts.SkipPermissions,
		TrustWorkspace:  opts.TrustWorkspace,
		SpawnedBy:       opts.SpawnedBy,
		Resume:          opts.Resume,
		sessionCwd:      opts.ResumeCwd,
	})
	if err != nil {
		return nil, err
	}
	return &AdoptResult{
		Agent:       agent,
		Action:      AdoptActionSpawned,
		SessionID:   opts.Resume,
		ProcessCmd:  beName,
		CreatedTmux: true,
	}, nil
}

func terminateStoppedProcess(ctx context.Context, pid int) error {
	if pid <= 0 {
		return nil
	}
	isGone := func() bool {
		if err := syscall.Kill(pid, 0); errors.Is(err, syscall.ESRCH) {
			return true
		}
		if procs, _, err := util.ReadProcs(ctx); err == nil {
			if info, ok := procs[pid]; !ok || info.Zombie() {
				return true
			}
		}
		return false
	}

	_ = syscall.Kill(pid, syscall.SIGTERM)
	_ = syscall.Kill(pid, syscall.SIGCONT)
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if isGone() {
			return nil
		}
		if err := sleepCtx(ctx, 100*time.Millisecond); err != nil {
			return err
		}
	}

	// Escalate to SIGKILL if SIGTERM was ignored.
	_ = syscall.Kill(pid, syscall.SIGKILL)
	_ = syscall.Kill(pid, syscall.SIGCONT)
	killDeadline := time.Now().Add(300 * time.Millisecond)
	for time.Now().Before(killDeadline) {
		if isGone() {
			return nil
		}
		if err := sleepCtx(ctx, 50*time.Millisecond); err != nil {
			return err
		}
	}
	if !isGone() {
		return fmt.Errorf("process %d did not exit after SIGTERM/SIGKILL", pid)
	}
	return nil
}

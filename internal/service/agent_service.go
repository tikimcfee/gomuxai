// Package service provides business logic for agent operations.
package service

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"github.com/google/uuid"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// AgentStatus represents the current state of an agent.
type AgentStatus string

const (
	AgentStatusIdle       AgentStatus = "idle"
	AgentStatusProcessing AgentStatus = "processing"
	AgentStatusWaiting    AgentStatus = "waiting" // blocked on a human, e.g. a permission prompt
	AgentStatusError      AgentStatus = "error"
	AgentStatusExited     AgentStatus = "exited"
	AgentStatusUnknown    AgentStatus = "unknown"
)

// CleanupError represents errors that occurred during cleanup.
// Cleanup continues through errors to maximize resource cleanup.
type CleanupError struct {
	Errors []error
}

func (e *CleanupError) Error() string {
	if len(e.Errors) == 0 {
		return "cleanup completed with no errors"
	}
	msgs := make([]string, len(e.Errors))
	for i, err := range e.Errors {
		msgs[i] = err.Error()
	}
	return fmt.Sprintf("cleanup errors: %s", strings.Join(msgs, "; "))
}

func (e *CleanupError) Add(err error) {
	if err != nil {
		e.Errors = append(e.Errors, err)
	}
}

func (e *CleanupError) HasErrors() bool {
	return len(e.Errors) > 0
}

// AgentService handles agent lifecycle operations.
type AgentService struct {
	cfg      *config.Config
	storage  storage.Storage
	tmux     tmux.TmuxOps
	backends *backend.Registry
	events   EventPublisher // nil: no bus
}

// NewAgentService creates a new agent service.
func NewAgentService(cfg *config.Config, store storage.Storage, tmuxClient tmux.TmuxOps, backends *backend.Registry) *AgentService {
	return &AgentService{
		cfg:      cfg,
		storage:  store,
		tmux:     tmuxClient,
		backends: backends,
	}
}

// SpawnOptions holds options for spawning an agent.
type SpawnOptions struct {
	Name            string
	Backend         string
	TaskFile        string // Path to task file (read from filesystem)
	TaskContent     string // Task content directly (from DB task repository)
	SkipPermissions bool   // Pass --dangerously-skip-permissions to Claude / Agy
	TrustWorkspace  bool   // Confirm the CLI's workspace trust prompt on launch
	SpawnedBy       string // agent that requested the spawn ("" for a user)
}

// agentNamePattern keeps names safe as tmux session names and as path
// components: cleanup removes .gomuxai/agents/<name>, so "../.." must not pass.
var agentNamePattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_-]*$`)

// ValidateAgentName reports whether name can be used for a new agent.
func ValidateAgentName(name string) error {
	if !agentNamePattern.MatchString(name) {
		return fmt.Errorf("invalid agent name %q: use letters, digits, '-' and '_', starting with a letter or digit", name)
	}
	return nil
}

// Spawn creates and starts a new agent.
func (s *AgentService) Spawn(ctx context.Context, opts SpawnOptions) (*storage.Agent, error) {
	if err := ValidateAgentName(opts.Name); err != nil {
		return nil, err
	}

	// A fresh provider, configured for this agent. Custom JSON backends
	// (--backend-config) have no factory, and spawn from their Backend.
	provider, err := s.backends.CreateProvider(opts.Backend, backend.ProviderConfig{
		SkipPermissions: opts.SkipPermissions,
		AgentDir:        s.cfg.AgentDir(opts.Name),
	})
	if err != nil {
		be, err := s.backends.Get(opts.Backend)
		if err != nil {
			return nil, err
		}
		return s.spawnWithBackend(ctx, opts, be)
	}
	return s.spawnWithProvider(ctx, opts, provider)
}

// spawnWithProvider spawns an agent on a built-in backend.
func (s *AgentService) spawnWithProvider(ctx context.Context, opts SpawnOptions, provider backend.Provider) (*storage.Agent, error) {
	sessionName, err := s.newSession(ctx, opts.Name)
	if err != nil {
		return nil, err
	}

	// Create the agent record before launching: backend hooks report status
	// against it as soon as the CLI starts.
	agent, err := s.storeAgent(ctx, opts, sessionName)
	if err != nil {
		return nil, err
	}
	abort := func(err error) (*storage.Agent, error) {
		s.tmux.KillSession(sessionName)
		s.storage.DeleteAgent(ctx, opts.Name)
		_ = os.RemoveAll(s.cfg.AgentDir(opts.Name))
		return nil, err
	}

	initMsg := initMessage(opts, provider.InitMessage)
	launchCmd, promptInLaunch, err := s.launchCommand(ctx, opts.Name, opts.SpawnedBy, provider, initMsg)
	if err != nil {
		return abort(err)
	}
	runCmd, err := s.writeLaunchScript(opts.Name, launchCmd)
	if err != nil {
		return abort(err)
	}
	if err := s.sendLine(sessionName, runCmd); err != nil {
		return abort(fmt.Errorf("launching backend: %w", err))
	}
	if err := s.waitForStartup(ctx); err != nil {
		return abort(err)
	}

	// Confirm the workspace trust prompt only when opted in via --trust-workspace
	// (trusting a repo also enables its own hooks and settings).
	if tp, ok := provider.(backend.TrustPrompter); ok && opts.TrustWorkspace {
		if err := s.confirmTrust(ctx, sessionName, tp); err != nil {
			return abort(err)
		}
		if err := s.waitForStartup(ctx); err != nil {
			return abort(err)
		}
	}

	// Send initial message if it was not already passed on the launch command line
	if !promptInLaunch && initMsg != "" {
		if err := s.sendLine(sessionName, initMsg); err != nil {
			return abort(fmt.Errorf("sending init message: %w", err))
		}
	}

	return agent, nil
}

// spawnWithBackend spawns an agent on a custom JSON backend. It only knows a
// launch command, typed into the session as is: no hooks, briefing, or
// launch script.
func (s *AgentService) spawnWithBackend(ctx context.Context, opts SpawnOptions, be backend.Backend) (*storage.Agent, error) {
	sessionName, err := s.newSession(ctx, opts.Name)
	if err != nil {
		return nil, err
	}
	abort := func(err error) (*storage.Agent, error) {
		s.tmux.KillSession(sessionName)
		return nil, err
	}

	if err := s.sendLine(sessionName, be.LaunchCommand()); err != nil {
		return abort(fmt.Errorf("launching backend: %w", err))
	}
	if err := s.waitForStartup(ctx); err != nil {
		return abort(err)
	}
	if initMsg := initMessage(opts, be.InitMessage); initMsg != "" {
		if err := s.sendLine(sessionName, initMsg); err != nil {
			return abort(fmt.Errorf("sending init message: %w", err))
		}
	}

	return s.storeAgent(ctx, opts, sessionName)
}

// newSession creates a new agent's tmux session, once the name is known to
// be free, and pipes its output to the agent's log.
func (s *AgentService) newSession(ctx context.Context, name string) (string, error) {
	existing, err := s.storage.GetAgent(ctx, name)
	if err != nil {
		return "", fmt.Errorf("checking existing agent: %w", err)
	}
	if existing != nil {
		return "", fmt.Errorf("agent %s already exists (session %s, created %s): pick another name, or agent cleanup %s first",
			name, existing.SessionName, existing.CreatedAt.Format("2006-01-02 15:04"), name)
	}
	if exists, _ := s.tmux.SessionExists(s.cfg.SessionName(name)); exists {
		return "", fmt.Errorf("tmux session %s already exists, but no agent %s does (a leftover session?): pick another name, or agent session kill %s first",
			s.cfg.SessionName(name), name, s.cfg.SessionName(name))
	}

	env := map[string]string{
		"AGENT_NAME": name,
		// Hooks publish status events under this host; without it they'd
		// fall back to the hostname, which AGENT_HOST may override.
		"AGENT_HOST": s.cfg.Host,
	}
	if s.cfg.AgentBinary != "" {
		// So the agent (and its hooks) can just run `agent`.
		env["PATH"] = filepath.Dir(s.cfg.AgentBinary) + string(os.PathListSeparator) + os.Getenv("PATH")
	}
	sessionName := s.cfg.SessionName(name)
	if err := s.tmux.CreateSession(sessionName, env); err != nil {
		return "", fmt.Errorf("creating session: %w", err)
	}
	_ = s.tmux.PipePane(sessionName, s.cfg.LogPath(name)) // non-fatal: the agent runs unlogged
	return sessionName, nil
}

// storeAgent records a spawned agent, killing its session if that fails.
func (s *AgentService) storeAgent(ctx context.Context, opts SpawnOptions, sessionName string) (*storage.Agent, error) {
	agent := &storage.Agent{
		Name:        opts.Name,
		SessionName: sessionName,
		Backend:     opts.Backend,
		TaskFile:    opts.TaskFile,
		LogFile:     s.cfg.LogPath(opts.Name),
		SpawnedBy:   opts.SpawnedBy,
		CreatedAt:   time.Now(),
	}
	if err := s.storage.CreateAgent(ctx, agent); err != nil {
		s.tmux.KillSession(sessionName)
		return nil, fmt.Errorf("storing agent: %w", err)
	}
	return agent, nil
}

// initMessage is a new agent's first message: the task itself, or the
// backend's request to read the task file.
func initMessage(opts SpawnOptions, fromFile func(taskFile string) string) string {
	if opts.TaskContent != "" {
		return opts.TaskContent
	}
	if opts.TaskFile != "" {
		return fromFile(opts.TaskFile)
	}
	return ""
}

// launchCommand builds the command that starts a built-in backend's CLI,
// wired up with each optional capability the backend has. promptInLaunch
// reports whether initMsg went into the command too.
func (s *AgentService) launchCommand(ctx context.Context, name, spawnedBy string, provider backend.Provider, initMsg string) (cmd string, promptInLaunch bool, err error) {
	cmd = provider.LaunchCommand()
	if hp, ok := provider.(backend.HookProvider); ok && s.cfg.AgentBinary != "" {
		// The spawn-time session id doubles as the spawn id: it is unique per
		// spawn, while _session_id itself moves on /clear and /resume.
		sessionID := uuid.NewString()
		reportCmd := fmt.Sprintf("%s report --hook --as %s --spawn-id %s",
			util.ShellQuote(s.cfg.AgentBinary), util.ShellQuote(name), util.ShellQuote(sessionID))
		cmd = hp.LaunchCommandWithHooks(reportCmd, sessionID)
		for _, key := range []string{StateKeySpawnID, StateKeySessionID} {
			if err := s.setState(ctx, name, key, sessionID); err != nil {
				return "", false, fmt.Errorf("storing %s: %w", key, err)
			}
		}
	}
	if ip, ok := provider.(backend.InstructionsProvider); ok {
		path, err := s.writeInstructions(name, spawnedBy)
		if err != nil {
			return "", false, err
		}
		cmd = ip.WithInstructions(cmd, path)
	}
	if ca, ok := provider.(backend.CommandAllower); ok {
		cmd = ca.AllowCommands(cmd, s.allowedCommandPrefixes())
	}
	if pfp, ok := provider.(backend.PromptFlagProvider); ok && initMsg != "" {
		return pfp.AppendPromptFlag(cmd, initMsg), true, nil
	}
	return cmd, false, nil
}

// cleanupExitGrace is how long cleanup lets an idle agent exit on its own.
const cleanupExitGrace = 1500 * time.Millisecond

// How long spawn looks for a workspace trust prompt with --trust-workspace.
const (
	trustPromptAttempts = 8
	trustPromptPoll     = 500 * time.Millisecond
)

// confirmTrust answers the CLI's workspace trust prompt. The prompt can take
// a few seconds to appear, and each CLI answers it differently; if it never
// shows up, spawn goes on without it.
func (s *AgentService) confirmTrust(ctx context.Context, sessionName string, tp backend.TrustPrompter) error {
	for attempt := 0; attempt < trustPromptAttempts; attempt++ {
		screen, err := s.tmux.Capture(sessionName, 0, true)
		if keys := tp.TrustKeys(screen); err == nil && keys != nil {
			for _, key := range keys {
				if err := s.tmux.SendSpecialKey(sessionName, key); err != nil {
					return fmt.Errorf("confirming workspace trust: %w", err)
				}
			}
			return nil
		}
		if err := sleepCtx(ctx, trustPromptPoll); err != nil {
			return err
		}
	}
	return nil
}

// waitForStartup gives a just-launched CLI the configured time to start.
func (s *AgentService) waitForStartup(ctx context.Context) error {
	if s.cfg.StartupDelay <= 0 {
		return nil
	}
	return sleepCtx(ctx, s.cfg.StartupDelay)
}

// sleepCtx waits for d, or until ctx is done.
func sleepCtx(ctx context.Context, d time.Duration) error {
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-time.After(d):
		return nil
	}
}

// sendLine types text into a session and presses Enter.
func (s *AgentService) sendLine(sessionName, text string) error {
	if err := s.tmux.SendKeys(sessionName, text); err != nil {
		return fmt.Errorf("sending keys: %w", err)
	}
	if err := s.tmux.SendSpecialKey(sessionName, "Enter"); err != nil {
		return fmt.Errorf("sending enter: %w", err)
	}
	return nil
}

// Send types a message into an agent, followed by Enter. It refuses while
// the agent is at a dialog: Enter there answers the dialog rather than
// sending a message (it approves a permission prompt, or picks "No, exit" at
// Claude's trust prompt), so a message must never be how a dialog gets
// answered. Use approve/deny, or SendUnchecked to override. It also refuses
// an agent whose CLI has exited, since the message would run in the shell
// left behind. It returns the status it found the agent in (unknown if it
// couldn't tell).
func (s *AgentService) Send(ctx context.Context, name string, message string) (AgentStatus, error) {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return AgentStatusUnknown, err
	}
	status, err := s.checkCanSend(ctx, agent)
	if err != nil {
		return status, err
	}
	return status, s.SendUnchecked(ctx, name, message)
}

// checkCanSend fails if the agent has exited, is waiting on a prompt, or
// shows a permission or trust dialog on screen. It returns the agent's
// status.
func (s *AgentService) checkCanSend(ctx context.Context, agent *storage.Agent) (AgentStatus, error) {
	status := AgentStatusUnknown
	if info, err := s.StatusInfo(ctx, agent.Name); err == nil {
		status = info.Status
		switch info.Status {
		case AgentStatusWaiting:
			prompt := "a prompt"
			if info.Detail != "" {
				prompt = fmt.Sprintf("a prompt (%s)", OneLine(info.Detail))
			}
			return status, fmt.Errorf("agent %s is waiting on %s, where the message's Enter would answer it: see it with agent status %s -v, then agent approve %s or agent deny %s (a person can send anyway with --force)",
				agent.Name, prompt, agent.Name, agent.Name, agent.Name)
		case AgentStatusExited:
			return status, fmt.Errorf("agent %s has exited, so the message would be typed into the shell left behind: agent logs %s shows how it ended, and agent cleanup %s removes it",
				agent.Name, agent.Name, agent.Name)
		}
	}
	provider, err := s.provider(agent)
	if err != nil {
		return status, nil // custom backend: nothing to recognize dialogs with
	}
	screen, err := s.tmux.Capture(agent.SessionName, 0, true)
	if err != nil {
		return status, nil // SendUnchecked reports a missing session
	}
	if pr, ok := provider.(backend.PermissionResponder); ok && pr.PermissionPromptVisible(screen) {
		return status, fmt.Errorf("agent %s has a permission prompt on screen, where the message's Enter would answer it: agent approve %s or agent deny %s first (a person can send anyway with --force)",
			agent.Name, agent.Name, agent.Name)
	}
	if tp, ok := provider.(backend.TrustPrompter); ok && tp.TrustKeys(screen) != nil {
		return status, fmt.Errorf("agent %s is at a workspace trust prompt, where the message's Enter would answer it: answer it in its session (%s), or clean it up and respawn with --trust-workspace",
			agent.Name, agent.SessionName)
	}
	return status, nil
}

// maxOneLine bounds OneLine's result.
const maxOneLine = 120

// OneLine shortens a status detail to one line, for messages that quote it.
func OneLine(s string) string {
	runes := []rune(strings.Join(strings.Fields(s), " "))
	if len(runes) > maxOneLine {
		return string(runes[:maxOneLine-1]) + "…"
	}
	return string(runes)
}

// sessionGone is the error for an agent whose tmux session no longer exists.
func sessionGone(agent *storage.Agent) error {
	return fmt.Errorf("agent %s's tmux session %s is gone (its CLI exited or the session was killed): agent logs %s shows its last output, and agent cleanup %s removes the record",
		agent.Name, agent.SessionName, agent.Name, agent.Name)
}

// SendUnchecked types a message into an agent without checking for dialogs.
func (s *AgentService) SendUnchecked(ctx context.Context, name string, message string) error {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return err
	}

	// Check session exists
	exists, err := s.tmux.SessionExists(agent.SessionName)
	if err != nil {
		return fmt.Errorf("checking session: %w", err)
	}
	if !exists {
		return sessionGone(agent)
	}

	return s.sendLine(agent.SessionName, message)
}

// Queue queues a message for delivery when agent is idle.
func (s *AgentService) Queue(ctx context.Context, senderID, receiverID, content string) error {
	if _, err := s.getAgent(ctx, receiverID); err != nil {
		return err
	}

	msg := &storage.Message{
		SenderID:   senderID,
		ReceiverID: receiverID,
		Content:    content,
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}

	return s.storage.CreateMessage(ctx, msg)
}

// Capture captures the current output from an agent's session.
func (s *AgentService) Capture(ctx context.Context, name string, lines int, raw bool) (string, error) {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return "", err
	}

	if exists, err := s.tmux.SessionExists(agent.SessionName); err == nil && !exists {
		return "", sessionGone(agent)
	}
	if lines <= 0 {
		lines = s.cfg.DefaultScrollback
	}

	// Capture with ANSI stripping by default (raw=false means strip)
	return s.tmux.Capture(agent.SessionName, lines, !raw)
}

// List returns all managed agents.
func (s *AgentService) List(ctx context.Context) ([]*storage.Agent, error) {
	return s.storage.ListAgents(ctx)
}

// Cleanup removes an agent and its resources.
// Returns a CleanupError if any non-fatal errors occurred during cleanup.
func (s *AgentService) Cleanup(ctx context.Context, name string) error {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return err
	}

	cleanupErrs := &CleanupError{}

	// Ask an idle agent to exit cleanly before the session is killed: its
	// hooks then record the exit, and some CLIs lose saved state when killed
	// (agy's maintainers advise /exit). Only when idle: typed into a dialog,
	// the exit command's Enter would answer the dialog instead.
	if info, err := s.StatusInfo(ctx, name); err == nil && info.Status == AgentStatusIdle {
		if p, err := s.provider(agent); err == nil && p.ExitCommand() != "" {
			if s.sendLine(agent.SessionName, p.ExitCommand()) == nil {
				time.Sleep(cleanupExitGrace)
			}
		}
	}

	// Stop log piping and kill the session, unless it's already gone
	exists, err := s.tmux.SessionExists(agent.SessionName)
	if err != nil {
		cleanupErrs.Add(fmt.Errorf("checking session exists: %w", err))
	} else if exists {
		if err := s.tmux.StopPipePane(agent.SessionName); err != nil {
			cleanupErrs.Add(fmt.Errorf("stopping pipe pane: %w", err))
		}
		if err := s.tmux.KillSession(agent.SessionName); err != nil {
			cleanupErrs.Add(fmt.Errorf("killing session: %w", err))
		}
	}

	// Delete messages
	if err := s.storage.DeleteMessagesByAgent(ctx, name); err != nil {
		cleanupErrs.Add(fmt.Errorf("deleting messages: %w", err))
	}

	// Remove the agent's directory (briefing, launch script, agy hooks)
	if err := os.RemoveAll(s.cfg.AgentDir(name)); err != nil {
		cleanupErrs.Add(fmt.Errorf("removing agent dir: %w", err))
	}

	// Delete agent record - this is the critical operation
	if err := s.storage.DeleteAgent(ctx, name); err != nil {
		return fmt.Errorf("deleting agent record: %w", err)
	}

	// Return cleanup errors if any occurred (non-fatal)
	if cleanupErrs.HasErrors() {
		return cleanupErrs
	}

	return nil
}

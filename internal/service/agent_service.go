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

	// Create provider with config (fresh instance for this agent)
	provider, err := s.backends.CreateProvider(opts.Backend, backend.ProviderConfig{
		SkipPermissions: opts.SkipPermissions,
		AgentDir:        s.cfg.AgentDir(opts.Name),
	})
	if err != nil {
		// Fall back to default backend if no factory
		var be backend.Backend
		be, err = s.backends.Get(opts.Backend)
		if err != nil {
			return nil, err
		}
		// Use legacy Backend interface
		return s.spawnWithBackend(ctx, opts, be)
	}

	return s.spawnWithProvider(ctx, opts, provider)
}

// spawnWithProvider spawns an agent using the Provider interface.
func (s *AgentService) spawnWithProvider(ctx context.Context, opts SpawnOptions, provider backend.Provider) (*storage.Agent, error) {
	// Check if agent already exists
	existing, err := s.storage.GetAgent(ctx, opts.Name)
	if err != nil {
		return nil, fmt.Errorf("checking existing agent: %w", err)
	}
	if existing != nil {
		return nil, fmt.Errorf("agent %q already exists", opts.Name)
	}

	// Create session name
	sessionName := s.cfg.SessionName(opts.Name)

	// Create tmux session
	env := map[string]string{
		"AGENT_NAME": opts.Name,
		// Hooks publish status events under this host; without it they'd
		// fall back to the hostname, which AGENT_HOST may override.
		"AGENT_HOST": s.cfg.Host,
	}
	if s.cfg.AgentBinary != "" {
		// So the agent (and its hooks) can just run `agent`.
		env["PATH"] = filepath.Dir(s.cfg.AgentBinary) + string(os.PathListSeparator) + os.Getenv("PATH")
	}
	if err := s.tmux.CreateSession(sessionName, env); err != nil {
		return nil, fmt.Errorf("creating session: %w", err)
	}

	// Create the agent record before launching: backend hooks report status
	// against it as soon as the CLI starts.
	logPath := s.cfg.LogPath(opts.Name)
	agent := &storage.Agent{
		Name:        opts.Name,
		SessionName: sessionName,
		Backend:     opts.Backend,
		TaskFile:    opts.TaskFile,
		LogFile:     logPath,
		CreatedAt:   time.Now(),
	}
	if err := s.storage.CreateAgent(ctx, agent); err != nil {
		s.tmux.KillSession(sessionName)
		return nil, fmt.Errorf("storing agent: %w", err)
	}
	abort := func(err error) (*storage.Agent, error) {
		s.tmux.KillSession(sessionName)
		s.storage.DeleteAgent(ctx, opts.Name)
		_ = os.RemoveAll(s.cfg.AgentDir(opts.Name))
		return nil, err
	}

	// Start log piping
	if err := s.tmux.PipePane(sessionName, logPath); err != nil {
		// Non-fatal, continue without logging
	}

	// Resolve initial message based on task source
	var initMsg string
	if opts.TaskContent != "" {
		initMsg = opts.TaskContent
	} else if opts.TaskFile != "" {
		initMsg = provider.InitMessage(opts.TaskFile)
	}

	// Launch the CLI tool, with status hooks when the backend supports them
	launchCmd := provider.LaunchCommand()
	if hp, ok := provider.(backend.HookProvider); ok && s.cfg.AgentBinary != "" {
		// The spawn-time session id doubles as the spawn id: it is unique per
		// spawn, while _session_id itself moves on /clear and /resume.
		sessionID := uuid.NewString()
		reportCmd := fmt.Sprintf("%s report --hook --as %s --spawn-id %s",
			util.ShellQuote(s.cfg.AgentBinary), util.ShellQuote(opts.Name), util.ShellQuote(sessionID))
		launchCmd = hp.LaunchCommandWithHooks(reportCmd, sessionID)
		for _, key := range []string{StateKeySpawnID, StateKeySessionID} {
			if err := s.setState(ctx, opts.Name, key, sessionID); err != nil {
				return abort(fmt.Errorf("storing %s: %w", key, err))
			}
		}
	}
	if ip, ok := provider.(backend.InstructionsProvider); ok {
		path, err := s.writeInstructions(opts.Name, opts.SpawnedBy)
		if err != nil {
			return abort(err)
		}
		launchCmd = ip.WithInstructions(launchCmd, path)
	}
	if ca, ok := provider.(backend.CommandAllower); ok {
		launchCmd = ca.AllowCommands(launchCmd, s.allowedCommandPrefixes())
	}
	promptInLaunch := false
	if pfp, ok := provider.(backend.PromptFlagProvider); ok && initMsg != "" {
		launchCmd = pfp.AppendPromptFlag(launchCmd, initMsg)
		promptInLaunch = true
	}
	if err := s.tmux.SendKeys(sessionName, launchCmd); err != nil {
		return abort(fmt.Errorf("launching backend: %w", err))
	}
	if err := s.tmux.SendSpecialKey(sessionName, "Enter"); err != nil {
		return abort(fmt.Errorf("sending enter: %w", err))
	}

	// Wait for CLI to start (configurable delay)
	if s.cfg.StartupDelay > 0 {
		select {
		case <-ctx.Done():
			return abort(ctx.Err())
		case <-time.After(s.cfg.StartupDelay):
		}
	}

	// Confirm the workspace trust prompt only when opted in via --trust-workspace
	// (trusting a repo also enables its own hooks and settings). The prompt can
	// take a few seconds to appear, and each CLI answers it differently.
	if tp, ok := provider.(backend.TrustPrompter); ok && opts.TrustWorkspace {
		for attempt := 0; attempt < trustPromptAttempts; attempt++ {
			screen, err := s.tmux.Capture(sessionName, 0, true)
			if keys := tp.TrustKeys(screen); err == nil && keys != nil {
				for _, key := range keys {
					if err := s.tmux.SendSpecialKey(sessionName, key); err != nil {
						return abort(fmt.Errorf("confirming workspace trust: %w", err))
					}
				}
				break
			}
			select {
			case <-ctx.Done():
				return abort(ctx.Err())
			case <-time.After(trustPromptPoll):
			}
		}
		if s.cfg.StartupDelay > 0 {
			select {
			case <-ctx.Done():
				return abort(ctx.Err())
			case <-time.After(s.cfg.StartupDelay):
			}
		}
	}

	// Send initial message if it was not already passed on the launch command line
	if !promptInLaunch && initMsg != "" {
		if err := s.tmux.SendKeys(sessionName, initMsg); err != nil {
			return abort(fmt.Errorf("sending init message: %w", err))
		}
		if err := s.tmux.SendSpecialKey(sessionName, "Enter"); err != nil {
			return abort(fmt.Errorf("sending init enter: %w", err))
		}
	}

	return agent, nil
}

// cleanupExitGrace is how long cleanup lets an idle agent exit on its own.
const cleanupExitGrace = 1500 * time.Millisecond

// How long spawn looks for a workspace trust prompt with --trust-workspace.
const (
	trustPromptAttempts = 8
	trustPromptPoll     = 500 * time.Millisecond
)

// spawnWithBackend spawns an agent using the legacy Backend interface.
func (s *AgentService) spawnWithBackend(ctx context.Context, opts SpawnOptions, be backend.Backend) (*storage.Agent, error) {
	// Check if agent already exists
	existing, err := s.storage.GetAgent(ctx, opts.Name)
	if err != nil {
		return nil, fmt.Errorf("checking existing agent: %w", err)
	}
	if existing != nil {
		return nil, fmt.Errorf("agent %q already exists", opts.Name)
	}

	// Create session name
	sessionName := s.cfg.SessionName(opts.Name)

	// Create tmux session
	env := map[string]string{
		"AGENT_NAME": opts.Name,
		// Hooks publish status events under this host; without it they'd
		// fall back to the hostname, which AGENT_HOST may override.
		"AGENT_HOST": s.cfg.Host,
	}
	if s.cfg.AgentBinary != "" {
		// So the agent (and its hooks) can just run `agent`.
		env["PATH"] = filepath.Dir(s.cfg.AgentBinary) + string(os.PathListSeparator) + os.Getenv("PATH")
	}
	if err := s.tmux.CreateSession(sessionName, env); err != nil {
		return nil, fmt.Errorf("creating session: %w", err)
	}

	// Start log piping
	logPath := s.cfg.LogPath(opts.Name)
	if err := s.tmux.PipePane(sessionName, logPath); err != nil {
		// Non-fatal, continue without logging
	}

	// Launch the CLI tool
	if err := s.tmux.SendKeys(sessionName, be.LaunchCommand()); err != nil {
		s.tmux.KillSession(sessionName)
		return nil, fmt.Errorf("launching backend: %w", err)
	}
	if err := s.tmux.SendSpecialKey(sessionName, "Enter"); err != nil {
		s.tmux.KillSession(sessionName)
		return nil, fmt.Errorf("sending enter: %w", err)
	}

	// Wait for CLI to start (configurable delay)
	if s.cfg.StartupDelay > 0 {
		select {
		case <-ctx.Done():
			s.tmux.KillSession(sessionName)
			return nil, ctx.Err()
		case <-time.After(s.cfg.StartupDelay):
		}
	}

	// Send initial message based on task source
	if opts.TaskContent != "" {
		// Task content provided directly (from DB task repository)
		initMsg := opts.TaskContent
		if err := s.tmux.SendKeys(sessionName, initMsg); err != nil {
			s.tmux.KillSession(sessionName)
			return nil, fmt.Errorf("sending task content: %w", err)
		}
		if err := s.tmux.SendSpecialKey(sessionName, "Enter"); err != nil {
			s.tmux.KillSession(sessionName)
			return nil, fmt.Errorf("sending enter: %w", err)
		}
	} else if opts.TaskFile != "" {
		// Task file path provided - use backend's init message format
		initMsg := be.InitMessage(opts.TaskFile)
		if initMsg != "" {
			if err := s.tmux.SendKeys(sessionName, initMsg); err != nil {
				s.tmux.KillSession(sessionName)
				return nil, fmt.Errorf("sending init message: %w", err)
			}
			if err := s.tmux.SendSpecialKey(sessionName, "Enter"); err != nil {
				s.tmux.KillSession(sessionName)
				return nil, fmt.Errorf("sending init enter: %w", err)
			}
		}
	}

	// Create agent record
	agent := &storage.Agent{
		Name:        opts.Name,
		SessionName: sessionName,
		Backend:     opts.Backend,
		TaskFile:    opts.TaskFile,
		LogFile:     logPath,
		CreatedAt:   time.Now(),
	}

	if err := s.storage.CreateAgent(ctx, agent); err != nil {
		s.tmux.KillSession(sessionName)
		return nil, fmt.Errorf("storing agent: %w", err)
	}

	return agent, nil
}

// Send types a message into an agent, followed by Enter. It refuses while
// the agent is at a dialog: Enter there answers the dialog rather than
// sending a message (it approves a permission prompt, or picks "No, exit" at
// Claude's trust prompt), so a message must never be how a dialog gets
// answered. Use approve/deny, or SendUnchecked to override.
func (s *AgentService) Send(ctx context.Context, name string, message string) error {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return err
	}
	if err := s.checkNotAtDialog(ctx, agent); err != nil {
		return err
	}
	return s.SendUnchecked(ctx, name, message)
}

// checkNotAtDialog fails if the agent is waiting on a prompt or shows a
// permission or trust dialog on screen.
func (s *AgentService) checkNotAtDialog(ctx context.Context, agent *storage.Agent) error {
	if info, err := s.StatusInfo(ctx, agent.Name); err == nil && info.Status == AgentStatusWaiting {
		return fmt.Errorf("agent %q is waiting on a prompt (%s); answer it with approve/deny first, or use --force", agent.Name, info.Detail)
	}
	provider, err := s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{})
	if err != nil {
		return nil // legacy backend: nothing to recognize dialogs with
	}
	screen, err := s.tmux.Capture(agent.SessionName, 0, true)
	if err != nil {
		return nil // SendUnchecked reports a missing session
	}
	if pr, ok := provider.(backend.PermissionResponder); ok && pr.PermissionPromptVisible(screen) {
		return fmt.Errorf("agent %q has a permission prompt on screen; answer it with approve/deny first, or use --force", agent.Name)
	}
	if tp, ok := provider.(backend.TrustPrompter); ok && tp.TrustKeys(screen) != nil {
		return fmt.Errorf("agent %q is at a workspace trust prompt; answer it (or respawn with --trust-workspace) before sending", agent.Name)
	}
	return nil
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
		return fmt.Errorf("session %q does not exist", agent.SessionName)
	}

	// Send message
	if err := s.tmux.SendKeys(agent.SessionName, message); err != nil {
		return fmt.Errorf("sending keys: %w", err)
	}
	if err := s.tmux.SendSpecialKey(agent.SessionName, "Enter"); err != nil {
		return fmt.Errorf("sending enter: %w", err)
	}

	return nil
}

// Queue queues a message for delivery when agent is idle.
func (s *AgentService) Queue(ctx context.Context, senderID, receiverID, content string) error {
	// Verify receiver exists
	agent, err := s.storage.GetAgent(ctx, receiverID)
	if err != nil {
		return fmt.Errorf("getting receiver: %w", err)
	}
	if agent == nil {
		return fmt.Errorf("agent %q not found", receiverID)
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
	agent, err := s.storage.GetAgent(ctx, name)
	if err != nil {
		return "", fmt.Errorf("getting agent: %w", err)
	}
	if agent == nil {
		return "", fmt.Errorf("agent %q not found", name)
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
	agent, err := s.storage.GetAgent(ctx, name)
	if err != nil {
		return fmt.Errorf("getting agent: %w", err)
	}
	if agent == nil {
		return fmt.Errorf("agent %q not found", name)
	}

	cleanupErrs := &CleanupError{}

	// Ask an idle agent to exit cleanly before the session is killed: its
	// hooks then record the exit, and some CLIs lose saved state when killed
	// (agy's maintainers advise /exit). Only when idle: typed into a dialog,
	// the exit command's Enter would answer the dialog instead.
	if info, err := s.StatusInfo(ctx, name); err == nil && info.Status == AgentStatusIdle {
		if p, err := s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{}); err == nil && p.ExitCommand() != "" {
			if s.tmux.SendKeys(agent.SessionName, p.ExitCommand()) == nil &&
				s.tmux.SendSpecialKey(agent.SessionName, "Enter") == nil {
				time.Sleep(cleanupExitGrace)
			}
		}
	}

	// Stop log piping
	if err := s.tmux.StopPipePane(agent.SessionName); err != nil {
		cleanupErrs.Add(fmt.Errorf("stopping pipe pane: %w", err))
	}

	// Kill session
	exists, err := s.tmux.SessionExists(agent.SessionName)
	if err != nil {
		cleanupErrs.Add(fmt.Errorf("checking session exists: %w", err))
	} else if exists {
		if err := s.tmux.KillSession(agent.SessionName); err != nil {
			cleanupErrs.Add(fmt.Errorf("killing session: %w", err))
		}
	}

	// Delete messages
	if err := s.storage.DeleteMessagesByAgent(ctx, name); err != nil {
		cleanupErrs.Add(fmt.Errorf("deleting messages: %w", err))
	}

	// Remove per-agent bundle directory (if any)
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

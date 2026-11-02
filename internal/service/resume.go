package service

import (
	"context"
	"fmt"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/storage"
)

// ResumeOptions holds options for resuming an ended agent.
type ResumeOptions struct {
	Name string
	// SessionID is the session the agent continues: its last one (Claude's
	// session id, agy's conversation id).
	SessionID       string
	SkipPermissions bool // Pass --dangerously-skip-permissions to Claude / Agy
	TrustWorkspace  bool // Confirm the CLI's workspace trust prompt on launch
}

// keptOnResume are the reserved state keys a resume keeps: the last turn's
// reply, and the transcript of the session it continues. The rest describe
// the run that ended (its status, prompts, spawn id) and would only mislead.
var keptOnResume = map[string]bool{StateKeyResult: true, StateKeyTranscript: true}

// Resume relaunches an ended agent on its session, under its own record:
// the same name, backend and parent, so its helpers stay its helpers, its
// own state keys stay, and messages held for it are delivered. It runs
// from the current directory (the CLI enters the session's first). Only an
// ended claude or agy agent can be resumed; a failed resume leaves it ended.
func (s *AgentService) Resume(ctx context.Context, opts ResumeOptions) (*storage.Agent, error) {
	agent, err := s.getAgent(ctx, opts.Name)
	if err != nil {
		return nil, err
	}
	if agent.Backend != "claude" && agent.Backend != "agy" {
		return nil, fmt.Errorf("agent %s runs %s: resume continues claude and agy sessions", agent.Name, agent.Backend)
	}
	if opts.SessionID == "" {
		return nil, fmt.Errorf("no session to resume agent %s on", agent.Name)
	}
	info, _, err := s.resolveStatus(ctx, agent.Name)
	if err != nil {
		return nil, err
	}
	if info.Status != AgentStatusEnded {
		return nil, fmt.Errorf("agent %s is %s, not ended: only an ended agent can be resumed (agent status %s -v)", agent.Name, info.Status, agent.Name)
	}

	// A spawned CLI that quit leaves its pane behind, running a bare shell.
	// An adopted pre-existing user session is preserved and relaunched into.
	existingSession := ""
	sessionCwd := ""
	if exists, _ := s.tmux.SessionExists(agent.SessionName); exists {
		if agent.Adopted {
			existingSession = agent.SessionName
			sessionCwd = agent.Cwd
			_ = s.tmux.StopPipePane(agent.SessionName)
		} else if err := s.tmux.KillSession(agent.SessionName); err != nil {
			return nil, fmt.Errorf("closing its old tmux session %s: %w", agent.SessionName, err)
		}
	}
	// Its run closes with the status it ended in, which its last report may
	// not say (no hook fires when the machine goes down).
	for key, value := range map[string]string{StateKeyStatus: string(AgentStatusEnded), StateKeyEndReason: string(info.EndReason)} {
		if err := s.setState(ctx, agent.Name, key, value); err != nil {
			return nil, fmt.Errorf("recording how its run ended: %w", err)
		}
	}

	resumed, err := s.Spawn(ctx, SpawnOptions{
		Name:            agent.Name,
		Backend:         agent.Backend,
		SpawnedBy:       agent.SpawnedBy,
		SkipPermissions: opts.SkipPermissions,
		TrustWorkspace:  opts.TrustWorkspace,
		Resume:          opts.SessionID,
		replacing:       agent,
		existingSession: existingSession,
		sessionCwd:      sessionCwd,
	})
	if err != nil {
		return nil, err
	}
	// Messages held for it when it ended never went out: the deliverer types
	// them in once it sits at its prompt.
	if s.HeldCount(ctx, agent.Name) > 0 {
		_ = s.startDeliverer(agent.Name)
	}
	return resumed, nil
}

// clearRunState deletes the reserved state keys that describe the agent's
// ended run, except keptOnResume.
func (s *AgentService) clearRunState(ctx context.Context, name string) error {
	entries, err := s.storage.ListState(ctx, name)
	if err != nil {
		return fmt.Errorf("reading its state: %w", err)
	}
	var del []string
	for _, e := range entries {
		if strings.HasPrefix(e.Key, "_") && !keptOnResume[e.Key] {
			del = append(del, e.Key)
		}
	}
	if len(del) == 0 {
		return nil
	}
	if err := s.storage.UpdateStates(ctx, nil, name, del); err != nil {
		return fmt.Errorf("clearing its ended run's status: %w", err)
	}
	return nil
}

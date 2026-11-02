package service

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// AdoptOptions holds options for adopting an existing tmux session onto the bus.
type AdoptOptions struct {
	Name        string
	SessionName string // explicit target tmux session (--session); "" uses CurrentPane
	CurrentPane string // caller's $TMUX_PANE (e.g. "%0"), "" when outside tmux
	Backend     string // explicit --backend override; "" auto-detects Claude or defaults to "shell"
	SpawnedBy   string
}

// AdoptResult describes the outcome of Adopt.
type AdoptResult struct {
	Agent     *storage.Agent
	SessionID string // detected Claude session ID, if any
}

type tmuxPaneInspector interface {
	FirstPane(sessionName string) (tmux.PaneInfo, error)
	SessionOfPane(pane string) (string, error)
}

// Adopt registers an existing tmux session on the agent bus without launching,
// stopping, or restarting any processes in the session.
func (s *AgentService) Adopt(ctx context.Context, opts AdoptOptions) (*AdoptResult, error) {
	if err := ValidateAgentName(opts.Name); err != nil {
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
	if targetSession == "" {
		return nil, errors.New("not in a tmux session: run agent adopt inside tmux or pass --session <tmux-session>")
	}

	pane, err := s.firstPane(targetSession)
	if err != nil {
		return nil, err
	}
	if opts.SessionName == "" && opts.CurrentPane != "" && pane.ID != "" && opts.CurrentPane != pane.ID {
		return nil, fmt.Errorf("tmux session %s has multiple panes and agent targets its first pane (%s, not current pane %s): run adopt in the first pane or move this pane to its own session",
			targetSession, pane.ID, opts.CurrentPane)
	}

	var paneSession *backend.PaneSession
	if pane.PID > 0 && (opts.Backend == "" || opts.Backend == "claude") {
		if p, err := s.backends.CreateProvider("claude", backend.ProviderConfig{}); err == nil {
			if pp, ok := p.(backend.PaneSessionProvider); ok {
				if ps, found, _ := pp.PaneSession(ctx, pane.PID); found && ps.ID != "" {
					paneSession = &ps
				}
			}
		}
	}

	backendName := opts.Backend
	if backendName == "" {
		if paneSession != nil {
			backendName = "claude"
		} else {
			backendName = "shell"
		}
	}

	if _, err := s.backends.Get(backendName); err != nil {
		return nil, err
	}
	if err := s.prepareExistingSession(ctx, opts.Name, targetSession); err != nil {
		return nil, err
	}
	agent, err := s.storeAgent(ctx, SpawnOptions{
		Name:            opts.Name,
		Backend:         backendName,
		SpawnedBy:       opts.SpawnedBy,
		existingSession: targetSession,
		sessionCwd:      pane.Cwd,
	}, targetSession)
	if err != nil {
		return nil, err
	}

	sessionID := ""
	if paneSession != nil {
		sessionID = paneSession.ID
		_ = s.setState(ctx, opts.Name, StateKeySessionID, paneSession.ID)
		_ = s.Report(ctx, opts.Name, StatusReport{
			Status:    fromBackendStatus(paneSession.Status),
			Event:     EventAdopted,
			Detail:    fmt.Sprintf("adopted session %s (pid %d, %s)", paneSession.ID, paneSession.PID, paneSession.Detail),
			SessionID: paneSession.ID,
		})
	}

	return &AdoptResult{
		Agent:     agent,
		SessionID: sessionID,
	}, nil
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

package service

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/termd"
)

// AdoptOptions holds options for adopting an existing termd session onto the bus.
type AdoptOptions struct {
	Name        string
	SessionName string // explicit target terminal id (--session); "" uses CurrentPane
	CurrentPane string // caller's $G3D_TERMD_ID, "" when outside g3d
	Backend     string // explicit --backend override; "" auto-detects Claude or defaults to "shell"
	SpawnedBy   string
}

// AdoptResult describes the outcome of Adopt.
type AdoptResult struct {
	Agent     *storage.Agent
	SessionID string // detected Claude session ID, if any
}

type paneInspector interface {
	FirstPane(sessionName string) (termd.PaneInfo, error)
	SessionOfPane(pane string) (string, error)
}

// Adopt registers an existing termd session on the agent bus without launching,
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
		return nil, errors.New("not in a termd session: run agent adopt inside g3d or pass --session <session>")
	}

	pane, err := s.firstPane(targetSession)
	if err != nil {
		return nil, err
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
	if pi, ok := s.tmux.(paneInspector); ok {
		return pi.SessionOfPane(pane)
	}
	return "", nil
}

func (s *AgentService) firstPane(sessionName string) (termd.PaneInfo, error) {
	if pi, ok := s.tmux.(paneInspector); ok {
		return pi.FirstPane(sessionName)
	}
	pid, err := s.panePID(sessionName)
	if err != nil {
		return termd.PaneInfo{}, err
	}
	cwd, _ := os.Getwd()
	return termd.PaneInfo{PID: pid, Cwd: cwd}, nil
}

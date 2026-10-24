package service

import (
	"context"
	"fmt"
	"regexp"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// Reserved state keys written by status reporting. The leading underscore
// keeps them clear of keys agents set for their own coordination.
const (
	StateKeyStatus    = "_status"
	StateKeyEvent     = "_event"
	StateKeyDetail    = "_detail"
	StateKeyResult    = "_result"
	StateKeySessionID = "_session_id"
	// StateKeySpawnID identifies one spawn of an agent name. Hook commands
	// carry it, so hooks from a previous agent with the same name (e.g. the
	// SessionEnd fired while cleanup kills it) can't write into a respawn.
	StateKeySpawnID = "_spawn_id"
)

// Status sources, from most to least authoritative.
const (
	StatusSourceNative = "native" // the backend CLI's own session status
	StatusSourceReport = "report" // hooks or `agent report`
	StatusSourceScrape = "scrape" // regex over captured terminal output
)

// StatusReport is a status update pushed by an agent or its backend's hooks.
type StatusReport struct {
	Status    AgentStatus
	Event     string
	Detail    string
	Result    string // only stored when non-empty
	SessionID string // only stored when non-empty
}

// StatusInfo describes an agent's status and where it came from.
type StatusInfo struct {
	Status    AgentStatus
	Source    string
	Event     string
	Detail    string
	UpdatedAt time.Time // time of the last report; zero for other sources
}

// ParseAgentStatus validates a status name.
func ParseAgentStatus(s string) (AgentStatus, error) {
	switch st := AgentStatus(s); st {
	case AgentStatusIdle, AgentStatusProcessing, AgentStatusWaiting,
		AgentStatusError, AgentStatusExited, AgentStatusUnknown:
		return st, nil
	}
	return "", fmt.Errorf("invalid status %q (want idle, processing, waiting, error, exited, or unknown)", s)
}

func fromBackendStatus(st backend.Status) AgentStatus {
	return AgentStatus(st.String())
}

// Report records a status update for an agent.
func (s *AgentService) Report(ctx context.Context, name string, r StatusReport) error {
	entries := map[string]string{
		StateKeyStatus: string(r.Status),
		StateKeyEvent:  r.Event,
		StateKeyDetail: r.Detail,
	}
	if r.Result != "" {
		entries[StateKeyResult] = r.Result
	}
	if r.SessionID != "" {
		entries[StateKeySessionID] = r.SessionID
	}
	for key, value := range entries {
		if err := s.setState(ctx, name, key, value); err != nil {
			return fmt.Errorf("storing %s: %w", key, err)
		}
	}
	return nil
}

// ReportHook records the status carried by a backend hook payload.
// Events that carry no status change are ignored. spawnID is the id the hook
// command was launched with; hooks from any other spawn of this agent name
// are rejected. An empty spawnID skips the check (hooks wired by hand).
func (s *AgentService) ReportHook(ctx context.Context, name, spawnID string, payload []byte) error {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return err
	}
	if spawnID != "" {
		if current := s.stateValue(ctx, name, StateKeySpawnID); current != spawnID {
			return fmt.Errorf("ignoring hook from spawn %s of %q (current spawn: %q)", spawnID, name, current)
		}
	}
	provider, err := s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{})
	if err != nil {
		return fmt.Errorf("getting backend: %w", err)
	}
	hp, ok := provider.(backend.HookProvider)
	if !ok {
		return fmt.Errorf("backend %q does not support hooks", agent.Backend)
	}
	hr, ok, err := hp.ParseHookEvent(payload)
	if err != nil || !ok {
		return err
	}
	return s.Report(ctx, name, StatusReport{
		Status:    fromBackendStatus(hr.Status),
		Event:     hr.Event,
		Detail:    hr.Detail,
		Result:    hr.Result,
		SessionID: hr.SessionID,
	})
}

// Status checks the current status of an agent.
func (s *AgentService) Status(ctx context.Context, name string) (AgentStatus, error) {
	info, err := s.StatusInfo(ctx, name)
	if err != nil {
		return AgentStatusUnknown, err
	}
	return info.Status, nil
}

// StatusInfo resolves an agent's status from, in order: the backend's native
// session status, the last reported status, and terminal scraping.
func (s *AgentService) StatusInfo(ctx context.Context, name string) (*StatusInfo, error) {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return nil, err
	}

	reported, err := s.reportedStatus(ctx, name)
	if err != nil {
		return nil, err
	}

	native, found, queried := s.nativeStatus(ctx, agent)
	if found {
		info := &StatusInfo{Status: native, Source: StatusSourceNative}
		// Hook detail is only trustworthy while it agrees with the backend;
		// e.g. an interrupted permission prompt fires no hook at all.
		if reported != nil && reported.Status == native {
			info.Event, info.Detail, info.UpdatedAt = reported.Event, reported.Detail, reported.UpdatedAt
		}
		return info, nil
	}
	if reported != nil {
		// The backend answered but no longer knows the session, yet the last
		// report says it's alive: the CLI died without its SessionEnd hook
		// (crash, kill -9, OOM). Echoing the stale report would let the
		// watcher type queued messages into the bare shell left behind.
		if queried && reported.Status != AgentStatusExited {
			return &StatusInfo{
				Status:    AgentStatusExited,
				Source:    StatusSourceNative,
				Event:     reported.Event,
				Detail:    "session not running (exited without a SessionEnd hook)",
				UpdatedAt: reported.UpdatedAt,
			}, nil
		}
		return reported, nil
	}

	status, err := s.scrapeStatus(agent)
	if err != nil {
		return nil, err
	}
	return &StatusInfo{Status: status, Source: StatusSourceScrape}, nil
}

// reportedStatus returns the last reported status, or nil if none was reported.
func (s *AgentService) reportedStatus(ctx context.Context, name string) (*StatusInfo, error) {
	entry, err := s.storage.GetState(ctx, name, StateKeyStatus)
	if err != nil {
		return nil, fmt.Errorf("reading reported status: %w", err)
	}
	if entry == nil {
		return nil, nil
	}
	info := &StatusInfo{
		Status:    AgentStatus(entry.Value),
		Source:    StatusSourceReport,
		UpdatedAt: entry.UpdatedAt,
	}
	info.Event = s.stateValue(ctx, name, StateKeyEvent)
	info.Detail = s.stateValue(ctx, name, StateKeyDetail)
	return info, nil
}

// nativeStatus asks the backend CLI for the session's status, if it can.
// queried reports whether the CLI answered at all; found, whether it knows
// the session and gave a usable status.
func (s *AgentService) nativeStatus(ctx context.Context, agent *storage.Agent) (status AgentStatus, found, queried bool) {
	sessionID := s.stateValue(ctx, agent.Name, StateKeySessionID)
	if sessionID == "" {
		return "", false, false
	}
	provider, err := s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{})
	if err != nil {
		return "", false, false
	}
	np, ok := provider.(backend.NativeStatusProvider)
	if !ok {
		return "", false, false
	}
	st, known, err := np.NativeStatus(ctx, sessionID)
	if err != nil {
		return "", false, false
	}
	if !known {
		return "", false, true
	}
	if st == backend.StatusUnknown {
		// Listed, but in a state we don't map: alive, so not a crash.
		return "", false, false
	}
	return fromBackendStatus(st), true, true
}

// scrapeStatus infers status from terminal output. Last resort for backends
// that neither report through hooks nor expose a native status.
func (s *AgentService) scrapeStatus(agent *storage.Agent) (AgentStatus, error) {
	be, err := s.backends.Get(agent.Backend)
	if err != nil {
		return AgentStatusUnknown, fmt.Errorf("getting backend: %w", err)
	}

	output, err := s.tmux.Capture(agent.SessionName, 50, true)
	if err != nil {
		return AgentStatusUnknown, fmt.Errorf("capturing output: %w", err)
	}

	if idlePattern := be.IdlePattern(); idlePattern != "" {
		matched, err := regexp.MatchString(idlePattern, output)
		if err != nil {
			return AgentStatusUnknown, fmt.Errorf("invalid idle pattern %q: %w", idlePattern, err)
		}
		if matched {
			return AgentStatusIdle, nil
		}
	}

	if procPattern := be.ProcessingPattern(); procPattern != "" {
		matched, err := regexp.MatchString(procPattern, output)
		if err != nil {
			return AgentStatusUnknown, fmt.Errorf("invalid processing pattern %q: %w", procPattern, err)
		}
		if matched {
			return AgentStatusProcessing, nil
		}
	}

	return AgentStatusUnknown, nil
}

// RespondPermission answers an agent's pending permission prompt. Unless
// force is set, the agent must currently be waiting.
func (s *AgentService) RespondPermission(ctx context.Context, name string, resp backend.PermissionResponse, force bool) error {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return err
	}
	if !force {
		info, err := s.StatusInfo(ctx, name)
		if err != nil {
			return err
		}
		if info.Status != AgentStatusWaiting {
			return fmt.Errorf("agent %q is %s, not waiting on a permission prompt (use --force to send anyway)", name, info.Status)
		}
	}

	provider, err := s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{})
	if err != nil {
		return fmt.Errorf("getting backend: %w", err)
	}
	pr, ok := provider.(backend.PermissionResponder)
	if !ok {
		return fmt.Errorf("backend %q does not support answering permission prompts", agent.Backend)
	}
	key, err := pr.PermissionKey(resp)
	if err != nil {
		return err
	}
	if err := s.tmux.SendSpecialKey(agent.SessionName, key); err != nil {
		return fmt.Errorf("sending %q: %w", key, err)
	}

	// Record the expected outcome now: a denied prompt fires no hook, and an
	// approved one only reports once the tool finishes.
	report := StatusReport{Status: AgentStatusProcessing, Event: "approved"}
	if resp == backend.PermissionDeny {
		report = StatusReport{Status: AgentStatusIdle, Event: "denied"}
	}
	return s.Report(ctx, name, report)
}

func (s *AgentService) getAgent(ctx context.Context, name string) (*storage.Agent, error) {
	agent, err := s.storage.GetAgent(ctx, name)
	if err != nil {
		return nil, fmt.Errorf("getting agent: %w", err)
	}
	if agent == nil {
		return nil, fmt.Errorf("agent %q not found", name)
	}
	return agent, nil
}

func (s *AgentService) setState(ctx context.Context, name, key, value string) error {
	return s.storage.SetState(ctx, &storage.StateEntry{
		AgentName: name,
		Key:       key,
		Value:     value,
		UpdatedAt: time.Now(),
	})
}

// stateValue returns a state value, or "" if unset or unreadable.
func (s *AgentService) stateValue(ctx context.Context, name, key string) string {
	entry, err := s.storage.GetState(ctx, name, key)
	if err != nil || entry == nil {
		return ""
	}
	return entry.Value
}

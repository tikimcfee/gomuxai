package service

import (
	"context"
	"fmt"
	"regexp"
	"strings"
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
	StateKeyTool      = "_tool" // tool the last event concerned, e.g. the one awaiting permission
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
	Tool      string
	Result    string // only stored when non-empty
	SessionID string // only stored when non-empty
	// ClearResult empties the stored result (a new turn started), so a
	// reader never takes the previous turn's reply for this one's.
	ClearResult bool
	// SpawnID, when non-empty, drops the report unless the agent's
	// current _spawn_id matches, so a stale exit wrapper can't overwrite
	// a respawned agent.
	SpawnID string
}

// StatusInfo describes an agent's status and where it came from.
type StatusInfo struct {
	Status    AgentStatus
	Source    string
	Event     string
	Detail    string
	Tool      string
	UpdatedAt time.Time // time of the last report; zero for other sources

	// The last recorded report, whatever Status was resolved from. Status can
	// lag it (native status trails hooks) or override it (interrupts).
	ReportedStatus AgentStatus
	ReportedAt     time.Time
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

// Report records a status update for an agent. All keys are written in one
// transaction, so concurrent reports can't leave a mix of both.
func (s *AgentService) Report(ctx context.Context, name string, r StatusReport) error {
	if r.SpawnID != "" {
		if current := s.stateValue(ctx, name, StateKeySpawnID); current != r.SpawnID {
			return fmt.Errorf("ignoring report from spawn %s of %q (current spawn: %q)", r.SpawnID, name, current)
		}
	}
	values := map[string]string{
		StateKeyStatus: string(r.Status),
		StateKeyEvent:  r.Event,
		StateKeyDetail: r.Detail,
		StateKeyTool:   r.Tool,
	}
	if r.Result != "" || r.ClearResult {
		values[StateKeyResult] = r.Result
	}
	if r.SessionID != "" {
		values[StateKeySessionID] = r.SessionID
	}
	now := time.Now()
	entries := make([]*storage.StateEntry, 0, len(values))
	for key, value := range values {
		entries = append(entries, &storage.StateEntry{AgentName: name, Key: key, Value: value, UpdatedAt: now})
	}
	if err := s.storage.SetStates(ctx, entries); err != nil {
		return err
	}
	s.publishStatus(name, r, now)
	return nil
}

// ReportHook records the status carried by a backend hook payload.
// Events that carry no status change are ignored. spawnID is the id the hook
// command was launched with; hooks from any other spawn of this agent name
// are rejected. An empty spawnID skips the check (hooks wired by hand).
func (s *AgentService) ReportHook(ctx context.Context, name, spawnID, event string, payload []byte) error {
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
	hr, ok, err := hp.ParseHookEvent(event, payload)
	if err != nil || !ok {
		return err
	}
	return s.Report(ctx, name, StatusReport{
		Status:    fromBackendStatus(hr.Status),
		Event:     hr.Event,
		Detail:    hr.Detail,
		Tool:      hr.Tool,
		Result:    hr.Result,
		SessionID: hr.SessionID,

		ClearResult: hr.ClearResult,
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
	info, reported, err := s.resolveStatus(ctx, name)
	if err != nil {
		return nil, err
	}
	if reported != nil {
		info.ReportedStatus, info.ReportedAt = reported.Status, reported.UpdatedAt
	}
	return info, nil
}

// resolveStatus returns the resolved status and the last report as recorded
// (nil if none).
func (s *AgentService) resolveStatus(ctx context.Context, name string) (*StatusInfo, *StatusInfo, error) {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return nil, nil, err
	}

	reported, err := s.reportedStatus(ctx, name)
	if err != nil {
		return nil, nil, err
	}
	var raw *StatusInfo // the report as recorded, before any adjustment below
	if reported != nil {
		copied := *reported
		raw = &copied
	}

	native, found, queried := s.nativeStatus(ctx, agent)
	if found {
		info := &StatusInfo{Status: native, Source: StatusSourceNative}
		// Hook detail is only trustworthy while it agrees with the backend;
		// e.g. an interrupted permission prompt fires no hook at all.
		if reported != nil && reported.Status == native {
			info.Event, info.Detail, info.Tool, info.UpdatedAt = reported.Event, reported.Detail, reported.Tool, reported.UpdatedAt
		}
		return info, raw, nil
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
			}, raw, nil
		}
		// Backends like agy fire PreToolUse before showing a permission
		// dialog and fire nothing while the dialog is up. When the dialog
		// is on screen after PreToolUse, surface the status as waiting so
		// callers and approve/deny see the pending prompt.
		if reported.Status == AgentStatusProcessing && reported.Event == "PreToolUse" && s.permissionPromptVisible(agent) {
			reported.Status = AgentStatusWaiting
			if reported.Detail != "" && !strings.HasPrefix(reported.Detail, "permission: ") {
				reported.Detail = "permission: " + reported.Detail
			}
		}
		return reported, raw, nil
	}

	status, err := s.scrapeStatus(agent)
	if err != nil {
		return nil, nil, err
	}
	return &StatusInfo{Status: status, Source: StatusSourceScrape}, nil, nil
}

// reportedStatus returns the last reported status, or nil if none was reported.
// It reads all keys in one query so it sees a single report, not a mix.
func (s *AgentService) reportedStatus(ctx context.Context, name string) (*StatusInfo, error) {
	entries, err := s.storage.ListState(ctx, name)
	if err != nil {
		return nil, fmt.Errorf("reading reported status: %w", err)
	}
	values := make(map[string]string, len(entries))
	var info *StatusInfo
	for _, e := range entries {
		values[e.Key] = e.Value
		if e.Key == StateKeyStatus {
			info = &StatusInfo{Status: AgentStatus(e.Value), Source: StatusSourceReport, UpdatedAt: e.UpdatedAt}
		}
	}
	if info == nil {
		return nil, nil
	}
	info.Event = values[StateKeyEvent]
	info.Detail = values[StateKeyDetail]
	info.Tool = values[StateKeyTool]
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

func (s *AgentService) permissionPromptVisible(agent *storage.Agent) bool {
	provider, err := s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{})
	if err != nil {
		return false
	}
	pr, ok := provider.(backend.PermissionResponder)
	if !ok {
		return false
	}
	screen, err := s.tmux.Capture(agent.SessionName, 0, true)
	if err != nil {
		return false
	}
	return pr.PermissionPromptVisible(screen)
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

// PermissionOptions guards RespondPermission against answering the wrong prompt.
type PermissionOptions struct {
	// Force sends the key even if the agent isn't waiting, is waiting on a
	// dialog the backend won't treat as a permission prompt, or shows no
	// prompt on screen.
	Force bool
	// ExpectTool, if set, refuses unless the pending prompt is for this tool,
	// so a prompt that replaced the one you inspected isn't answered blind.
	ExpectTool string
}

// RespondPermission answers an agent's pending permission prompt.
func (s *AgentService) RespondPermission(ctx context.Context, name string, resp backend.PermissionResponse, opts PermissionOptions) error {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return err
	}
	info, err := s.StatusInfo(ctx, name)
	if err != nil {
		return err
	}
	if !opts.Force && info.Status != AgentStatusWaiting {
		return fmt.Errorf("agent %q is %s, not waiting on a permission prompt (use --force to send anyway)", name, info.Status)
	}
	if opts.ExpectTool != "" && info.Tool != opts.ExpectTool {
		return fmt.Errorf("agent %q is waiting on %q, not %q", name, info.Tool, opts.ExpectTool)
	}

	provider, err := s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{})
	if err != nil {
		return fmt.Errorf("getting backend: %w", err)
	}
	pr, ok := provider.(backend.PermissionResponder)
	if !ok {
		return fmt.Errorf("backend %q does not support answering permission prompts", agent.Backend)
	}
	tool := info.Tool
	if opts.Force {
		tool = "" // answer whatever is on screen as a plain permission prompt
	}
	key, err := pr.PermissionKey(resp, tool)
	if err != nil {
		return fmt.Errorf("agent %q: %w", name, err)
	}
	// The status above can be hundreds of ms old (native status runs the
	// backend CLI), and the prompt may have been answered since. For Claude
	// that turns deny's Escape into an interrupt of the now-running turn, and
	// no key means "No" without that risk (the "No" option acts like Escape,
	// and its number varies). So confirm the prompt is on screen right before
	// the keypress. This narrows the race to the gap between the capture and
	// the keypress (a few tmux calls, milliseconds) but can't close it: a
	// prompt answered inside that gap still gets the key.
	if !opts.Force {
		screen, err := s.tmux.Capture(agent.SessionName, 0, true)
		if err != nil {
			return fmt.Errorf("capturing screen: %w", err)
		}
		if !pr.PermissionPromptVisible(screen) {
			return fmt.Errorf("agent %q has no permission prompt on screen; it may have just been answered (use --force to send anyway)", name)
		}
	}
	if err := s.tmux.SendSpecialKey(agent.SessionName, key); err != nil {
		return fmt.Errorf("sending %q: %w", key, err)
	}

	// Record the expected outcome now: a denied prompt fires no hook, and an
	// approved one only reports once the tool finishes.
	report := StatusReport{Status: AgentStatusProcessing, Event: "approved", Tool: info.Tool}
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

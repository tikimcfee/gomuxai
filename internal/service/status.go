package service

import (
	"context"
	"encoding/json"
	"fmt"
	"regexp"
	"sort"
	"strconv"
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
	StateKeyTool      = "_tool"       // tool the last event concerned, e.g. the one awaiting permission
	StateKeyToolInput = "_tool_input" // that tool call's full input (JSON), when it may need permission
	// StateKeyTranscript is the path of the CLI's own transcript of the
	// session, from the last hook payload that named one.
	StateKeyTranscript = "_transcript"
	// StateKeySpawnID identifies one spawn of an agent name. Hook commands
	// carry it, so hooks from a previous agent with the same name (e.g. the
	// SessionEnd fired while cleanup kills it) can't write into a respawn.
	StateKeySpawnID = "_spawn_id"
	// StateKeySentAt is when a message was last typed into the agent (send,
	// a queued delivery, or spawn's first message), and StateKeyTurnAt when
	// its last turn started (a report that is processing and clears the
	// result). wait --next compares them to catch a turn that started after
	// a send and ended before the wait began.
	StateKeySentAt = "_sent_at"
	StateKeyTurnAt = "_turn_at"
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
	ToolInput string // full input (JSON) of the tool call that may need permission
	Result    string // only stored when non-empty
	SessionID string // only stored when non-empty
	// Transcript is the path of the CLI's transcript file; only stored
	// when non-empty.
	Transcript string
	// ClearResult empties the stored result (a new turn started), so a
	// reader never takes the previous turn's reply for this one's.
	ClearResult bool
	// SpawnID, when non-empty, drops the report unless the agent's
	// current _spawn_id matches, so a stale exit wrapper can't overwrite
	// a respawned agent.
	SpawnID string

	// Prompt, CallKey, and Agent update the pending permission prompts
	// (see backend.HookReport).
	Prompt  backend.PromptChange
	CallKey string
	Agent   string
	// DropPrompts are pending prompts' state keys to delete: the prompts
	// approve or deny answered.
	DropPrompts []string
}

// stateKeyPromptPrefix starts the key of each pending permission prompt,
// followed by the time it was raised (zero-padded nanoseconds, so keys sort
// oldest first). Each prompt has its own key, so concurrent hooks add and
// remove prompts without losing each other's.
const stateKeyPromptPrefix = "_prompt."

// pendingPrompt is a permission prompt raised and not yet answered, stored
// as JSON under its stateKeyPromptPrefix key.
type pendingPrompt struct {
	key    string
	Call   string `json:"call"`
	Agent  string `json:"agent,omitempty"`
	Tool   string `json:"tool"`
	Input  string `json:"input,omitempty"`
	Detail string `json:"detail"`
}

// StatusInfo describes an agent's status and where it came from.
type StatusInfo struct {
	Status    AgentStatus
	Source    string
	Event     string
	Detail    string
	Tool      string
	ToolInput string    // full input (JSON) of the tool call a permission prompt is about
	UpdatedAt time.Time // time of the last report; zero for other sources

	// The last recorded report, whatever Status was resolved from. Status can
	// lag it (native status trails hooks) or override it (interrupts).
	ReportedStatus AgentStatus
	ReportedAt     time.Time

	// When a message was last typed into the agent, and when its last turn
	// started; zero if never, or if the agent has never reported.
	SentAt time.Time
	TurnAt time.Time

	// Held is how many messages wait for the agent: held for the end of its
	// turn (see Send), or queued for agent watch.
	Held int

	pending []pendingPrompt // unanswered permission prompts, oldest first
	prompt  *pendingPrompt  // while waiting: the one on screen, which Tool and Detail describe
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
	if err := s.checkSpawn(ctx, name, r.SpawnID, "report"); err != nil {
		return err
	}
	now := time.Now()
	u, err := s.reportUpdate(ctx, name, r, now)
	if err != nil {
		return err
	}
	if err := s.storage.RecordReport(ctx, u.Entries, name, u.Delete, u.Event); err != nil {
		return err
	}
	s.publishStatus(name, r, now)
	return nil
}

// reportUpdate is the state change that records r, reported at now.
func (s *AgentService) reportUpdate(ctx context.Context, name string, r StatusReport, now time.Time) (storage.StateUpdate, error) {
	values := map[string]string{
		StateKeyStatus: string(r.Status),
		StateKeyEvent:  r.Event,
		StateKeyDetail: r.Detail,
		StateKeyTool:   r.Tool,
		// Always written, so an input never outlives the call it describes.
		StateKeyToolInput: r.ToolInput,
	}
	if r.Result != "" || r.ClearResult {
		values[StateKeyResult] = r.Result
	}
	if r.SessionID != "" {
		values[StateKeySessionID] = r.SessionID
	}
	if r.Transcript != "" {
		values[StateKeyTranscript] = r.Transcript
	}
	if r.ClearResult && r.Status == AgentStatusProcessing {
		values[StateKeyTurnAt] = now.Format(time.RFC3339Nano)
	}
	del, err := s.promptChanges(ctx, name, r, now, values)
	if err != nil {
		return storage.StateUpdate{}, err
	}
	entries := make([]*storage.StateEntry, 0, len(values))
	for key, value := range values {
		entries = append(entries, &storage.StateEntry{AgentName: name, Key: key, Value: value, UpdatedAt: now})
	}
	ev := &storage.AgentEvent{At: now, Kind: storage.AgentStatus, Agent: name,
		Status: string(r.Status), Event: r.Event, Detail: r.Detail, Tool: r.Tool,
		SessionID: r.SessionID, Transcript: r.Transcript}
	if r.Status == AgentStatusWaiting {
		ev.ToolInput = r.ToolInput // only prompts: every tool call's input would bloat the log
	}
	return storage.StateUpdate{Entries: entries, AgentName: name, Delete: del, Event: ev}, nil
}

// promptChanges applies r's change to the pending permission prompts: a new
// prompt goes into values, and the keys of prompts that ended are returned.
func (s *AgentService) promptChanges(ctx context.Context, name string, r StatusReport, now time.Time, values map[string]string) ([]string, error) {
	del := r.DropPrompts
	if r.Prompt == backend.PromptOpened {
		data, err := json.Marshal(pendingPrompt{Call: r.CallKey, Agent: r.Agent, Tool: r.Tool, Input: r.ToolInput, Detail: r.Detail})
		if err != nil {
			return nil, err
		}
		values[fmt.Sprintf("%s%020d", stateKeyPromptPrefix, now.UnixNano())] = string(data)
		return del, nil
	}
	if r.Prompt == backend.PromptNone {
		return del, nil
	}
	pending, err := s.pendingPrompts(ctx, name)
	if err != nil {
		return nil, err
	}
	for _, p := range pending {
		switch {
		case r.Prompt == backend.PromptsAllEnded,
			r.Prompt == backend.PromptsMainEnded && p.Agent == "":
			del = append(del, p.key)
		case r.Prompt == backend.PromptResolved && p.Call == r.CallKey && p.Agent == r.Agent:
			return append(del, p.key), nil // the oldest of identical calls
		}
	}
	return del, nil
}

// pendingPrompts returns the agent's unanswered permission prompts, oldest
// first.
func (s *AgentService) pendingPrompts(ctx context.Context, name string) ([]pendingPrompt, error) {
	entries, err := s.storage.ListState(ctx, name)
	if err != nil {
		return nil, fmt.Errorf("reading pending prompts: %w", err)
	}
	return promptsIn(entries), nil
}

// promptsIn picks the pending prompts out of an agent's state entries,
// oldest first.
func promptsIn(entries []*storage.StateEntry) []pendingPrompt {
	var pending []pendingPrompt
	for _, e := range entries {
		if !strings.HasPrefix(e.Key, stateKeyPromptPrefix) {
			continue
		}
		p := pendingPrompt{key: e.Key}
		if json.Unmarshal([]byte(e.Value), &p) == nil {
			pending = append(pending, p)
		}
	}
	sort.Slice(pending, func(i, j int) bool { return pending[i].key < pending[j].key })
	return pending
}

// checkSpawn refuses a report (or hook, per what) sent by spawnID, unless
// that is the agent's current spawn. An empty spawnID passes.
func (s *AgentService) checkSpawn(ctx context.Context, name, spawnID, what string) error {
	if spawnID == "" {
		return nil
	}
	if current := s.stateValue(ctx, name, StateKeySpawnID); current != spawnID {
		return fmt.Errorf("ignoring %s from spawn %s of %q (current spawn: %q)", what, spawnID, name, current)
	}
	return nil
}

// ReportHook records the status carried by a backend hook payload.
// Events that carry no status change are ignored. spawnID is the id the hook
// command was launched with; hooks from any other spawn of this agent name
// are rejected. An empty spawnID skips the check (hooks wired by hand).
func (s *AgentService) ReportHook(ctx context.Context, name, spawnID, event string, payload []byte) error {
	_, err := s.reportHook(ctx, name, spawnID, event, payload, false)
	return err
}

// HandleHook is ReportHook for the hook itself. A hook that ends the
// agent's turn also hands it the messages held for it (see Send), where the
// backend can: out is what the hook must print for that, nil if nothing.
func (s *AgentService) HandleHook(ctx context.Context, name, spawnID, event string, payload []byte) (out []byte, err error) {
	return s.reportHook(ctx, name, spawnID, event, payload, true)
}

func (s *AgentService) reportHook(ctx context.Context, name, spawnID, event string, payload []byte, deliver bool) ([]byte, error) {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return nil, err
	}
	if err := s.checkSpawn(ctx, name, spawnID, "hook"); err != nil {
		return nil, err
	}
	provider, err := s.provider(agent)
	if err != nil {
		return nil, fmt.Errorf("getting backend: %w", err)
	}
	hp, ok := provider.(backend.HookProvider)
	if !ok {
		return nil, fmt.Errorf("backend %q does not support hooks", agent.Backend)
	}
	hr, ok, err := hp.ParseHookEvent(event, payload)
	if err != nil || !ok {
		return nil, err
	}
	if s.backgroundWorkRunning(ctx, name, hr) {
		return nil, nil
	}
	// Decided first, so a failure to record the report can't cost it.
	approval := s.hookApproval(provider, hr)
	r := StatusReport{
		Status:    fromBackendStatus(hr.Status),
		Event:     hr.Event,
		Detail:    hr.Detail,
		Tool:      hr.Tool,
		ToolInput: hr.ToolInput,
		Result:    hr.Result,
		SessionID: hr.SessionID,

		Transcript:  hr.Transcript,
		ClearResult: hr.ClearResult,

		Prompt:  hr.Prompt,
		CallKey: hr.CallKey,
		Agent:   hr.Agent,
	}
	var out []byte
	if hd, ok := provider.(backend.HookDeliverer); ok && deliver && hd.CanDeliver(hr) {
		out, err = s.reportDelivering(ctx, name, r, hd)
	} else {
		out, err = approval, s.Report(ctx, name, r)
	}
	if err != nil || hr.Transcript == "" {
		return out, err
	}
	return out, s.recordTranscript(name, agent.Backend, hr.Transcript)
}

// backgroundWorkRunning reports whether hr is Claude's idle_prompt
// Notification arriving while the turn's Stop said background tasks are
// still running. The notification fires ~60s into any stay at the prompt,
// background work or not; recorded, it would turn "processing, N background
// task(s) running" into idle with no result, which a coordinator waiting
// for idle takes for done whenever native status can't overrule it. The
// task's completion starts a new turn, whose hooks report as usual.
func (s *AgentService) backgroundWorkRunning(ctx context.Context, name string, hr backend.HookReport) bool {
	if hr.Event != "Notification" || hr.Status != backend.StatusIdle {
		return false
	}
	last, err := s.reportedStatus(ctx, name)
	return err == nil && last != nil && last.Status == AgentStatusProcessing && last.Event == "Stop"
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
	info.Held = s.HeldCount(ctx, name)
	if reported != nil {
		info.ReportedStatus, info.ReportedAt = reported.Status, reported.UpdatedAt
		info.SentAt, info.TurnAt = reported.SentAt, reported.TurnAt
		info.pending = reported.pending
	}
	if info.Status == AgentStatusWaiting && len(info.pending) > 0 {
		// Describe the prompt on screen: the oldest unanswered one, as
		// Claude shows them in the order they were raised. The last report
		// may be a later prompt of the same batch, or a hook that fired
		// since (an answered call's PostToolUse, the Stop of a turn whose
		// background subagent is asking).
		p := info.pending[0]
		info.prompt = &p
		info.Event, info.Detail, info.Tool, info.ToolInput = "PermissionRequest", p.Detail, p.Tool, p.Input
		if n := len(info.pending) - 1; n > 0 {
			info.Detail += fmt.Sprintf("; %d more queued", n)
		}
		if nanos, err := strconv.ParseInt(strings.TrimPrefix(p.key, stateKeyPromptPrefix), 10, 64); err == nil {
			info.UpdatedAt = time.Unix(0, nanos)
		}
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

	native, found, queried := s.nativeStatus(ctx, agent)
	if found && native == AgentStatusWaiting && s.deniedSince(agent, reported) {
		// Claude's session list still says waiting for a moment after a
		// deny (which fires no hook), so a send right after deny was refused.
		return reported, reported, nil
	}
	if found {
		info := &StatusInfo{Status: native, Source: StatusSourceNative}
		// Hook detail is only trustworthy while it agrees with the backend;
		// e.g. an interrupted permission prompt fires no hook at all.
		if reported != nil && reported.Status == native {
			info.Event, info.Detail, info.Tool, info.ToolInput, info.UpdatedAt = reported.Event, reported.Detail, reported.Tool, reported.ToolInput, reported.UpdatedAt
		}
		return info, reported, nil
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
			}, reported, nil
		}
		// Backends like agy fire PreToolUse before showing a permission
		// dialog and fire nothing while the dialog is up. When the dialog
		// is on screen after PreToolUse, surface the status as waiting so
		// callers and approve/deny see the pending prompt.
		if reported.Status == AgentStatusProcessing && reported.Event == "PreToolUse" && s.permissionPromptVisible(agent) {
			waiting := *reported
			waiting.Status = AgentStatusWaiting
			if waiting.Detail != "" && !strings.HasPrefix(waiting.Detail, "permission: ") {
				waiting.Detail = "permission: " + waiting.Detail
			}
			return &waiting, reported, nil
		}
		return reported, reported, nil
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
	var sentAt, turnAt time.Time
	for _, e := range entries {
		values[e.Key] = e.Value
		switch e.Key {
		case StateKeyStatus:
			info = &StatusInfo{Status: AgentStatus(e.Value), Source: StatusSourceReport, UpdatedAt: e.UpdatedAt}
		case StateKeySentAt:
			sentAt = e.UpdatedAt
		case StateKeyTurnAt:
			turnAt = e.UpdatedAt
		}
	}
	if info == nil {
		return nil, nil
	}
	info.SentAt, info.TurnAt = sentAt, turnAt
	info.Event = values[StateKeyEvent]
	info.Detail = values[StateKeyDetail]
	info.Tool = values[StateKeyTool]
	info.ToolInput = values[StateKeyToolInput]
	info.pending = promptsIn(entries)
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
	provider, err := s.provider(agent)
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

// deniedLag bounds how long after a deny native status may still say
// waiting before it is believed again. Observed: under a second.
const deniedLag = 5 * time.Second

// deniedSince reports whether the last report is a deny recorded within
// deniedLag that left no prompt pending or on screen: a native "waiting"
// then is the CLI catching up, not a prompt.
func (s *AgentService) deniedSince(agent *storage.Agent, reported *StatusInfo) bool {
	return reported != nil && reported.Event == "denied" && time.Since(reported.UpdatedAt) < deniedLag &&
		len(reported.pending) == 0 && !s.permissionPromptVisible(agent)
}

func (s *AgentService) permissionPromptVisible(agent *storage.Agent) bool {
	provider, err := s.provider(agent)
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
		return AgentStatusUnknown, unloadedBackend(agent)
	}

	output, err := s.tmux.Capture(agent.SessionName, 50, true)
	if err != nil {
		return AgentStatusUnknown, fmt.Errorf("capturing output: %w", err)
	}

	for _, p := range []struct {
		status  AgentStatus
		pattern string
	}{
		{AgentStatusIdle, be.IdlePattern()},
		{AgentStatusProcessing, be.ProcessingPattern()},
	} {
		if p.pattern == "" {
			continue
		}
		matched, err := regexp.MatchString(p.pattern, output)
		if err != nil {
			return AgentStatusUnknown, fmt.Errorf("invalid %s pattern %q: %w", p.status, p.pattern, err)
		}
		if matched {
			return p.status, nil
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

// RespondPermission answers an agent's pending permission prompt. It
// returns the status it found the agent in, which describes the prompt it
// answered.
func (s *AgentService) RespondPermission(ctx context.Context, name string, resp backend.PermissionResponse, opts PermissionOptions) (*StatusInfo, error) {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return nil, err
	}
	info, err := s.StatusInfo(ctx, name)
	if err != nil {
		return nil, err
	}
	if !opts.Force && info.Status != AgentStatusWaiting {
		return nil, fmt.Errorf("agent %s is %s, not waiting on a permission prompt, so there is nothing to answer (agent status %s -v shows what it's doing; --force sends the key anyway)", name, info.Status, name)
	}
	if opts.ExpectTool != "" && info.Tool != opts.ExpectTool {
		return nil, fmt.Errorf("agent %s is waiting on %q, not %q: check the new prompt with agent status %s -v before answering it", name, info.Tool, opts.ExpectTool, name)
	}

	provider, err := s.provider(agent)
	if err != nil {
		return nil, fmt.Errorf("agent %s runs backend %q, which can't answer permission prompts: use agent keys %s@%s to press the keys yourself", name, agent.Backend, name, s.cfg.Host)
	}
	pr, ok := provider.(backend.PermissionResponder)
	if !ok {
		return nil, fmt.Errorf("agent %s runs backend %q, which can't answer permission prompts: use agent keys %s@%s to press the keys yourself", name, agent.Backend, name, s.cfg.Host)
	}
	tool := info.Tool
	if opts.Force {
		tool = "" // answer whatever is on screen as a plain permission prompt
	}
	key, err := pr.PermissionKey(resp, tool)
	if err != nil {
		return nil, fmt.Errorf("agent %s: %w", name, err)
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
			return nil, fmt.Errorf("capturing screen: %w", err)
		}
		if !pr.PermissionPromptVisible(screen) {
			return nil, fmt.Errorf("agent %s has no permission prompt on screen; it may have just been answered (agent status %s -v shows what it's doing; --force sends the key anyway)", name, name)
		}
	}
	if err := s.tmux.SendSpecialKey(agent.SessionName, key); err != nil {
		return nil, fmt.Errorf("sending %q: %w", key, err)
	}

	// Record the expected outcome now: a denied prompt fires no hook, and an
	// approved one only reports once the tool finishes.
	report := StatusReport{Status: AgentStatusProcessing, Event: "approved", Tool: info.Tool}
	if resp == backend.PermissionDeny {
		report = StatusReport{Status: AgentStatusIdle, Event: "denied"}
	}
	if p := info.prompt; p != nil {
		// The answered prompt leaves the queue now, so the next one is
		// described while the approved call runs.
		report.DropPrompts = []string{p.key}
		if resp == backend.PermissionDeny {
			// Escape cancels the rest of that agent's batch too. Denying a
			// subagent's call ends only that call: the turn goes on.
			for _, q := range info.pending[1:] {
				if q.Agent == p.Agent {
					report.DropPrompts = append(report.DropPrompts, q.key)
				}
			}
			if p.Agent != "" {
				report.Status = AgentStatusProcessing
			}
		}
	}
	return info, s.Report(ctx, name, report)
}

// unloadedBackend is the error for an agent whose custom backend this
// process didn't load: custom backends are only known through the
// --backend-config they were spawned with.
func unloadedBackend(agent *storage.Agent) error {
	return fmt.Errorf("agent %s runs backend %q, which isn't built in: pass the --backend-config file it was spawned with (e.g. agent --backend-config <file> status %s)", agent.Name, agent.Backend, agent.Name)
}

// provider returns an instance of agent's backend for the optional
// capabilities it implements, or an error for a custom JSON backend.
func (s *AgentService) provider(agent *storage.Agent) (backend.Provider, error) {
	return s.backends.CreateProvider(agent.Backend, backend.ProviderConfig{})
}

func (s *AgentService) getAgent(ctx context.Context, name string) (*storage.Agent, error) {
	agent, err := s.storage.GetAgent(ctx, name)
	if err != nil {
		return nil, fmt.Errorf("getting agent: %w", err)
	}
	if agent == nil {
		return nil, s.notFound(ctx, name)
	}
	return agent, nil
}

// Agent returns the named agent's record, or an *AgentNotFoundError.
func (s *AgentService) Agent(ctx context.Context, name string) (*storage.Agent, error) {
	return s.getAgent(ctx, name)
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

package service

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/claudereg"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// HookSession is what the processes say about the Claude Code session a
// hook fired in.
type HookSession struct {
	// CLIPID is the Claude Code process that ran the hook: the nearest of the
	// hook's ancestors with a proven registry entry; 0 if none was found.
	CLIPID int
	// Owner is the workgroup agent whose pane runs that process as its
	// session (the shallowest Claude Code process under the pane); "" for a
	// session no agent owns, such as one a person started, or a `claude -p`
	// run from inside an agent's session.
	Owner string
	// SpawnHooked says the process was launched with gomuxai's per-spawn
	// hooks (an agent spawned before the global hook), whose own
	// `agent report --hook` reports it.
	SpawnHooked bool
}

// FindHookSession places the hook process self (its pid), fired for
// sessionID, among the running processes: the Claude Code CLI above it, and
// the agent pane above that. claudePID is $CLAUDE_PID, which Claude Code
// sets for its hooks ("" if unset).
func (s *AgentService) FindHookSession(ctx context.Context, self int, sessionID, claudePID string) (HookSession, error) {
	chain, err := util.ProcAncestors(ctx, self)
	if err != nil {
		return HookSession{}, err
	}
	dir, err := claudereg.Dir()
	if err != nil {
		return HookSession{}, err
	}
	// The CLI that ran the hook is one of its own ancestors, so it runs. It
	// is the nearest whose registry entry is proven, or holds the payload's
	// session (proof it can't give: a start time Claude Code doesn't record),
	// or that Claude Code named in $CLAUDE_PID (its first event can come
	// before it writes its registry file).
	ownCLI := func(pid int) (bool, error) {
		if claudePID != "" && strconv.Itoa(pid) == claudePID {
			return true, nil
		}
		e, found, err := claudereg.ReadPID(dir, pid, claudereg.System{})
		return found && (e.Verdict == claudereg.Live || e.Verdict == claudereg.Unproven && e.SessionID == sessionID), err
	}
	// Any running process with a registry entry above that CLI, below the
	// pane, is another CLI: the one whose session this one started inside.
	otherCLI := func(pid int) (bool, error) {
		e, found, err := claudereg.ReadPID(dir, pid, claudereg.System{})
		return found && (e.Verdict == claudereg.Live || e.Verdict == claudereg.Unproven), err
	}
	panes := func() (map[int]string, map[string]string, error) {
		panes, err := s.tmux.PanePIDs()
		if err != nil {
			return nil, nil, err
		}
		agents, err := s.storage.ListAgents(ctx)
		if err != nil {
			return nil, nil, err
		}
		bySession := map[string]string{}
		for _, a := range agents {
			bySession[a.SessionName] = a.Name
		}
		return panes, bySession, nil
	}
	hs, err := placeHook(chain, ownCLI, otherCLI, panes)
	if err == nil && hs.CLIPID != 0 {
		if args, ok := util.ProcArgs(hs.CLIPID); ok {
			hs.SpawnHooked = spawnHooked(args)
		}
	}
	return hs, err
}

// placeHook finds, in chain (a hook's ancestors, nearest first), the CLI
// that ran it (the nearest process ownCLI accepts) and the agent whose pane
// holds that CLI as its own session: walking on up, the first process that
// is a pane (panes: pane pid -> tmux session; agents: tmux session -> agent),
// unless another CLI (otherCLI) comes first, which makes this one a CLI
// started from inside that one's session. A CLI whose pane is gone (its
// parent died, as when cleanup kills a session) reaches no pane: no agent.
func placeHook(chain []int, ownCLI, otherCLI func(pid int) (bool, error), panes func() (map[int]string, map[string]string, error)) (HookSession, error) {
	at := -1
	for i, pid := range chain {
		ok, err := ownCLI(pid)
		if err != nil {
			return HookSession{}, err
		}
		if ok {
			at = i
			break
		}
	}
	if at < 0 {
		return HookSession{}, nil
	}
	hs := HookSession{CLIPID: chain[at]}
	paneOf, agentOf, err := panes()
	if err != nil {
		return hs, err
	}
	for _, pid := range chain[at+1:] {
		if session, ok := paneOf[pid]; ok {
			hs.Owner = agentOf[session]
			break
		}
		if ok, err := otherCLI(pid); err != nil || ok {
			return hs, err
		}
	}
	return hs, nil
}

// spawnHooked reports whether a Claude Code command line carries gomuxai's
// per-spawn hooks: a --settings whose hooks run `agent report --hook`. The
// words are joined first: on macOS ps splits the settings JSON on spaces.
func spawnHooked(args []string) bool {
	i := slices.Index(args, "--settings")
	return i >= 0 && strings.Contains(strings.Join(args[i+1:], " "), " report --hook")
}

// claudeHookPayload holds what the global hook reads before handing the
// payload on.
type claudeHookPayload struct {
	Event               string `json:"hook_event_name"`
	SessionID           string `json:"session_id"`
	TranscriptPath      string `json:"transcript_path"`
	AgentID             string `json:"agent_id"`
	AgentTranscriptPath string `json:"agent_transcript_path"`
	Reason              string `json:"reason"`
}

// SessionReports returns every session's last report from the global hook,
// by session id.
func (s *AgentService) SessionReports(ctx context.Context) (map[string]*storage.SessionReport, error) {
	return s.storage.SessionReports(ctx)
}

// HandleClaudeHook is the global hook: Claude Code runs it for every event
// of every session (agent hooks install registers it). It records the
// session's report, and when an agent owns the session, the agent's report
// too, as its own hook would; out is what the hook must print (held
// messages, a pre-approval), only ever for an agent's session.
func (s *AgentService) HandleClaudeHook(ctx context.Context, hs HookSession, payload []byte, at time.Time) (out []byte, err error) {
	var p claudeHookPayload
	if err := json.Unmarshal(payload, &p); err != nil {
		return nil, fmt.Errorf("decoding hook payload: %w", err)
	}
	if p.SessionID == "" {
		return nil, fmt.Errorf("a %s payload with no session_id", p.Event)
	}
	if hs.SpawnHooked {
		return nil, nil // its spawn's own hook reports it
	}
	if err := s.recordSessionReport(ctx, p, payload, at); err != nil {
		return nil, err
	}
	if hs.Owner == "" {
		return nil, nil
	}
	if p.AgentID == "" {
		// Seen first, so this very Stop may hand over held messages.
		if err := s.setState(ctx, hs.Owner, StateKeyHookedSession, p.SessionID); err != nil {
			return nil, err
		}
	}
	return s.HandleHook(ctx, hs.Owner, "", "", payload)
}

// recordSessionReport stores the report the event makes for the session it
// is about: the subagent's (agent-<id>, as agent sessions names it) for a
// subagent's event, else the session's.
func (s *AgentService) recordSessionReport(ctx context.Context, p claudeHookPayload, payload []byte, at time.Time) error {
	r := &storage.SessionReport{SessionID: p.SessionID, Harness: "claude", Event: p.Event, At: at, Transcript: p.TranscriptPath}
	if p.AgentID != "" {
		r.SessionID, r.Transcript = "agent-"+p.AgentID, p.AgentTranscriptPath
		r.Status = string(AgentStatusProcessing)
		if p.Event == "SubagentStop" {
			r.Status, r.EndReason = string(AgentStatusEnded), string(backend.EndFinished)
		}
		return s.storage.RecordSessionReport(ctx, r)
	}
	if p.Event == "SessionEnd" && p.Reason == "clear" {
		// The CLI goes on in a new session (its SessionStart follows).
		r.Status, r.EndReason, r.Detail = string(AgentStatusEnded), string(backend.EndReplaced), "/clear"
		return s.storage.RecordSessionReport(ctx, r)
	}
	hr, ok, err := namedagents.NewClaudeBackend().ParseHookEvent(p.Event, payload)
	if err != nil || !ok {
		return err
	}
	r.Status, r.EndReason, r.Detail = string(fromBackendStatus(hr.Status)), string(hr.EndReason), hr.Detail
	return s.storage.RecordSessionReport(ctx, r)
}

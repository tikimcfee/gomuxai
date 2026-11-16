package service

import (
	"context"
	"path/filepath"

	"github.com/tikimcfee/gomuxai/internal/claudereg"
)

// Who runs a command, and whose conversation a background session is.
//
// AGENT_NAME and AGENT_HOST are set in an agent's tmux session, and every
// process started from it inherits them: a terminal daemon a tool started,
// a shell it opened, and Claude Code's background-session daemon, which is
// one per user (per config dir), started by whichever CLI first needs it,
// with that CLI's environment. Claude Code 2.1.325 hands a session sent to
// the background (/bg) to a spare host the daemon started ahead of time, so
// agent A's conversation runs, and runs its tools, with the AGENT_NAME of
// agent B, whose /bg started the daemon (friction #48: A's messages went
// out as "[from B@host]"). So the sender is found from what the processes
// and Claude Code's records say, and AGENT_NAME counts only where they say
// nothing.

// CallerAgent returns the agent whose session runs the process whose
// ancestors are chain (the process itself first, then its parent, and so
// on): walking up, the first that is
//
//   - an agent's pane (its tmux session's first pane process): that agent
//     (a pane this store records no agent for may be another agent
//     binary's: the processes don't say);
//   - a Claude Code background session's process: the agent whose
//     conversation it carries on (backgroundOwner), or no agent, never the
//     agent whose pane the daemon happens to run under.
//
// A Claude Code CLI on the way that is no background session (a `claude
// -p` a tool started, say) is passed over: its pane's agent ran it. known
// is false when the walk reaches neither (a process outside this tmux
// server, or detached from its session): the processes don't say. A
// background session no agent owns is known: no agent's, whatever
// AGENT_NAME its process inherited from the CLI that started the daemon.
func (s *AgentService) CallerAgent(ctx context.Context, chain []int) (name string, known bool, err error) {
	paneOf, err := s.tmux.PanePIDs()
	if err != nil {
		return "", false, err
	}
	agentOf, err := s.agentsBySession(ctx)
	if err != nil {
		return "", false, err
	}
	dir, err := claudereg.Dir()
	if err != nil {
		return "", false, err
	}
	for _, pid := range chain {
		if session, ok := paneOf[pid]; ok {
			name := agentOf[session]
			return name, name != "", nil
		}
		e, found, err := claudereg.ReadPID(dir, pid, claudereg.System{})
		if err != nil || !found || e.Kind != claudereg.KindBackground ||
			e.Verdict != claudereg.Live && e.Verdict != claudereg.Unproven {
			continue
		}
		name, err := s.backgroundOwner(ctx, e.SessionID)
		return name, err == nil, err
	}
	return "", false, nil
}

// agentsBySession maps each agent's tmux session to its name.
func (s *AgentService) agentsBySession(ctx context.Context) (map[string]string, error) {
	agents, err := s.storage.ListAgents(ctx)
	if err != nil {
		return nil, err
	}
	out := map[string]string{}
	for _, a := range agents {
		out[a.SessionName] = a.Name
	}
	return out, nil
}

// backgroundOwner returns the agent whose conversation Claude Code
// background session sessionID carries on, or "" if none does: the agent
// already on that session (an earlier hook or status check moved it
// there), or else the agent whose transcript records the move into it (its
// continued-in line, followed through any later moves). The transcript is
// the conversation's own record: the pane's CLI may have exited when the
// session went to the background (/bg frees the terminal), and the
// background process runs wherever the daemon put it.
func (s *AgentService) backgroundOwner(ctx context.Context, sessionID string) (string, error) {
	agents, err := s.storage.ListAgents(ctx)
	if err != nil {
		return "", err
	}
	for _, a := range agents {
		if s.stateValue(ctx, a.Name, StateKeySessionID) == sessionID {
			return a.Name, nil
		}
	}
	for _, a := range agents {
		if movedInto(s.stateValue(ctx, a.Name, StateKeyTranscript), sessionID) {
			return a.Name, nil
		}
	}
	return "", nil
}

// movedInto reports whether the conversation whose transcript is at path
// moved into session id, directly or through later moves (each session's
// transcript sits beside the one it continued from).
func movedInto(path, id string) bool {
	seen := map[string]bool{}
	for range 16 {
		if path == "" || seen[path] {
			return false
		}
		seen[path] = true
		next := claudereg.ContinuedIn(path)
		switch next {
		case "":
			return false
		case id:
			return true
		}
		path = filepath.Join(filepath.Dir(path), next+".jsonl")
	}
	return false
}

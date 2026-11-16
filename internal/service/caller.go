package service

import (
	"context"

	"github.com/tikimcfee/gomuxai/internal/claudereg"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Who runs a command, and whose conversation a background session is.
//
// AGENT_NAME and AGENT_HOST are set in an agent's tmux session, and every
// process started from it inherits them: a terminal daemon a tool started,
// a shell it opened, and Claude Code's background-session daemon, which is
// one per user and a child of whichever CLI started it first. From Claude
// Code 2.1.325 that daemon hands a moved session to a spare host it started
// ahead of time, so agent A's conversation can run, and run its tools, in a
// process under agent B's pane with B's AGENT_NAME (friction #48: A's
// messages went out as "[from B@host]"). So the sender is found from what
// the processes and Claude Code's records say, and AGENT_NAME counts only
// where they say nothing.

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
// there), or else the agent whose pane's CLI held the conversation that
// moved into it (claudereg.Continuation, which follows the moves recorded
// in the transcripts).
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
	panes, err := s.tmux.PanePIDs()
	if err != nil {
		return "", err
	}
	snap, err := claudereg.ReadSystem()
	if err != nil {
		return "", err
	}
	tree, err := util.ReadProcTree(ctx)
	if err != nil {
		return "", err
	}
	agentOf := map[string]string{}
	for _, a := range agents {
		agentOf[a.SessionName] = a.Name
	}
	for panePID, session := range panes {
		name := agentOf[session]
		if name == "" {
			continue
		}
		e, found, err := snap.UnderPane(tree, panePID)
		if err != nil || !found {
			continue
		}
		if moved, ok := snap.Continuation(e, claudereg.FindTranscript); ok && moved.SessionID == sessionID {
			return name, nil
		}
	}
	return "", nil
}

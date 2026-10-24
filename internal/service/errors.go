package service

import (
	"context"
	"fmt"

	"github.com/tikimcfee/gomuxai/internal/util"
)

// AgentNotFoundError is returned for a name that isn't an agent on this
// host. Its message says what to use instead: the agent whose tmux session
// the name is, close names, or how many agents there are and how to list
// them.
type AgentNotFoundError struct {
	Name    string
	Host    string
	Count   int      // agents on the host
	Similar []string // close agent names
	// SessionOf is the agent whose tmux session Name is, if it is one.
	SessionOf string
	// Remote is set when the request came over the bus, so listing the
	// agents takes --host.
	Remote bool
}

func (e *AgentNotFoundError) Error() string {
	msg := fmt.Sprintf("agent %q not found", e.Name)
	if e.Host != "" {
		msg += " on host " + e.Host
	}
	switch {
	case e.SessionOf != "":
		return fmt.Sprintf("%s: that is the tmux session of agent %s, so use the agent name %s", msg, e.SessionOf, e.SessionOf)
	case len(e.Similar) > 0:
		return fmt.Sprintf("%s; did you mean %s?", msg, util.OrList(e.Similar, nil))
	case e.Count == 0:
		return msg + " (it has no agents; agent spawn starts one)"
	}
	list := "agent list"
	if e.Remote && e.Host != "" {
		list += " --host " + e.Host
	}
	agents := "agents"
	if e.Count == 1 {
		agents = "agent"
	}
	return fmt.Sprintf("%s (%d %s there: %s)", msg, e.Count, agents, list)
}

// notFound builds the error for name, which has no agent record.
func (s *AgentService) notFound(ctx context.Context, name string) error {
	e := &AgentNotFoundError{Name: name, Host: s.cfg.Host}
	agents, err := s.storage.ListAgents(ctx)
	if err != nil {
		return e
	}
	e.Count = len(agents)
	names := make([]string, len(agents))
	for i, a := range agents {
		names[i] = a.Name
		if a.SessionName == name {
			e.SessionOf = a.Name
		}
	}
	e.Similar = util.Similar(name, names)
	return e
}

// remoteErr marks an AgentNotFoundError as answering a request from another
// host, whose agent list takes --host.
func remoteErr(err error) error {
	if nf, ok := err.(*AgentNotFoundError); ok {
		c := *nf
		c.Remote = true
		return &c
	}
	return err
}

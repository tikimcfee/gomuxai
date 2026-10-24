package service

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// agentInstructions is the briefing every spawned agent gets, so it knows it
// is one of several agents and how to reach the others. Backends deliver it
// through their own mechanism (backend.InstructionsProvider).
func (s *AgentService) agentInstructions(name, spawnedBy string) string {
	bin := "agent"
	if s.cfg.AgentBinary != "" {
		bin = s.cfg.AgentBinary
	}
	by := "a user"
	if spawnedBy != "" {
		by = fmt.Sprintf("agent %q", spawnedBy)
	}
	var b strings.Builder
	fmt.Fprintf(&b, `# Working with other agents (gomuxai)

You are agent %q on host %q, started by %s through gomuxai. Other AI agents
may be running alongside you, on this host or others. You coordinate with them
through the `+"`agent`"+` command. Run it by its full path, %s, since your
shell's PATH may not include it:

    agent list                         # agents on this host; --host H for another host
    agent send NAME "message"          # message an agent (NAME@HOST on another host)
    agent status NAME -v               # what an agent is doing right now
    agent wait NAME --for idle --next --timeout 10m   # wait for its next turn to finish
    agent state get NAME _result       # its latest reply

Messages from other agents start with "[from NAME@HOST]". The sender can't see
your terminal: to answer, send your reply back with `+"`agent send NAME@HOST \"...\"`"+`.
Keep messages short and self-contained, and reply once per message unless asked
for more, so agents don't talk past each other. Never report another agent's
answer before its message has actually arrived; wait for it (or use agent wait).

Your status (idle, working, waiting on a permission prompt) is reported
automatically. Use `+"`agent`"+` rather than tmux to reach other agents.
`, name, s.cfg.Host, by, bin)
	return b.String()
}

// writeInstructions saves the briefing in the agent's own directory and
// returns its path.
func (s *AgentService) writeInstructions(name, spawnedBy string) (string, error) {
	dir := s.cfg.AgentDir(name)
	if err := os.MkdirAll(dir, 0700); err != nil {
		return "", fmt.Errorf("creating agent dir: %w", err)
	}
	path, err := filepath.Abs(filepath.Join(dir, "instructions.md"))
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(path, []byte(s.agentInstructions(name, spawnedBy)), 0600); err != nil {
		return "", fmt.Errorf("writing instructions: %w", err)
	}
	return path, nil
}

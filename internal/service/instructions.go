package service

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/util"
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
    agent state get NAME _result       # its latest reply (agents on this host only)

Messages from other agents start with "[from NAME@HOST]". The sender can't see
your terminal: to answer, send your reply back with `+"`agent send NAME@HOST \"...\"`"+`.
Keep messages short and self-contained, and reply once per message unless asked
for more, so agents don't talk past each other. Never report another agent's
answer before its message has actually arrived; wait for it (or use agent wait).

Your status (idle, working, waiting on a permission prompt) is reported
automatically. Use `+"`agent`"+` rather than tmux to reach other agents.
`, name, s.cfg.Host, by, bin)
	fmt.Fprintf(&b, `
## Helpers

For parallel or long-running work that a person may want to watch or steer,
spawn a helper: it runs in its own tmux session, where they can see it. Your
CLI's built-in subagents are still fine for quick lookups. Name helpers
<yourname>-<role>:

    agent spawn %[1]s-review --backend claude   # starts in the directory you run it from
    agent send %[1]s-review "the task"
    agent wait %[1]s-review --for idle --next --timeout 30m
    agent state get %[1]s-review _result
    agent cleanup %[1]s-review

Clean up your helpers when you're done with them. You can clean up only your
own helpers and theirs; `+"`agent list`"+` shows each agent's parent.
`, name)
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

// writeLaunchScript saves the launch command in the agent's directory and
// returns the short command that runs it. Typing the full command into the
// new session's shell can lose its end: until the shell's line editor is up,
// the terminal holds typed input in canonical mode, which macOS caps at 1024
// bytes (MAX_CANON), and Claude's hook settings alone run past that.
func (s *AgentService) writeLaunchScript(name, launchCmd string) (string, error) {
	dir := s.cfg.AgentDir(name)
	if err := os.MkdirAll(dir, 0700); err != nil {
		return "", fmt.Errorf("creating agent dir: %w", err)
	}
	path, err := filepath.Abs(filepath.Join(dir, "launch.sh"))
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(path, []byte(launchCmd+"\n"), 0600); err != nil {
		return "", fmt.Errorf("writing launch script: %w", err)
	}
	return "sh " + util.ShellQuote(path), nil
}

// agentSafeCommands are the agent subcommands a spawned agent may run without
// a permission prompt: they read status or deliver messages that the
// recipient's own permission prompts still guard. approve/deny, keys, and
// send --force are left out: they would let one agent answer another
// agent's prompts, which is the user's call. So are spawn and cleanup, which
// start and stop agents: the user approves each one.
var agentSafeCommands = []string{"send", "list", "status", "wait", "events", "state get"}

// allowedCommandPrefixes are agentSafeCommands as the agent may type them:
// bare (on PATH) or by the full path the briefing gives.
func (s *AgentService) allowedCommandPrefixes() []string {
	var prefixes []string
	for _, sub := range agentSafeCommands {
		prefixes = append(prefixes, "agent "+sub)
		if s.cfg.AgentBinary != "" {
			prefixes = append(prefixes, s.cfg.AgentBinary+" "+sub)
		}
	}
	return prefixes
}

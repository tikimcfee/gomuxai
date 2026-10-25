package service

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/allowcmd"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// heldBriefing tells an agent on backendName how messages sent while it
// works reach it (see held.go).
func heldBriefing(backendName string) string {
	if backendName == "agy" {
		return `A message sent to you while you're working is held until your current step
(a model call and its tools) ends, then handed to you as a new prompt,
starting with its "[from ...]" tag, in the middle of your turn: it's a
message like any other, so act on it and reply as asked. Likewise, a message
you send to a busy agent reaches it when its turn or step ends;
` + "`agent send NAME --now \"...\"`" + ` types it in at once instead.
`
	}
	return `A message sent to you while you're working is held until your turn ends, then
handed to you as Stop hook feedback ("A message arrived while you were
working: ..."): it's a message like any other, so act on it and reply as
asked. Likewise, a message you send to a busy agent reaches it when its turn
ends; ` + "`agent send NAME --now \"...\"`" + ` types it in at once instead, to interrupt.
`
}

// agentInstructions is the briefing every spawned agent gets, so it knows it
// is one of several agents and how to reach the others. Backends deliver it
// through their own mechanism (backend.InstructionsProvider).
func (s *AgentService) agentInstructions(name, spawnedBy, backendName string) string {
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
shell's PATH may not include it. These commands run without a permission
prompt when each is one plain command: not through a shell variable
($A send ...), and not chained or piped:

    agent list                         # agents on this host; --host H for another host
    agent send NAME "message"          # message an agent (NAME@HOST on another host)
    agent status NAME -v               # what an agent is doing right now
    agent capture NAME --lines 50      # its screen, e.g. to see why it's stuck
    agent logs NAME                    # what it did: prompts, tool calls, replies
    agent wait NAME --for idle,waiting --next --timeout 10m   # its next turn ends, or it needs a person
    agent state get NAME _result       # its latest reply (claude agents on this host)

Messages from other agents start with "[from NAME@HOST]". The sender can't see
your terminal: to answer, send your reply back with `+"`agent send NAME@HOST \"...\"`"+`.
Keep messages short and self-contained, and reply once per message unless asked
for more, so agents don't talk past each other. Never report another agent's
answer before its message has actually arrived; wait for it (or use agent wait).
A message starting with "[from a person on HOST]" or
"[from a Claude session on HOST]" came from someone with no agent record (a
person, or an AI session a person started), so there is no agent to send to:
answer it in your reply, as you would a prompt typed into your terminal.

%s
Your status (idle, working, waiting on a permission prompt) is reported
automatically. Use `+"`agent`"+` rather than tmux to reach other agents.
`, name, s.cfg.Host, by, bin, heldBriefing(backendName))
	fmt.Fprintf(&b, `
## Helpers

For parallel or long-running work that a person may want to watch or steer,
spawn a helper: it runs in its own tmux session, where they can see it. Your
CLI's built-in subagents are still fine for quick lookups. Name helpers
<yourname>-<role>:

    agent spawn %[1]s-review --backend claude   # starts in the directory you run it from
    agent send %[1]s-review "the task"
    agent wait %[1]s-review --for idle,waiting --next --timeout 30m
    agent state get %[1]s-review _result
    agent cleanup %[1]s-review

With several helpers, wait on them in one command, which prints a line as
each one's turn ends (--any returns at the first):

    agent wait %[1]s-api %[1]s-db --for idle,waiting --next --timeout 30m

Clean up your helpers when you're done with them. You can clean up only your
own helpers and theirs; `+"`agent list`"+` shows each agent's parent.
`, name)
	b.WriteString(`
## When the agent tool gets in your way

If ` + "`agent`" + ` confuses you, an error doesn't say what to do, or it lacks
something you need, file it, then carry on with your work. Check
` + "`agent friction list`" + ` first, and ` + "`agent friction +1 ID`" + ` a matching entry instead:

    agent friction add "summary" --kind help|error|missing|bug|docs|other
`)
	return b.String()
}

// writeInstructions saves the briefing in the agent's own directory and
// returns its path.
func (s *AgentService) writeInstructions(name, spawnedBy, backendName string) (string, error) {
	dir := s.cfg.AgentDir(name)
	if err := os.MkdirAll(dir, 0700); err != nil {
		return "", fmt.Errorf("creating agent dir: %w", err)
	}
	path, err := filepath.Abs(filepath.Join(dir, "instructions.md"))
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(path, []byte(s.agentInstructions(name, spawnedBy, backendName)), 0600); err != nil {
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
// a permission prompt: they read status, screens, transcripts, sessions, or
// history (a coordinator needs capture and logs to see why a helper is stuck,
// and sessions to find its peers), deliver
// messages that
// the recipient's own permission prompts still guard, or file and vote on
// friction, which only writes a note. approve/deny, keys, and send --force
// are left out: they would let one agent answer another agent's prompts,
// which is the user's call. So are spawn and cleanup, which start and stop
// agents: the user approves each one. So are friction accept and resolve,
// which are for the people working on the tool.
var agentSafeCommands = []string{
	"send", "deliver", "list", "status", "wait", "events", "capture", "logs", "sessions", "transcript", "state get", "history",
	"friction add", "friction +1", "friction list", "friction show",
}

// hookApproval is the hook output that approves hr's shell command without
// a prompt, or nil. It approves what the allow rules can't match, since they
// match the command's text: this binary by any path or symlink, or with -q
// before the subcommand. Only one plain run of it with a subcommand in
// agentSafeCommands qualifies (see allowcmd); anything else is left to the
// normal permission flow, so nothing is ever denied here.
func (s *AgentService) hookApproval(provider backend.Provider, hr backend.HookReport) []byte {
	ha, ok := provider.(backend.HookAllower)
	if !ok || hr.Command == "" || s.cfg.AgentBinary == "" {
		return nil
	}
	c := allowcmd.Checker{Program: s.cfg.AgentBinary, Subcommands: agentSafeCommands}
	if c.Check(hr.Command, hr.Cwd) != nil {
		return nil
	}
	return ha.AllowOutput("gomuxai: a safe agent command, pre-allowed for spawned agents")
}

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

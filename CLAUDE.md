# CLAUDE.md

You are an agent running in a tmux session. This file tells you how to coordinate with other agents.

---

## The Golden Rule

**The `agent` binary is your only interface to the outside world.**

Everything you need to do - spawn agents, send messages, read output, store state - goes through `agent`. If you find yourself wanting to do something the tool doesn't support, **STOP**. Don't hack around it. Report the limitation.

```bash
# YES - Use the tool
agent send helper "analyze this"
agent state set myname status "blocked"

# NO - Don't bypass the tool
echo "message" > /tmp/some-hack.txt  # BAD
tmux send-keys -t session "text"     # BAD - use agent keys
```

---

## Core Commands

### Spawning Agents

```bash
agent spawn <name> --backend claude [task-file]
```

**Flags:**
- `--backend <name>` - Backend to use (required). Built-in: `claude`, `aider`
- `--backend-config <file>` - Path to custom backend JSON config (alternative to `--backend`)
- `--task <name>` - Reference a task from the DB task repository (see Tasks section)
- `--skip-permissions` - Pass `--dangerously-skip-permissions` to Claude CLI

**Behavior:**
- Task file argument reads from filesystem; `--task` flag reads from DB
- The `AGENT_NAME` env var is automatically set in the spawned session
- You'll need to `send` the first message yourself (spawn doesn't auto-send)

```bash
# Spawn and give it work
agent spawn helper --backend claude
agent send helper "Read task at tasks/research.md and execute it"

# Spawn with a stored task from DB
agent spawn helper --backend claude --task research-auth

# Spawn with auto-approve (use with caution)
agent spawn auto-worker --backend claude --skip-permissions
```

### Sending Messages

**Two modes. Pick the right one.**

| Mode | Command | When |
|------|---------|------|
| Immediate | `agent send <name> "msg"` | Interrupt now, urgent |
| Queued | `agent queue <name> "msg"` | Wait until idle, polite |

```bash
# Urgent - sends immediately, may interrupt
agent send helper "STOP - there's a critical bug"

# Polite - waits for idle prompt
agent queue helper "When you're done, please review auth.go"

# Queue with explicit sender (for coordination/tracking)
agent queue helper --from coordinator "Priority task incoming"
```

**Queue flags:**
- `--from <sender>` - Sender ID. Defaults to `AGENT_NAME` env var, then "user"

**Delivery by status:** `idle` and `error` deliver (`error` is a failed turn, e.g. a rate limit; the agent is back at its prompt). `processing` and `waiting` hold the message. `exited` fails it, as does `unknown` after 5 checks in a row.

**Wart:** `queue` requires `agent watch` running somewhere. If the watcher isn't running, messages sit forever.

### Reading Output

```bash
agent capture <name> --lines 100
```

Returns terminal output, ANSI codes stripped. Use `--raw` to keep them.

### Checking Status

```bash
agent status <name>        # e.g. "helper: waiting"
agent status <name> -v     # also: source, last event, detail
```

Returns: `idle`, `processing`, `waiting` (blocked on a human, e.g. a permission prompt), `error`, `exited`, or `unknown`.

For `claude` and `agy` agents, status comes from the CLI itself: spawn wires lifecycle hooks to `agent report --hook` (and for `claude`, `agent status` also checks `claude agents --json`). No terminal scraping once hooks have fired. Other backends fall back to regex scraping, which can say `unknown`.

When a Claude agent finishes a turn, its final reply is stored for you:

```bash
agent state get helper _result
```

### Answering Permission Prompts

```bash
agent status helper -v                  # detail: permission: Bash: rm -rf build / tool: Bash
agent approve helper --expect Bash      # yes, once, only if it's still the Bash prompt
agent approve helper --always           # yes, and don't ask again (meaning varies by dialog)
agent deny helper                       # no (for Claude/agy this cancels the turn; follow up with send)
```

**Read the detail before approving.** `approve`/`deny` refuse unless the agent is `waiting`, and refuse dialogs that are questions for the user rather than permission prompts (Claude's `AskUserQuestion`, `ExitPlanMode`; agy's `ask_question`): answer those with `send`/`keys`. `--expect <tool>` guards against a different prompt having replaced the one you inspected. Right before the keypress they also check that the prompt is still on screen, because Claude's deny key is Escape, and Escape on a turn that is already running (someone else answered the prompt first) cancels it. A prompt answered within the last few milliseconds before the keypress can still slip through. `--force` overrides the checks. Note: `agy` has no `PermissionRequest`/`waiting` hook event (its status remains `processing` after `PreToolUse`), so `agent approve`/`agent deny` on an `agy` agent requires `--force`.

### Reporting Your Own Status

`claude` and `agy` agents report automatically. Anything else can report by hand:

```bash
agent report processing "running the test suite"
agent report idle
```

The name defaults to `$AGENT_NAME`; use `--as <name>` otherwise.

---

## Data Storage (All in SQLite)

Everything is in one database. No filesystem hunting.

### Tasks

Store reusable task definitions:

```bash
agent task set research-auth -f tasks/research-auth.md   # From file
agent task set quick-task "Do X, Y, Z"                   # Inline
agent task get research-auth                              # Retrieve
agent task list                                           # List all
agent task clear research-auth                            # Remove
```

Tasks exist independently of agents. Multiple agents can reference the same task.

### State (Key-Value)

Store your working state:

```bash
agent state set myname status "analyzing auth module"
agent state set myname progress "3/5 files done"
agent state get myname status
agent state list myname
agent state clear myname
```

**Use this for coordination.** Other agents can read your state:

```bash
# Helper writes its status
agent state set helper status "done"
agent state set helper result "Found 3 vulnerabilities"

# Coordinator reads it
agent state get helper status
agent state get helper result
```

**Behaviors:**
- State persists until explicitly cleared or the agent is cleaned up (cleanup deletes state and logs)
- Reading a non-existent key prints "No state found for X.Y" (not an error)

### Logs

Query captured output from DB (ANSI-stripped):

```bash
agent logs <name>                  # Last 50 entries
agent logs <name> --lines 100      # More entries
agent logs <name> --since 5m       # Since 5 minutes ago
```

### Capture vs Logs: When to Use Which

| Method | Source | Use When |
|--------|--------|----------|
| `capture` | Live terminal buffer | Need current screen state, checking prompts |
| `logs` | DB (persistent) | Need historical output, timestamped entries |

**Capture** is ephemeral - it's whatever's in the tmux scrollback right now. Good for status checks and reading recent output.

**Logs** are persistent - entries are stored with timestamps as the agent runs. Good for debugging what happened, auditing, or recovering context after the fact.

---

## Agent Identity

When an agent is spawned, the `AGENT_NAME` environment variable is set in its session. This is useful for:

1. **Self-identification** - Agent knows its own name without being told
2. **Queue sender defaults** - `agent queue` uses `AGENT_NAME` as default `--from`
3. **State namespacing** - Conventionally use your own name for state keys

```bash
# Inside a spawned agent session, AGENT_NAME is set
echo $AGENT_NAME  # "helper" (or whatever name was used in spawn)

# Queue uses it automatically
agent queue coordinator "I'm done"  # --from defaults to $AGENT_NAME
```

---

## Low-Level Tmux Control

For when you need raw terminal access.

**Naming convention:** When you spawn an agent named `helper`, the tmux session is named `agent-helper`. High-level commands use agent names; low-level commands use session names.

```bash
agent session new <name>           # Create raw session (no CLI tool)
agent session kill <name>          # Kill session
agent session list                 # List all sessions

agent keys <session> "text"        # Send text + Enter
agent keys <session> "text" --no-enter  # Send text only
agent key <session> c-c            # Send Ctrl+C
agent key <session> Enter          # Send Enter

agent clear <session>              # Clear screen + scrollback
```

**Special keys:** `Enter`, `Escape`, `Tab`, `Space`, `c-c`, `c-d`, `c-z`, `c-l`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PgUp`, `PgDn`, `F1`-`F12`

---

## Rules

### 1. Always Use the Tool

Don't shell out to tmux directly. Don't write to random files. Don't hack around limitations. The tool is the interface.

### 2. Report What You Can't Do

If the tool doesn't support something you need:

```bash
agent state set myname blocked "Need feature X - agent tool doesn't support it"
```

Then stop. Don't improvise.

### 3. Clean Up After Yourself

```bash
# When done with helpers
agent cleanup helper

# When cleaning up ALL your spawned agents
agent cleanup --all --as myname

# Clean up all except specific agents
agent cleanup --all --as myname --except builder,tester
```

**Cleanup flags:**
- `--all` - Clean up all agents (requires `--as`)
- `--as <name>` - Your agent name (prevents self-termination)
- `--except <names>` - Comma-separated list of agents to skip

### 4. Use State for Coordination

Don't rely on capturing output to coordinate. State is explicit:

```bash
# BAD - fragile coordination via output parsing
output=$(agent capture helper --lines 5)
if [[ "$output" == *"done"* ]]; then ...

# GOOD - explicit coordination via state
status=$(agent state get helper status)
if [[ "$status" == "done" ]]; then ...
```

**The distinction:**
- **Coordination** (signaling between agents) → Use state. Explicit, reliable.
- **Diagnostics** (understanding what's happening) → Use capture. Visual inspection, debugging.

### 5. Check Before Spawning

```bash
agent list
```

Is there already an agent doing what you need? Reuse it instead of spawning duplicates.

---

## Workflow Example

```bash
# 1. Spawn a helper
agent spawn analyzer --backend claude

# 2. Give it work
agent send analyzer "Analyze auth.go for security issues"

# 3. Let it work, check periodically
agent status analyzer  # idle? processing?

# 4. When idle, read results
agent capture analyzer --lines 200

# 5. Or check its state (if it wrote any)
agent state get analyzer result

# 6. Send follow-up if needed
agent queue analyzer "Also check session.go"

# 7. When done, cleanup
agent cleanup analyzer
```

---

## Common Patterns

### Coordinator + Specialists

```bash
# Spawn specialists
agent spawn api-analyzer --backend claude
agent spawn db-analyzer --backend claude

# Assign tasks
agent send api-analyzer "Analyze API routes, store findings in state"
agent send db-analyzer "Analyze database queries, store findings in state"

# Wait for both (poll state)
while true; do
  api_done=$(agent state get api-analyzer status)
  db_done=$(agent state get db-analyzer status)
  [[ "$api_done" == "done" && "$db_done" == "done" ]] && break
  sleep 5
done

# Collect results
agent state get api-analyzer findings
agent state get db-analyzer findings

# Cleanup
agent cleanup api-analyzer
agent cleanup db-analyzer
```

### Handoff Pattern

```bash
# Agent A finishes, signals ready
agent state set agent-a status "done"
agent state set agent-a output-file "results.json"
agent queue agent-b "Agent A is done. Read results from state."
```

---

## Warts & Limitations

| Issue | Workaround |
|-------|------------|
| `queue` needs watcher running | Start `agent watch` in background (use `--interval` to tune) |
| `status` can return `unknown` (non-Claude backends) | Check state instead, or capture output |
| Spawn doesn't auto-send task | Use `send` after spawn |
| No streaming capture | Poll with `capture` periodically |
| Backend always required | Use `--backend claude` or config file |
| Watch interval defaults to 2s | Use `agent watch --interval 5s` for less frequent polling |

---

## Data Location

All data lives next to the binary:

```
.gomuxai/
└── gomuxai.db    # Everything: agents, messages, tasks, logs, state
```

Delete the folder, delete all traces. Portable.

---

## If Something Goes Wrong

### Diagnostic Steps

1. Check the agent exists: `agent list`
2. Check session exists: `agent session list`
3. Check status: `agent status <name>`
4. Capture output: `agent capture <name> --lines 200`
5. Check state: `agent state list <name>`

### Common Failure Modes and Recovery

**Agent not responding (status: unknown)**
```bash
# Check if it's actually stuck or just busy (uses agent name)
agent capture helper --lines 50

# If stuck at a prompt you don't recognize, try sending Enter (uses session name)
agent key agent-helper Enter

# If truly hung, send interrupt
agent key agent-helper c-c
```

Note: `capture` uses agent name (`helper`), but `key` uses session name (`agent-helper`).

**Session exists but agent record doesn't (orphaned session)**
```bash
# List raw tmux sessions
agent session list

# Kill the orphan directly
agent session kill agent-helper
```

**Agent record exists but session doesn't (stale record)**
```bash
# Cleanup will handle this - it checks session existence
agent cleanup helper
```

**Queue messages not being delivered**
```bash
# Check if watcher is running (it should print periodic status)
# If not running, start it:
agent watch

# Or run in background with custom interval
agent watch --interval 5s &
```

Multiple watchers can run safely - they'll just duplicate delivery checks.

**Agent spawned but CLI didn't start**
```bash
# Capture to see what happened
agent capture helper --lines 50

# Maybe the backend CLI isn't installed or PATH issue
# Kill and retry with corrected environment
agent cleanup helper
```

### When to Use Escape Hatches

The low-level commands exist for recovery:

| Situation | Escape Hatch |
|-----------|--------------|
| Need to send Ctrl+C to interrupt | `agent key <session> c-c` |
| Need to clear garbled terminal | `agent clear <session>` |
| Need raw session without agent record | `agent session new <name>` |
| High-level command failing | Drop to `agent keys` / `agent capture` |

### Reporting Blockers

If the tool errors or doesn't do what you expect, **set your state to blocked and report it**:

```bash
agent state set myname status "BLOCKED"
agent state set myname error "agent send failed with: <error>"
```

Then stop. Don't improvise workarounds that bypass the tool.

---

## Development Notes

This section explains implementation details for developers working on the tool.

### Architecture: Why Tmux?

The tool is a coordination layer over tmux sessions. Each agent runs in its own tmux session, which provides:

1. **Process isolation** - Agents can crash without taking down others
2. **Persistent terminals** - Sessions survive disconnects
3. **Scriptable I/O** - Send keys, capture output programmatically
4. **Escape hatches** - Low-level control when abstractions fail

The high-level commands (`spawn`, `send`, `queue`, `status`) are conveniences. The low-level commands (`session`, `keys`, `key`, `capture`) expose the underlying tmux operations for when you need direct control.

### Status Detection Internals

`AgentService.StatusInfo` (`internal/service/status.go`) resolves status from three sources, most authoritative first:

1. **native**: the backend CLI's own session status (`NativeStatusProvider`). Claude: `claude agents --json`, matched on the `--session-id` spawn assigns.
2. **report**: the last status written by hooks or `agent report`, stored in reserved state keys (`_status`, `_event`, `_detail`, `_tool`, `_result`, `_session_id`, `_spawn_id`), written in one transaction per report.
3. **scrape**: regex over captured terminal output, for backends with neither.

Native wins when it disagrees with the last report: an interrupted Claude permission prompt fires no hook at all, so the report would stay stuck at `waiting`. When the CLI answers but no longer lists the session while the last report still says it's alive, status is `exited`: the process died without its `SessionEnd` hook (crash, `kill -9`).

**Claude hooks** (`internal/backend/namedagents/claude_hooks.go`) are injected at launch with `claude --settings '<json>'`, so nothing is written into the agent's project. Each hook runs `<agent binary> report --hook --as <name> --spawn-id <id>`, which reads the payload on stdin, never writes stdout, and exits 0 once the CLI has initialized (a broken DB dir still exits 1). `--spawn-id` drops reports from an earlier agent of the same name, e.g. the `SessionEnd` of the claude that `cleanup` just killed landing on a respawn. The event → status mapping and the observed payloads are in `exploration/claude-hooks-findings.md`. Hooks are only wired when `Config.AgentBinary` is set (it defaults to the running executable).

**Antigravity (`agy`) hooks** (`internal/backend/namedagents/agy.go`) are injected at launch via `agy --add-dir .gomuxai/agents/<name>`, pointing to a generated per-agent `.agents/hooks.json` bundle that `cleanup` removes. Because `agy` runs hooks with `cwd` set to the bundle dir and omits the event name from stdin JSON, each hook invokes `<agent binary> report --hook --as <name> --spawn-id <id> --event <Event>`. `PreInvocation`, `PreToolUse`, and `PostToolUse` map to `processing` (recording `toolCall.name` in `_tool`); `Stop` maps to `idle`, `processing` (`fullyIdle: false`), or `error`. `agy` has no `SessionEnd` or `PermissionRequest`/`waiting` hook event, so `agent approve`/`agent deny` refuses `agy` agents unless `--force` is passed. Initial tasks are passed via `-i <prompt>`, and `--trust-workspace` on `agent spawn` opts into confirming `agy`'s first-launch workspace trust prompt. See `exploration/agy-hooks-findings.md`.

**Scrape patterns** (legacy, `internal/backend/namedagents/*.go`):
- Claude idle `^>\s*$` no longer matches Claude Code's `❯` prompt; it only matters for Claude sessions spawned without hooks.
- Aider idle `^aider>\s*$`; processing "Thinking", "Working", "Applying", "Generating", "Streaming".

### Custom Backend Configuration

Create a JSON file with this structure:

```json
{
  "name": "my-backend",
  "launch_command": "my-cli --some-flag",
  "init_message": "Please read {taskfile}",
  "exit_command": "/quit",
  "idle_pattern": "^> $",
  "processing_pattern": "(Working|Thinking)"
}
```

Use with: `agent spawn helper --backend-config my-backend.json`

### Database Schema

All data is in SQLite (`.gomuxai/gomuxai.db`):

- `agents` - Name, session, backend, task file, created timestamp
- `messages` - Sender, receiver, content, status (pending/delivered), timestamps
- `tasks` - Name, content, source file, created timestamp
- `state` - Agent name, key, value, updated timestamp
- `logs` - Agent name, content (ANSI-stripped), captured timestamp

---

## Remember

- **The tool is the interface.** Everything through `agent`.
- **State is for coordination.** Write yours, read others'.
- **If it doesn't work, report it.** Don't hack around.
- **Clean up.** Don't leave orphan agents.

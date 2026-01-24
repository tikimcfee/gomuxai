# AGENT_INTERFACE.md

This document describes how to use the `agent` CLI tool to orchestrate AI agents. It is intended for integration into your agent's context or instructions.

---

## What This Tool Does

The `agent` tool manages AI coding agents running in terminal sessions. You can:

- **Spawn** agents with different backends (Claude, Aider, or custom)
- **Send messages** to agents (immediately or queued for when idle)
- **Read output** from agents (live or historical)
- **Coordinate** through shared key-value state
- **Recover** from failures using low-level terminal controls

Each agent runs in an isolated tmux session. This matters because when something goes wrong, you can diagnose and recover using the low-level commands.

**Naming:** When you spawn an agent named `helper`, it creates a tmux session named `agent-helper`. High-level commands use the agent name (`helper`); low-level session commands use the session name (`agent-helper`).

---

## The Coordination Model

**The `agent` tool is both the interface and the protocol.**

When you spawn an agent, that agent runs in a terminal with full shell access - including access to the `agent` CLI itself. This means:

- You use `agent` to spawn workers and send them tasks
- Your workers use `agent` to set their state, signal completion, and communicate
- Your workers can spawn their own sub-workers using `agent`
- Everyone reads and writes to the same shared state

**The working environment is recursive.** An orchestrator spawns workers using `agent`. Those workers can spawn their own workers using `agent`. Everyone uses the same interface, sees the same state, follows the same patterns. The tool is the protocol.

This has practical implications:

| Implication | What it means |
|-------------|---------------|
| **Coordination is cooperative** | Agents must be told to update state - it's not automatic. Include instructions like "when done, run `agent state set <yourname> status done`" in your tasks. |
| **State is a shared namespace** | Any agent can read or write any state. Use agent names as prefixes to avoid collisions. |
| **Agents can self-heal** | Spawned agents have the same diagnostic tools you do. They can check siblings' status, capture output, send interrupts. |
| **Patterns apply at every level** | The same coordination patterns work whether you're the top-level orchestrator or a sub-worker three levels deep. |

**Example task with coordination instructions:**
```bash
agent spawn analyzer --backend claude
agent send analyzer "Analyze auth.go for security issues. When finished:
1. Run: agent state set analyzer status done
2. Run: agent state set analyzer findings '<your summary>'
Then wait for further instructions."
```

The spawned agent receives these instructions, does the analysis, and runs those `agent state set` commands to signal completion. You poll the state to know when it's done.

---

## How This Works

A quick mental model before diving into commands:

```
┌─────────────────────────────────────────────────────────┐
│                    You (orchestrator)                   │
│                    using `agent` CLI                    │
└─────────────────┬───────────────────┬───────────────────┘
                  │ spawn             │ spawn
                  ▼                   ▼
        ┌─────────────────┐ ┌─────────────────┐
        │   Worker A      │ │   Worker B      │
        │ (tmux session)  │ │ (tmux session)  │
        │ has `agent` CLI │ │ has `agent` CLI │
        └────────┬────────┘ └────────┬────────┘
                 │                   │
                 ▼                   ▼
        ┌─────────────────────────────────────┐
        │         Shared State (SQLite)       │
        │  - agent state set/get              │
        │  - agent logs                       │
        │  - agent task repository            │
        └─────────────────────────────────────┘
```

**Lifecycle:**
1. **Spawn** - Create agent in isolated tmux session
2. **Send** - Deliver task (agent starts working)
3. **Poll** - Check state or status until done
4. **Collect** - Read results from state or capture output
5. **Cleanup** - Remove agent, freeing resources

**Communication channels:**
- `send` / `queue` → Messages into the agent's terminal
- `capture` / `logs` → Output from the agent's terminal
- `state` → Shared key-value store (preferred for coordination)

---

## Quick Reference

| Intent | Command |
|--------|---------|
| Create an agent | `agent spawn <name> --backend claude` |
| Send message now | `agent send <name> "message"` |
| Send message when idle | `agent queue <name> "message"` |
| Check if idle | `agent status <name>` |
| Read recent output | `agent capture <name> --lines 100` |
| Set coordination state | `agent state set <name> <key> <value>` |
| Read coordination state | `agent state get <name> <key>` |
| Remove an agent | `agent cleanup <name>` |
| List all agents | `agent list` |

---

## Core Operations

### Creating Agents

```bash
agent spawn helper --backend claude
agent send helper "Your task description here"
```

Spawning creates the agent but doesn't automatically send work. Always follow with `send`.

**Options:**
- `--backend claude` or `--backend aider` - Which AI CLI to run
- `--backend-config <file>` - Use a custom backend (JSON config file)
- `--task <name>` - Use a pre-stored task from the database
- `--skip-permissions` - Auto-approve all actions (Claude only, use carefully)

### Communicating with Agents

**Use `send` when:** You need immediate delivery. This interrupts whatever the agent is doing.

```bash
agent send helper "Stop what you're doing - critical bug found"
```

**Use `queue` when:** You want polite delivery. Message waits until agent is idle.

```bash
agent queue helper "When you're done, also check auth.go"

# Specify sender explicitly (useful for tracking/coordination)
agent queue helper --from coordinator "Priority task"
```

**Queue options:**
- `--from <sender>` - Sender ID for tracking. Defaults to `AGENT_NAME` env var if set, otherwise "user".

Note: `queue` requires a watcher process running (`agent watch`). Start it in the background if using queued messages.

### Reading Agent Output

**For current state:** Use `capture` to see what's on screen now.

```bash
agent capture helper --lines 100
agent capture helper --lines 100 --raw  # Keep ANSI escape codes
```

**For history:** Use `logs` to see timestamped past output.

```bash
agent logs helper --since 10m
```

### Checking Agent Status

```bash
agent status helper
```

Returns: `idle`, `processing`, or `unknown`

**When you get `unknown`:** The agent might be in an unusual state. For diagnostics, use `capture` to visually inspect what's on screen. For coordination, rely on explicit state instead of status.

```bash
# Diagnostic: visually inspect the terminal
agent capture helper --lines 20

# Coordination: check explicit state (preferred)
agent state get helper status
```

### Coordinating Through State

State is the reliable way to coordinate between agents. Use explicit signals, not output parsing.

**The distinction:**
- **Coordination** (signaling between agents) → Use state. Don't grep output for "done".
- **Diagnostics** (understanding what's happening) → Use capture. Look at screen to debug.

```bash
# GOOD: Explicit coordination through state
agent state set helper status "done"
agent state set helper result "Found 3 issues in auth.go"

# Coordinator reads explicit signals
agent state get helper status
agent state get helper result

# BAD: Parsing output for coordination
output=$(agent capture helper --lines 10)
if [[ "$output" == *"done"* ]]; then ...  # Fragile, don't do this
```

State entries persist until explicitly cleared or the agent is cleaned up. Use `agent state clear <name>` to remove all state for an agent.

**Reading non-existent keys:** Returns "No state found for X.Y" (not an error). Check for this in scripts.

### Agent Identity

When an agent is spawned, the `AGENT_NAME` environment variable is automatically set in its session. This enables:

- **Self-identification** - The agent knows its own name
- **Automatic sender tracking** - `queue` uses `AGENT_NAME` as the default `--from` value

```bash
# Inside a spawned agent, AGENT_NAME is set
echo $AGENT_NAME  # "helper"

# Queue automatically uses it as sender
agent queue coordinator "I'm done"  # --from defaults to "helper"
```

### Cleaning Up

Always clean up agents when done:

```bash
agent cleanup helper
```

To clean up multiple agents:

```bash
agent cleanup --all --as myname              # All except yourself
agent cleanup --all --as myname --except db  # All except yourself and "db"
```

**Cleanup options:**
- `--all` - Clean up all agents (requires `--as`)
- `--as <name>` - Your agent name (prevents self-termination)
- `--except <names>` - Comma-separated list of agents to skip

**Note:** Cleanup deletes the agent's state and logs. If you need to preserve results, read them before cleanup.

---

## When to Use What

| Situation | Do This |
|-----------|---------|
| Need agent to do something now | `send` |
| Have follow-up work for later | `queue` |
| Need to check if agent is ready | `status`, then `capture` if unknown |
| Need to pass data between agents | Write to `state`, read from `state` |
| Need current screen content | `capture` |
| Need to debug what happened | `logs` |
| Agent seems stuck | `capture` to diagnose, then recovery steps |
| Done with an agent | `cleanup` |

---

## Handling Failures

The tool exposes low-level controls for recovery. When high-level commands fail, drop down to these.

### Agent Not Responding

```bash
# See what's on screen (high-level command uses agent name)
agent capture helper --lines 50

# If stuck at unexpected prompt, try Enter (low-level uses session name: agent-<name>)
agent key agent-helper Enter

# If truly hung, interrupt it
agent key agent-helper c-c
```

Note: `capture` uses agent name (`helper`), but `key` uses session name (`agent-helper`).

### Status Returns Unknown

This means the output doesn't match expected patterns. Don't trust status alone - verify with capture or state.

```bash
# Check actual screen
agent capture helper --lines 20

# Or rely on explicit state instead
status=$(agent state get helper status)
```

### Session Problems

```bash
# List actual tmux sessions
agent session list

# Kill orphaned session manually
agent session kill agent-helper

# Agent record exists but session gone? Cleanup handles it
agent cleanup helper
```

### Messages Not Delivering

Queued messages need the watcher running:

```bash
# Start watcher (runs in foreground, prints status periodically)
agent watch

# Or background with custom interval
agent watch --interval 5s &

# Check if a watcher might be running (look for the process)
ps aux | grep "agent watch"
```

The watcher prints periodic status when running. If you're not seeing queue deliveries and no watcher output, start one. Multiple watchers can run safely (they'll just duplicate delivery checks).

---

## Patterns

### Parallel Workers

Spawn multiple agents, wait for all to finish via state polling:

```bash
# Check for existing agents first
agent list

# Spawn workers
agent spawn worker-1 --backend claude
agent spawn worker-2 --backend claude

# Send tasks WITH coordination instructions
# The agents will run these `agent state set` commands themselves
agent send worker-1 "Analyze module A for security issues.
When finished, run these commands:
  agent state set worker-1 status done
  agent state set worker-1 findings '<your summary here>'"

agent send worker-2 "Analyze module B for security issues.
When finished, run these commands:
  agent state set worker-2 status done
  agent state set worker-2 findings '<your summary here>'"

# Poll until both done
while true; do
  s1=$(agent state get worker-1 status)
  s2=$(agent state get worker-2 status)
  [[ "$s1" == "done" && "$s2" == "done" ]] && break
  sleep 5
done

# Collect results from shared state
agent state get worker-1 findings
agent state get worker-2 findings

# Clean up (this deletes their state too, so collect first)
agent cleanup worker-1
agent cleanup worker-2
```

### Pipeline Handoff

Agent A completes, signals Agent B. This happens *inside* Agent A's task - the agent runs these commands itself:

```bash
# Inside Agent A's task instructions, tell it to do this when done:
# "When finished, run:
#   agent state set agent-a status done
#   agent state set agent-a output results.json
#   agent queue agent-b 'Agent A finished. Check agent-a state for output location.'"

# Agent A runs those commands, which:
# 1. Signals completion via state
# 2. Records output location in state
# 3. Notifies Agent B via queue (delivered when B is idle)
```

The orchestrator doesn't need to be in the loop - agents can coordinate directly with each other using the same `agent` CLI.

### Storing Reusable Tasks

Store task definitions for reuse across agents:

```bash
# Store a task
agent task set code-review -f tasks/code-review-prompt.md

# Use it when spawning
agent spawn reviewer --backend claude --task code-review
```

---

## Integration Checklist

When integrating this tool into your agent's workflow:

1. **Always clean up** - Don't leave orphaned agents
2. **Use state for coordination** - Don't parse output for signals
3. **Check before spawning** - Use `agent list` to avoid duplicates
4. **Handle unknown status** - Fall back to capture or state
5. **Start the watcher** - If using `queue`, ensure `agent watch` is running
6. **Use escape hatches** - When high-level fails, drop to low-level commands

---

## Command Reference

### Agent Lifecycle

| Command | Purpose |
|---------|---------|
| `agent spawn <name> --backend <backend>` | Create agent |
| `agent list` | List all agents |
| `agent status <name>` | Check idle/processing/unknown |
| `agent cleanup <name>` | Remove agent and session |

### Communication

| Command | Purpose |
|---------|---------|
| `agent send <name> "msg"` | Immediate delivery |
| `agent queue <name> "msg"` | Deliver when idle |
| `agent watch` | Start delivery watcher |

### Output

| Command | Purpose |
|---------|---------|
| `agent capture <name> --lines N` | Current terminal content |
| `agent logs <name> --lines N` | Historical output from DB |
| `agent logs <name> --since 5m` | Output since duration |

### State

| Command | Purpose |
|---------|---------|
| `agent state set <name> <key> <val>` | Write state |
| `agent state get <name> <key>` | Read state |
| `agent state list <name>` | List all state for agent |
| `agent state clear <name>` | Delete all state |

### Tasks

| Command | Purpose |
|---------|---------|
| `agent task set <name> "content"` | Store task inline |
| `agent task set <name> -f file.md` | Store task from file |
| `agent task get <name>` | Retrieve task |
| `agent task list` | List all tasks |
| `agent task clear <name>` | Delete task |

### Low-Level (Recovery)

| Command | Purpose |
|---------|---------|
| `agent session list` | List tmux sessions |
| `agent session new <name>` | Create raw session |
| `agent session kill <name>` | Kill session |
| `agent keys <session> "text"` | Send text + Enter |
| `agent keys <session> "text" --no-enter` | Send text without Enter |
| `agent key <session> <key>` | Send special key |
| `agent clear <session>` | Clear terminal |

**Special keys for `agent key`:** `Enter`, `Escape`, `Tab`, `Space`, `c-c`, `c-d`, `c-z`, `c-l`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PgUp`, `PgDn`, `F1`-`F12`

**Remember:** Low-level commands use session names (`agent-helper`), not agent names (`helper`).

---

## Data

All data is stored in `.gomuxai/gomuxai.db` (SQLite) next to the binary. Delete the `.gomuxai/` folder to reset everything.

---

## Summary

- **Spawn** agents, **send** them work, **capture** their output
- **State** is for coordination - explicit signals beat output parsing
- **Status** can be unreliable - verify with capture or state
- **Low-level commands** exist for recovery - use them when abstractions fail
- **Clean up** when done - don't leave orphans

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

- Backend is **required** (`--backend claude` or `--backend-config file.json`)
- Task file is optional - content is stored in DB
- You'll need to `send` the first message yourself

```bash
# Spawn and give it work
agent spawn helper --backend claude
agent send helper "Read task at tasks/research.md and execute it"
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
```

**Wart:** `queue` requires `agent watch` running somewhere. If the watcher isn't running, messages sit forever.

### Reading Output

```bash
agent capture <name> --lines 100
```

Returns terminal output, ANSI codes stripped. Use `--raw` to keep them.

### Checking Status

```bash
agent status <name>
```

Returns: `idle`, `processing`, or `unknown`

**Wart:** Status detection uses regex patterns. It can be wrong. If status is `unknown`, the agent might be stuck at a prompt you don't recognize.

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

### Logs

Query captured output from DB (ANSI-stripped):

```bash
agent logs <name>                  # Last 50 entries
agent logs <name> --lines 100      # More entries
agent logs <name> --since 5m       # Since 5 minutes ago
```

---

## Low-Level Tmux Control

For when you need raw terminal access:

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

**Special keys:** `Enter`, `Escape`, `Tab`, `Space`, `c-c`, `c-d`, `c-z`, `c-l`, `Up`, `Down`, `Left`, `Right`

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
```

The `--as` flag prevents you from killing yourself.

### 4. Use State for Coordination

Don't rely on capturing output to coordinate. State is explicit:

```bash
# BAD - fragile
output=$(agent capture helper --lines 5)
if [[ "$output" == *"done"* ]]; then ...

# GOOD - explicit
status=$(agent state get helper status)
if [[ "$status" == "done" ]]; then ...
```

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
| `queue` needs watcher running | Start `agent watch` in background |
| `status` can return `unknown` | Check state instead, or capture output |
| Spawn doesn't auto-send task | Use `send` after spawn |
| No streaming capture | Poll with `capture` periodically |
| Backend always required | Use `--backend claude` or config file |

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

1. Check the agent exists: `agent list`
2. Check session exists: `agent session list`
3. Check status: `agent status <name>`
4. Capture output: `agent capture <name> --lines 200`
5. Check state: `agent state list <name>`

If the tool errors or doesn't do what you expect, **set your state to blocked and report it**:

```bash
agent state set myname status "BLOCKED"
agent state set myname error "agent send failed with: <error>"
```

---

## Remember

- **The tool is the interface.** Everything through `agent`.
- **State is for coordination.** Write yours, read others'.
- **If it doesn't work, report it.** Don't hack around.
- **Clean up.** Don't leave orphan agents.

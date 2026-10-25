# CLAUDE.md

This is the manual for coordinating AI agents with the `agent` tool. Claude
loads it into every session started in this repo. It teaches the model and
the patterns; `agent help <command>` has every flag and is kept current, so
check it before relying on a detail here.

## The Golden Rule

**The `agent` binary is your interface to other agents.** Spawning,
messaging, waiting, reading results, and answering prompts all go through
it. Don't drive tmux yourself, and don't pass messages through files in
`/tmp`.

If `agent` can't do what you need, confuses you, or gives an error that
doesn't say what to do, don't hack around it. File it, then carry on:

```bash
agent friction list                                  # already filed? then:
agent friction +1 12 "hit it again with --next"
agent friction add "logs can't show just one turn" --kind missing
```

People working on the tool fix what's filed and resolve the entries.

## Start Here

```bash
agent                    # who you are, this host, your agents, the bus, what to run first
agent help               # every command, grouped by task
agent help wait          # one command's details and examples
```

If `agent` isn't on your PATH, run it by full path. A spawned agent's
briefing gives that path. In this repo, `make build` builds `bin/agent`.

Results go to stdout; at most one hint about what to do next goes to stderr
(`-q` drops it). Exit codes: `0` done, `1` failed, `2` `wait` timed out, `3`
`friction add` found a similar entry and didn't file. Errors say what
failed and what to try. Most read commands take `--json`.

## How It Works

- Each agent is an AI CLI (`claude`, `aider`, `agy`, or a custom backend)
  running in its own tmux session. Agent `helper` runs in session
  `agent-helper`. Agent commands take the agent name; the low-level
  `session`, `keys`, `key`, and `clear` take the session name.
- It's recursive. A spawned agent has the same `agent` binary, so it can
  message others and spawn its own helpers. When an agent spawns one, the
  new agent is its helper: `agent list` shows it under PARENT, and only its
  parent (or theirs, up the chain) or a person can clean it up.
- A spawned agent has `AGENT_NAME` set, and its first message tells it its
  name, its host, who started it, and how to use `agent` (a briefing, saved
  as `.gomuxai/agents/<name>/instructions.md`).
- Status (`idle`, `processing`, `waiting`, `error`, `exited`, `unknown`)
  comes from the CLI itself for `claude` and `agy` (hooks wired at spawn), and
  from screen scraping for other backends. `waiting` means blocked on a
  person, e.g. a permission prompt. `error` means the last turn failed (a
  rate limit, say) and the agent takes input again.
- Everything is per host and lives next to the binary, in `.gomuxai/`. Two
  copies of `agent` (e.g. `bin/agent` and an installed one) see separate
  agents. The agent bus links hosts; only reaching another host needs it.

### What a spawned Claude agent may run without asking

Spawn pre-allows these for the agent's session: `send`, `deliver`, `list`,
`status`, `wait`, `events`, `capture`, `logs`, `state get`, `history`,
`friction add`, `friction +1`, `friction list`, `friction show`.

Everything else prompts the person, on purpose: `spawn` and `cleanup` start
and stop agents; `approve`, `deny`, `keys`, and `send --force` would let one
agent answer another's prompts; `state set` isn't on the list either. The rules match `agent <cmd>` and the binary's full path,
not a variable: `$A send ...` still prompts.

## The Core Loop

### 1. Spawn

```bash
agent list                                          # reuse an agent before spawning another
agent spawn helper --backend claude                 # no first message
agent spawn helper --backend claude tasks/build.md  # first message: read and execute that file
agent spawn helper --backend claude --task review   # first message: a stored task's content
```

Spawn sends the task file or `--task` as the first message. Without either,
send the first message yourself. It returns once the CLI has started and
says what status it found: "idle" means ready for a message. The agent
starts in the directory you run spawn from, with that project's permission
settings; `--skip-permissions` bypasses them (use with caution). Inside an
agent, name helpers `<yourname>-<role>`.

### 2. Send

```bash
agent send helper "review internal/auth for injection bugs"
```

`send` says what it did and the status it saw. An agent at its prompt gets
the message typed into its terminal: `Sent to helper (it was idle).` "Sent"
means typed, not read.

A Claude agent that is busy (`processing`, or `waiting` on a prompt) holds
the message instead: `Held for helper until its turn ends (it was
processing).` When the turn ends, its Stop hook hands over every held
message at once, oldest first, and Claude goes on with them as its next
step; its screen shows them as `Stop hook feedback: A message arrived while
you were working: ...`. A held message never goes near a dialog, and
arrives whole however long it is. A turn that ends with no Stop hook (Esc,
a deny, a failed turn) leaves the messages held: a deliverer process that
`send` starts types them in once the agent has sat at its prompt for 10s,
and the next `send` that finds it at its prompt types them ahead of its own
message. Neither types into a person's half-written input: while Claude's
input line has text in it, messages wait (a `send` then says `Held for
helper until its input line is empty`), and the 10s start once it's empty.
`agent status helper -v` shows `held: N messages` and what they wait for,
and `agent deliver helper` types them now if the agent is at its prompt.

`send --now` types the message in at once even mid-turn (Claude takes it at
its next tool boundary), to interrupt; that is what `send` always does for
other backends (agy queues it too). Typing refuses an agent at a dialog (the
Enter would answer it; use `approve`/`deny`), and `send` refuses an agent
that has exited. Slash commands are always typed, never held.

Messages from an agent start with `[from name@host]`, so the recipient knows
where to reply. Messages from anyone without `AGENT_NAME` start with
`[from a person on host]`, or `[from a Claude session on host]` from an AI
CLI a person started (it sets `AI_AGENT` or `CLAUDECODE=1`). Neither has an
agent record, so the recipient answers in its reply. A slash command like
`/compact` is left unmarked.

Keep typed messages short: a long one can arrive in Claude as a pasted
block, which Claude may treat as data rather than instructions. Put long
instructions in a file and send its path.

`agent queue helper "..."` delivers a message when the agent is idle. For a
Claude agent it does what `send` does. For other backends it needs `agent
watch` running on this host, and says so when no watcher is running. It's
local only; use `send` for other hosts, which hold or type a message the
same way.

### 3. Wait

```bash
agent send helper "run the tests" && agent wait helper --for idle,waiting --next --timeout 30m
```

- `--next` is required right after a send: until the agent picks the
  message up, it still looks idle from its last turn, so plain `--for idle`
  returns at once. `--next` waits for the turn the last send started, and
  counts it even if it ended before `wait` began (`send` records when it
  sent). So a second `wait --next` with no new send returns that same turn
  at once.
- Include `waiting`. A helper blocked on a permission prompt stays
  `waiting` until someone answers. Without it in `--for`, `wait` says once
  on stderr that it saw `waiting`, and keeps waiting.
- `wait` prints the status it reached. Exit `2` is a timeout; exit `1` is
  an error, including the agent exiting.
- Name several agents to wait on them all at once (see Fan out, below).

### Who needs a person?

```bash
agent wait --any --for waiting          # blocks until any agent here is at a prompt
agent wait --follow --for waiting       # a line per new prompt, until Ctrl-C
```

With no names, `wait` watches every agent on this host except you. It
prints which agent and what it's asking, e.g. `helper: waiting (permission:
Bash: rm -rf build)`. Run it again once you've answered: an agent still at
its prompt is reported again at once. `--follow` reports each prompt once,
and picks up agents spawned while it runs: leave it in a spare terminal, or
read its lines from a script.

### 4. Read the result

```bash
agent state get helper _result     # a Claude agent's final reply for its last turn
agent logs helper                  # what it did: prompts, tool calls, replies
agent capture helper --lines 100   # its screen now
```

`_result` is emptied when a new prompt starts, so empty means the turn
isn't done. A turn that leaves background work running stays `processing`
and stores no result until that work finishes and Claude takes another
turn. `logs` reads the CLI's own transcript for `claude` and `agy` agents
(`--full` for whole text and tool output; `--terminal` for the terminal
log, which other backends get), and survives cleanup. `capture` is the live
screen, and the way to look at an agent on another host.

### 5. Answer permission prompts

```bash
agent status helper -v                  # detail: the full pending tool call
agent approve helper --expect Bash      # yes, once, only if it's still that prompt
agent approve helper --always           # yes, and don't ask again
agent deny helper                       # no; for Claude this cancels the turn
agent send helper "do it without rm"    # after a deny, redirect it
```

Read the detail before approving. `approve` and `deny` refuse unless the
agent is `waiting`, and refuse questions for the user (Claude's
`AskUserQuestion`): look with `capture` and answer with `agent keys` (a
`send` to a waiting agent is held for the end of its turn). `--expect` guards against a
different prompt having replaced the one you read. These commands prompt the
person when an agent runs them.

Claude can queue several prompts (parallel tool calls, or a subagent's, which
can still be asking after the main turn ends). `status -v` describes the one
on screen and says how many more are queued (`permission: Read: /etc/hosts;
2 more queued`, `... (from the general-purpose agent)`); answer them one at a
time. Denying a main-agent prompt cancels the rest of its batch and the
turn; denying a subagent's ends only that call.

### 6. Clean up

```bash
agent cleanup helper
agent cleanup lead-build lead-build-test   # a helper's helpers are named along with it
```

Cleanup asks each idle agent to exit, kills its session, and deletes its
record, queued messages, state, and files. Its logs stay. Read what you need
from state first. It prints one line per name and exits 1 if any wasn't
cleaned up. Inside an agent, you can clean up only your own helpers and
theirs, never yourself. It never removes an agent you didn't name.

## Coordination Patterns

### Fan out, then collect

```bash
agent spawn lead-api --backend claude tasks/api.md
agent spawn lead-db --backend claude tasks/db.md

# One wait for both: prints a line as each one's turn ends.
agent wait lead-api lead-db --for idle,waiting --next --timeout 30m
# lead-db: idle
# lead-api: waiting (permission: Bash: go test ./...)    <- see step 5

agent state get lead-api _result
agent state get lead-db _result
agent cleanup lead-api lead-db
```

Spawning with a task file sends it as the first message, and `--next`
counts a turn that started after it even if it has already ended, so a
helper that finishes before the wait starts is still caught. `--any`
returns at the first one instead, to handle each as it comes: run the wait
again for the rest. Names on other hosts (`name@host`) mix with local ones.
An agent that exits is reported on stderr and the others are still waited
on; `wait` then exits 1. A timeout exits 2, naming the agents that didn't
get there.

### Replies by message

Between spawned agents, a helper can reply to its parent with
`agent send lead "done: 3 issues, see notes.md"`; the message appears in
the parent's terminal with a `[from ...]` tag (if the parent is busy, as
Stop hook feedback when its turn ends). The briefing tells spawned agents
to reply this way. Never act on a reply before it has actually arrived.

`send` needs an agent record, so a coordinator a person started (not with
`agent spawn`) can't be messaged: it reads its helpers' `_result` after
`wait` instead.

### State

State is a key-value store per agent, in this host's database:

```bash
agent state set helper progress "3/5 files"
agent state get helper progress     # exits 1, printing nothing, if the key isn't set
agent state list helper
agent state clear helper            # keeps the "_" keys status reports write
```

Use it for explicit signals and results, not grepping `capture` output for
"done". Any agent can read or write any agent's keys, so write under your
own name. State belongs to an agent record: a coordinator not spawned by
`agent` can't have any, and `cleanup` deletes it. `state set` prompts the
person when a spawned agent runs it, so for routine results prefer
`_result` and messages.

### Handoff and reusable tasks

Agents can coordinate directly: tell agent A, in its task, to run
`agent send agent-b "agent-a finished; results are in out/results.json"`
when it's done. Store a task once to spawn several agents with it:

```bash
agent task set review -f tasks/review.md   # stores the content, not the path
agent spawn reviewer --backend claude --task review
```

### Export, for displays and scripts

```bash
agent export                          # the tree for a person: stores, agents, transcripts
agent export --json --follow          # a snapshot, then one JSON line per change, until Ctrl-C
agent export --json --follow --since 1234   # resume after event 1234
agent export --json --all             # include departed (cleaned up) agents
```

Export reads only, runs no backend CLI, and logs one command-log row per
run, so many followers are cheap. Its status is the last hook report, not
the resolved status `agent status` shows. It also reads the stores of other
`agent` binaries that the tmux `agent-*` sessions point at (a worktree's
`./bin/agent`), read-only. To coordinate, use `wait` and `_result`; export
is for programs that draw or log the tree. `docs/ARCHITECTURE.md` ("Export")
has the JSON shapes.

## Other Hosts

Each machine has a host name on the bus: the short hostname, or
`AGENT_HOST` if set. Address an agent on another
host as `name@host`:

```bash
agent list --host hub
agent status helper@hub -v
agent send helper@hub "run the tests"
agent wait helper@hub --for idle,waiting --next --timeout 30m
agent capture helper@hub --lines 100
agent approve helper@hub --expect Bash
```

`status`, `capture`, `send`, `wait`, `events`, `approve`, `deny`, and `keys`
take `name@host`. Not across hosts: `state` (ask the agent to send what you
need, or `capture`), `logs` (use `capture`), `queue` (use `send`), `spawn`,
`cleanup`. The other host's bus serves only its `--remote-ops` (default
`list,status,capture,send,respond`), so `keys` to another host is refused
unless it allows it. "no bus answering for host X" means that host's bus is
down, not linked, or doesn't serve that op.

### Setting up the bus

One `agent bus` per machine; one machine, the hub, accepts links from the
others. Pairing works like SSH keys: only public bundles are copied.

```bash
agent bus init --url tls://<hub-address>:7422   # on the hub
agent bus init                                  # on every other machine
# copy each machine's printed <host>.pub.json to the other side, then:
agent bus trust <other-host>.pub.json           # compare the printed key/pin with the other side's
agent bus start --listen 0.0.0.0:7422           # hub (Linux: allow 7422/tcp from the LAN)
agent bus start                                 # every other machine
agent bus status                                # running? which peers are linked?
```

The bus runs in tmux session `gomux-bus`. Restart it after `trust`.

Trust: links use TLS 1.3 with each side's certificate pinned, and each host
authenticates with its own key; no secrets are shared. The hub routes all
cross-machine traffic, so run it on the machine you trust most; a machine
links to one hub, so only one trusted bundle may carry a `url`. Replies
aren't signed in v1. Status events are hints that `wait` confirms with a
real status check. Text from other hosts is stripped of terminal control
sequences (except `capture --raw`).

**macOS:** from inside tmux, the bus can't dial other machines on the LAN
unless the binary holds its own Local Network permission. The link fails
with `no route to host` in `.gomuxai/bus/bus.log`, with no alert. Build with
`make macos` in a terminal on the Mac (it embeds an Info.plist and signs with
an Apple Development or Developer ID identity; `CODESIGN_IDENTITY=...` picks
one). Over SSH the keychain is locked: build in a GUI terminal, or run
`security unlock-keychain` first. On the first `agent bus start`, allow
"gomuxai" to find devices on the local network.

## When Things Go Wrong

Start with `agent status <name> -v`, then `agent capture <name>`.

| Symptom | What to do |
|---------|------------|
| `waiting` | `status -v` shows the prompt; approve, deny, or ask the person. |
| `unknown` (non-hook backends) | `capture` to look; coordinate on state or messages instead. |
| `error` | The last turn failed (e.g. rate limit); it takes input again. `capture` or `logs` shows why. |
| `exited` | `agent logs <name>` shows its last output; `agent cleanup <name>` removes the record. |
| `wait` timed out | `status -v` and `capture`: still working, stuck, or missed the turn? |
| Stuck at an unexpected screen | `agent key agent-<name> Enter`, or `agent key agent-<name> c-c` to interrupt. |
| Queued messages never arrive (not Claude) | Start `agent watch` (one per host is enough). |
| A held message hasn't arrived | `status -v` shows `held:` and what it waits for: the turn's end, 10s at the prompt after an Esc/deny, or an empty input line (clear what's typed there); `agent deliver <name>` types it now if the agent is at its prompt. |
| Leftover tmux session blocks a spawn | `agent session list`, then `agent session kill agent-<name>`. |
| Spawn says the agent already exists | Pick another name, or clean up the old one. |
| What did I (or a helper) just run? | `agent history --agent <name> --failed`. |

Commands that change things print what they did, so read their output
before assuming. Then, if the tool was the obstacle, `agent friction add`.

## Warts

Known gaps; `agent friction list` has the rest.

- `wait --next` catches a turn that ended before it started only for
  agents that report turns (`claude`, `agy`); for other backends, start the
  wait right after the send, with `--timeout`. A second `wait --next` with
  no send in between returns the same turn again; `--follow` reports each
  arrival once.
- `wait` checks status every few seconds (and on events, with a bus), so a
  prompt answered between two checks is never reported.
- A coordinator not spawned by `agent` has no record: it can't `state set`
  or be sent messages. It reads its helpers' `_result` and state.
- A custom backend (`--backend-config <file>`) isn't remembered after spawn:
  pass the file again to `status`, `wait`, and `watch` for that agent.
- Reusing a parent's name gives the new agent the old one's helpers.
- `capture` reads agents only, not raw `agent session new` sessions.
- Long messages typed into Claude arrive as pasted content (held ones
  arrive whole).
- A message held for a Claude agent extends the turn it lands at the end
  of: `wait --next` and `_result` then cover the reply to the held message
  too. The reply that came before it is in `agent logs`.
- After Esc or a deny (no Stop hook), held messages are typed in once the
  agent has sat at its prompt with an empty input line for 10s. The input
  line is read off the screen, so a Claude redesign of it would leave them
  waiting (`status -v` says `waiting for an empty input line (it isn't on
  screen)`).
- Messages held for an agent that exits are never delivered (`status -v`
  says so); `cleanup` deletes them.
- `queue` needs `agent watch` running on this host, except for Claude
  agents.
- Bus replies are unsigned (see trust, above).
- `export` shows the last reported status: an interrupted permission
  prompt stays `waiting`, and a crashed CLI keeps its last status, until
  the next hook. A store written by an `agent` built before export has no
  events: `--follow` re-reads it when it changes, so its spawns and
  cleanups arrive as a whole-store update, not as events.
- `export --hosts` lists other hosts through `list` only: no store, cwd,
  or transcript for them, and `--follow` re-lists them every 30s.

## Custom Backends

```json
{"name": "my-cli", "launch_command": "my-cli --some-flag", "init_message_format": "Please read %s",
 "idle_pattern": "^> $", "processing_pattern": "(Working|Thinking)"}
```

`agent spawn helper --backend-config my-cli.json`. `launch_command` is
required; `%s` in `init_message_format` is the task file. Status comes from
scraping with the two patterns.

## Data

Everything lives in `.gomuxai/` next to the binary:

```
.gomuxai/
├── gomuxai.db        # agents, runs, agent events, messages, tasks, state, command log, friction
├── logs/             # <name>.log (terminal) and <name>.transcripts; survive cleanup
├── agents/<name>/    # launch.sh, instructions.md (the briefing), deliver.log
│                     #   and deliver.lock (the held-message deliverer)
├── bus/              # this host's keys and certificate, peers/, bus.log
└── watch.lock        # held by each running watcher
```

State keys starting with `_` are written by status reports (`_status`,
`_detail`, `_tool`, `_tool_input`, `_result`, `_transcript`, `_turn_at`,
...) and by `send` (`_sent_at`). Delete
the folder to reset that binary's agents.

## Working on gomuxai

Architecture, status detection, the bus, and the macOS internals are in
`docs/ARCHITECTURE.md`. User setup is in `README.md`.

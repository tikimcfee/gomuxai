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

Each host has its own friction box. When an entry is the same problem as
one filed on another host, link them; both stay open and count their own
votes, `show` and `list` name the other, and resolving either resolves both
over the bus:

```bash
agent friction link 4 34@monolith1    # this host's #4 is monolith1's #34
agent friction show 4                 # lists #34@monolith1, its status and votes there
agent friction resolve 4 --note "status reads the trust prompt (abc1234)"
                                      # resolves #34@monolith1 too (--only: just #4)
agent friction links --dangling       # links gone stale, with what to run
```

A link goes stale when one end is closed and the other isn't (`out of
step`), the other host has no link back (`one-sided`), has no such entry
or a different one under that id (`not found`), or doesn't answer
(`unreachable`, with what was last seen). `show` and `list` flag it with `!`,
and `show` and `links` say what to run. If a host is unreachable when you
link or resolve, it's done here and the command prints what's left; once
that host answers, `agent friction resolve 4 --linked` passes #4's
resolution on. `unlink` removes a link on both hosts.

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
  new agent is its helper: `agent list` nests it under its parent, and only
  its parent (or theirs, up the chain) or a person can clean it up.
- An existing tmux session can also be registered on the bus without
  restarting what's running in it: `agent adopt <name>` (or `--session <session>`).
  Cleaning up an adopted agent unregisters it and leaves its tmux session running.
  gomuxai never launches anything in an adopted session, so `agent resume`
  refuses one: if its CLI exits, start it there yourself (`claude -c`
  continues its last conversation) and adopt keeps tracking it.
- A spawned agent has `AGENT_NAME` set, and its first message tells it its
  name, its host, who started it, and how to use `agent` (a briefing, saved
  as `.gomuxai/agents/<name>/instructions.md`).
- Status (`idle`, `processing`, `waiting`, `error`, `ended`, `unknown`;
  `agent statuses` says what each means) comes from the CLI itself for
  `claude` and `agy`, and from screen scraping for other backends. Claude
  reports through one hook every Claude Code session runs (`agent hooks
  install` registers it in Claude Code's user settings; spawn refuses a
  Claude agent until it is there), which finds its agent from the running
  processes, and Claude Code's session registry (`~/.claude/sessions/`, each
  entry proven by pid and start time) says which session an agent's pane
  runs, including one a person started there by hand. agy's hooks are wired
  at spawn. `waiting` means blocked on a person, e.g. a permission prompt,
  the CLI's workspace trust prompt (Claude or agy in a folder it hasn't
  trusted, which runs no hook: read off its screen, tool `trust`, detail
  `trust: <folder>`), or another dialog Claude shows on its own, at startup
  or when a turn ends, that no hook reports (an offer, a notice, a picker:
  read off its screen, tool `dialog`, detail `dialog: <title>; Enter
  picks: <option>`). Keys typed then go to the dialog, so `send` holds
  messages until it is gone.
  `error` means the last turn failed (a rate limit, say) and the agent takes
  input again. `ended` comes with an end reason (`quit`, `gone`, `replaced`,
  `finished`, `parent`, `closed`), shown as `ended (quit)`. What agy's hooks don't
  report is read off its screen: a permission or `ask_question` dialog
  (`waiting`), and a turn that Esc or Ctrl+C ended, which fires no hook
  (`idle`, detail `interrupted; at its prompt`, source `scrape`).
- Everything is per host and lives next to the binary, in `.gomuxai/`. Two
  copies of `agent` (e.g. `bin/agent` and an installed one) see separate
  agents. The agent bus links hosts; only reaching another host needs it.

### What a spawned Claude agent may run without asking

Spawn pre-allows these for the agent's session: `send`, `deliver`, `list`,
`status`, `wait`, `events`, `capture`, `logs`, `sessions`, `transcript`, `state get`,
`history`, `friction add`, `friction +1`, `friction list`, `friction show`,
`friction link`, `friction links`.

Everything else prompts the person, on purpose: `spawn` and `cleanup` start
and stop agents; `approve`, `deny`, `keys`, and `send --force` would let one
agent answer another's prompts; `state set` isn't on the list either, nor
are `friction accept`, `resolve` and `unlink` (which erases a link on both
hosts).

Two things allow them. Spawn adds rules for `agent <cmd>` and the
binary's full path, and gomuxai's own PreToolUse hook approves the same
commands however the binary is reached: `./bin/agent status`, another
relative path, a symlink, or `agent -q list`. The hook approves only one
plain run of this binary with a safe subcommand. A chain (`;`, `&&`, a
pipe), a variable or `$(...)`, an `env` or `A=1` prefix, a glob, or a
redirection other than `2>&1` and `>/dev/null` gets the normal prompt, and
so does `$A send ...`: the hook can't see the shell's variables, so run
the binary by name or path. The hook never denies anything, and a
settings `deny` rule still wins over it.

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
says what status it found: "idle" means ready for a message. In a folder
the CLI hasn't trusted, it says the agent is waiting at its workspace trust
prompt: `agent approve <name>` trusts the folder (a Claude agent's first
message waits for that, then goes in), or respawn with `--trust-workspace`,
which answers it at launch (trusting a folder enables its own hooks and
settings, so only for folders you trust). A Claude agent is launched
with `--settings '{"skillOverrides":{"auto-mode-setup":"off"}}'` (its
session only): that turns off `/auto-mode-setup` there, and with it the
"Teach auto mode about your environment?" offer, which comes up when a
turn ends and would take the next message's keys. The agent
starts in the directory you run spawn from (`--cwd DIR` picks another),
with that project's permission settings; `--skip-permissions` bypasses
them (use with caution). Inside an agent, name helpers
`<yourname>-<role>`.

`--resume <session>` makes the new agent continue a Claude or agy session
that has ended, in the directory it ran in; `-c`/`--continue` picks the
session the CLI's own `--continue` would (Claude: the most recent in this
directory, or `--cwd`'s; agy: the same, falling back to a parent or child
directory), says which (id, title, when last active), and goes on as
`--resume` with it. Either refuses a session still running.

### 2. Send

```bash
agent send helper "review internal/auth for injection bugs"
```

`send` says what it did and the status it saw. An agent at its prompt gets
the message typed into its terminal: `Sent to helper (it was idle).` "Sent"
means typed, not read.

A Claude or agy agent that is busy (`processing`, or `waiting` on a prompt)
holds the message instead: `Held for helper until its turn ends (it was
processing).` A hook hands over every held message at once, oldest first:

- Claude, when the turn ends: its Stop hook's output, and Claude goes on with
  them as its next step; its screen shows them as `Stop hook feedback: A
  message arrived while you were working: ...`.
- agy, when its current step ends (`Held for helper until its current step
  ends`): its PostInvocation hook, which fires after each model call and its
  tools, mid-turn or last. Each message shows in the pane as a `> [from ...]`
  prompt of its own, and the model answers them in the same turn. One over
  64 KB goes in labeled parts, since agy silently cuts longer ones.

A held message never goes near a dialog, and arrives whole however long it
is. A turn that ends with no such hook (Esc or Ctrl+C, a deny, a failed
turn) leaves the messages held: a deliverer process that `send` starts types
them in once the agent has sat at its prompt for 10s, and the next `send`
that finds it at its prompt types them ahead of its own message. Neither
types into a person's half-written input: while the agent's input line has
text in it, messages wait (a `send` then says `Held for helper until its
input line is empty`), and the 10s start once it's empty. `agent status
helper -v` shows `held: N messages` and what they wait for, and `agent
deliver helper` types them now if the agent is at its prompt.

`send --now` types the message in at once even mid-turn (Claude takes it at
its next tool boundary, agy queues it for its turn's end), to interrupt;
that is what `send` always does for other backends. Typing refuses an agent
at a dialog (the Enter would answer it; use `approve`/`deny`, or `agent key`
for a question), and `send` refuses an agent whose session has ended. Slash commands
are always typed, never held.

Typing into Claude waits for its input box to show the message before
pressing Enter, then checks the box emptied, pressing Enter again (3 in
all) while it still shows the message: a busy Claude otherwise takes an
Enter that arrives with the text for a newline. If the message still sits
there, `send` fails saying so (don't send it again), `agent status` prints
an `unsent:` line, and `agent key agent-<name> Enter` submits it. If the
input box can't be read at all, `send` still types the message but says
`Typed into <name> without checking it was submitted: <why>`, and `agent
status` shows an `unchecked:` line until a turn starts.

Messages from an agent start with `[from name@host]`, so the recipient knows
where to reply. Messages from anyone without `AGENT_NAME` start with
`[from a person on host]`, or `[from a Claude session on host]` from an AI
CLI a person started (it sets `AI_AGENT` or `CLAUDECODE=1`). Neither has an
agent record, so the recipient answers in its reply. A slash command like
`/compact` is left unmarked.

Keep typed messages short: a long one arrives in Claude whole, but as a
pasted block, which Claude may treat as data rather than instructions. Put
long instructions in a file and send its path.

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
screen, and the way to look at an agent on another host. It prints plain
text; `--raw` keeps the colors and styling as SGR escape codes (`| cat -v`
to read them), which tell, say, dim placeholder text from typed text. To
watch an agent, use `capture` rather than `tmux attach -r` (see Warts).

### 5. Answer permission prompts

```bash
agent status helper -v                  # detail: the full pending tool call
agent approve helper --expect Bash      # yes, once, only if it's still that prompt
agent approve helper --always           # yes, and don't ask again
agent deny helper                       # no; for Claude this cancels the turn
agent send helper "do it without rm"    # after a deny, redirect it
agent approve helper --expect trust     # trust the folder at its workspace trust prompt
```

Read the detail before approving. `approve` and `deny` refuse unless the
agent is `waiting`, and refuse questions for the user (Claude's
`AskUserQuestion`, agy's `ask_question`, whose detail starts `question:`):
look with `capture` and answer in its session (`agent key agent-helper 2`
picks agy's option 2; `agent keys` types text). A `send` to a waiting agent
is held, never typed into the dialog. `--expect` guards against a
different prompt having replaced the one you read. These commands prompt the
person when an agent runs them.

The CLI's workspace trust prompt is one too (`status -v`: tool `trust`,
detail `trust: <folder>`): `approve` picks "Yes, I trust this folder" (for
good, so no `--always`) and the CLI starts; `deny` picks "No, exit", the CLI
exits, and the agent is `ended (quit)`. Approve only folders you'd let it
work in: trusting one also enables its own hooks and settings.

So is a dialog Claude shows on its own (`status -v`: tool `dialog`, detail
`dialog: <title>; Enter picks: <option>`): `deny` presses Escape, which
closes it (an offer takes that for "not now"; no turn is cancelled), and
`approve` presses Enter, picking the option the detail names. `--expect
dialog` names it; `agent key agent-<name> <key>` answers it otherwise.
Messages held meanwhile go in once it is gone and the agent has been at
its prompt for 10s.

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
agent cleanup --ended --dry-run            # every ended agent here, and why; changes nothing
agent cleanup --ended
```

Cleanup asks each idle agent to exit, kills its session, and deletes its
record, queued messages, state, and files. Its logs stay. Read what you need
from state first. It prints one line per name and exits 1 if any wasn't
cleaned up. Inside an agent, you can clean up only your own helpers and
theirs, never yourself. Given names, it never removes an agent you didn't
name.

`--ended` names them for you: every agent on this host whose status (as
`agent status` resolves it) is ended, that you may clean up. An ended
agent's helpers that are still running stay: for a person it says so, and
inside an agent it skips the ended one too, since only a person could reach
its helpers once it's gone. An adopted agent's tmux session stays running.
It ends with a total; `--dry-run` lists each agent, how it ended, what
happens to its session, and how many held messages go with it.

### After a reboot: resume

```bash
agent resume helper          # an ended agent, on its last session
```

A reboot (or a killed tmux server) ends every agent: `ended (gone)`.
`resume` (or `resume name@host`, for another host's) starts one again on
its last session, in the directory it ran in, keeping its name, backend, parent (its helpers stay its helpers), its
own state keys and `_result`, and delivers messages held for it. Claude
and agy agents only, and not an adopted one (its tmux session is a
person's: start the CLI there yourself). Inside an agent, only your own
helpers. To continue
a session no agent owns, `agent spawn <name> --resume <session>`, or
`--continue` for the most recent one in the directory.

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
has the JSON shapes. A departed agent's `session_id` is the one its
transcript's path names. `resumable` says whether `agent resume` would
continue an agent (claude or agy, not adopted, with a session, ended);
with `--hosts`, the other host works it out, and the field is absent
(unknown) from a host on an older `agent`.

### Sessions: every agent session on the host

```bash
agent sessions                        # one line each: when, harness, project, status, workgroup, title
agent sessions --workgroup            # only sessions a workgroup agent owns (--local: only the others)
agent sessions --harness claude --json   # docs/EVENTS.md's Session shape
```

`sessions` lists the transcripts Claude Code, agy and Kimi Code keep,
whether `agent spawn` started the session or a person ran the CLI: same
fields for both, with `workgroup` naming the owning agent (name, parent,
store, host) or null. Subagent sessions are listed too, with
`parent_session`. Status for a session no workgroup owns comes from the
harness: Claude Code's session registry and what the global hook heard
(`ended (quit)` once it heard the session end) for Claude, agy's presence lock (running
shows `unknown`), and for Kimi the transcript's age; `agent help sessions`
has the rules, and `docs/ARCHITECTURE.md` ("Sessions") where each field
comes from. This host only. `claude_projects_dir`, `agy_brain_dir` and
`kimi_sessions_dir` in the JSON app config change where it looks.

```bash
agent transcript helper               # one session's events as JSON lines: a session line, then events
agent transcript 3f3a9c --follow      # by session id (or its start), live until Ctrl-C
agent transcript helper --from 18342  # resume after the line at that byte: the "at" of your last event
```

`transcript` is the stream a display or script reads, in the framing
`docs/EVENTS.md` describes: each prompt, thinking block, reply and tool
call whole, a tool's result as an update of its call. It takes an agent's
name, a session id or a transcript's path. `--follow` polls every 250ms,
and for a Claude agent sends its closing reply from the Stop hook before
the transcript holds it (a provisional say, updated when the copy lands).
`--until <byte>` ends a window; `--requests` adds what each model call was
sent. `agent logs` is the same events for a person.

## Other Hosts

Each machine has a host name on the bus: `AGENT_HOST` if set, else the
name its bus identity has (`agent bus rename` changes it; a former name in
`AGENT_HOST` means the current one), else the machine's name (on macOS
its LocalHostName, which the network doesn't change; elsewhere the short
hostname). Address an agent on another
host as `name@host`:

```bash
agent list --host hub
agent status helper@hub -v
agent send helper@hub "run the tests"
agent wait helper@hub --for idle,waiting --next --timeout 30m
agent capture helper@hub --lines 100
agent approve helper@hub --expect Bash
agent spawn helper@hub --backend claude --cwd /srv/project tasks/build.md
```

`status`, `capture`, `logs`, `send`, `wait`, `events`, `approve`, `deny`,
`keys`, `spawn`, `resume` and `attach` take `name@host` (`spawn` also takes
`--host hub`). Host names
match ignoring case for this host and the peers it trusts (a hub trusts
every peer): `@il-air-m2` reaches `IL-Air-M2`. A host known only through the
hub is reached by its exact name; a "no bus answering" error names the
hosts this one knows. Not across hosts: `state` (ask the agent to send what
you need, or `capture`), `queue` (use `send`), `cleanup` (clean up on that
host).
The other host's bus serves only its `--remote-ops` (default
`list,status,capture,send,respond,friction,logs`), so `keys`, `spawn` and
`resume` to another host are refused unless it lists them. "no bus answering for host X" means that
host's bus is down, not linked, or doesn't serve that op.

A remote spawn's `--cwd` and task file are paths on that host (`--cwd`
absolute; without it, the agent starts in that user's home directory);
`--task` is read from your own store and its content sent. `--resume`,
`--attach`, `--backend-config`, `--skip-permissions`, and
`--trust-workspace` are refused: run spawn on that host for those. A host
that lists `spawn` lets every linked peer run commands there as its user
(spawn an agent, then approve its prompts), so list it only where you'd
give peers a shell. Spawned by agent `lead` on `laptop`, its
parent there is `lead@laptop`, and it replies with `agent send
lead@laptop`; spawned by a person, it has no parent. Spawn prints the agent as
`name@host` with its status, so `agent wait helper@hub` follows it.

```bash
agent logs helper@hub --lines 20     # what logs prints there, same flags
agent resume helper@hub              # that host's own resume; needs resume in its --remote-ops
```

`logs name@host` returns what `agent logs` prints on that host, cut to fit
a bus message (512 KB; the newest events are kept, and stderr says how
much was left out). `resume name@host` runs that host's resume, so its
refusals are that host's (not claude or agy, no session, not ended,
adopted); like `spawn`, it starts a process as that host's user, so it is
off for peers unless the host lists it, and like a remote spawn it refuses
`--skip-permissions` and `--trust-workspace` (run resume on that host for
those). Inside an agent, the other
host lets you resume only your own helpers there (`lead@laptop`'s).

```bash
agent attach helper@hub              # ssh -t hub '<hub's agent binary>' attach 'helper'
agent resume helper@hub --attach     # ssh -t hub '<...>' resume 'helper' --attach
agent bus ssh hub me@hub.lan         # where ssh goes for hub (default: the host's name)
```

The bus can't carry a terminal, so `attach name@host` and `resume
name@host --attach` run ssh: to the host's destination (`agent bus ssh
<host> <dest>` sets one, `agent bus status` shows them; `~/.ssh/config`
still applies), running the agent binary that host names in its `list`
reply (an absolute path of plain characters, quoted). `agent` becomes ssh
(exec, so a signal to it reaches ssh and nothing is orphaned), and so
needs a terminal. ssh is a separate trust path from the bus: your ssh
keys, not the bus's pinned ones. Attaching to another host needs `ssh
<dest>` to work from here; if ssh fails, its own error is what you see,
after a line saying it was ssh. Over ssh, `resume --attach` runs as you on
that host, as if typed there, not through its `resume` op.

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

The bus runs in tmux session `gomux-bus`. `agent bus restart` restarts it
with the settings it runs with (`--listen`, `--client-port`,
`--remote-ops`); run it after `trust`. nats-server can't reload trust, so
every trust change applies on a restart.

Dropping a peer, or renaming a host:

```bash
agent bus untrust old-laptop      # removes its bundle and ssh destination, restarts the bus
                                  # (--no-restart: later, with agent bus restart)
agent bus rename studio           # this host is now "studio": same keys, the certificate
                                  # signed again under the new name, bus restarted as studio
# copy the printed studio.pub.json to each peer, then on each:
agent bus trust studio.pub.json --renames IL-Air-M2
```

Untrusting on one side cuts the link (untrust prints what the other side
runs to drop it too); untrusting the hub leaves this host reaching no
other host. A rename keeps the keys, so `trust --renames` checks the new
bundle has the old name's key and pin before replacing it, rewrites that
host's references to the old name (its agents' `spawned_by`, friction
links, the `bus ssh` destination), and restarts the bus. A plain `trust`
of a bundle with a trusted peer's keys is refused and points at
`--renames`. Until the hub has re-trusted a renamed leaf, the leaf links
but requests to `name@<new>` don't reach it. Former names stay aliases on
both sides (`bus/host.previous` here, the peer bundle's `aka` there):
`AGENT_HOST=<old>` in agents spawned before the rename means the new name,
and `name@<old>` (in held messages, briefings) reaches the host. Hosts
that only reach the renamed one through the hub keep their references to
the old name.

Trust: links use TLS 1.3 with each side's certificate pinned, and each host
authenticates with its own key; no secrets are shared. The hub routes all
cross-machine traffic, so run it on the machine you trust most; a machine
links to one hub, so only one trusted bundle may carry a `url`. Replies
aren't signed in v1. Status events are hints that `wait` confirms with a
real status check. Text from other hosts is stripped of terminal control
sequences; `capture --raw` keeps only the SGR codes (colors, bold, dim).

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
| `waiting` | `status -v` shows the prompt; approve, deny, or ask the person. Tool `trust` is the CLI's workspace trust prompt: approve trusts the folder. Tool `dialog` is a dialog Claude showed on its own (an offer, a notice): `deny` closes it, and held messages go in. |
| `send` says a dialog took the message, or `Typed into X without checking ...: its input box never showed the message` | A dialog came up as it typed (or the screen isn't one agent reads): `status -v` and `capture` show it; `deny` closes a dialog, then send again. |
| `unknown` (non-hook backends) | `capture` to look; coordinate on state or messages instead. |
| `error` | The last turn failed (e.g. rate limit); it takes input again. `capture` or `logs` shows why. |
| `ended (...)` | The reason says how (`agent statuses`); `agent resume <name>` continues its session (claude, agy); `agent logs <name>` shows its last output; `agent cleanup <name>` removes the record. |
| `wait` timed out | `status -v` and `capture`: still working, stuck, or missed the turn? |
| Stuck at an unexpected screen | `agent key agent-<name> Enter`, or `agent key agent-<name> c-c` to interrupt. |
| A key fails: "a read-only client is attached" | Detach the client it names (`tmux detach-client -t /dev/pts/N`); watch with `agent capture` instead of `tmux attach -r`. |
| Queued messages never arrive (not Claude) | Start `agent watch` (one per host is enough). |
| A held message hasn't arrived | `status -v` shows `held:` and what it waits for: the turn's (or agy's step's) end, 10s at the prompt after an Esc/deny, or an empty input line (clear what's typed there); `agent deliver <name>` types it now if the agent is at its prompt. |
| `send` says the message is still in the input box; `status` shows `unsent:` | Enter didn't submit it: `agent key agent-<name> Enter` does. Don't send it again. |
| Leftover tmux session blocks a spawn | `agent session list`, then `agent session kill agent-<name>`. |
| Spawn says the agent already exists | Pick another name, or clean up the old one; if it has ended, `agent resume <name>` continues it. |
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
  arrive as plain text).
- A message held for a Claude or agy agent extends the turn it lands in:
  `wait --next` and `_result` then cover the reply to the held message
  too. The reply that came before it is in `agent logs`.
- After Esc or a deny (no Stop hook), held messages are typed in once the
  agent has sat at its prompt with an empty input line for 10s. The input
  line is read off the screen, so a Claude or agy redesign of it would leave
  them waiting (`status -v` says `waiting for an empty input line (it isn't
  on screen)`).
- agy's interrupted turns and its permission and `ask_question` dialogs are
  read off its screen (the footer under its input box, the dialog's
  `Question 1/N:`). If an agy update redraws those, an interrupted agent
  stays `processing` with its messages held (`send --now` types anyway), and
  a question shows as `processing` (a send is still held). A turn counts as
  interrupted only once its last hook is 3s old. `export` shows such a turn
  as `processing`, its last report.
- Claude's own dialogs (tool `dialog`) are read off its screen, by their
  footer of key hints (`Enter to confirm · Esc to cancel`) under a border.
  A dialog drawn without one reads as `idle` (send then says its input box
  never showed the message, rather than "Sent"), and `export` shows the
  last report. Only the auto-mode offer is prevented at spawn.
- A message held for agy in the few milliseconds between its last
  PostInvocation and its Stop is typed by the deliverer, 10s later.
- Messages held for an agent that exits are never delivered (`status -v`
  says so); `cleanup` deletes them.
- `queue` needs `agent watch` running on this host, except for Claude and
  agy agents.
- Pre-allowed commands still prompt when run through a variable
  (`$A send`), chained, or piped; see "What a spawned Claude agent may run
  without asking".
- Bus replies are unsigned (see trust, above).
- A read-only tmux client (`tmux attach -r`) attached to any session makes
  tmux refuse `send-keys` to every session. `agent` pastes text and common
  keys past it instead (Enter, Escape, Tab, BSpace, Space, the arrows,
  C-a to C-z, single characters), so `send`, `approve`, and the like still
  work; any other key name fails, naming the client to detach.
- `export` shows the last reported status: an interrupted permission
  prompt stays `waiting`, and a crashed CLI keeps its last status, until
  the next hook. A store written by an `agent` built before export has no
  events: `--follow` re-reads it when it changes, so its spawns and
  cleanups arrive as a whole-store update, not as events.
- `export --hosts` lists other hosts through `list` only: no store or
  transcript for them, and `--follow` re-lists them every 30s. A host on an
  older `agent` sends no `detail`, `session_id`, `cwd`, `attached` or
  `resumable`: absent means unknown.

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
├── bus/              # this host's keys and certificate, peers/, bus.log, ssh.json
└── watch.lock        # held by each running watcher
```

State keys starting with `_` are written by status reports (`_status`,
`_detail`, `_tool`, `_tool_input`, `_result`, `_transcript`, `_turn_at`,
...) and by `send` (`_sent_at`, `_unsubmitted`, `_unchecked`). Delete
the folder to reset that binary's agents.

## Working on gomuxai

Architecture, status detection, the bus, and the macOS internals are in
`docs/ARCHITECTURE.md`. User setup is in `README.md`.

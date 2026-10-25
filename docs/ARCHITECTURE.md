# gomuxai architecture

How the `agent` tool works, for people changing it. For using it, see
[`README.md`](../README.md) (people) and [`CLAUDE.md`](../CLAUDE.md)
(agents). For any command's flags and
behavior, `agent help <command>` is the reference: its examples are tested,
so it stays current where this file can drift.

## Overview

`agent` is one Go binary. Each agent is an AI CLI (Claude Code, agy, aider,
or a custom one) running in its own tmux session. The binary starts those
sessions, types into them, and records everything about them in one SQLite
database per data dir. Claude and agy agents report their own status through
lifecycle hooks that run `agent report --hook`; nothing has to scrape their
screens. An optional bus (embedded NATS, one per host) lets `name@host`
commands, status events, and `agent wait` reach other machines.

Most `agent` commands are short-lived: open the database, do one thing,
exit. The long-running processes are the agents' CLIs, `agent watch`
(delivers queued messages), and `agent bus run` (the bus daemon).

The data dir is `.gomuxai/` next to the binary (`config.DefaultConfig`), so
a dev build in `bin/` has its own (`bin/.gomuxai/`), separate from an
installed one:

```
.gomuxai/
├── gomuxai.db               # agents, runs, agent_events, messages, tasks, state, command_log, friction
├── logs/
│   ├── <name>.log           # tmux pipe-pane output (survives cleanup)
│   └── <name>.transcripts   # "<backend>\t<path>" per CLI transcript (survives cleanup)
├── agents/<name>/           # per-agent bundle, removed by cleanup
│   ├── instructions.md      # the briefing
│   ├── launch.sh            # the backend's full launch command
│   ├── AGENTS.md            # agy only: the briefing, where agy loads it
│   └── .agents/hooks.json   # agy only: the hooks bundle
├── bus/                     # bus identity, trusted peers, bus.url, bus.log
├── state/                   # created by EnsureDirs, unused (state is in the DB)
└── watch.lock               # flock held by each running `agent watch`
```

## Package map

| Package | What it does |
|---|---|
| `cmd/agent` | `main`: runs `cli.Execute`, exits with `cli.ExitCode(err)`. Also holds `Info.plist` for `make macos`. |
| `internal/cli` | Cobra commands, one file per command (or group), output and exit-code conventions, the command log. |
| `internal/service` | `AgentService`: spawn, send, queue, capture, cleanup, status resolution, reports, permission answers, transcripts, the briefing, and the bus RPC handler. |
| `internal/storage` | The `Storage` interface and its SQLite implementation: schema, migrations, queries. |
| `internal/backend` | Backend interfaces (`Backend`, `Provider`, and the optional capability interfaces), the registry, and custom JSON backends (`generic.go`). |
| `internal/backend/namedagents` | The built-in backends: `claude`, `agy`, `aider`, and the hook plumbing they share (`hooks.go`). |
| `internal/backend/types` | `Status`, `HookReport`, `PermissionResponse`, shared by backends and service. |
| `internal/allowcmd` | Decides whether a shell command line is one plain run of a given binary with a safe subcommand (parsed with `mvdan.cc/sh`), for the PreToolUse approval. |
| `internal/tmux` | `TmuxOps` over the `tmux` command: sessions, send keys, capture, pipe-pane. Errors carry tmux's own message. |
| `internal/natsbus` | The bus: contract (subjects, ops, wire types), identity and trust, the daemon's server config, RPC server and client, status events and `Wait`. `natstest` starts throwaway NATS servers for tests. |
| `internal/transcript` | Readers for Claude Code and agy session transcripts, into one `Session`/`Turn`/`Step` shape. |
| `internal/watcher` | The `agent watch` loop: delivers queued messages when their agent is idle. |
| `internal/export` | The tree `agent export` prints: a snapshot across stores and hosts, kept current from agent events, sessions, and transcripts. |
| `internal/config` | `Config`, its defaults (data dir next to the binary, host name, startup delay), and paths derived from it. |
| `internal/macos` | Local Network support for the bus on macOS: Info.plist detection and disclaimed spawn. |
| `internal/buildinfo` | The build id (`c7253f9-dirty`, a version, or `devel`) from the Go toolchain's embedded VCS info. |
| `internal/util` | Shell quoting, ANSI stripping, terminal sanitizing, terminal-log cleanup (`TerminalLines`), "did you mean" suggestions. |

## Why tmux

Each agent runs in a tmux session named `agent-<name>` (`Config.SessionPrefix`).
tmux gives:

1. **Process isolation.** An agent can crash without taking others down.
2. **Persistent terminals.** Sessions survive disconnects, and a person can
   `tmux attach` to watch or steer any agent.
3. **Scriptable I/O.** `send-keys` types messages and answers prompts;
   `capture-pane` reads the screen; `pipe-pane` logs everything.
4. **Escape hatches.** `agent session`, `keys`, `key`, and `clear` expose
   raw tmux for when the higher-level commands fail.

`internal/tmux` runs the `tmux` command itself, and a failure's error
(`tmux.Error`) carries what tmux printed on stderr ("tmux send-keys: can't
find pane: %9"). Session targets are exact (`=name`): tmux otherwise falls
back to a prefix match, so `agent-a` could reach `agent-ab`. Every
session-level operation targets the session's first pane by id.

`capture` reads plain text (`capture-pane -p`); `capture --raw` and
`TmuxOps.CaptureStyled` add `-e`, which keeps the SGR escape codes (colors,
bold, dim). tmux re-encodes the screen, so neither carries the program's
other escape sequences.

**Read-only clients.** While any client attached with `tmux attach -r` is
on the server, tmux refuses `send-keys` to every session ("client is
read-only"): with no `-c`, send-keys checks the most recently active client,
whichever session it watches. The check is meant to keep a read-only viewer
from typing through key bindings, not to lock the session, and
`paste-buffer` isn't refused. So on that refusal, `SendKeys` and
`SendSpecialKey` paste instead: `set-buffer`, then `paste-buffer -d -r -S`
(`-r` keeps newlines, `-S` keeps control characters, which tmux 3.5+
otherwise turns into `^C`; without `-p` there's no bracketed paste), which
types the same bytes `send-keys -l` does. A key is pasted as the bytes a
terminal sends for it, for names with fixed bytes: Enter, Escape, Tab,
BSpace, Space, the arrows (in the pane's cursor key mode,
`#{keypad_cursor_flag}`), C-a to C-z, and single characters. Any other key
name fails with a `tmux.ReadOnlyError` that names the read-only clients
(`list-clients`) and says to detach them or watch with `agent capture`.

The cost is that input is typed, not delivered: a message is "sent" once its
keys are in the terminal, whether or not the CLI read it, and anything typed
at a dialog answers the dialog. `Send` and the watcher guard against that
(see [Messages](#messages)). The alternatives considered for sharing a TUI
are in `exploration/tmux-tui-connector-ideation.md`.

## Spawning an agent

`AgentService.Spawn` (`internal/service/agent_service.go`):

1. Validates the name (`^[A-Za-z0-9][A-Za-z0-9_-]*$`): it becomes a tmux
   session name, a path component, and a bus subject token.
2. Refuses if the agent record or the tmux session already exists.
3. Creates the session with `AGENT_NAME`, `AGENT_HOST`, and `PATH` prefixed
   with the binary's directory, and starts `pipe-pane` into
   `logs/<name>.log`.
4. Stores the agent record, with `spawned_by` from the caller's
   `AGENT_NAME` ("" for a person) and the spawn's cwd, and in the same
   transaction opens a run and records a `spawned` agent event (see
   [Export](#export)). The record exists before the CLI starts, because
   hooks report against it immediately.
5. Builds the launch command from the backend's capabilities (below), writes
   it to `agents/<name>/launch.sh`, and types only `sh <path>` into the
   session. Typed input sits in the terminal's canonical-mode buffer until
   the shell's line editor is up, and macOS caps that at 1024 bytes
   (`MAX_CANON`); Claude's inline hook settings alone are longer.
6. Waits `StartupDelay` (2s), confirms the workspace trust prompt if
   `--trust-workspace`, and types the first message unless it went on the
   command line. The first message is the `--task` content, or the backend's
   "read this task file" line for a task file.

For a built-in backend, any failure after the session exists kills it and
deletes the record and the agent dir.

Built-in backends implement `backend.Provider` plus whichever optional
interfaces (`internal/backend/provider.go`) they support; `launchCommand`
applies each one it finds:

| Interface | claude | agy | aider |
|---|---|---|---|
| `HookProvider`: status hooks | `--session-id <id> --settings '<hooks json>'` | `--add-dir <agent dir>` with `.agents/hooks.json`, plus the exit wrapper | — |
| `InstructionsProvider`: the briefing | `--append-system-prompt-file instructions.md` | `AGENTS.md` in the `--add-dir` directory | — |
| `CommandAllower`: pre-allowed commands | `--allowedTools 'Bash(agent status:*)' ...` | — | — |
| `PromptFlagProvider`: first message on the command line | — | `-i '<task>'` | — |
| `TrustPrompter`: answers the trust prompt | moves to "Yes, I trust this folder" first (the default is "No, exit") | Enter | — |
| `NativeStatusProvider` | `claude agents --json` | — | — |
| `PermissionResponder` | yes | yes | — |
| `TranscriptProvider` | `transcript.Claude` | `transcript.Agy` | — |
| `HookDeliverer`: held messages from a hook's output | `Stop` → `additionalContext` | `PostInvocation` → `injectSteps` + `force_continue` | — |
| `HookAllower`: approves a safe `agent` command from a hook | `PreToolUse` → `permissionDecision: allow` | — | — |
| `InputReader`: the input line, so typing never lands in a draft | after `❯` | after `>` | — |
| `TurnEndReader`: a turn that ended with no hook | — (native status) | the footer under the input box | — |
| `QuestionPrompter`: a question dialog with no hook | — (`PermissionRequest`) | `ask_question`'s `Question 1/N:` | — |

Notes on each:

- **Hooks** are wired only when `Config.AgentBinary` is set (it defaults to
  the running executable). Each hook runs
  `<binary> report --hook --as <name> --spawn-id <uuid>`. The uuid is made
  per spawn and stored as both `_spawn_id` and `_session_id`; Claude gets it
  as `--session-id`. Nothing is written into the agent's project. Claude
  registers 11 events (`claudeHookEvents`); agy registers `PreInvocation`,
  `PreToolUse`, `PostToolUse`, `PostInvocation` (delivery only), and
  `Stop`, each with `--event <Event>`
  because agy's payload omits the event name and runs hooks with the bundle
  dir as cwd. Each hook has a 10s timeout.
- **agy's launch command** is prefixed with
  `env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY`: with those set (a tmux
  server started over SSH passes them on) agy skips the OS keyring and comes
  up signed out.
- **The briefing** (`service/instructions.go`) tells the agent its name,
  host, and who started it, how to message, wait on, and read other agents,
  how to run helpers, and how to file friction. It says that a message
  arriving as Stop hook feedback is a message to act on, and that `send
  --now` interrupts a busy agent. It uses the binary's full path, since the
  agent's shell may not have it on `PATH`.
- **Pre-allowed commands** (`agentSafeCommands`): `send`, `deliver`, `list`,
  `status`, `wait`, `events`, `capture`, `logs`, `state get`, `history`,
  `friction add`, `friction +1`, `friction list`, `friction show`. Each becomes two rules, `agent <cmd>` and
  `<full binary path> <cmd>`, for that Claude session only. Left out on
  purpose: `approve`, `deny`, `keys`, and `send --force` (one agent answering
  another's prompts), `spawn` and `cleanup` (a person approves each), and
  `friction accept`/`resolve` (for the people working on the tool). Rules
  match the literal command text, so `./bin/agent send` or `agent -q send`
  would still prompt; the PreToolUse hook covers those (next item).
- **The PreToolUse approval** (`AgentService.hookApproval`, `allowcmd`):
  for a Bash call, the hook prints Claude's
  `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow",...}}`
  when the command is exactly one run of this binary with a subcommand in
  `agentSafeCommands` (the same list the rules come from), and nothing
  otherwise, so the normal permission flow decides. The command is parsed
  with `mvdan.cc/sh/v3/syntax` (bash dialect). The program may be the bare
  name (found on the hook's `PATH`), an absolute path, or a path relative
  to the payload's `cwd`; it must resolve, symlinks followed, to
  `Config.AgentBinary`. Only `-q`/`--quiet` may come before the
  subcommand. Refused: more than one command, pipes, `&&`/`||`, `&`, `!`,
  subshells and compound commands, assignments (`A=1 agent ...`), `env`
  and other wrappers, any `$` expansion or substitution (so `$A send`
  still prompts: the hook can't see the shell's variables), globs, braces,
  tildes, backslashes outside double quotes, comments (zsh without
  `interactive_comments` reads `#` as text), and every redirection except
  `>/dev/null`, `2>/dev/null`, `&>/dev/null` (and `>>`), `2>&1`, and
  `>&2`. Unquoted text is limited to `[A-Za-z0-9_@%+=:,./-]` with no
  leading `=` (zsh's `=cmd`), so a word means the same in bash and zsh;
  anything else must be quoted. It never denies. Checked live (Claude Code
  2.1.310): the allow skips the prompt in default mode, and a settings
  `deny` rule still wins over it. The decision is made before the status
  report is recorded, so a failed report doesn't lose it; an unknown agent
  or stale spawn id gets nothing.
- **Trust prompts** are answered only with `--trust-workspace`, since
  trusting a repo also enables its own hooks and settings. Spawn polls the
  screen up to 8 times, 500ms apart, and goes on if no prompt shows.
- **Custom JSON backends** (`--backend-config`) have no provider: their
  launch command is typed as is, with no launch script, hooks, briefing, or
  dialog handling. The registry knows them only from the file, so it must be
  passed again to `status`, `wait`, and `watch`.

### spawned_by and cleanup

`spawned_by` builds the agent tree that `list` shows (helpers after their
parent) and that `cleanup` enforces (`internal/cli/cleanup.go`). A person may
clean up anything. An agent may clean up only its descendants, never itself
(its own session would die partway), and a helper's own helpers must be
named in the same command, since once the helper's record is gone nothing
records whose they are.

`AgentService.Cleanup` types the backend's exit command (`/exit`) into an
idle agent and waits 1.5s, so the CLI exits cleanly and its hooks record it.
Then it stops pipe-pane, kills the session, deletes the agent's messages
and agent dir, and deletes the record; state rows go with it through the
foreign key's `ON DELETE CASCADE`. The same transaction closes the agent's
run (with its last `_status` and `_transcript`) and records a `removed`
agent event, so the run stays readable (`export --all`). The terminal log
and the transcripts list stay.

## Messages

`agent send` (`AgentService.Send`) types the message and Enter, or holds it
(below). A message
from an agent (`AGENT_NAME` set) is prefixed `[from <name>@<host>] `, and
one without `AGENT_NAME` `[from a person on <host>] `, or `[from a Claude
session on <host>] ` when the environment shows an AI CLI (`AI_AGENT`'s tool
name, the part before its first `_`, or `CLAUDECODE=1`; `unrecordedSender`).
Those name no agent to reply to, and their slash commands stay unmarked, or
they would no longer be ones (`markSender`). Before
typing, `checkCanSend` refuses an `exited` agent (the text would run in the
shell left behind), a `waiting` agent, and any agent with a permission or
trust dialog on screen (the Enter would answer it). `--force` skips the
checks, and only works for a person at a terminal: it is refused when
`AGENT_NAME` is set or stdin isn't a TTY. Right before typing,
`SendUnchecked` records `_sent_at` (spawn does too, for the first message),
for `wait --next`.

### Held messages

Typing into a busy Claude agent lands in its input box, mixed with
whatever a person is drafting there, and nothing says when it was read
(agy queues typed text for the turn's end, but typing into its
`ask_question` dialog answers it). So
for an agent that *holds messages* (its backend is a `HookDeliverer` and
spawn wired its hooks: `_spawn_id` is set), `Send` without `--now` does
this (`service/held.go`):

| Status `Send` finds | What it does |
|---|---|
| `idle`, `error`, `unknown` | `typeHeld`: claims any messages still held, and types them and this one as one prompt, oldest first (after the dialog check) |
| `processing`, `waiting` | `hold`: stores a `pending` row in `messages`, records `_sent_at`, re-checks (below), and starts a deliverer |
| `exited` | refuses |

Slash commands are never held: a hook can only hand over text. Who
delivers a held message:

1. **The delivering hook.** `report --hook` calls `HandleHook`, and for an event
   the backend `CanDeliver` (Claude: `Stop`, with or without background
   tasks; agy: `PostInvocation`, below), `reportDelivering` claims every pending row and writes the
   status report in one transaction. With messages claimed, the report is
   `processing`, event `delivered`, detail `N held messages delivered`,
   and it clears `_result` and sets `_turn_at` (the turn goes
   on, so `wait --next` waits for its real end and `_result` isn't the
   interim reply). The hook prints `DeliveryOutput`: for Claude,
   `{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":
   "<N> messages arrived while you were working, oldest first:\n\n..."}}`.
   Claude continues the turn with it and shows it as `Stop hook feedback:`
   (a `{"decision":"block"}` works too but shows as `Stop hook error`). With
   nothing held, the report and output are as before: idle, no output.
   `ReportHook` (status only) never claims.

   agy delivers from `PostInvocation`, which fires after each model call
   and its tools, including the turn's last, and prints
   `{"injectSteps":[{"userMessage":"[from lead@hub] ..."},...],"terminationBehavior":"force_continue"}`:
   one `userMessage` per message, each shown in the pane as a `> [from ...]`
   prompt of its own, which the model answers in the same turn
   (`exploration/agy-delivery-findings.md` compares the alternatives: Stop
   `continue` and the other step types work but show nothing). The output
   is built with `encoding/json` from a struct of exactly those two fields,
   since agy parses it strictly and drops a bad one silently. A message
   over 64 KB is split into labeled parts (`[part 2/3 of the message
   above]`), since agy cuts a step at ~192 KB without a word. Its
   `HookReport` is `DeliveryOnly`: with nothing held, the hook prints
   nothing and records nothing (it says nothing `Stop` won't). A failed or
   `USER_CANCELED` invocation is left to `Stop`: `force_continue` would
   override the cancel. agy's other hooks never print.
2. **`hold`'s re-check.** The turn may have ended between `Send`'s status
   check and the insert, its Stop finding nothing. The claim is the
   transaction's first statement, so it takes SQLite's write lock at once:
   a row inserted concurrently is either claimed by the hook, or committed
   after it, and then the hook's report is visible to `hold`'s re-read.
   If the last report says the turn is over (`turnOver`: `idle`, `error`,
   or a `Stop` that left background work running, while native status
   still says busy), `hold` types the held messages at once. The same
   path covers native status lagging a `Stop`, and a message sent during
   background work (the prompt takes input while the work runs).
3. **The deliverer.** A turn can end with no hook at all: observed on
   v2.1.310, Esc during a tool call or at a dialog, and a deny, fire
   nothing (no `Stop`, no `PostToolUseFailure`, and no `idle_prompt`
   Notification later either, though that follows a normal `Stop` after
   60s). Only native status sees the agent back at its prompt. So `hold`
   starts `agent deliver --wait <name>` (`startDeliverer`), detached with
   `setsid` so it outlives the sender's Bash tool call, logging to
   `agents/<name>/deliver.log`. It holds `agents/<name>/deliver.lock`
   (`flock`; one that finds it taken exits at once, and the holder looks
   again after releasing it, so a message held in between isn't missed).
   `DeliverHeld` checks every 2s; once the agent has been at its prompt
   (`idle`/`error`, or the background-work `Stop`) with an empty input
   line for 10s, it calls `typeHeld`. The grace gives a person who pressed
   Esc time to type first, and restarts whenever the input line has text.
   It exits when nothing is held, or the agent has exited or is gone. A
   `StopFailure` (rate limit) is delivered the same way. agy fires no hook
   on Esc or Ctrl+C either, and has no native status: its status turns
   `idle` from the screen (`TurnEndReader`, see Status resolution), and the
   deliverer goes on from there.
4. **The next `Send`** that finds the agent at its prompt types the held
   messages ahead of its own; `agent deliver <name>` does it by hand.

**The input line.** `typeHeld` (every non-`--now` typing into an agent
that holds messages) first reads the input line off a styled capture
(`TmuxOps.CaptureStyled`, `capture-pane -e`) through the backend's
`InputReader`. Claude's is the text between the last two `─` rules, after
the `❯` prompt, with dim text (SGR 2: the `Try "..."` placeholder and
suggestions) left out; agy's is the same box after its `>` prompt. After
Esc, agy moves text a person queued during the turn back into it. If it has text in it, or isn't on screen, typing
would land in a person's draft, so `typeHeld` returns `InputBlockedError`:
`Send` holds the message instead (`Held for X until its input line is
empty`), the deliverer keeps waiting, and `status -v` shows `held: N,
waiting for an empty input line (it has text in it)`.
5. **`agent watch`**, if running, delivers pending rows as before.

Every deliverer claims rows before delivering (`ClaimPendingMessages`, or
`ClaimMessage` per row for the watcher), so each message is delivered once;
`typeHeld` puts its rows back to `pending` if typing fails. Messages
held for an agent that exits stay `pending` (`status -v` says they won't
be delivered) until `cleanup` deletes them. `StatusInfo.Held` counts the
pending rows, shown by `status -v`, `--json`, the orientation, and the
`status` RPC.

Remote sends get this on the receiving host: `SendRequest.now` (ignored by
older hosts, which always type) and an optional `SendResponse` (older hosts
send none, and the CLI says just `Sent`); `SendResponse.held_until` is the
backend's `DeliveryPoint` (`its turn ends`, `its current step ends`), for
the `Held for X until ...` line. Backends without `HookDeliverer` keep
typing.

### Queue

`agent queue` to an agent that holds messages is `Send` (no watcher
needed). For other agents it stores a `pending` row in `messages`. `agent
watch` (`internal/watcher`) delivers it: every `--interval` (2s) it checks
each agent that has pending messages and, when the agent is `idle` or
`error` (a failed turn leaves the CLI at its prompt, and no hook fires again
until the next prompt), claims the oldest and types it (`Send` with `Now`).
`processing` and `waiting` hold the message;
`exited` fails it, as do 5 `unknown` checks in a row or 3 failed sends
(backoff 1s, doubling). Each watcher holds a shared `flock` on
`.gomuxai/watch.lock`, which is how `queue` tells whether one is running.

## Status

### Resolution: native, report, scrape

`AgentService.StatusInfo` (`internal/service/status.go`) resolves status
from three sources, most authoritative first:

1. **native**: the CLI's own view of the session (`NativeStatusProvider`).
   Claude only: `claude agents --json`, matched on `_session_id`, mapping
   `idle`, `busy` → `processing`, and `waiting`/`blocked` → `waiting`. The
   output is cached for 500ms per process (so `list` runs the CLI once, not
   once per agent), with a 3s timeout; a failure is cached too.
2. **report**: the last status written by a hook or `agent report`.
3. **scrape**: the backend's idle and processing regexes over the last 50
   lines of the screen, for backends with neither. A custom backend needs
   its `--backend-config` loaded for this.

The rules between them:

- Native wins when it disagrees with the report. An interrupted Claude
  permission prompt fires no hook, so the report would stay `waiting`
  forever. The report's event, detail, and tool are shown only while the
  report agrees with native.
- If the CLI answers but doesn't list the session, and the last report says
  the agent is alive, the status is `exited` ("session not running"): the CLI
  died without its `SessionEnd` hook (crash, `kill -9`, OOM). Otherwise the
  watcher would type queued messages into the shell.
- A session listed in a state the backend doesn't map counts as alive with
  no native answer; the report decides.
- **After a deny**, Claude's session list still says `waiting` for a moment
  (under a second observed), and a deny fires no hook. Within 5s of a
  recorded `denied` report that left no pending prompt and no dialog on
  screen (`deniedSince`), the report's status wins, so a `send` right after
  `deny` isn't refused.
- **agy dialog promotion.** agy fires `PreToolUse` before it shows a
  permission dialog and nothing while it's up. When the report is
  `processing` from `PreToolUse` and `AgyBackend.PermissionPromptVisible`
  sees the dialog at the bottom of the screen, the status becomes `waiting`
  with detail `permission: ...`, so `approve`/`deny` (and `--expect`) work
  without `--force`. Its `ask_question` dialog works the same way
  (`screenStatus`, `QuestionPrompter`): after its `PreToolUse`, the
  dialog's `Question 1/N:` heading or `↑/↓ Navigate · enter Select · esc
  Skip` hint above the `esc to cancel` footer makes it `waiting`, detail
  `question: ask_question: Which color? (red, green)` (from the tool's
  input), so `send` holds instead of typing (whose Enter would pick the
  highlighted option, its text becoming an untagged prompt), `send --now`
  and `typeHeld`'s dialog check refuse, and `approve`/`deny` refuse it as a
  question. A question whose options are Yes and No isn't taken for a
  permission prompt.
- **agy turns that end with no hook.** Esc and Ctrl+C fire nothing in agy
  (not even the running step's `PostToolUse`), so the report stays
  `processing`. When it has stood for `turnEndLag` (3s, time for agy to
  redraw after a hook), is not a `Stop` (one that says `processing` left
  background tasks running), and `AgyBackend.AtPrompt` sees the input box
  at the bottom with an idle footer under it (`? for shortcuts`, or a blank
  left side while text is typed; a turn shows `esc to cancel` or `Press up
  to edit queued messages`), the status is `idle`, source `scrape`, detail
  `interrupted; at its prompt` when agy's `Interrupted · What should
  Antigravity CLI do instead?` line is right above the box (else `at its
  prompt; no hook reported the turn's end`). Nothing is recorded: the next
  hook reports as usual, and `export` keeps showing the last report. The
  screen was chosen over agy's log (`Cancelling conversation <id>`, with a
  per-agent `--log-file`): it is the same kind of signal gomuxai already
  reads for agy's dialogs, it needs no log tailing, and it catches any turn
  that ends without a hook, not only a cancel. Only a footer known to be
  idle counts, so an unknown screen (a menu, a redesign) stays
  `processing`.

`StatusInfo` also returns the last report as recorded (`ReportedStatus`,
`ReportedAt`), which `wait --next` uses to spot a whole turn between checks,
and the times of the last send and turn start (`SentAt`, `TurnAt`), which it
uses to spot a whole turn before the wait.

### Reports and the reserved state keys

`AgentService.Report` writes these keys for the agent in one transaction
(`SetStates`), so a reader never sees half of two concurrent reports:

| Key | Holds |
|---|---|
| `_status` | `idle`, `processing`, `waiting`, `error`, `exited` (or `unknown`) |
| `_event` | the hook event, `report` for `agent report`, `approved`/`denied`, `delivered` (a Stop handed over held messages) |
| `_detail` | one line of context, e.g. `Bash: go test ./...` or `permission: ...` |
| `_tool` | the tool the event concerns |
| `_tool_input` | the full JSON input of a call that may need permission (≤64 KiB); always written, so it never outlives its call |
| `_result` | Claude's last reply (`Stop`'s `last_assistant_message`); written only when set or cleared |
| `_session_id` | the CLI's session id; spawn sets it, hooks update it (`/clear`, `/resume`, agy's conversation id) |
| `_spawn_id` | the spawn's uuid, set once by spawn |
| `_turn_at` | when the last turn started: written with a report that is `processing` and clears `_result` (Claude `UserPromptSubmit`, agy's first `PreInvocation`) |
| `_sent_at` | when a message was last typed in; written by `send` (and queued deliveries and spawn's first message), not by `Report` |
| `_transcript` | the CLI's transcript path from the last hook payload that named one |
| `_prompt.<nanos>` | one per unanswered Claude permission prompt: its call key, subagent, tool, input, and detail (JSON) |

**Pending prompts.** Claude raises one `PermissionRequest` per call, all at
once for parallel calls, and shows them one at a time ("1 of 4"), oldest
first; a background subagent's prompts fire in the main session too, with
`agent_id`, and can outlive the main turn's `Stop`. So the last report alone
can't say which prompt is on screen. Each `PermissionRequest` also stores a
`_prompt.<nanos>` key (its own key, so concurrent hook processes can't lose
each other's), and while the status is `waiting`, `StatusInfo` describes the
oldest one, adding "; N more queued". A prompt ends when its call's
`PostToolUse`/`PostToolUseFailure` arrives (matched by a hash of tool and
input, and the subagent), when `approve`/`deny` answers it, when the main
agent takes a prompt (`UserPromptSubmit`, for its own prompts), or when a
turn ends with nothing running (`Stop` with no background tasks,
`StopFailure`, `SessionStart`, `SessionEnd`). Report writes and deletes in
one transaction (`RecordReport`), which also appends a `status` agent event
(see [Export](#export)).

Timestamps are stored with nanoseconds: a whole turn can fit in one second,
and `wait --next` tells reports apart by time. Every recorded report also
publishes a status event (see [Status events](#status-events-and-wait)).

`--spawn-id` guards against stale writers: a report or hook whose spawn id
isn't the agent's current `_spawn_id` is dropped. Without it, the
`SessionEnd` of the Claude that `cleanup` just killed could land on a
respawn of the same name.

Hook mode (`report --hook`) reads the payload on stdin, writes stdout only
to hand over held messages (Claude's `Stop`, agy's `PostInvocation`; see
Held messages), and exits 0 even when the report fails (errors go to stderr), so a reporting
problem can't disturb the agent. It still exits 1 if the binary can't start,
e.g. the database can't be opened.

### Hook mapping

The observed payloads and firing order are in
`exploration/claude-hooks-findings.md` and `exploration/agy-hooks-findings.md`.
What the code maps them to:

**Claude** (`ClaudeBackend.ParseHookEvent`, `namedagents/claude_hooks.go`):

| Event | Status | Notes |
|---|---|---|
| `SessionStart` | idle | clears `_result`, except after `compact` |
| `UserPromptSubmit` | processing | clears `_result` |
| `PreToolUse`, `PostToolUse` | processing | detail `Tool: <its file, else its most telling input>`, from the tool table (`transcript.DescribeTool`), paths absolute; `PostToolUse` ends the call's pending prompt. `PreToolUse` for Bash carries the command and `cwd`, and prints an allow for a safe `agent` command (see [Spawning](#spawning-an-agent)) |
| `PostToolUseFailure` | processing | detail `failed: ...`; ends the call's pending prompt |
| `PermissionRequest` | waiting | detail `permission: ...` (plus `(from the <type> agent)` for a subagent's); stores `_tool_input` and a pending prompt |
| `PreCompact` | processing | |
| `Notification` | idle | only `idle_prompt`; `permission_prompt` arrives ~6s late and is ignored. `idle_prompt` also fires ~60s into background work, so `ReportHook` drops it while the last report is a `Stop` that left the agent `processing` |
| `Stop` | idle | sets `_result`; stays processing while background tasks (shells, subagents) are running, since their completion starts a new turn. With messages held, processing, event `delivered`, and the hook prints them (see [Held messages](#held-messages)) |
| `StopFailure` | error | e.g. API error, rate limit |
| `SessionEnd` | exited | ignored for `reason: clear` (a `SessionStart` follows) |

**agy** (`AgyBackend.ParseHookEvent`, `namedagents/agy.go`):

| Event | Status | Notes |
|---|---|---|
| `PreInvocation` | processing | |
| `PreToolUse` | processing | stores `_tool_input` for dialog promotion; `ask_question`'s detail is its first question and options |
| `PostToolUse` | processing | `failed: ...` on error |
| `PostInvocation` | — | `DeliveryOnly`: recorded (processing, `delivered`) only when it hands over held messages; ignored on an error or `USER_CANCELED` |
| `Stop` | idle, processing, or error | error for an error or `ERROR`/`MAX_*` termination; processing when `fullyIdle` is false |

agy has no `SessionEnd` hook and no session list, so its launch command ends
with `; <binary> report exited --as <name> --spawn-id <uuid>`: the shell
records `exited` however agy ends (`/exit`, crash, `kill -9`), and the spawn
id keeps a stale wrapper from marking a respawn `exited`. Arguments added
later (`--add-dir`, `-i`) are inserted before that suffix.

### Answering permission prompts

`AgentService.RespondPermission` refuses unless the agent is `waiting`,
refuses if `--expect <tool>` names a different tool, and refuses dialogs that
are questions rather than yes/no prompts (Claude's `AskUserQuestion` and
`ExitPlanMode`, agy's `ask_question`). The status it checked can be hundreds
of milliseconds old, so right before the keypress it captures the screen and
requires the dialog to still be there: Claude's deny key is Escape, and
Escape on a running turn interrupts it. That narrows the race to a few tmux
calls; it doesn't close it. Keys: `1` approve, `2` approve always, Escape
deny. It then records the expected outcome (`processing`/`approved`, or
`idle`/`denied`), because a denied prompt fires no hook, and drops the
answered pending prompt, so the next one is described while the approved
call runs. Deny drops the rest of that agent's batch too (Escape cancels
it), and denying a subagent's call records `processing`: only that call
ends, and the turn goes on.

## Results and transcripts

**`_result`** is Claude's final reply for the turn, from the `Stop` hook. It
is emptied when a new prompt is submitted (or on `/clear`), so an empty value
means the turn hasn't finished. A turn that leaves background work running
stays `processing` and stores no result. agy's `Stop` payload carries no
reply, so agy agents never set `_result`; read them with `agent logs`.

**`_transcript`** and `logs/<name>.transcripts`: hook payloads name the CLI's
own transcript file (Claude `transcript_path`, agy `transcriptPath`).
`ReportHook` stores the path in `_transcript` and appends
`<backend>\t<path>` to `logs/<name>.transcripts` when it differs from the
last line. The list lives beside the log, so like the log it outlives
cleanup and spans `/clear` and respawns under the same name. The files:

- Claude: `~/.claude/projects/<encoded project>/<session id>.jsonl`
- agy: `~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript_full.jsonl`

**`internal/transcript`** reads both into the event stream
[`docs/EVENTS.md`](EVENTS.md) describes (types in `events.go`): one event per
prompt, thinking block, reply, tool call and harness record, each tagged with
its turn, a tool's result arriving as an update of its call. A `Reader`'s
`Parser` takes a transcript's bytes in order and returns the events of the
whole lines among them, keeping what later lines need (the open turn, calls
waiting for results), and reports where it stopped (`Pos`), so the same code
reads a whole file or what a live session appends. A parse always starts at
the file's start, because what a line means depends on the lines before it;
`ResumeAfter` resumes after a line by reading the lines up to it silently
(`Skip`), so what follows is numbered and turned as the whole stream is.
`Latest` folds a stream into each event's latest state.
`Finals` merges the final reply a Stop hook reports (`last_assistant_message`)
into a stream ahead of the transcript, which writes it only after the hook ran:
a provisional say that the transcript's copy then updates. `HarvestClaude`
reads a session's metadata alone, line by line, by the same rules.
Backends hand out their reader through `backend.TranscriptProvider`. The
behaviour was ported from glyph3d-rs `native/src/agent_transcript/` and
glyph3d-js's session adapter, and `testdata/corpus` holds the transcripts
with the exact events they must produce. `ReadFile` parses the whole file a
megabyte at a time but keeps only the last events it was asked for and their
updates, since transcripts run to tens of megabytes (75 MB parses in about
0.2 s); the parser forgets calls once they are done. The
formats are the CLIs' internal files, not APIs: lines that aren't JSON
objects are counted in `Parser.Skipped`, and unknown records are ignored.

**`agent logs`** (`internal/cli/logs.go`) reads the newest transcript, going
back to older ones until it has `--lines` events (each in its latest state),
and prints `--- session <id>` where each earlier session ends; `--json`
prints the stream itself, a session line then its events. It falls back to the
terminal log (with a hint saying why) when there's no transcript: another
backend, no hook fired yet, or the file is gone or unparseable. `--terminal`
asks for it directly. The terminal log is the raw pipe-pane stream, cleaned by
`util.TerminalLines` (escapes dropped, cursor moves turned into spaces and
line breaks, redraws collapsed). Logs are per host; `logs name@host` is
refused in favor of `capture`.

## Export

`agent export` (`internal/cli/export.go`, `internal/export`) is the
supported feed of the agent tree for displays: what they would otherwise
read from the database's tables, which aren't an interface.

### Where it comes from

- **Agent events.** Spawn, cleanup (and a failed spawn's cleanup), and
  every recorded status report append a row to `agent_events` in the same
  transaction as the write it describes, so an event is there exactly when
  its change is. Ids only grow (`AUTOINCREMENT`, and SQLite commits one
  writer at a time), so a follower tails `id > cursor`. A status event
  carries the report; `tool_input` only for a `waiting` report, since every
  tool call's input would bloat the table. The newest 20000 are kept
  (pruned every 100th insert, like the command log).
- **Runs.** Spawn opens one, cleanup closes it with the last status and
  transcript, so `--all` lists departed agents without the command log.
  A store from before runs has its departed agents rebuilt from the command
  log's spawns (newest per name, `run` 0) and `logs/<name>.transcripts`.
- **The snapshot** reads agents, runs, the reserved status keys, and the
  newest event id in one read transaction, so the snapshot holds every
  event up to its cursor and none after.
- **Status** is the last report, not `StatusInfo`: export runs no backend
  CLI (`claude agents --json`), so any number of followers cost a query
  every 250ms each. Known differences from `agent status`: an interrupted
  permission prompt stays `waiting`, a crashed CLI keeps its last status
  (`attached` stays true: the shell is still there), and with several
  queued prompts `detail` describes the newest, not the one on screen.
- **Sessions and transcripts** are checked every second: `tmux
  list-sessions -F '#{session_name}'` and a stat of each live agent's
  transcript. A change is a `changed` line with `reason` `session` or
  `transcript`.
- **Other stores.** An agent spawned by another binary is recorded in that
  binary's store. For each `agent-*` session no read store records, export
  asks tmux for the session's `PATH` (spawn puts the binary's dir first),
  opens `<dir>/.gomuxai/gomuxai.db` read-only, and adds it (a `store` line
  while following). `--store` adds one up front. A store written by an
  older binary has no `agent_events`: `--follow` checks its `PRAGMA
  data_version`, re-reads it when that moved, and sends a `changed` line
  (no `id`) per agent that changed, or a whole `store` line when agents
  came or went, since nothing says whether they were spawned or cleaned up. Sessions still unmatched are listed as `unrecorded`.
- **Other hosts** (`--hosts all|h1,h2`) come from the bus's `list` op, so
  they have no store, cwd, transcript, or `attached`, and their status is
  the remote host's resolved status. `--follow` lists them again every 30s
  and sends a `host` line when one changed. No bus protocol was added.

### The stream

Without `--follow`, `--json` prints the snapshot, indented. With it, every
line is one compact JSON object; the first is the snapshot, unless
`--since <id>` resumes (only when the store still keeps every event after
`<id>`; otherwise it starts with a snapshot and says so on stderr). A
follower keys agents by `host` + `store` + `name`, and `run` tells two
runs of one name apart. Every agent object has the same fields:

```json
{"name":"lead-t1","host":"laptop","kind":"tmux","run":12,"parent":"lead","backend":"claude",
 "session":"agent-lead-t1","store":"/home/user/gomuxai/bin/.gomuxai","cwd":"/home/user/gomuxai",
 "created_at":"2026-10-24T14:00:00.5Z","live":true,"attached":true,
 "status":"waiting","event":"PermissionRequest","detail":"permission: Bash: rm -rf build","tool":"Bash",
 "tool_input":"{\"command\":\"rm -rf build\"}","status_at":"2026-10-24T14:01:02.25Z",
 "session_id":"5f0c…","transcript":"/home/user/.claude/projects/-home-user-gomuxai/5f0c….jsonl",
 "dialect":"claude","transcript_size":48211,"transcript_mtime":"2026-10-24T14:01:02.3Z"}
```

`kind` is `tmux` for every agent so far (room for CLI sessions that agent
didn't start). Absent fields are empty: `parent` for a person's agent,
`ended_at` while live, `store` and `attached` for another host's agents.
A departed agent (`live: false`, `ended_at` set) keeps its last report.

| `type` | Fields | When |
|---|---|---|
| `snapshot` | `host`, `stores` (`dir`, `this`, `cursor`, `events`, `error`), `hosts` (`host`, `agents`, `error`), `agents`, `unrecorded` (`session`, `store`) | first |
| `spawned` | `id`, `store`, `at`, `agent` | spawn recorded an agent |
| `changed` | `id` (for an agent event), `store`, `at`, `reason` (`status`, `session`, `transcript`), `agent` | an agent changed; the whole agent, as now |
| `removed` | `id`, `store`, `at`, `agent` (departed) | cleanup deleted the record |
| `store` | `store`, `agents` | a store found while following, or agents came or went in a store without events: replace its agents |
| `host` | `host`, `agents` | another host's agents changed: replace them |
| `unrecorded` | `sessions` | the unmatched sessions changed: replace the list |

```json
{"type":"spawned","id":3,"store":"/home/user/gomuxai/bin/.gomuxai","at":"2026-10-24T14:00:00.5Z","agent":{"name":"lead-t1","live":true,"attached":true,…}}
{"type":"changed","id":4,"store":"/home/user/gomuxai/bin/.gomuxai","at":"2026-10-24T14:00:03.1Z","reason":"status","agent":{"name":"lead-t1","status":"processing","event":"PreToolUse","detail":"Bash: go test ./...",…}}
{"type":"changed","store":"/home/user/gomuxai/bin/.gomuxai","at":"2026-10-24T14:00:04Z","reason":"transcript","agent":{"name":"lead-t1","transcript_size":51003,…}}
{"type":"removed","id":29,"store":"/home/user/gomuxai/bin/.gomuxai","at":"2026-10-24T14:02:00Z","agent":{"name":"lead-t1","live":false,"attached":false,"ended_at":"2026-10-24T14:02:00Z",…}}
{"type":"store","store":{"dir":"/home/user/.local/bin/.gomuxai","cursor":0,"events":false},"agents":[…]}
{"type":"host","host":{"host":"hub","agents":2},"agents":[…]}
{"type":"unrecorded","sessions":[{"session":"agent-stray"}]}
```

Every status report is a line, even one superseded before the follower
read it (a prompt answered within 250ms still shows as `waiting`).
Without `--json`, `--follow` prints a line of text per change, and skips
transcript growth.

## The bus

The bus adds cross-host reach and push status events. SQLite stays each
host's source of truth: an agent is a tmux session on one host, so that host
owns its state. Everything on one host works without a bus.

Each host runs one `agent bus run` process: an embedded `nats-server` (no
JetStream, no monitoring, no system account) with a client port for the
host's own CLI processes, plus an RPC server that executes ops through the
local `AgentService` (`service.NewRPCHandler`). One host, the hub, also
listens for leaf links from the others. CLI commands are only ever clients.

### Subjects

All under `gomux.` (`natsbus/contract.go`):

| Subject | Use |
|---|---|
| `gomux.<host>.rpc.<op>` | requests for `<host>`'s bus |
| `gomux.<host>.agent.<name>.status` | status events (core NATS, not persisted) |
| `gomux.<host>.local.>` | local clients only, never carried by links (`agent bus status` queries `gomux.<host>.local.bus`) |
| `_INBOX.<host>.>` | replies to `<host>`'s requests; each host has its own prefix |

Host and agent names share one pattern, `^[A-Za-z0-9][A-Za-z0-9_-]*$`, so
they can't contain `.`, `*`, `>`, or spaces. The host name is `$AGENT_HOST`
or the short hostname with unsafe characters replaced.

### Identity and pinned TLS

`agent bus init` writes the host's identity into `.gomuxai/bus/`
(`natsbus/identity.go`):

| File | What | Mode |
|---|---|---|
| `host` | the host name the identity is bound to | 0644 |
| `host.nk` | nkey seed the host signs leaf links with | 0600 |
| `local.nk` | nkey seed local CLIs and hooks authenticate with | 0600 |
| `tls.key`, `tls.crt` | self-signed certificate | 0600, 0644 |
| `peers/<host>.json` | one trusted peer bundle each | |
| `<host>.pub.json` | this host's public bundle, to copy to peers | 0644 |

A public bundle is `{host, nkey, cert_sha256, url?}`: the public nkey, the
SHA-256 of the certificate's public key, and, for the hub, its `tls://` leaf
URL. Trust works like SSH `known_hosts`: no CA, nothing secret leaves a
host, and `agent bus trust` installs a peer's bundle (re-trusting replaces
it). `init` refuses to replace an existing identity, `LoadIdentity` refuses
secret files readable by group or others, and an identity loaded under a
different host name is refused (it would sign as a host the peers don't
know).

Every connection is TLS 1.3, pinned by key:

- **Leaf links.** The hub requires a client certificate and checks it
  against the pinned set of trusted peers (`TLSPinnedCerts`); leaves check
  the hub's against its bundle (`pinVerifier`). TLS comes first
  (`TLSHandshakeFirst`), so unauthenticated dialers get no plaintext INFO.
  Peers then authenticate with their nkey (challenge-response).
- **Local clients.** The client port listens on `127.0.0.1` (default 7420)
  and serves the host's own certificate; `ConnectLocal` pins it and
  authenticates with `local.nk`. A stale `bus.url` or another user holding
  the port can't impersonate the bus. With no `bus.url`, `ConnectLocal`
  returns `ErrNoBus` at once, since hooks call it on every event.

### Permissions and topology

A host started with `--listen` is the hub; the others dial every trusted
peer whose bundle has a URL, and `serverOptions` refuses to start with more
than one. On the hub, each peer's leaf connection gets (`peerPermissions`):

- **publish**: requests `gomux.*.rpc.*` (minus the hub's own ops not in its
  `--remote-ops`), replies `_INBOX.>`, and status events for its own host
  only (every other host's are denied).
- **subscribe** (what the hub forwards to it): requests for itself, replies
  under its own `_INBOX.<peer>.`, and all status events.

Each leaf also refuses, as `DenyImports`, requests for its own ops not in
its `--remote-ops` and anything under its own `local.>`. The bus serves
local clients every op; peers get `--remote-ops` (default `list,status,
capture,send,respond`; `keys` is local-only).

### RPC ops

`natsbus/rpc.go` serves each op on its subject, one goroutine per request,
at most 32 in flight (more get a "busy" reply), with handler panics turned
into error replies. Every reply is an `RPCResponse{error, data}`. The client
(`rpc_client.go`) times out after 10s and strips terminal control sequences
from any text another host sent (`util.SanitizeTerminal`); `capture --raw`
keeps only SGR sequences (`util.SanitizeStyled`), so a hostile host's
screen can't set titles, write the clipboard, move the cursor, or switch
modes in your terminal.

| Op | Request → reply | CLI | Remote by default |
|---|---|---|---|
| `list` | `ListRequest` → `ListResponse` (agents with status and parent) | `list --host H` | yes |
| `status` | `StatusRequest` → `StatusResponse` (mirrors `StatusInfo`) | `status`, `wait` | yes |
| `capture` | `CaptureRequest` → `CaptureResponse` | `capture` | yes |
| `send` | `SendRequest` (`now`) → `SendResponse` (status, held, typed); none from older hosts | `send` | yes |
| `respond` | `RespondRequest` (`approve`, `approve_always`, `deny`, `force`, `expect_tool`) → none | `approve`, `deny` | yes |
| `keys` | `KeysRequest` (text, then a key) → none | `keys name@host` | no |

`natsbus/ops_test.go` keeps `AllOps` and the server's op table in step. The
remote side runs the same service code as a local call (`Send` holds a
message for a busy Claude agent and refuses to type into dialogs;
`RespondPermission` still checks the screen), and the CLI
adds the `[from ...]` prefix before the request leaves. `cli/remote.go`
routes `name@host`: a bare name or `name@<this host>` stays local and needs
no bus.

### Status events and wait

`Report` publishes a `StatusEvent` (host, agent, status, event, detail,
tool, time) for every report it records, through the service's optional
`EventPublisher`. Publishing is best-effort: the stored report is the source
of truth, so a missing or failing bus never fails a report. The CLI's
publisher (`cli/events.go`) connects on the first report only, so commands
that never report never touch the bus. A publish only fills the client's
buffer and a hook process lives for milliseconds, so `Execute` flushes
before exit, bounded at 200ms. `DecodeStatusEvent` rejects an event whose
body names another host or agent than its subject.

`natsbus.Wait`, the core of `agent wait`:

1. Subscribe to the agent's status subject and flush, so the server has the
   subscription before step 2.
2. Check the current status (`StatusInfo` locally; the `status` RPC for a
   remote agent). A transition right after this check still arrives as an
   event.
3. Loop on events, a periodic re-check (5s with a bus, 2s polling without),
   and a 1.5s re-check after an event (native status can lag a hook).

Events are hints: the hub routes them and could forge one, so an event that
would end the wait only triggers a real check. `--next` ignores the `--for`
statuses until a check has seen the agent in another one, or seen a report
newer than the first check's that is already back in a `--for` status (a
whole turn between checks), or seen `SentAt < TurnAt < ReportedAt` with the
report in a `--for` status (a whole turn after the last send, before the
wait). The send marker only counts a turn that started after the send, so a
turn already running when the message was typed (Claude queues it) doesn't
end the wait when it finishes. `WaitOptions.After` replaces the send marker
with "a `--for` report newer than this", which `--follow` uses to report each
arrival once. Unless `exited` is in `--for`, the wait fails as soon as the
agent is `exited`. A timeout exits 2.

`agent wait` with several names (`internal/cli/wait.go`) runs one
`natsbus.WaitObservation` per agent, in goroutines that share the bus
connection, and prints each arrival as it comes. Local names are looked up
first, so a typo fails at once instead of waiting out the timeout. With no
names, `--any`/`--follow` list this host's agents (minus `AGENT_NAME`), and
`--follow` lists them again every 10s to pick up new ones; there, an agent
that has exited or been cleaned up is dropped silently. A single named agent
without `--follow` keeps the original output (`name: status`) and errors.

### Security model, in short

Anything that can `send` to an agent can make it run commands, so the bus is
treated like SSH:

- Links: TLS 1.3, certificates pinned by key; nkey authentication per host;
  no shared secrets, and only public bundles are copied.
- Least privilege: fixed ops only, per-host `--remote-ops`, no raw keys
  remotely by default, no general remote shell. Peers can't publish events
  as another host, and only receive replies under their own inbox.
- Local clients: `127.0.0.1` only, nkey from a 0600 file, and the bus's
  certificate pinned by the client.
- The hub is trusted as the router: it sees everything that crosses it and
  could forge events, which is why consumers confirm events with a status
  check. Run it on the machine you trust most.
- Text from other hosts is sanitized before printing.
- **Known gap: replies are unsigned.** A peer may publish to any `_INBOX.`
  subject (narrower response permissions stop genuine replies from crossing
  the link), so a peer that learns another host's reply subject could
  answer in its place. Reply subjects are random but not secret.
  `TestSecurity_PeerCantForgeRepliesToOthers` documents it and runs with
  `GOMUXAI_KNOWN_BUGS=1`.

`tests/natsbus/security_test.go` and `daemon_test.go` test each of these
against a hub with real leaves, including a hostile one.

### macOS Local Network

macOS (15+) only lets a process reach the local network if its
*responsible* code holds the Local Network permission (Apple TN3179). A
tmux server detaches from the terminal that started it, so for processes in
tmux responsibility resolves to nothing that can hold the grant: dialing the
hub over the LAN fails with `EHOSTUNREACH` ("no route to host" in
`bus.log`) and no alert. Loopback, the internet, and accepting connections
are unaffected. The fix has two halves, and both are needed:

1. **The build.** `make macos` (on a Mac) links with cgo and the system
   linker to embed `cmd/agent/Info.plist` (bundle identifier and
   `NSLocalNetworkUsageDescription`) as `__TEXT,__info_plist`, then signs
   with an Apple-issued identity (`CODESIGN_IDENTITY`, default the first in
   the keychain; ad hoc signatures don't keep the grant). macOS keys the
   grant on the bundle identifier, so rebuilds keep it.
2. **The spawn.** `agent bus run` re-spawns itself with
   `responsibility_spawnattrs_setdisclaim` (`internal/macos`,
   `cli/bus_macos.go`), so the bus is its own responsible process and macOS
   asks about this binary. It does so only when the executable embeds an
   Info.plist; a plain build runs in-process, which still works from a
   terminal tab. The parent waits for the child, exits with its status, and
   forwards SIGTERM/SIGHUP; Ctrl-C reaches the child directly through the
   shared process group. The child exits if the parent dies.

On macOS, `warnLocalNetwork` prints a note at bus start when the bus runs
in tmux without having been disclaimed and will dial a hub. Cross-compiled
darwin builds (`make build-darwin`, `CGO_ENABLED=0`) have neither half.

### The gomux-bus session

`agent bus start` runs `env AGENT_HOST=<host> <binary> bus run ...` in the
tmux session `gomux-bus`, pinning the host so the bus runs as the identity
that started it, and waits up to 5s for it to answer. The session name is
the same for every data dir on the tmux server, while each bus records its
URL in its own data dir. So `start` and `stop` first check that this data
dir's bus is the one answering (`checkOwnBusSession`), and otherwise leave
the session alone: it likely belongs to another binary (e.g. a dev build in
`bin/`). `stop` interrupts the bus so it removes `bus.url`, then kills the
session.

## Storage

One SQLite database, `.gomuxai/gomuxai.db`, through `modernc.org/sqlite`
(pure Go, no cgo).

### Schema

| Table | Columns | Notes |
|---|---|---|
| `agents` | `name` PK, `session_name`, `backend`, `task_file`, `log_file`, `created_at`, `spawned_by`, `cwd` | one row per live agent |
| `runs` | `id`, `name`, `parent`, `backend`, `session`, `cwd`, `created_at`, `ended_at`, `status`, `transcript` | one per spawn; cleanup sets `ended_at` and the last status and transcript; indexed on `(name, id)` |
| `agent_events` | `id`, `at`, `kind`, `agent`, `run`, `status`, `event`, `detail`, `tool`, `tool_input`, `session_id`, `transcript` | `spawned`/`status`/`removed`, for `export --follow`; the newest 20000 are kept |
| `messages` | `id`, `sender_id`, `receiver_id`, `content`, `status`, `created_at` | held and queued messages: `pending`/`delivered`/`failed`; indexed on `(receiver_id, status)`; claimed with `UPDATE ... WHERE status = 'pending'` |
| `tasks` | `name` PK, `content`, `source_file`, `created_at` | independent of agents |
| `state` | `agent_name`, `key`, `value`, `updated_at`; PK `(agent_name, key)` | FK to `agents` with `ON DELETE CASCADE`: state needs an agent record |
| `command_log` | `id`, `started_at`, `duration_ms`, `host`, `agent`, `parent`, `cwd`, `args` (JSON), `exit_code`, `error`, `build` | `agent history`; indexed on `(agent, id)` |
| `friction` | `id`, `uid`, `host`, `filed_by`, `parent`, `cwd`, `build`, timestamps, `kind`, `summary`, `detail`, `command` (JSON), `status`, accept and resolve fields, `duplicate_of` | the friction box |
| `friction_votes` | `id`, `friction_id` (FK, cascade), `uid`, `host`, `voter`, `parent`, `build`, `created_at`, `note`, `command` | `+1`s |

There is no logs table: logs are files (see
[Results and transcripts](#results-and-transcripts)).

### Migrations

`migrate` runs `CREATE TABLE IF NOT EXISTS` for every table on each open,
then `addColumn` for columns that older databases lack (so far
`agents.spawned_by` and `agents.cwd`). `addColumn` checks `pragma_table_info` first and
tolerates another process adding the column between the check and the
`ALTER TABLE`. A new column goes in both the `CREATE TABLE` (new databases)
and an `addColumn` call (old ones).

### Concurrency

Many short-lived processes write at once: every hook of every agent is its
own `agent report --hook`. The DSN sets `busy_timeout(5000)`,
`foreign_keys(1)`, and `synchronous(NORMAL)` on every pooled connection (a
one-off `PRAGMA` would reach only one), and `journal_mode = WAL` is set once,
since it's stored in the file. Multi-key writes that must be seen together
(`SetStates`) use a transaction. A transaction that will write must not
start with a plain read: once another process commits, SQLite can't
upgrade its read to a write (`SQLITE_BUSY`, which `busy_timeout` doesn't
wait out). So `DeleteAgent` starts with the run's `UPDATE ... RETURNING`.

Readers that only read (`export`) open the store with `OpenReadOnly`
(`mode=ro`, no migrations) and use short transactions, so they never hold
up writers in WAL mode.

### Command log

`Execute` records every run in `command_log` when it ends: args (each capped
at 300 bytes), duration, exit code, error, `AGENT_NAME`, cwd, and build id.
Help is recorded too (`recordCommand` opens the store itself when setup
didn't); `report --hook` (every tool call of every agent) and `bus run` (a
daemon) are not. `export --follow` is recorded once, when it ends (Ctrl-C
or SIGTERM; a SIGPIPE or SIGKILL leaves no row). Recording is best effort, bounded at 500ms. The log keeps
the newest 20000 rows: every 100th insert deletes older ones, so most
inserts don't pay for pruning.

### The friction box

`agent friction` is a request box for the tool itself. Entries move
`open` → `accepted` → `resolved`, `wontfix`, or `duplicate`; a duplicate's
votes (and its own filing) count toward the original. `add` and `+1` attach
the caller's last failed command from the command log within 15 minutes
(copied, since the log is pruned). `add` refuses a summary similar to an open
or accepted entry's (word overlap, `cli/friction_similar.go`), lists the
matches, and exits 3; `--new` files anyway. The box is per host for now;
each entry and vote has a `uid` so boxes can be merged later. Resolved
entries, via `friction export`, are the tool's changelog.

## CLI conventions

Shared by every command (`internal/cli/output.go`, `exit.go`, `root.go`):

- **stdout is the result, stderr the hint.** `hint()` prints at most one
  suggested next step per command, only after a real check showed it's
  needed; `-q` drops it. Commands whose stdout is a data stream (`events`,
  `watch`) say what they're doing once with `notice()`.
- **Errors say what failed, what was found, and what to try.** Unknown
  commands, subcommands, and flags get "did you mean" suggestions
  (`util.Similar`: one edit per three letters, so a typo finds its command
  and an unrelated word finds none) or point at `agent help`. The root's
  `Args` (`rootArgs`) and a replacement `help` command apply that rule in
  place of cobra's own suggestions. Argument errors add the usage line and the command's
  first example (`explainArgErrors`). A command that only groups
  subcommands uses `runGroup`.
- **Exit codes.** 0 done, 1 failed, 2 `wait` timed out, 3 `friction add`
  found a similar entry. Commands return an `exitError` for codes other than
  1 instead of calling `os.Exit`, so `Execute` still records the run and
  flushes the bus publisher; `main` exits with `ExitCode(err)`.
- **`--json`** (on `list`, `status`, `state get`/`list`, `task get`/`list`,
  `session list`, `bus status`, `logs`, `history`, `friction list`/`show`,
  `events`, `export`) goes through `printJSON`: indented, with `&`, `<`, `>`
  not HTML-escaped. Streams (`events --json`, `export --json --follow`)
  print one compact object per line instead.
- **Setup.** `PersistentPreRunE` (`setup`) loads config, the backend
  registry (and `--backend-config`), the database, tmux, and the service for
  every command except help.
- **Help is grouped by task** (`commandGroups` in `root.go`), and tested:
  `TestHelpExamplesParse` (`cli/commands_test.go`) parses every example line
  in every command's help and fails on an unknown command or flag or a wrong
  argument count; `TestHelpLayout` requires a one-line `Short` and a group
  for every command; every command needs an `Example`. When you change a
  command, change its help and examples with it.
- **`agent` alone** prints an orientation (`orient.go`): who you are, the
  host, your helpers (or all agents), the bus, queued messages.

## Testing

```bash
make test          # go test -v ./...
go test ./...      # quieter; under a minute, most of it tests/natsbus
make lint          # golangci-lint
```

- **`internal/*/..._test.go`**: in-package tests: CLI output, hints, exit
  codes, help examples, cleanup rules, logs, history, friction; storage
  pruning and friction queries; runs, agent events, and reading old stores
  read-only; the export snapshot and stream (temp stores, fake tmux and
  bus); transcript parsing against fixtures in
  `internal/transcript/testdata`; watcher internals; build ids; the RPC op
  tables.
- **`tests/<package>/`**: black-box tests per package. `backend`: hook
  payload mapping for Claude and agy, dialog detection, launch commands.
  `service`: spawn, send, status resolution (with a fake Claude backend whose
  native status the test controls), events, RPC. `natsbus`: real buses in
  process, linked hub and leaves with real identities, RPC across links,
  events, `Wait`, and the security tests. `storage`, `config`, `tmux`,
  `util`, `watcher`: as named.
- **tmux** is needed by `tests/tmux`, `tests/service`, and part of
  `internal/cli`; they skip without it. They use real sessions with a unique
  session prefix and a temp database. No test needs claude or agy installed.
- **Opt-ins.** `GOMUXAI_KNOWN_BUGS=1` runs tests for known open bugs (now
  the unsigned-replies one). `GOMUX_REAL_TRANSCRIPTS=<path>[:<path>...]`
  parses real transcripts on your machine as a smoke test.

## Known gaps

- Replies over the bus are unsigned (see the security model).
- One hub per bus network; the hub is trusted and can forge events.
- `state` and logs are per host and not served over the bus:
  `state get name@host` and `logs name@host` are refused. `queue` is local
  too: it refuses other hosts and suggests `send`.
- A coordinator not spawned by `agent` has no agent record, so it can't
  `state set` (the state table's foreign key).
- A reused parent name inherits the old parent's helpers (`spawned_by` is a
  name).
- `wait --next` catches a turn that ended before the wait only for backends
  that report turn starts (`claude`, `agy`). The marker isn't consumed: a
  second `wait --next` with no new send returns the same turn.
- `wait` polls between events, so a prompt answered between two checks is
  never reported, and with several quick prompts `--follow` may print only
  the last.
- `_result` is Claude-only; agy agents' replies are only in their
  transcripts.
- Long messages typed into a Claude TUI arrive as pasted content (held
  messages arrive whole, as hook output).
- A held message extends the turn it's delivered at the end of, so
  `_result` and `wait --next` cover its reply too.
- The input line is read off the screen: a Claude or agy redesign of it
  would leave held messages waiting ("it isn't on screen") until the reader
  is updated. A person who starts typing between the check and the typing
  (milliseconds) still gets the message mixed in.
- agy's interrupted turns and question dialogs are read off the screen too:
  a redesign of its footer would leave an interrupted agent `processing`
  (its held messages held, until `send --now` or its next turn), and a
  question `processing` (sends to it are held anyway). A message held in
  the milliseconds between agy's last `PostInvocation` and its `Stop` is
  typed by the deliverer after its 10s grace.
- agy's `Stop` hook arguments have a `FinalModelOutput` field in the
  binary's protobuf (not checked in a live payload), which `_result` could
  use; not wired yet.
- A deliverer polls native status every 2s for as long as messages are
  held, e.g. while an agent sits at a prompt nobody answers.
- Custom backends aren't remembered after spawn: pass `--backend-config`
  again.
- The PreToolUse approval finds a bare `agent` on the hook's `PATH`, which
  can differ from the agent shell's if its profile changes `PATH`, aliases
  `agent`, or defines a function by that name; the `Bash(agent ...:*)`
  rules already trust the name the same way. Commands through a variable
  (`$A send`) still prompt.
- `agent capture` reads managed agents only, not raw sessions.
- With a read-only tmux client attached anywhere, keys other than the
  pasteable ones (see [tmux](#why-tmux)) fail until it detaches.
- `approve`/`deny` can still hit a prompt answered in the milliseconds
  between the screen check and the keypress.
- Two data dirs on one machine can't both `bus start`: they share the
  `gomux-bus` session name and the default client port.
- `export` shows reported, not resolved, status (see [Export](#export));
  a store from a binary built before it has no events, and other hosts
  come only from `list`. An old binary writing into a store that has
  `agent_events` (a downgrade) records no events, and followers miss its
  changes.
- `runs` are never pruned (one row per spawn).
- The open friction entries: `agent friction list`.

## History

- [`docs/history/NATS_INTEGRATION_PLAN.md`](history/NATS_INTEGRATION_PLAN.md):
  the bus's design and revision record. The March JetStream design it
  describes (server, coordinator, streams, subjects) was removed; this file
  describes what was built.
- [`exploration/claude-hooks-findings.md`](../exploration/claude-hooks-findings.md),
  [`exploration/agy-hooks-findings.md`](../exploration/agy-hooks-findings.md):
  the probes behind the hook mapping, with payloads and firing order.
- [`exploration/tmux-tui-connector-ideation.md`](../exploration/tmux-tui-connector-ideation.md):
  mechanisms for sharing a TUI between agents and people.

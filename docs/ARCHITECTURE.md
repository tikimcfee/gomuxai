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
lifecycle hooks: Claude Code runs `agent hook` for every session on the
machine (registered once by `agent hooks install`), and each agy agent's
hooks run `agent report --hook`; nothing has to scrape their screens. Claude
Code's own session registry says which Claude sessions are running and
where. An optional bus (embedded NATS, one per host) lets `name@host`
commands, status events, and `agent wait` reach other machines.

Most `agent` commands are short-lived: open the database, do one thing,
exit. The long-running processes are the agents' CLIs, `agent watch`
(delivers queued messages), and `agent bus run` (the bus daemon).

The data dir is `.gomuxai/` next to the binary (`config.DefaultConfig`), so
a dev build in `bin/` has its own (`bin/.gomuxai/`), separate from an
installed one:

```
.gomuxai/
├── gomuxai.db               # agents, runs, agent_events, messages, tasks, state, session_reports, command_log, friction, friction_links
├── logs/
│   ├── <name>.log           # tmux pipe-pane output (survives cleanup)
│   ├── <name>.transcripts   # "<backend>\t<path>" per CLI transcript (survives cleanup)
│   └── hook-errors.log      # what `agent hook` failed at, one line each
├── backups/                 # Claude Code's settings.json, copied before `agent hooks` edits it
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
| `internal/backend/types` | `Status`, `EndReason`, `HookReport`, `PermissionResponse`, shared by backends and service; `Statuses` and `EndReasons` hold each word's meaning, which `agent statuses` prints. |
| `internal/claudereg` | Reads Claude Code's session registry (`~/.claude/sessions/<pid>.json`), proves each entry against the running processes, and finds the session a tmux pane runs. |
| `internal/allowcmd` | Decides whether a shell command line is one plain run of a given binary with a safe subcommand (parsed with `mvdan.cc/sh`), for the PreToolUse approval. |
| `internal/tmux` | `TmuxOps` over the `tmux` command: sessions, send keys, capture, pipe-pane. Errors carry tmux's own message. |
| `internal/natsbus` | The bus: contract (subjects, ops, wire types), identity and trust, the daemon's server config, RPC server and client, status events and `Wait`. `natstest` starts throwaway NATS servers for tests. |
| `internal/transcript` | Readers for Claude Code, agy and Kimi Code session transcripts, into one event stream (`docs/EVENTS.md`). |
| `internal/watcher` | The `agent watch` loop: delivers queued messages when their agent is idle. |
| `internal/export` | The tree `agent export` prints: a snapshot across stores and hosts, kept current from agent events, sessions, and transcripts. |
| `internal/discovery` | `agent sessions`: where each harness keeps its transcripts (the one dir table), each session's metadata, the join to workgroup agents, and every session's status. |
| `internal/config` | `Config`, its defaults (data dir next to the binary, host name, startup delay), and paths derived from it. |
| `internal/macos` | Local Network support for the bus on macOS: Info.plist detection and disclaimed spawn. |
| `internal/buildinfo` | The build id (`c7253f9-dirty`, a version, or `devel`) from the Go toolchain's embedded VCS info. |
| `internal/util` | Shell quoting and word splitting, ANSI stripping, terminal sanitizing, terminal-log cleanup (`TerminalLines`), "did you mean" suggestions, the process tree (`ReadProcTree`, `ProcAncestors`, `ProcArgs`), atomic file writes. |

## Why tmux

Each agent runs in a tmux session named `agent-<name>` (`Config.SessionPrefix`).
tmux gives:

1. **Process isolation.** An agent can crash without taking others down.
2. **Persistent terminals.** Sessions survive disconnects, and a person can
   attach (`agent attach <name>`) to watch or steer any agent.
3. **Scriptable I/O.** `paste-buffer` types messages, `send-keys` keys;
   `capture-pane` reads the screen; `pipe-pane` logs everything.
4. **Escape hatches.** `agent session`, `keys`, `key`, and `clear` expose
   raw tmux for when the higher-level commands fail.

`internal/tmux` runs the `tmux` command itself, and a failure's error
(`tmux.Error`) carries what tmux printed on stderr ("tmux send-keys: can't
find pane: %9"). Session targets are exact (`=name`): tmux otherwise falls
back to a prefix match, so `agent-a` could reach `agent-ab`. Every
session-level operation targets the session's first pane by id.

`clear` types nothing: `send-keys -R` resets the pane's terminal (blank
screen, cursor home), and `clear-history`, in the same tmux command, then
drops the scrollback, which includes the old screen, since `scroll-on-clear`
(on by default) moves it there. Typing `clear` into the shell raced
`clear-history` and could leave the old screen behind; it also reached an
agent's CLI as a message.

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

**Typing messages.** A message (a send, held messages typed in, spawn's
first message) is typed with `TmuxOps.PasteText`: `paste-buffer -d -r -S
-p`, where `-p` wraps it in bracketed paste codes (`ESC [200~` ...
`ESC [201~`) if the pane's program asked for bracketed paste, as Claude
does; one that didn't gets the bytes `send-keys -l` would type. A pty's
input queue holds 1024 bytes on macOS (4096 on Linux), so the program reads
a longer write in pieces: 1022 bytes, then the rest. Typed without
brackets, Claude took the pieces for separate pastes and kept only the
last, so a 1066-byte message arrived as its last 44 bytes (friction #36,
#37). The brackets make the pieces one paste, as a person's paste is. A
message can't end its paste early: `ESC [201~` in it is dropped. Shell
command lines (spawn's launch) and keys still go through `send-keys`.

The cost is that input is typed, not delivered: a message is "sent" once its
keys are in the terminal, whether or not the CLI read it, and anything typed
at a dialog answers the dialog. `Send` and the watcher guard against that
(see [Messages](#messages)). The alternatives considered for sharing a TUI
are in `exploration/tmux-tui-connector-ideation.md`.

## Spawning an agent

`AgentService.Spawn` (`internal/service/agent_service.go`):

1. Validates the name (`^[A-Za-z0-9][A-Za-z0-9_-]*$`): it becomes a tmux
   session name, a path component, and a bus subject token.
2. Refuses if the agent record or the tmux session already exists (and
   says `agent resume <name>` when that agent has ended).
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
   (`MAX_CANON`), which a launch command with its flags and pre-allowed
   command rules can pass.
6. Waits `StartupDelay` (2s), confirms the workspace trust prompt if
   `--trust-workspace`, and types the first message unless it went on the
   command line. The first message is the `--task` content, or the backend's
   "read this task file" line for a task file. Where the CLI's input box
   can be read (Claude), spawn first waits for it, or a dialog in its
   place, as `send` does (`awaitInputBox`). If the trust prompt is up
   instead, the first message is held (a pending message, as for a busy
   agent) rather than typed: Claude's dialog drops a paste, and an Enter
   that reached it would pick its highlighted "No, exit". Approving the
   prompt starts the deliverer, which types it once the CLI is at its
   prompt.

For a built-in backend, any failure after the session exists kills it and
deletes the record and the agent dir.

Built-in backends implement `backend.Provider` plus whichever optional
interfaces (`internal/backend/provider.go`) they support; `launchCommand`
applies each one it finds:

| Interface | claude | agy | aider |
|---|---|---|---|
| `HookProvider`: maps hook payloads to status | yes | yes | — |
| `GlobalHooker`: hooks from the CLI's user settings, for every session | `--session-id <id>`; spawn fails unless `agent hooks install` has registered this binary's hook | — | — |
| `SpawnHooker`: hooks wired into each spawn's launch command | — | `--add-dir <agent dir>` with `.agents/hooks.json`, plus the exit wrapper | — |
| `InstructionsProvider`: the briefing | `--append-system-prompt-file instructions.md` | `AGENTS.md` in the `--add-dir` directory | — |
| `CommandAllower`: pre-allowed commands | `--allowedTools 'Bash(agent status:*)' ...` | — | — |
| `PromptFlagProvider`: first message on the command line | — | `-i '<task>'` | — |
| `TrustPrompter`: reads and answers the trust prompt | moves to "Yes, I trust this folder" first (the default is "No, exit") | Enter ("Yes" is the default) | — |
| `NativeStatusProvider` | Claude Code's session registry | — | — |
| `PaneSessionProvider`: the session a pane runs | the registry, placed by the process tree | — | — |
| `PermissionResponder` | yes | yes | — |
| `TranscriptProvider` | `transcript.Claude` | `transcript.Agy` | — |
| `HookDeliverer`: held messages from a hook's output | `Stop` → `additionalContext` | `PostInvocation` → `injectSteps` + `force_continue` | — |
| `HookAllower`: approves a safe `agent` command from a hook | `PreToolUse` → `permissionDecision: allow` | — | — |
| `InputReader`: the input line, so typing never lands in a draft | after `❯` | after `>` | — |
| `SubmitChecker`: a typed message's Enter can arrive as a newline, so typing checks it submitted | yes | — | — |
| `TurnEndReader`: a turn that ended with no hook | — (native status) | the footer under the input box | — |
| `QuestionPrompter`: a question dialog with no hook | — (`PermissionRequest`) | `ask_question`'s `Question 1/N:` | — |

Notes on each:

- **Hooks** are used only when `Config.AgentBinary` is set (it defaults to
  the running executable). Nothing is written into the agent's project.
  - **Claude** (`GlobalHooker`): the hooks are Claude Code's user settings'
    entry for every session, which `agent hooks install` writes (see [The
    global hook](#the-global-hook-agent-hook)). Spawn checks that every
    event in `ClaudeHookEvents` runs this binary's `hook`, and fails with
    what is missing and `agent hooks install` otherwise: an agent without
    them couldn't report, take held messages, or have its `agent` commands
    approved. It pins a new uuid as `--session-id` (a `--resume` keeps the
    resumed session's id) and stores it as `_session_id`. There is no
    spawn id: the hook finds its agent from the process tree, and a CLI
    that cleanup killed has no pane above it, so its last hooks reach no
    agent (a respawn of the same name included).
  - **agy** (`SpawnHooker`): each hook runs
    `<binary> report --hook --as <name> --spawn-id <uuid> --event <Event>`.
    The uuid is made per spawn and stored as both `_spawn_id` and
    `_session_id`. agy registers `PreInvocation`, `PreToolUse`,
    `PostToolUse`, `PostInvocation` (delivery only), and `Stop`; `--event`
    because agy's payload omits the event name, and an absolute binary path
    because agy runs hooks with the bundle dir as cwd.

  Each hook has a 10s timeout.
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
  `status`, `wait`, `events`, `capture`, `logs`, `sessions`, `transcript`,
  `state get`, `history`, `friction add`, `friction +1`, `friction list`, `friction show`,
  `friction link`, `friction links`. Each becomes two rules, `agent <cmd>` and
  `<full binary path> <cmd>`, for that Claude session only. Left out on
  purpose: `approve`, `deny`, `keys`, and `send --force` (one agent answering
  another's prompts), `spawn` and `cleanup` (a person approves each), and
  `friction accept`/`resolve` (for the people working on the tool) and
  `friction unlink` (it erases a link on both hosts). Rules
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
- **Trust prompts** are answered at spawn only with `--trust-workspace`,
  since trusting a repo also enables its own hooks and settings. Spawn polls
  the screen up to 8 times, 500ms apart, and goes on if no prompt shows.
  Without it, the agent is `waiting` at the prompt (see [Resolution](#resolution-native-report-scrape)),
  spawn says so, and `agent approve` answers it. Both CLIs draw the same
  dialog (`namedagents/trust.go`): an `Accessing workspace:` header over
  the folder (wrapped at the screen's width), a question (Claude's `Quick
  safety check`, agy's `Do you trust the contents of this project?`), and
  the options `No, exit` and `Yes, I trust this folder` among the last six
  non-blank lines, the highlighted one marked (`❯` Claude, `>` agy). All of
  that must be there, options at the bottom, so text quoting the dialog
  higher up (a transcript, a file) isn't taken for it. The keys move from
  the highlighted option to the wanted one (Claude lists No first, agy Yes
  first); with neither marked, there are no keys rather than a guess.
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

An agent spawned over the bus (`spawn name@host`) gets `spawned_by`
`<caller>@<caller host>` on that host, or "" when a person spawned it.
Such a parent names no local agent, so its helpers nest under nothing in
`list` there, and only a person on that host can clean them up: `cleanup`
doesn't cross hosts, and no agent there is their ancestor. The hub doesn't
verify the name; it is the caller's claim, like a `[from name@host]` tag.
It must carry a host: the bus refuses a bare `spawned_by`, which would make
the new agent a helper of the serving host's agent of that name.

`cleanup --ended` picks the names itself, from every agent's
`StatusInfo` (the resolved status, so an agent whose tmux session is gone
counts though its last report said idle), all read before anything is
removed. The same rules then apply, with one difference: where named
cleanup refuses a helper whose own helpers would be left behind, `--ended`
skips it, since those helpers haven't ended and aren't its to remove. A
person gets every ended agent, with a note naming the helpers that stay.
Ended agents outside the caller's tree are counted, not listed. `--dry-run`
stops after the choice, printing each agent's end, what `Cleanup` would do
to its session (kill it, leave an adopted one running, or nothing if it's
gone) and `StatusInfo.Held`.

`AgentService.Cleanup` types the backend's exit command (`/exit`) into an
idle agent and waits 1.5s, so the CLI exits cleanly and its hooks record it.
Then it stops pipe-pane, kills the session, deletes the agent's messages
and agent dir, and deletes the record; state rows go with it through the
foreign key's `ON DELETE CASCADE`. The same transaction closes the agent's
run (with its last `_status`, `_end_reason` and `_transcript`) and records a `removed`
agent event, so the run stays readable (`export --all`). The terminal log
and the transcripts list stay.


### Resuming an ended agent

`agent resume <name>` (`AgentService.Resume`, `internal/service/resume.go`)
starts an ended claude or agy agent again on its last session, under its
own record. The CLI (`resumeAgent`, which the bus's `resume` op runs too)
finds that session as `agent transcript <name>` does, checks it can be
continued (`checkResumable`, as `spawn --resume`), and passes the
directory it ran in as `ResumeOptions.Cwd`, where the tmux session starts.
It doesn't `chdir`: the bus serves requests concurrently in one process.
The service then:

1. Refuses an adopted agent (`AdoptedResumeError`), and refuses unless
   the agent's resolved status is `ended`; then kills a leftover tmux
   session (a CLI that quit leaves its pane's shell).
2. Writes `_status` `ended` and its end reason, so its run closes with how
   it really ended (a reboot fires no hook).
3. Spawns with `Resume` set and the old record to replace: a new tmux
   session, then `storage.ReplaceAgent`, which in one transaction closes
   the old run (a `removed` event), rewrites the record in place, and opens
   a new run (a `spawned` event). The record is updated, not deleted:
   deleting it would cascade to the agent's state.
4. Deletes the reserved keys that described the ended run, keeping
   `_result` and `_transcript`; the agent's own keys stay. The launch then
   writes a new `_spawn_id` and `_session_id`.
5. Starts a deliverer if messages were held for it: they never went out
   while it was ended.

Its name, backend and parent stay, so its helpers stay its helpers. A
failed resume leaves the agent ended (its record and files are kept).

An adopted agent (`agent adopt`) runs in a tmux session the person made,
so resume refuses it: gomuxai doesn't type a launch into their shell, and
doesn't create an `agent-<name>` session to replace theirs. The error says
to start the CLI there (`claude -c` continues its last conversation), which
pane adoption then tracks; if their session is gone, to clean the agent up
and `spawn --continue` (or `--resume`) it in a session of gomuxai's own.
The CLI refuses before it looks for the session or enters its directory.
`Spawn` therefore never runs in an existing session: Adopt records the
agent itself (`storeAgent` with `existingSession`).

### Continue: picking the session

`spawn --continue` (`continueSession`, `discovery.Continue`) picks the
session the CLI's own `--continue` would, then goes through `spawn
--resume`'s path with its id, so the record knows the session and a running
one is refused (`checkResumable`). Passing `-c` to the CLI instead would
leave the agent not knowing which session it is on. Claude's `-c` is "the
most recent conversation in the current directory": the session whose
recorded cwd is the directory (Claude keeps a project's sessions in one
folder named for it) with the newest transcript. agy's is the most recent
conversation in its workspace, falling back (agy's changelog) to one in a
parent or child directory. Subagent sessions are never picked. The
directory is `--cwd`, or spawn's; the agent starts where the picked
session ran, as with `--resume`.

## Messages

`agent send` (`AgentService.Send`) types the message and Enter, or holds it
(below). A message
from an agent (`AGENT_NAME` set) is prefixed `[from <name>@<host>] `, and
one without `AGENT_NAME` `[from a person on <host>] `, or `[from a Claude
session on <host>] ` when the environment shows an AI CLI (`AI_AGENT`'s tool
name, the part before its first `_`, or `CLAUDECODE=1`; `unrecordedSender`).
Those name no agent to reply to, and their slash commands stay unmarked, or
they would no longer be ones (`markSender`). Before
typing, `checkCanSend` refuses an `ended` agent (the text would run in the
shell left behind), a `waiting` agent, and any agent with a permission or
trust dialog on screen (the Enter would answer it; at Claude's trust
prompt it would pick "No, exit"). `--force` skips the
checks, and only works for a person at a terminal: it is refused when
`AGENT_NAME` is set or stdin isn't a TTY. Right before typing,
`SendUnchecked` records `_sent_at` (spawn does too, for the first message),
for `wait --next`.

### Held messages

Typing into a busy Claude agent lands in its input box, mixed with
whatever a person is drafting there, and nothing says when it was read
(agy queues typed text for the turn's end, but typing into its
`ask_question` dialog answers it). So
for an agent that *holds messages*, `Send` without `--now` does this
(`service/held.go`). An agent holds messages when its backend is a
`HookDeliverer` and a hook is known to reach its CLI (`holdsMessages`):
spawn wired its hooks (`_spawn_id` is set: agy), or the global hook has
reported the agent's current session (`_hooked_session` equals
`_session_id`: Claude). A Claude CLI started without the hook installed,
say by hand in the agent's pane, never sets `_hooked_session`, so messages
to it are typed, not held for a `Stop` that would never hand them over.

| Status `Send` finds | What it does |
|---|---|
| `idle`, `error`, `unknown` | `typeHeld`: claims any messages still held, and types them and this one as one prompt, oldest first (after the dialog check) |
| `processing`, `waiting` | `hold`: stores a `pending` row in `messages`, records `_sent_at`, re-checks (below), and starts a deliverer |
| `ended` | refuses |

Slash commands are never held: a hook can only hand over text. Who
delivers a held message:

1. **The delivering hook.** The hook (`agent hook` for Claude, `report
   --hook` for agy) calls `HandleHook`, and for an event
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
   It exits when nothing is held, or the agent's session has ended, or the
   agent is gone. A
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
the `❯` prompt (a named session, from `--name` or `/rename`, has its name
set into the top rule: `──── name ─` counts as a rule), with dim text (SGR 2: the `Try "..."` placeholder and
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
held for an agent whose session ends stay `pending` (`status -v` says they won't
be delivered) until `cleanup` deletes them. `StatusInfo.Held` counts the
pending rows, shown by `status -v`, `--json`, the orientation, and the
`status` RPC.

**Submitting a typed message.** Typing a message and pressing Enter are two
writes to the pane (the paste, then `send-keys Enter`), and Claude
handles whatever has arrived by the time it reads as one input. While its
event loop is busy (a long render, a paste it is still collecting), the
text and the Enter arrive in the same read; Claude takes that for a paste,
the Enter becomes a newline in it, and the message sits in the input box
until someone presses Enter. The first Enter after that is often eaten too.
So `SendUnchecked`, for a backend that is a `SubmitChecker` (Claude), types
through `typeMessage` (`internal/service/submit.go`):

1. It reads the input box (`InputReader`), types the text, and waits for
   the box to show something new that stays unchanged for 150ms: the text,
   a `[Pasted text #N +M lines]` placeholder, or an image path turned into
   `[Image #N]`. Only then does it press Enter, which arrives on its own.
2. It waits for the box to empty. A box that shows the message only now
   (the text and the Enter arrived together) restarts the wait.
3. While the box still shows what was typed, it presses Enter again, up to
   3 Enters in all.
4. If the message is still there, it records the box's text as
   `_unsubmitted` and returns `UnsubmittedError`, which says not to send the
   message again and that `agent key <session> Enter` submits it.

It presses no further Enter once the box is off screen (a dialog may have
replaced it) or shows anything other than what was typed (a person typing).
A box it can't read before typing (not on screen, or the capture failed)
gets type-then-Enter without the checks, and says so: `SendResult.Unchecked`
(`unchecked` on the bus) names why, `send` prints `Typed into X without
checking it was submitted: <why>`, the deliverer logs it, and `_unchecked`
records it, so `status` prints an `unchecked:` line (`unchecked`,
`unchecked_at` in `--json`) until a turn starts after it.
`Config.SubmitWait` (`submit_wait`, default 3s) bounds each wait; 0 turns
the checks off. A responsive Claude takes about 0.3s per message.

`typeHeld` leaves messages that were typed but not submitted claimed (they
sit in the box) rather than holding them again. While the box still shows
`_unsubmitted`, `status` prints an `unsent:` line (`unsubmitted_at` in
`--json`), and the input-line check says `a message typed into it wasn't
submitted: agent key <session> Enter submits it` instead of `it has text in
it`, so `send` and `status -v` name the cause. `send --now` types after it
and submits both.

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
`ended` fails it, as do 5 `unknown` checks in a row or 3 failed sends
(backoff 1s, doubling). Each watcher holds a shared `flock` on
`.gomuxai/watch.lock`, which is how `queue` tells whether one is running.

## Status

### Statuses and end reasons

An agent and a session have the same statuses. The words and what each
means live in one place, `types.Statuses` and `types.EndReasons`
(`internal/backend/types/status.go`); `agent statuses` prints them, and
`agent status -v` prints the one that applies on a `means:` line.

| Status | Means |
|---|---|
| `idle` | at its prompt, its turn over: a message can be typed in (a background shell it started may still run) |
| `processing` | a turn is running (or a turn ended leaving background work, whose completion starts the next turn) |
| `waiting` | stopped on something only a person can answer: a permission prompt, a question, or the CLI's workspace trust prompt |
| `error` | its last turn failed (an API error, a rate limit); it is at its prompt |
| `ended` | no CLI runs the session any more; the end reason says how it ended |
| `unknown` | nothing reliable says yet (a CLI still starting, or a harness with no status of its own) |

`ended` always comes with an end reason (`_end_reason`; `end_reason` in
JSON). A line shows both: `ended (quit)`.

| End reason | Means |
|---|---|
| `quit` | the CLI ended the session and said so (Claude's `SessionEnd` hook; the detail has Claude's reason), or the shell it ran in saw it exit (agy; the detail has the exit code) |
| `gone` | its process isn't running and nothing recorded it ending: a crash, `kill -9`, OOM, or a session from before reports were kept |
| `replaced` | the agent went on in a newer session: a `/clear`, a resume, a respawn, or a CLI started anew in its pane |
| `finished` | a subagent that recorded finishing its task |
| `parent` | a subagent whose parent session ended |
| `closed` | its agent was cleaned up, which closed its pane |

`agent report ended` takes `--end-reason` (`quit` when not given), and
refuses one with any other status (`ParseEndReason`). A report writes
`_end_reason` every time, so a reason never outlives its end.

### Resolution: native, report, scrape

`AgentService.StatusInfo` (`internal/service/status.go`) resolves status
from three sources, most authoritative first:

1. **native**: the CLI's own view of the session (`NativeStatusProvider`).
   Claude only: [Claude Code's session
   registry](#claude-codes-session-registry), matched on `_session_id`. A
   session is listed when a running process (proven, or one whose start
   time can't be checked) holds it, and its status maps `busy` →
   `processing`, `idle` → `idle`, `shell` → `processing` (detail `turn
   over; a background shell it started is still running, and its end starts
   the next turn`: not done, as a `Stop` with background tasks isn't, and as
   `claude agents` reports it), `waiting` → `waiting`
   (detail `waiting for <waitingFor>`), and no status yet → `unknown`. The
   registry is files, read afresh on every check: nothing is cached.
2. **report**: the last status written by a hook or `agent report`.
3. **scrape**: the backend's idle and processing regexes over the last 50
   lines of the screen, for backends with neither. A custom backend needs
   its `--backend-config` loaded for this.

Before any of these, an agent whose tmux session doesn't exist is `ended
(gone)` whatever its last report said (an `ended` report stands, with its
reason): no session, no CLI.

Next, the **workspace trust prompt** (`trustPromptStatus`). A Claude or agy
CLI started in a folder it hasn't trusted asks first, and until it is
answered it runs no hooks (Claude's global ones included) and Claude's
registry doesn't list the session, so the sources above would say
`unknown`. While nothing has reported (or the last report is approve or
deny answering it, in case the keys didn't take), status reads the screen
with the backend's `TrustPrompter`: the prompt makes it `waiting`, source
`scrape`, tool `trust`, detail `trust: <folder>` (the agent's directory if
the folder scrolled off). So `wait --for waiting`, `wait --any` and
`--follow` report it like a permission prompt, `send` refuses to type into
it, and `approve`/`deny` answer it (below). Approving records
`processing`/`trusted` (detail `trusted <folder>; its CLI is starting`):
for 30s after it (`trustStartGrace`) a session Claude's registry doesn't
list yet is starting, not `ended (gone)`. A reboot or a killed tmux server ends every
agent at once and fires no hook, and a backend with no native session list
(agy) would otherwise show its last report, `idle`, forever.

The rules between native and report are one function,
`discovery.ReconcileNative`, which `agent sessions` applies too:

- Native wins when it disagrees with the report. An interrupted Claude
  permission prompt fires no hook, so the report would stay `waiting`
  forever. The report's event, detail, and tool are shown only while the
  report agrees with native.
- If the CLI answers but doesn't list the session, status first asks what
  the agent's pane runs ([pane adoption](#pane-adoption), below). If the
  pane runs no other session, and the last report says the agent is alive,
  the status is `ended (gone)` ("session not running"): the CLI died
  without its `SessionEnd` hook (crash, `kill -9`, OOM). Otherwise the
  watcher would type queued messages into the shell. When the pane couldn't
  be read, the detail says why.
- A registry entry with a kind or status gomuxai doesn't know is never
  guessed at: that session's native status is `unknown`, detail naming the
  value and the Claude Code version, so its report stands; the others are
  read as usual. A file that can't be read as an entry at all (not JSON, no
  pid or session id; a half-written file for a moment) could be any
  session's, so while there is one (`Snapshot.Problems`, `Snapshot.Err`)
  the readers don't conclude a session isn't running: native status, the
  pane's session and `agent sessions`' live view answer with the error.
  The report then stands, and `status -v` prints the reason on a `native:
  unread:` line (`--json`: `native_error`); `agent sessions` shows it as a
  hint.
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

### Claude Code's session registry

Claude Code keeps one file per running CLI process,
`<config dir>/sessions/<pid>.json` (`$CLAUDE_CONFIG_DIR`, else
`~/.claude`): it writes it at start, rewrites it as the status moves, and
removes it at exit. It is Claude's own record of which session each process
holds, and what `claude agents` is built on. `internal/claudereg` reads it.
The format isn't a published contract, so the reader takes only the fields
it names (`pid`, `sessionId`, `cwd`, `version`, `kind`, `entrypoint`,
`status`, `waitingFor`, `name`, timestamps, `procStart` or `procStartFt`,
`pidDomain`, `tmux`), and refuses an entry with no pid or session id, or a
pid other than its file name's. A `kind` (`interactive`, `bg`, `daemon`,
`daemon-worker`) or `status` (`busy`, `idle`, `shell`, `waiting`) outside
that vocabulary is kept with `Entry.Unknown` saying so: a Claude Code that
writes a new value needs gomuxai taught what it means, and until then that
session's status is `unknown`, never guessed (its process is still proven
and placed). A missing kind is `interactive` and a missing status not yet
known, as Claude reads them. Claude Code 2.1.316 ships a switch, off, that
writes the start time as `procStartFt` instead of `procStart`; the reader
takes either, and a `procStartFt` unlike this OS's start time makes the
entry unproven, not stale.

An entry is trusted only when proven (`Proof`), the check Claude Code makes
itself:

| Verdict | When |
|---|---|
| live | its pid is running, and that process started when the entry's `procStart` says |
| stale | its pid isn't running, or now belongs to a process that started at another time (the CLI crashed or was killed without removing its file) |
| foreign | its `pidDomain` isn't this host's (another container sharing the directory) |
| unproven | its pid is running, but the start time can't be compared (no `procStart`, or none readable on this OS) |

The start time is the one Claude records: on Linux the 22nd field of
`/proc/<pid>/stat` (clock ticks since boot, fields counted from the last
`)`), on macOS `ps -o lstart=` in the C locale and UTC. The pid domain is
`linux:<machine id>:<pid namespace link>`; on macOS Claude writes the
literal `linux`, which gomuxai doesn't compare. A zombie (Linux state `Z`:
exited, not yet reaped) counts as not running. `Snapshot.Sessions` maps each session to the live or unproven
entry that holds it (when two do, a CLI stopped with Ctrl-Z and the session
resumed in another, the proven one, then the one updated last).

An entry's `tmux` field is **not** used to place a session: Claude copies it
from its environment, so a `claude -p` run from inside an agent's pane
names that pane too. Where a session runs comes from the process tree
instead (`ps -A -o pid=,ppid=`): `Snapshot.UnderPane` takes the live
entries whose process descends from the pane's process (`#{pane_pid}`, from
`tmux list-panes -a`), and returns the shallowest. A `claude -p` that a tool
started inside the session is deeper, so it is never taken for the pane's
own. Two at the same depth (two CLIs side by side under one shell) are an
error naming both, unless one was updated last.

### Pane adoption

A person can quit an agent's Claude and start another in the same pane, or
`claude --resume` a different session there; the old CLI's last hook (if
any) says nothing about the new one, and a CLI started by hand may carry no
hooks at all. So when native status doesn't list `_session_id`,
`adoptPaneSession` asks the backend (`PaneSessionProvider`) what the
agent's pane runs. If it runs a session other than the one on record, that
session is recorded as the agent's: a report with event `adopted`, detail
`its pane runs session <id> now (pid <pid>, <status in words>); session
<old> before it ended`, the new `_session_id`, and `_result` and
`_transcript` cleared (another conversation's reply and file aren't this
one's). `agent sessions` then joins the agent to the session the scan found
under that id. A pane that runs nothing of the backend's, or the same
session, changes nothing, and status goes on as above.

### Reports and the reserved state keys

`AgentService.Report` writes these keys for the agent in one transaction
(`SetStates`), so a reader never sees half of two concurrent reports:

| Key | Holds |
|---|---|
| `_status` | `idle`, `processing`, `waiting`, `error`, `ended` (or `unknown`) |
| `_end_reason` | with `ended`: how the session ended (`quit`, `gone`, ...); empty otherwise, since every report writes it |
| `_event` | the hook event, `report` for `agent report`, `approved`/`denied`, `trusted`/`trust-declined` (approve/deny at a trust prompt), `delivered` (a Stop handed over held messages), `adopted` (its pane runs another session) |
| `_detail` | one line of context, e.g. `Bash: go test ./...` or `permission: ...` |
| `_tool` | the tool the event concerns |
| `_tool_input` | the full JSON input of a call that may need permission (≤64 KiB); always written, so it never outlives its call |
| `_result` | Claude's last reply (`Stop`'s `last_assistant_message`); written only when set or cleared |
| `_session_id` | the CLI's session id; spawn sets it, hooks update it (`/clear`, `/resume`, agy's conversation id), and pane adoption sets the session the pane runs |
| `_spawn_id` | agy only: the spawn's uuid, set once by spawn (a Claude agent spawned with per-spawn hooks, before the global hook, has one too) |
| `_hooked_session` | Claude: the session the global hook last reported for the agent's main session; while it equals `_session_id`, the agent holds messages |
| `_turn_at` | when the last turn started: written with a report that is `processing` and clears `_result` (Claude `UserPromptSubmit`, agy's first `PreInvocation`) |
| `_sent_at` | when a message was last typed in; written by `send` (and queued deliveries and spawn's first message), not by `Report` |
| `_unsubmitted` | what Claude's input box showed when Enter failed to submit a typed message; deleted by the next typing whose message doesn't stay in the box |
| `_unchecked` | why a message was typed into Claude without checking it was submitted (its input box couldn't be read); `status` shows it until a turn starts after it, and a checked typing deletes it |
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

`--spawn-id` guards agy's hooks against stale writers: a report or hook
whose spawn id isn't the agent's current `_spawn_id` is dropped, so the exit
wrapper of an agy that `cleanup` just killed can't mark a respawn of the
same name `ended`. Claude's global hook needs none: it places itself by the
process tree, and a killed CLI has no pane above it.

Hook mode (`report --hook`, and `agent hook`) reads the payload on stdin,
writes stdout only to hand over held messages (Claude's `Stop`, agy's
`PostInvocation`; see Held messages) or to approve a safe `agent` command
(Claude's `PreToolUse`), and exits 0 even when the report fails (errors go
to stderr), so a reporting problem can't disturb the agent. `report --hook`
still exits 1 if the binary can't start, e.g. the database can't be opened.

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
| `SessionEnd` | ended (quit) | detail: Claude's reason (`prompt_input_exit`, `logout`, ...); ends every pending prompt. Ignored for `reason: clear` (a `SessionStart` follows); the session's own report is then `ended (replaced)`, detail `/clear` |
| `SubagentStart`, `SubagentStop` | — | not an agent report; the global hook records the subagent's session report (`processing`, then `ended (finished)`) |

**agy** (`AgyBackend.ParseHookEvent`, `namedagents/agy.go`):

| Event | Status | Notes |
|---|---|---|
| `PreInvocation` | processing | |
| `PreToolUse` | processing | stores `_tool_input` for dialog promotion; `ask_question`'s detail is its first question and options |
| `PostToolUse` | processing | `failed: ...` on error |
| `PostInvocation` | — | `DeliveryOnly`: recorded (processing, `delivered`) only when it hands over held messages; ignored on an error or `USER_CANCELED` |
| `Stop` | idle, processing, or error | error for an error or `ERROR`/`MAX_*` termination; processing when `fullyIdle` is false |

agy has no `SessionEnd` hook and no session list, so its launch command ends
with `; <binary> report ended --end-reason quit --as <name> --spawn-id
<uuid> "agy exited with code $?"`: the shell records `ended (quit)`, with
agy's exit code as the detail, however agy ends (`/exit`, crash, `kill
-9`), and the spawn id keeps a stale wrapper from marking a respawn
`ended`. Arguments added later (`--add-dir`, `-i`) are inserted before that
suffix.

### The global hook (`agent hook`)

`agent hooks install` (`internal/cli/hooks.go`,
`namedagents/claude_settings.go`) registers this binary in Claude Code's
user settings (`$CLAUDE_CONFIG_DIR/settings.json`, else
`~/.claude/settings.json`): for each of the 13 events in `ClaudeHookEvents`
(`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`,
`PostToolUse`, `PostToolUseFailure`, `PreCompact`, `Notification`,
`SubagentStart`, `SubagentStop`, `Stop`, `StopFailure`, `SessionEnd`), a
group `{"matcher": "", "hooks": [{"type": "command", "command": "<binary>
hook", "timeout": 10}]}`. An entry is gomuxai's when its command is two
words, an executable named `agent` (or as this binary is named) and `hook`.
One hook serves one agent binary: the store beside the binary it runs
records the sessions it places, so a Claude agent another binary spawned
isn't heard. An entry naming a binary that is gone is re-pointed at this
one; one naming a binary that still exists is re-pointed only with
`--force` (that binary's agents would stop reporting), and spawn refuses a
Claude agent while the hook runs another binary, naming it. Every other
hook and setting stays as it was; numbers keep the digits written. Before
writing, the file is copied to `<data dir>/backups/settings.json.<UTC
time>`; the write is atomic (`util.WriteFileAtomic`: a temp file renamed
over it) and keeps a symlinked settings file a symlink, writing its target.
A file that isn't a JSON object, or whose `hooks` aren't shaped as Claude
Code reads them (an object of event arrays of groups with a `hooks` array of
objects), is refused and left alone, naming the first bad place. Nothing is
written when every entry is current. `remove` takes gomuxai's entries out,
dropping events left empty; `status` prints each event's state
(`installed`, `another agent binary's`, `missing`) and the last 5 lines of
`logs/hook-errors.log`. Claude Code reads hooks when a session starts, so
running sessions pick a change up when restarted (or through `/hooks`).

Claude Code then runs `<binary> hook` for every event of every session on
the machine: a workgroup agent's, a person's, a `claude -p`. `agent hook`
reads the payload and places itself (`FindHookSession`, `placeHook`):

1. **The CLI above it**: walking up its own ancestors (`/proc/<pid>/stat`
   on Linux, one `ps` listing elsewhere), the nearest process that is the
   CLI: its registry entry (`claudereg.ReadPID`) is live, or unproven but
   holding the payload's session id, or it is the pid Claude Code sets in
   `$CLAUDE_PID` for its hooks. Being an ancestor of the running hook is
   proof the process runs; the last two cover a CLI whose start time can't
   be compared, and the first event of a session, which can come before
   Claude writes its registry file (seen at `SessionStart`).
2. **The agent above that CLI**: walking on up, the first process that is
   a tmux pane's (`list-panes -a -F '#{pane_pid} #{session_name}'`), if its
   session is an agent's. Another CLI on the way first (any running
   process with a registry entry, proven or not) means this CLI was started
   from inside that one's session (a `claude -p` a tool ran), so no agent
   owns it. A CLI whose pane is gone (cleanup killed the session) reaches
   no pane, so no agent.

Then `HandleClaudeHook`:

- A CLI whose command line carries gomuxai's per-spawn hooks (`--settings`
  whose hooks run `report --hook`; a Claude agent spawned before the global
  hook) is skipped (`SpawnHooked`): its own hook reports it.
- **Every session's report** goes to `session_reports`, keyed by session
  id: the status the event maps to, as in the table above, with its end
  reason, event, detail, transcript and time (a stored newer one wins, since
  hook processes can finish out of order). A subagent's events (they carry
  `agent_id`) are recorded under `agent-<agent id>`, as `agent sessions`
  names it: `processing`, and `ended (finished)` at `SubagentStop`. A
  `SessionEnd` with reason `clear` is `ended (replaced)`. This is how
  `agent sessions` knows how a session no agent owns ended.
- **An agent's session** also gets the agent's report, through `HandleHook`
  as `report --hook` would (no spawn id). For a main-session event it first
  sets `_hooked_session` to the payload's session id, so the very `Stop`
  that follows can hand over held messages. Only here does the hook print
  anything (held messages, the `PreToolUse` approval): a session no agent
  owns never sees output from it.

It exits 0 whatever goes wrong once it runs (like `report --hook`, it
exits 1 only if the binary can't start, e.g. the database can't be opened).
When placing fails, the session's report is still recorded, and the error
says why no agent got one. Errors go to stderr and,
one line each with a UTC time, to `logs/hook-errors.log`.

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

A **workspace trust prompt** (status tool `trust`, which `--expect trust`
names) is answered by `respondTrust` instead: right before the keypress it
captures the screen, requires the trust dialog there, and reads which
option is highlighted from that same capture. Approve picks "Yes, I trust
this folder" and records `processing`/`trusted`; deny picks "No, exit",
the CLI exits, and it records `ended (quit)`/`trust-declined`. `approve
--always` is refused (trusting a folder is already for good), and so is a
prompt with neither option highlighted. If spawn held the agent's first
message, approve starts the deliverer. The other way round, the
permission-prompt path refuses when its pre-keypress capture shows a trust
dialog, so a status that described a permission prompt never sends `1` or
Escape into one.

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

**`internal/transcript`** reads both, and Kimi Code's
`~/.kimi-code/sessions/<workspace>/session_<uuid>/agents/<agent>/wire.jsonl`
(no hook reports it; `agent sessions` finds it), into the event stream
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
What only the files beside a transcript say (Kimi's `state.json`, which of
agy's two logs it is, agy's index of the subagents a conversation started,
the parent transcript that numbers a Claude subagent's turns) comes in
through `Options`, which `FileOptions` reads for a path; `ReadFile` and a
resumed parse take the same ones. `Latest` folds a stream into each event's
latest state.
`Finals` merges the final reply a Stop hook reports (`last_assistant_message`)
into a stream ahead of the transcript, which writes it only after the hook ran:
a provisional say that the transcript's copy then updates.

The session's metadata (cwd, title, agent name, model, version, branch,
continued-in) follows one set of rules, `internal/transcript/meta.go`,
which the parsers apply as they read and `agent sessions` applies to the
head and tail it reads (`HarvestClaude`, `HarvestKimi` for a line;
`HarvestAgy`, `HarvestClaudeSubagent`, `FileOptions` for the files beside
a transcript). Each line's value is placed by its byte offset, not by when
it was read, so a reader going backwards from the end gets what a parse
from the start gets: the newest value wins, the oldest for the cwd.
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

**`agent transcript`** (`internal/cli/transcript.go`) is the stream for
displays: one session's events, whole, as [`docs/EVENTS.md`](EVENTS.md)
frames them. It resolves a workgroup agent's name (the last transcript in
its list), a session id or a unique start of one, or a path, by the same
scan and `discovery.Resolve` as `agent sessions`, whose Session is the
stream's `session` line. The initial read stops at the size that Session
recorded, so the line describes the end of that read. Every read parses
from byte 0, through `Finals`: `--from` and `--until` only filter what is
written, by each event's `at`, so a window's events are numbered, turned
and merged as a whole read's. (`--from` is checked as `ResumeAfter`
checks it, a line's start with its line whole, but not parsed with it,
since `Finals` must see the skipped lines' final replies.) `--follow`
polls the transcript every 250ms (there is no file watcher in the repo; a
poll is a `stat` of the path, an `fstat` of the open file and a 64-byte
`pread`) and feeds the parser what was appended, keeping a line not yet
whole for the next poll. A path that's gone, names another file
(`os.SameFile`), shrank, or no longer holds the last 64 bytes read (a
truncate and regrow between polls) ends the stream with an error.

A listed session's path may name no file yet: `discovery.Resolve` lists a
live agent's current transcript before its harness writes it (Claude Code
creates it at the first prompt). If that session hasn't ended, the target
is unwritten and `--follow` waits (`awaitTranscript`): it `stat`s the path
every 250ms and resolves the session again every 30s, and once the file is
there resolves it once more and streams it as any other. A session that has
ended with no file is an error, at once or when a wait sees it; so is a
path no session lists. Without `--follow` an unwritten transcript is an
error that says it isn't written yet.

Each poll also asks whether the reader has gone: `poll(2)` on stdout with
no events reports `POLLERR` (a Linux pipe whose reader closed) or `POLLHUP`
(a socket) without a write (`transcript_poll.go`, Linux and darwin; on
darwin a pipe's reader closing may only show at the next write). SIGPIPE
is ignored, so a write to a closed stdout fails with `EPIPE` instead of
killing the process, and a notice to a closed stderr is just lost; SIGINT
and SIGTERM cancel the stream. All of these exit 0.

The `session` line's metadata (title, model, version, branch, agent name,
cwd) comes from the stream's own parser after each read, which has seen
every line, where the opening line had discovery's head-and-tail read; a
value the parser hasn't found leaves the known one. Status comes from
checks beside the poll, one at a time in a goroutine the stream waits for
before it returns, so a slow one never holds up the poll. For a session an
agent of this store owns, a report check every 2s reads that agent's
record and state from the store alone (`ownAgent`: what export reads, no
tmux, no other store) and resolves the probed session against it with no
harness list, counting only when the agent has reported since the status
was last set. A full check, `discovery.Resolve` over every store's agents,
Claude Code's session registry and the global hook's session reports, as
`agent sessions` does it, runs every 30s,
the wait doubling up to 5 minutes while the transcript doesn't grow. A
`session` line is written when anything but `size`, `mtime` and
`status_at` changed; its `size` is how far the stream has read.

For a Claude agent of this store whose `_transcript` is the file, each new
`_result` (its `updated_at` newer than the last one seen, and not older
than the current turn's first event) is offered to `Finals.Provisional`
for the current turn, at the parser's `Pos`: where the reply's line will
land. A Stop that delivers held messages empties `_result` (the turn goes
on), so it makes none.

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
- **Runs.** Spawn opens one, cleanup closes it with the last status, end
  reason and transcript, so `--all` lists departed agents without the
  command log.
  A store from before runs has its departed agents rebuilt from the command
  log's spawns (newest per name, `run` 0) and `logs/<name>.transcripts`.
- **The snapshot** reads agents, runs, the reserved status keys, and the
  newest event id in one read transaction, so the snapshot holds every
  event up to its cursor and none after.
- **Status** is the last report, not `StatusInfo`: export reads no session
  registry, process tree or screen, so any number of followers cost a
  query every 250ms each. Known differences from `agent status`: an
  interrupted permission prompt stays `waiting`, a crashed CLI keeps its
  last status (`attached` stays true: the shell is still there) where
  `status` says `ended (gone)`, a CLI started anew in the pane shows only
  once a status check has adopted it, and with several queued prompts
  `detail` describes the newest, not the one on screen. A status event
  whose session id differs from the agent's last, with no transcript,
  empties the agent's `transcript` fields: the old file isn't the new
  session's.
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
- **Other hosts** (`--hosts all|h1,h2`) come from the bus's `list` op:
  the host's resolved status and its `end_reason` and `detail`, and
  `session_id`, `cwd`, `attached` and `resumable` as that host works them
  out; no `store` or transcript fields. A host that predates those fields
  (before `list` carried them) leaves them out, and **absent means
  unknown**: `attached` and `resumable` are then missing, not `false`.
  `--follow` lists them again every 30s and sends a `host` line when one
  changed.
- **Resumable** is whether `agent resume` would continue the agent, by
  resume's own rules (`types.Resumable`, which `service.ResumeRefusal`
  gives the reasons for): a claude or agy agent gomuxai started (not
  adopted), with a session id, that has ended, and still has a record. For
  this host's agents export applies them to what it knows: the last report
  says `ended`, or the tmux session is gone (where `agent status` says
  `ended (gone)`); the session id is `_session_id`, else the transcript
  path's. Another host's comes from its `list`, which uses the resolved
  status and the newest transcript's session id. A departed agent is never
  resumable.

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
 "created_at":"2026-10-24T14:00:00.5Z","live":true,"attached":true,"resumable":false,
 "status":"waiting","event":"PermissionRequest","detail":"permission: Bash: rm -rf build","tool":"Bash",
 "tool_input":"{\"command\":\"rm -rf build\"}","status_at":"2026-10-24T14:01:02.25Z",
 "session_id":"5f0c…","transcript":"/home/user/.claude/projects/-home-user-gomuxai/5f0c….jsonl",
 "dialect":"claude","transcript_size":48211,"transcript_mtime":"2026-10-24T14:01:02.3Z"}
```

`kind` is `tmux` for every agent so far (room for CLI sessions that agent
didn't start). Absent fields are empty: `parent` for a person's agent,
`ended_at` while live, `store` and the transcript fields for another
host's agents. For another host that predates them, absent means unknown:
`attached`, `resumable`, `end_reason`, `detail`, `session_id` and `cwd`
are then left out, and `attached` and `resumable` are never `false` for
"don't know". This host's agents always carry `attached` and
`resumable`.
`end_reason` is set beside `"status":"ended"`. A departed agent (`live:
false`, `ended_at` set) keeps its last report's event and detail, and its
status is `ended (closed)`, unless that report had already ended it with a
reason, which it keeps.
Runs don't record the session id, so a departed agent's `session_id` is
the one its transcript's path names (`transcript.SessionIDFromPath`).

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

## Sessions

`agent sessions` (`internal/cli/sessions.go`, `internal/discovery`) lists
every agent session on the host: workgroup agents' and the ones a person
ran, in one shape, the Session of [`docs/EVENTS.md`](EVENTS.md). It only
reads, and runs no backend CLI: for Claude it reads Claude Code's session
registry and the global hook's `session_reports`.

**Where.** One table (`discovery.Dirs`, ported from glyph3d-rs
`crates/glyph-session-dirs`):

| harness | default dirs (each kept only if it exists) | app config key |
|---|---|---|
| claude | `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects` | `claude_projects_dir` |
| agy | `~/.gemini/antigravity/brain` (desktop), `~/.gemini/antigravity-cli/brain` (CLI) | `agy_brain_dir` |
| kimi | `~/.kimi-code/sessions` | `kimi_sessions_dir` |

A configured path (in the JSON app config `--backend-config` loads) is the
only dir scanned for its harness, even if missing; `""` turns the harness
off.

**What.** Each session's fields come from the harness's own files, never
the storage folder's name, and only the head and tail are read. The rules
are the readers' (`internal/transcript/meta.go`, under "Results and
transcripts"); discovery only decides how much to read:

| field | claude | agy | kimi |
|---|---|---|---|
| `session_id` | file stem; a subagent's `agent-<id>` | the brain dir's name | `session_<uuid>` without the prefix; a subagent's `<uuid>:<agent>` |
| `cwd` | the first record's `cwd` | `history.jsonl`'s first `workspace` for it, else a subagent's `workspaceUris` | `state.json` `workDir`/`cwd`, else `session_index.jsonl` |
| `title` | newest `custom-title`, else newest `ai-title`; a subagent's meta.json `description` | `annotations/<id>.pbtxt`, else a subagent's role | `state.json` `title`, else `lastPrompt`; none on a subagent |
| `agent_name` | newest `agent-name` record | - | - |
| `model` | newest assistant `message.model` (not `<synthetic>`) | - (in its steps, which a listing doesn't read) | newest `llm.request` `model` |
| `version`, `git_branch` | the newest record with a `version`: both, even an empty branch | - | - |
| `parent_session` | the `<uuid>/subagents/` dir (a workflow's agents in `subagents/workflows/<run>/`); `tool_id` = meta.json `toolUseId` | `brain/<parent>/.system_generated/subagents/<id>.json` | the session dir of `agents/<agent>/` |
| `continued_in` | newest `continued-in` record | - | - |

Claude records with `isSidechain` and no `agentId` (an old main
transcript's subagent records) are another session's and set nothing.

A Claude transcript is never read whole: the head grows from 16 KiB until
a record has a cwd (at most 256 KiB), and the end is read backwards, at
most its last 1 MiB, until the metadata is found and, for a main session,
a title. Claude re-appends the custom title beside every AI title it
writes, so the search stops 64 KiB past the newest AI title; a title older
than the last 1 MiB isn't found. A subagent's transcript holds no title,
so only its metadata is looked for. Measured on 1851 transcripts (896 MB):
117 MB read, at most about 0.5 MB per file. `project` is the cwd's last
part.

**Workgroup join.** The agents come from export's tree with `--all` (this
store and those its tmux sessions point at), each with its name's
`logs/<name>.transcripts`. A transcript belongs to the run whose current or
last transcript it is; one only the history names, to that name's newest
run created before the file's last write. A subagent's session belongs to
its parent session's agent, at any depth. Every transcript an agent names
is read first, wherever it lives, so the history rule knows its last write.
Paths and stores are compared with symlinks resolved, so a symlinked
`~/.claude` lists a session once. A live agent whose current session no
hook has named a transcript for (one adopted from its pane) owns the
session the scan found under its `_session_id`. A live agent with no
transcript yet is listed with an empty `path` and its backend as `harness`.

**Status** (`discovery.Resolve`); every `ended` carries its end reason:
- a live agent's current session: its last report, reconciled with the
  session registry by `discovery.ReconcileNative`, the one rule `agent
  status` uses too: the registry's status when a running process holds the
  session (but not a `waiting` within 5s of a deny), `ended (gone)` when
  none does and the report says it is alive, else the report (an `ended`
  report with no reason reads as `gone`). `agent status` then reads the
  screen and adopts the pane's session as well; `sessions` doesn't. agy:
  `ended (gone)` once agy no longer holds the conversation open;
- an agent's other sessions: `ended (replaced)`; a departed run's last
  session: `ended (closed)`, or the reason its last report gave if it had
  already ended;
- Claude, no workgroup: the registry's status; else, if the global hook
  heard the session end, `ended` with that reason (`quit`, `replaced`);
  else `ended (gone)`; `unknown` if the registry can't be read;
- agy, no workgroup: agy holds an exclusive `flock` on
  `presence/<conversation id>.lock` while a conversation is open. On Linux
  that is read from `/proc/locks`, taking no lock: a lock on the file's
  inode whose holder has the file open (its `/proc/<pid>/fd`; the device
  `/proc/locks` names is the superblock's, which differs from `stat`'s on
  btrfs). Elsewhere a non-blocking shared `flock`, released at once, which
  for that moment would make agy's own non-blocking lock fail. `unknown` while held
  (idle and busy look the same), else `ended (gone)`. A subagent conversation
  without its own lock is open while its record says `ALIVE` and its parent
  is open;
- Kimi, which records nothing about running: `unknown` if its transcript
  changed in the last 10 minutes, else `ended (gone)`;
- a subagent, in this order: `ended (finished)` if it recorded finishing
  (Claude's meta.json `finished`, agy's state) or the global hook saw its
  `SubagentStop`; `ended (gone)` if agy says it isn't open; `ended
  (parent)` if its parent session ended; `processing` if the global hook
  saw it start and not stop; else `unknown` (or, with no parent found and
  no sign of life, the 10-minute rule above).

`status_at` is the report's time where the report stands, else the
transcript's last write (a departed run's: when it ended).

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
they can't contain `.`, `*`, `>`, or spaces. The host name is `$AGENT_HOST`,
else the name in the bus identity's `host` file (`.gomuxai/bus/host`), else
the machine's name with unsafe characters replaced: macOS's LocalHostName
(`scutil --get LocalHostName`), since its kernel hostname follows DHCP and
reverse DNS, and the short hostname elsewhere.

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
capture,send,respond,friction,logs`; `keys`, `spawn` and `resume` are
local-only unless listed, since `keys` types anything and `spawn` and
`resume` start a process on the host). `logs` only reads, like `capture`.
`friction` is on by default: what it lets a peer do (read entries, link
them, resolve a linked one) is far less than `send` already does.

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
| `list` | `ListRequest` → `ListResponse` (`agent_binary`: the serving host's agent binary, absolute; agents: `name`, `session`, `backend`, `spawned_by`, `adopted`, `created_at`, resolved `status`, `end_reason`, `detail`, `session_id`, `cwd`, `attached`, `resumable`; older hosts send no `detail` onward) | `list --host H`, `export --hosts` | yes |
| `status` | `StatusRequest` → `StatusResponse` (mirrors `StatusInfo`) | `status`, `wait` | yes |
| `capture` | `CaptureRequest` → `CaptureResponse` | `capture` | yes |
| `send` | `SendRequest` (`now`) → `SendResponse` (status, held, typed); none from older hosts | `send` | yes |
| `respond` | `RespondRequest` (`approve`, `approve_always`, `deny`, `force`, `expect_tool`) → none | `approve`, `deny` | yes |
| `keys` | `KeysRequest` (text, then a key) → none | `keys name@host` | no |
| `spawn` | `SpawnRequest` (`agent`, `backend`, `cwd`, `task_file`, `task_content`, `spawned_by`) → `SpawnResponse` (`name`, `session`, `backend`, `spawned_by`, `cwd`, `task_file`, `status`, `end_reason`) | `spawn name@host`, `spawn --host H` | no |
| `logs` | `LogsRequest` (`agent`, `lines`, `full`, `json`, `terminal`) → `LogsResponse` (`output`: what `agent logs` prints there, `hint`: its stderr hint, `truncated`: bytes left out) | `logs name@host` | yes |
| `resume` | `ResumeRequest` (`agent`, `resumed_by`) → `ResumeResponse` (`name`, `session`, `backend`, `session_id`, `cwd`, `held`, `status`, `end_reason`) | `resume name@host` | no |
| `friction` | `FrictionRequest` (`action`: `get` `ids`; `link`/`unlink`/`resolve` `id`, with `from` (required), `by`, `note`; resolve's `uid` and `status`; resolve closes only an entry linked to `from`) → `FrictionResponse` (`entries` with their `links` and `requested`, `missing`, `resolved`) | `friction show`/`list`/`links`/`link`/`unlink`/`resolve` | yes |

`natsbus/ops_test.go` keeps `AllOps` and the server's op table in step.
`logs` and `resume` run logic that lives in the CLI (`agent logs`'s
rendering, and `resumeAgent`, which finds the session to continue), so the
service's handler takes them as `service.RPCExtras`; `agent bus start`
serves `cli.busHandler`, which supplies both. A handler without them
refuses those two ops.

`logs` replies with what `agent logs` prints on the serving host, built as
chunks (an event, or with `--full` its lines; a JSON line; a session's
separator or session line; a terminal log line) so a reply over
`MaxLogsReply` (512 KB, encoded; the bus's message limit is 1 MB) drops
whole chunks from the start, puts back the session line or separator the
first kept chunk belongs to, and reports `truncated`. A single text chunk
too big alone keeps its end; a JSON line too big alone is left out, since
half a line isn't JSON. The client strips terminal controls from `output`
and `hint` as from `capture`, and says on stderr when the reply was cut.

`resume` runs the serving host's own `resumeAgent`, so a refusal is that
host's resume's (adopted, not claude or agy, no session, not ended), and
an agent not found is not found there. `resumed_by` is the caller as
`name@host`: as on that host, an agent may resume only its own helpers
(the parent chain must contain it); a bare name, or one naming the serving
host, is refused. As for `spawn`, the caller refuses `--skip-permissions`
and `--trust-workspace` for another host's agent before sending, and
`ResumeRequest` has no field for them: a peer never starts a process on
another host with its permission checks bypassed. The client waits up to
`SpawnTimeout`, as for spawn. The
remote side runs the same service code as a local call (`Send` holds a
message for a busy Claude agent and refuses to type into dialogs;
`RespondPermission` still checks the screen), and the CLI
adds the `[from ...]` prefix before the request leaves. For `spawn`, the
caller reads `--task` from its own store and sends the content
(`task_content`); `task_file` and `cwd` are paths on the remote host, `cwd`
absolute (empty: the home directory of the user the bus runs as), and a relative
`task_file` is relative to `cwd`. The caller refuses `--resume`,
`--attach`, `--backend-config`, `--skip-permissions` and
`--trust-workspace` for a remote spawn before sending; `SpawnRequest` has
no field for `--skip-permissions`, `--trust-workspace` or `--resume`, so a
peer can't ask for them either. (Confirming a trust prompt is saved in the
host user's CLI config and would outlast the agent.) So `cwd` must be a
directory the CLI already trusts there, or a person there confirms the
prompt. A `spawned_by` naming the serving host itself (in any case) is
refused: that agent didn't ask over the bus. The client waits up to
`SpawnTimeout` (2m) for the reply, since the far side sits out the CLI's
startup first. `cli/remote.go`
routes `name@host`: a bare name or `name@<this host>` stays local and needs
no bus.

Host names match ignoring case, but subjects don't: a request to
`gomux.il-air-m2.rpc.list` reaches no bus when the host is `IL-Air-M2`.
So the CLI spells a host as the bus does before it builds a subject
(`canonicalHost`): this host's name, or a trusted peer's (`bus/peers/`),
whichever matches ignoring case; `name@host`, `list --host`, `spawn
--host`, `wait` and `export --hosts` all go through it. A hub trusts every
peer, so it knows every spelling; a leaf trusts only the hub, and reaches
other leaves by the exact name it is given. A request no bus answers fails
with `natsbus.NoRespondersError`, to which the CLI adds the hosts it knows.

### Attach over ssh

The bus can't carry a terminal, so `attach name@host` and `resume
name@host --attach` (`cli/ssh.go`) run ssh themselves; a display or agent
never builds the ssh command or guesses a binary's path:

1. **Where:** the destination set per peer with `agent bus ssh <host>
   <dest>` (`bus/ssh.json`, plain config, shown by `bus status`), else the
   host's name. It may be a host, `user@host`, an alias or a bracketed
   address, never starting with `-`; `~/.ssh/config` still applies.
2. **What:** the host's `list` reply names its agent binary
   (`agent_binary`, `cfg.AgentBinary`, absolute with symlinks resolved).
   The caller refuses one that isn't an absolute path of plain characters
   (`ValidateAgentBinary`), refuses an agent the reply doesn't list, and
   says so for a host that predates the field, without running ssh.
3. **Run:** `ssh -t -- <dest> '<binary>' attach '<name>'` (or `resume
   '<name>' --attach`): ssh hands the command to a shell there, so both
   are quoted. It needs a terminal here, and says so without one.
4. **Exec:** `execInto` replaces the agent process with ssh
   (`syscall.Exec`), so a signal to `agent attach` reaches ssh and nothing
   is orphaned; local attach does the same with tmux. The command-log row
   is written just before, since nothing runs after. Before the exec, a
   line on stderr says it is ssh, not the bus, and needs `ssh <dest>` to
   work: once ssh runs, its errors are its own.

Over ssh, the remote `resume --attach` is the person's own command on that
host, not the `resume` op: that host's `--remote-ops` doesn't apply, and
the `--skip-permissions` and `--trust-workspace` refusals are the caller's.

### Status events and wait

`Report` publishes a `StatusEvent` (host, agent, status, `end_reason`,
event, detail, tool, time) for every report it records, through the service's optional
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
arrival once. Unless `ended` is in `--for`, the wait fails as soon as the
agent is `ended`. A timeout exits 2.

`agent wait` with several names (`internal/cli/wait.go`) runs one
`natsbus.WaitObservation` per agent, in goroutines that share the bus
connection, and prints each arrival as it comes. Local names are looked up
first, so a typo fails at once instead of waiting out the timeout. With no
names, `--any`/`--follow` list this host's agents (minus `AGENT_NAME`), and
`--follow` lists them again every 10s to pick up new ones; there, an agent
whose session has ended or that has been cleaned up is dropped silently. A single named agent
without `--follow` keeps the original output (`name: status`) and errors.

### Security model, in short

Anything that can `send` to an agent can make it run commands, so the bus is
treated like SSH:

- Links: TLS 1.3, certificates pinned by key; nkey authentication per host;
  no shared secrets, and only public bundles are copied.
- Least privilege: fixed ops only, per-host `--remote-ops`, no raw keys
  and no `spawn` remotely by default, no general remote shell. Listing
  `spawn` on a host amounts to letting every linked peer run commands
  there as the bus's user, at least as much as `keys`: with `send` and
  `respond` (on by default) a peer can spawn a Claude agent and approve its
  Bash prompts, or spawn aider and send it `/run ...`. List it only on a
  host whose peers you'd give a shell. Peers can't publish events as
  another host, and only receive replies under their own inbox.
- Local clients: `127.0.0.1` only, nkey from a 0600 file, and the bus's
  certificate pinned by the client.
- The hub is trusted as the router: it sees everything that crosses it and
  could forge events, which is why consumers confirm events with a status
  check. Run it on the machine you trust most.
- Text from other hosts is sanitized before printing.
- Attaching to another host's agent runs ssh, a separate trust path (the
  person's ssh keys and `~/.ssh/config`), to run the binary that host
  names in its `list` reply. The path must be absolute and of plain
  characters (`ValidateAgentBinary`), and is shell-quoted, as the agent
  name is; the destination can't start with `-`, and `--` ends ssh's
  options. A host (or the hub, see below) that lies about its path can at
  most make ssh run another existing program there, as the person, with
  `attach <name>` as its arguments.
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
| `runs` | `id`, `name`, `parent`, `backend`, `session`, `cwd`, `created_at`, `ended_at`, `status`, `transcript`, `end_reason` | one per spawn; cleanup sets `ended_at` and the last status, transcript and end reason; indexed on `(name, id)` |
| `agent_events` | `id`, `at`, `kind`, `agent`, `run`, `status`, `event`, `detail`, `tool`, `tool_input`, `session_id`, `transcript`, `end_reason` | `spawned`/`status`/`removed`, for `export --follow`; the newest 20000 are kept |
| `session_reports` | `session_id` PK, `harness`, `status`, `end_reason`, `event`, `detail`, `transcript`, `at` | the global hook's last report for every Claude session it heard (a subagent's under `agent-<id>`), for `agent sessions`; a write older than the stored one is dropped; never pruned |
| `messages` | `id`, `sender_id`, `receiver_id`, `content`, `status`, `created_at` | held and queued messages: `pending`/`delivered`/`failed`; indexed on `(receiver_id, status)`; claimed with `UPDATE ... WHERE status = 'pending'` |
| `tasks` | `name` PK, `content`, `source_file`, `created_at` | independent of agents |
| `state` | `agent_name`, `key`, `value`, `updated_at`; PK `(agent_name, key)` | FK to `agents` with `ON DELETE CASCADE`: state needs an agent record |
| `command_log` | `id`, `started_at`, `duration_ms`, `host`, `agent`, `parent`, `cwd`, `args` (JSON), `exit_code`, `error`, `build` | `agent history`; indexed on `(agent, id)` |
| `friction` | `id`, `uid`, `host`, `filed_by`, `parent`, `cwd`, `build`, timestamps, `kind`, `summary`, `detail`, `command` (JSON), `status`, accept and resolve fields, `duplicate_of` | the friction box |
| `friction_votes` | `id`, `friction_id` (FK, cascade), `uid`, `host`, `voter`, `parent`, `build`, `created_at`, `note`, `command` | `+1`s |
| `friction_links` | `id`, `friction_id` (FK, cascade), `peer_host` (`COLLATE NOCASE`), `peer_id`, `peer_uid`, `linked_by`, `linked_at`, `note`, `confirmed_at`, `peer_*` (status, kind, summary, votes, resolved_by, resolution, checked_at); unique `(friction_id, peer_host, peer_id)` | this host's half of a link to another host's entry, and what was last seen of it |

There is no logs table: logs are files (see
[Results and transcripts](#results-and-transcripts)).

### Migrations

`migrate` runs `CREATE TABLE IF NOT EXISTS` for every table on each open,
then `addColumn` for columns that older databases lack (so far
`agents.spawned_by`, `agents.cwd`, `runs.end_reason` and
`agent_events.end_reason`). `addColumn` checks `pragma_table_info` first,
tolerates another process adding the column between the check and the
`ALTER TABLE`, and says whether this call added it. A new column goes in
both the `CREATE TABLE` (new databases) and an `addColumn` call (old ones).
The open that adds `agent_events.end_reason` also rewrites the status
`exited`, which stores from before end reasons used, to `ended` in
`state`, `runs` and `agent_events`, once.

### Concurrency

Many short-lived processes write at once: every hook of every Claude session
on the machine is its own `agent hook`, and every hook of an agy agent its
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
didn't); `report --hook` and `agent hook` (every hook event of every
session) and `bus run` (a daemon) are not. `export --follow` is recorded once, when it ends (Ctrl-C
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
matches, and exits 3; `--new` files anyway. The box is per host; each
entry and vote has a `uid` so boxes can be merged later. Resolved entries,
via `friction export`, are the tool's changelog.

**Links across hosts** (`cli/friction_links.go`, `service/friction.go`).
The same problem filed on two hosts is linked with `friction link 4
34@monolith1`. Unlike a same-store `--dup`, both entries stay open and
count their own votes (each may say why it happened on its host).
Each host stores its own half in `friction_links`, so `show 34` on the
other host knows about `#4` without asking. A half keeps the other entry's
`uid` (a reset store's new `#34` isn't taken for the old one), whether the
other host was seen to hold its half (`confirmed_at`), and what was last
seen of the other entry (`peer_*`).

Everything that crosses hosts goes through the bus's `friction` op, and
the calling CLI drives it: the serving host only reads, records or removes
its half, and resolves its own entry. It never calls out. An entry closed
there as a duplicate stands for its original (the reply's `requested` says
so; links move to the original with `--dup`, and the caller's half follows
on its next check).

- `link` asks the other host to record its half, then records this one.
  If the other host doesn't answer (including a host whose bus predates
  the op), the half is recorded here unconfirmed and link prints what to
  run there; if it refuses (no such entry), nothing is recorded.
- `show`, `list` and `links` check links with one `get` per host, at most
  3s each, in parallel, and save what they see in the link. A host that
  doesn't answer shows its last-seen state. A link's state is computed,
  never stored: `out of step` (one end open, the other closed), `one-sided`
  (no link back), `not found` (no such id, or a different `uid`),
  `unreachable`, or `linked`. (Not "missing": that is a friction kind,
  and `show` prints the other entry's kind in parentheses, `#34@monolith1
  (bug) open, +2 there`, so neither reads as the other.) `export` uses only
  the saved state, with no bus.
- `resolve` (fixed or won't fix) closes the entry here, then sends
  `resolve` for each link, with the uid it knows and the same status and
  note (`resolved_by` is `name@host`). The other host closes its entry only
  if it is still open, the uid matches, and it (or the original it was
  closed into) links back to the request's `from`. `from` is required, so
  a mistyped id or a one-sided link closes nothing; the refusal says to
  link from that side first, and the caller prints it. The reply carries
  the entry's links, which the caller follows in turn, with a visited set,
  so a chain A-B-C resolves and a loop ends; each request's `from` is the
  entry whose link led to it (C is linked to B, not A). An entry here reached that way is closed locally.
  Hosts that don't answer are listed with what to run; `resolve ID
  --linked` later passes a closed entry's resolution on, walking the same
  way (already-closed entries are walked through, not changed). `--only`
  skips the walk. A resolve also updates the other host's half with this
  entry's new state.
- `resolve --dup` moves the duplicate's links to its original
  (`UpdateFriction`), dropping any the original already has.

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
  `internal/cli`; they skip without it. They use real sessions and a temp
  database. No test needs claude or agy installed.
- **A private tmux server per test binary.** Tests never touch the tmux
  server the environment names, which is where the real agents and the bus
  run. Each package whose tests can run tmux (`internal/cli`, `tests/tmux`,
  `tests/service`, `tests/backend`, `tests/natsbus`, `tests/watcher`) calls
  `tmuxtest.Run` from its `TestMain`: it makes a short socket directory
  (`gmx*` under `$TMPDIR`, or `/tmp` if that's too long for a socket path),
  unsets `TMUX` and `TMUX_PANE`, points `TMUX_TMPDIR` at it for the whole
  process, so every tmux the code and its children run uses that server,
  starts it with `exit-empty off` so it stays up with no sessions, as the
  shared server always has some (a test killing its only session otherwise
  raced the server's exit), and after the tests kills that server by its socket (`tmux -S`, with `TMUX`
  left out) and removes the directory. Why: tests sharing the real server
  collided on fixed session names when two runs overlapped (friction #9),
  and inside an agent's pane `$TMUX` names the real server *ahead of*
  `TMUX_TMPDIR`, so a `TMUX_TMPDIR=$dir tmux kill-server` meant for a test
  server killed every agent on the host, twice (#10). Unsetting `TMUX` is
  what makes `TMUX_TMPDIR` count. With a server per binary, fixed names like
  `rpc-cli-worker` can only meet within one binary, whose tests run one at a
  time and kill their sessions as they end.
- **The guard.** `internal/tmux` refuses to run tmux from a test binary
  (`testing.Testing()`) that isn't on its private server: `GOMUXAI_TEST_TMUX_TMPDIR`
  (set by `tmuxtest.Run`) is missing, `$TMUX` is set, or `TMUX_TMPDIR` was
  changed. The refusal names the fix, so a new package that forgets its
  `TestMain` fails at once instead of reaching the real server.
  `tmuxtest.Require(t)` makes the same check for a test that runs `tmux`
  itself (the setup helpers call it). `internal/tmux`'s own tests run a fake
  tmux, which declares itself the private server. A test binary killed
  mid-run (a `-timeout` panic) leaves its private server and `gmx*`
  directory behind; they touch nothing else, and `env -u TMUX
  TMUX_TMPDIR=<dir> tmux kill-server` ends one. By hand, always use
  `env -u TMUX` with a private `TMUX_TMPDIR` (made with `mktemp -d -t gmx`:
  a scratchpad path is too long for a socket) or `tmux -S <socket>`.
- **Waiting.** Tests wait for state, never sleep for it: `internal/await`
  polls a condition (a shell's printed marker, a screen, a status, a log
  line) and on timeout says what it waited for and what it last saw. A
  check that something did *not* happen waits for a later event that
  proves the moment has passed (a probe message from the same connection,
  a logged decision, a later line on the pane). The few `time.Sleep`s left
  in tests say why they aren't waits.
- **Opt-ins.** `GOMUXAI_KNOWN_BUGS=1` runs tests for known open bugs (the
  unsigned-replies one).
  `GOMUX_REAL_TRANSCRIPTS=<path>[:<path>...]` parses real transcripts on
  your machine as a smoke test.

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
- Long messages typed into a Claude TUI arrive whole, as pasted content
  (held messages arrive as hook output).
- A held message extends the turn it's delivered at the end of, so
  `_result` and `wait --next` cover its reply too.
- The input line is read off the screen: a Claude or agy redesign of it
  would leave held messages waiting ("it isn't on screen") until the reader
  is updated, and typing into Claude would press Enter without checking,
  as before. A person who starts typing between the check and the typing
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
- `runs` are never pruned (one row per spawn), nor are `session_reports`
  (one row per Claude session and subagent the global hook heard).
- Claude Code's session registry and hook payloads are its internal files,
  not a contract. A registry entry with a kind or status gomuxai doesn't
  know reads as `unknown` for that session until this package learns it;
  a file that can't be read at all stops the readers from calling any
  session gone, and status falls back to the reports, saying why. Pane
  adoption needs a proven entry, so it waits while Claude records no
  comparable start time.
- A Claude agent needs `agent hooks install` (spawn refuses without it). A
  Claude CLI that started before the install, or with the hooks removed,
  reports nothing and holds no messages until it is restarted; native
  status and pane adoption still see it.
- Pane adoption happens in a status check (`status`, `list`, `wait`, a
  `send`); `export` and `sessions` show the new session only after one.
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

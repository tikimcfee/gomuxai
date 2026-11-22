# gomuxai architecture

How the `agent` tool works, for people changing it. For using it, see
[`README.md`](../README.md) (people) and [`CLAUDE.md`](../CLAUDE.md)
(agents). For any command's flags and
behavior, `agent help <command>` is the reference: its examples are tested,
so it stays current where this file can drift.

## Overview

`agent` is two binaries installed side by side: the Rust CLI `agent`
(`rs/crates/agent`) and `agent-busd` (`busd/`, Go), the embedded `nats-server`
process that `agent bus run` starts as a child. Each agent is an AI CLI
(Claude Code, agy, aider, shell, or a custom one) running in its own
`g3d-termd` session (namespace `agents` by default). `agent` starts those
sessions, types into them, and records everything about them in one SQLite
database per data dir. Claude and agy agents report their own status through
lifecycle hooks: Claude Code runs `agent hook` for every session on the
machine (registered once by `agent hooks install` or `agent skill install`),
and each agy agent's hooks run `agent report --hook`; nothing has to scrape
their screens. Claude Code's own session registry says which Claude sessions
are running and where. An optional bus (one `agent-busd` + `agent bus run`
pair per host) lets `name@host` commands, fleet views, template starts, status
and focus events, and `agent wait` reach other machines.

Most `agent` commands are short-lived: open the database when needed, do one
thing, exit. The longer-lived processes are the agents' CLIs, `agent run`
(runs a script in its own memory-limited scope until it finishes), `agent watch`
(delivers queued messages), and `agent bus run` + `agent-busd` (the bus).

The data dir is `.gomuxai/` next to the `agent` binary
(`agent_base::config::default_config`), so a dev build in `bin/` has its own
(`bin/.gomuxai/`), separate from an installed one:

```
.gomuxai/
├── gomuxai.db               # agents, runs, agent_events, messages, tasks, state, session_reports, command_log, friction, friction_votes, friction_links, jobs, scripts, host_state
├── templates.json           # optional: agent templates this host agrees to start over the bus
├── scripts/<name>           # scripts promoted by `agent run --promote`
├── logs/
│   ├── <name>.log           # terminal scrollback snapshot (written on cleanup)
│   ├── <name>.transcripts   # "<backend>\t<path>" per CLI transcript (survives cleanup)
│   └── hook-errors.log      # what `agent hook` failed at, one line each
├── backups/                 # Claude Code's settings.json, copied before `agent hooks` edits it;
│                            #   gomuxai-<date>-<time>.db, the database as `agent manage backup` copied it
├── agents/<name>/           # per-agent bundle, removed by cleanup
│   ├── instructions.md      # the briefing
│   ├── launch.sh            # the backend's full launch command
│   ├── deliver.lock         # flock held by `agent deliver --wait <name>`
│   ├── deliver.log          # output of detached `agent deliver --wait <name>`
│   ├── AGENTS.md            # agy only: the briefing, where agy loads it
│   └── .agents/hooks.json   # agy only: the hooks bundle
├── bus/                     # bus identity, peers/, settings.json, ssh.json, bus.url, bus.log (plus busd.ready/links/err on macOS)
├── state/                   # created by Config::ensure_dirs, unused (state is in the DB)
└── watch.lock               # flock held by each running `agent watch`
```

## Crate map

The Rust workspace lives in `rs/` (`rs/Cargo.toml`), with the Go bus sidecar in
`busd/`. Crates depend only on crates below them in this list:

| Crate / dir | What it does |
|---|---|
| `rs/crates/agent` | `main.rs` (`cli::execute`, `cli::exit::exit_code`) and `cli/`: one module per command (or group), cobra-compatible command lookup and pflag-compatible flag parsing (`command.rs`, `flags.rs`, `help.rs`, `root.rs`), `Ctx`, remote routing (`remote.rs`, `remote_ops.rs`), output and exit codes (`output.rs`, `exit.rs`), and the command log (`history.rs`). |
| `rs/manager` (`agent-manager`) | The manager GUI (Makepad), a Cargo workspace of its own so Makepad stays out of this one's lock file, tests and lints. It links `agent-manage` only: it runs the `agent` binary beside it for its data (`agent manage view --json`) and for every action. |
| `rs/crates/agent-fleet` | Fleet features ([`docs/FLEET.md`](FLEET.md)): cross-host `fleet` view and baseline `--report` (`fleet.rs`), host `focus` and `--follow` (`focus.rs`), `templates.json` management (`templates.rs`) and `template` RPC handler (`template_op.rs`), handoff lookup, staleness, and re-brief (`handoff.rs`), heavy-job runner `run` with Linux systemd scopes (`run.rs`, `run/`), and `skill` installer/viewer (`skill.rs`). |
| `rs/crates/agent-service` | `AgentService` (`service/`): spawn, adopt, resume, revive, send, handover, submit checks, held-message delivery, capture, cleanup, status resolution, reports, hooks, permission/trust/dialog answers, shared `skill_text` and agent briefing (`instructions.rs`), friction link helpers, and `RpcHandler` (`rpc.rs`); plus `watcher` (`watcher.rs`, the `agent watch` loop). |
| `rs/crates/agent-export` | The tree `agent export` prints (`export.rs`): a snapshot across stores and hosts, kept current from agent events, sessions, and transcripts, opening stores read-only. |
| `rs/crates/agent-manage` | The manager's view as types and rules, with no store, bus or service code (`lib.rs`): `View`, `HostView`, `AgentRow`, `SessionRow`, `SessionState` and `classify`, `is_stale`, `FrictionRow`, `HistoryRow`, `DataReport`, `LlmUsage` (llm-exp's cached plan limits, `llm_usage.rs`), and the `human_*` formatters. What `agent manage view --json` prints, what op `manage` answers with, and what the GUI deserializes. |
| `rs/crates/agent-bus` | Bus client and RPC server behind plain blocking functions (`tokio`, `async-nats`, and `rustls` stay internal): contract (`contract.rs`: subjects, ops, wire types), identity and pinned TLS (`identity.rs`), local client and `RpcClient` (`client.rs`), `RpcServer` (`rpc.rs`), status and focus events and `wait` (`events.rs`), and `agent-busd` child management (`daemon.rs`). |
| `busd/` (`agent-busd`, Go) | Standalone Go module wrapping `nats-server/v2` and `nkeys` (`main.go`, `daemon.go`, `identity.go`, `contract.go`): runs the embedded NATS server for the local client port and TLS 1.3 SPKI-pinned hub/leaf links, started as a child by `agent bus run`. |
| `rs/crates/agent-backend` | Backend traits (`Backend`, `Provider`, and optional capability traits in `provider.rs`), `Registry` (`registry.rs`), custom JSON backends (`generic.rs`), recorded session-name lookup (`session_name.rs`), and `namedagents/` (`claude.rs`, `claude_hooks.rs`, `claude_settings.rs`, `claude_dialog.rs`, `agy.rs`, `aider.rs`, `shell.rs`, `trust.rs`, `hooks.rs`). |
| `rs/crates/agent-discovery` | `agent sessions` (`dirs.rs`, `index.rs`, `sessions.rs`, `resolve.rs`, `continue.rs`, `claude.rs`, `agy.rs`, `kimi.rs`, `lock_*.rs`): harness transcript directories, head/tail metadata scan, workgroup join, `continue` session picker, and `reconcile_native` / `resolve`. |
| `rs/crates/agent-transcript` | Readers for Claude Code, agy, and Kimi Code session transcripts (`claude.rs`, `claude_rewind.rs`, `agy.rs`, `kimi.rs`, `meta.rs`, `tools.rs`, `final.rs`, `transcript.rs`) into one event stream (`events.rs`, [`docs/EVENTS.md`](EVENTS.md)), and the resumable count of a transcript's token usage (`usage.rs`). |
| `rs/crates/agent-claudereg` | Reads Claude Code's session registry (`~/.claude/sessions/<pid>.json`; `registry.rs`), proves each entry against running processes (`proof.rs`, `proof_*.rs`), places terminal panes (`running.rs`), and follows `continued-in` chains (`continued.rs`). |
| `rs/crates/agent-termd` | `Ops` and `Client` (`ops.rs`, `client.rs`, `wire.rs`, `chord.rs`, `paths.rs`) over `g3d-termd`'s Unix socket protocol (v1.5), built on the `g3d` and `g3d-wire` crates, in a dedicated namespace (`agents` by default, refusing `default` and `wall`); `testing.rs` isolates tests in private socket dirs. |
| `rs/crates/agent-allowcmd` | `Checker` (`allowcmd.rs`): strict shell lexer deciding whether a command line is one plain run of a given binary with a safe subcommand, for `PreToolUse` approval. |
| `rs/crates/agent-store` | `Store` over bundled SQLite (`rusqlite`): schema, column migrations, transactions, and queries (`sqlite.rs`, `tree.rs`, `models.rs`, `commands.rs`, `friction.rs`, `friction_links.rs`, `session_reports.rs`, `host_rename.rs`, `jobs.rs`, `host_state.rs`, `manage.rs`, `gotime.rs`, `gojson.rs`). |
| `rs/crates/agent-base` | Shared bottom layer: `config` (`Config`, defaults, paths, namespace rules), `types` (`Status`, `EndReason`, `STATUSES`, `END_REASONS`, `HookReport`, `PermissionResponse`, `resumable`), `macos` (`Info.plist` detection and disclaimed spawn), `buildinfo` (git revision, dirty flag, `rustc` version, platform), `await_` (condition polling), `util` (shell quoting/splitting, ANSI stripping/sanitizing, `terminal_lines`, `similar`, `read_proc_tree`, `proc_ancestors`, `proc_args`, `write_file_atomic`), and `gostd` (path cleaning, OS error wording, `%q` quoting, duration formatting). |

## Why g3d-termd

Each agent runs in a `g3d-termd` terminal named `agent-<name>`
(`Config::session_prefix`) inside a dedicated daemon namespace (`agents` by
default, configured via `--termd-ns` or `GOMUXAI_TERMD_NS`, and refusing
`default` and `wall` so agent sessions never share a daemon with interactive or
wall terminals). `g3d-termd` gives:

1. **Process isolation.** An agent can crash without taking others down, and
   `kill` terminates the terminal's entire process tree.
2. **Persistent terminals.** Sessions survive disconnects, and a person can
   attach (`agent attach <name>`, which `exec`s `g3d --ns <ns> attach --take
   <session>`, detaching with `Ctrl-\` twice) to watch or steer any agent.
3. **Structured socket I/O.** `agent-termd` (`agent_termd::Ops`) speaks
   `g3d-termd`'s local Unix socket protocol (v1.5) directly over `g3d` and
   `g3d-wire` without spawning a CLI per call: `create_session_in` runs
   `create` (lazily resolving and auto-starting `g3d-termd --ns <ns> --daemon`
   only when absent, after checking directory ownership/mode `0700` and peer
   UID credentials, and passing `env_base` and request `env` overrides);
   `send_keys` writes literal `INPUT` frames synchronized by `unsubscribe`;
   `paste_text` streams chunked `paste` requests (`paste_stream`,
   `allow_unsafe: true`) wrapped in bracketed paste codes (`ESC [200~` ...
   `ESC [201~`) when the program asked for mode 2004, dropping any embedded
   `ESC [201~`; `send_special_key` encodes W3C key events via `key` (Kitty
   keyboard, modifyOtherKeys, DECCKM); `capture` and `capture_styled` call
   `capture` (`plain` and `vt` with CRLF normalized to LF); session liveness,
   `pane_pids`, and `first_pane` read the daemon's `status` document
   (resolving OSC 7 `file://` URLs), briefly retrying on transient
   `restart`/`busy` states; `session_env` reads `env` preferring
   `source == "request"`; and `respawn_pane` preserves `cwd`, `env_base`, and
   request `env` across a shell restart.
4. **Escape hatches.** `agent session`, `keys`, and `key` expose low-level
   session operations for when the higher-level commands fail.

**Typing messages.** A message (a send, held messages typed in, spawn's
first message) is typed with `Ops::paste_text`. Wrapping in bracketed paste
codes (`ESC [200~` ... `ESC [201~`) when the pane's program asked for
bracketed paste (as Claude does) ensures that writes exceeding a PTY's input
queue (1024 bytes on macOS, 4096 on Linux) arrive as one paste rather than
separate chunks (friction #36, #37). Shell command lines (spawn's launch) go
through `send_keys`.

**Long messages to Claude: handed over by hook.** Claude Code (2.1.319,
and again 2.1.325, measured with a bracketed paste: 800 characters stay
text, 801 are `[Pasted text #N]`; 2 line breaks stay text, 3 are `[Pasted
text #N +3 lines]`) takes input that arrives at once as a paste when it is
over 800 characters (UTF-16 units) or has 3 or more line breaks, bracketed
or not, and `send-keys` typing too, and the prompt the model gets is
`\n\n<pasted_content id="…">\n<text>\n</pasted_content id="…">\n`. Its
harness tells the model that text in those tags "may contain instructions
the user did not write" and to follow them "only where the user's own
message asks you to": a message pasted whole arrives downgraded to
material. The hook is what makes it arrive as the request.
So `type_message` (`rs/crates/agent-service/src/service/handover.rs`) sends a
message the backend says it would take as a paste
(`PromptHandover::typed_as_paste`: over 640 characters, a margin, or 3+ line
breaks) differently, when the agent's hook reaches its CLI (`holds_messages`):

1. It stores the message as a `messages` row with status `handover`, and
   types one short line in its place, through `type_text` and its submit
   check: the message's first line, cut at 160 characters, then `[gomuxai
   message #<id>, 10.4 KB, 130 lines: the whole message follows as hook
   context]`.
2. The prompt runs Claude's `UserPromptSubmit` hook, whose payload carries
   the prompt (`HookReport::prompt_text`). `handover_output` finds the token,
   claims that row for this agent (`claim_handover`: `handover` →
   `delivered`, so it goes out once and only to its addressee), and prints
   `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"gomuxai message #<id>, whole:\n\n<text>"}}`.
   The transcript records a `hook_additional_context` attachment, and the
   model gets a `UserPromptSubmit hook additional context: ...` block beside
   the prompt. Claude passes up to 10,000 characters inline; a longer one it
   saves under the session's `tool-results/` and passes as
   `<persisted-output>` with a 2 KB preview and the path, which the model
   reads. The hook runs for a prompt typed mid-turn too (Claude takes it in
   at a tool boundary), so `--now` works the same.
3. `type_handover` waits up to 3 `submit_wait`s (9s) for the row to leave
   `handover`. If the hook never took it, it marks the row `failed` (a late
   hook then hands over nothing) and returns `HandoverError`: the agent got
   only the line, and the error says the hook isn't reaching the agent's
   session. That is the failure friction #49 and #50 hit: Claude Code ran
   the agent's conversation in a background session the hook didn't place
   (see "Sessions Claude moves into the background"); with the session
   placed, a 4 KB single-line message and a 39-line one arrived whole,
   idle and mid-turn, held and `--now`, and the hook took each in well
   under a second. `type_held` doesn't hold such messages again, or the
   deliverer would type the line over and over. A line that wasn't
   submitted (`UnsubmittedError`) or couldn't be checked keeps its row:
   whoever submits it gets the message.

Spawn's first message (`type_first_message`) goes the same way, once the
hook has reported the session (up to 15s). The turn starts and ends as for
any typed message, so `wait --next` and `_result` cover the reply, and over
the bus the receiving host does all of it. agy needs none of this: it
records a long multi-line typed message as the request itself
(`<USER_REQUEST>...` in its transcript).

The cost is that input is typed, not delivered: a message is "sent" once its
keys are in the terminal, whether or not the CLI read it, and anything typed
at a dialog answers the dialog. `send` and the watcher guard against that
(see [Messages](#messages)).

## Spawning an agent

`AgentService::spawn` (`rs/crates/agent-service/src/service/agent_service.rs`):

1. Validates the name (`^[A-Za-z0-9][A-Za-z0-9_-]*$`): it becomes a
   `g3d-termd` session name, a path component, and a bus subject token.
2. Refuses if the agent record or the session already exists (and
   says `agent resume <name>` when that agent has ended).
3. Creates the session with `AGENT_NAME`, `AGENT_HOST`, `GOMUXAI_TERMD_NS`,
   and `PATH` prefixed with the binary's directory, removing any stale
   `logs/<name>.log` snapshot from a previous run.
4. Stores the agent record, with `spawned_by` the calling agent (see
   [Who sends](#who-sends); "" for a person) and the spawn's cwd, and in the same
   transaction opens a run and records a `spawned` agent event (see
   [Export](#export)). The record exists before the CLI starts, because
   hooks report against it immediately. When `spawn --handoff <file>` is
   given (`rs/crates/agent/src/cli/spawn.rs`), its absolute path is stored as
   the agent's `handoff` state key for `agent revive`.
5. Builds the launch command from the backend's capabilities (below), writes
   it to `agents/<name>/launch.sh`, and types only `sh <path>` into the
   session. Typed input sits in the terminal's canonical-mode buffer until
   the shell's line editor is up, and macOS caps that at 1024 bytes
   (`MAX_CANON`), which a launch command with its flags and pre-allowed
   command rules can pass.
6. Waits `startup_delay` (2s), confirms the workspace trust prompt if
   `--trust-workspace`, and types the first message unless it went on the
   command line. The first message is the `--task` content, or the backend's
   "read this task file" line for a task file. Where the CLI's input box
   can be read (Claude), spawn first waits for it, or a dialog in its
   place, as `send` does (`await_input_box`). If the trust prompt is up
   instead, the first message is held (a pending message, as for a busy
   agent) rather than typed: Claude's dialog drops a paste, and an Enter
   that reached it would pick its highlighted "No, exit". Approving the
   prompt starts the deliverer, which types it once the CLI is at its
   prompt. Another dialog on screen (`DialogReader`, see [Dialogs no hook
   reports](#dialogs-no-hook-reports)) holds it the same way, and spawn
   starts the deliverer at once (`hold_first_message`).

For a built-in backend, any failure after the session exists kills it and
deletes the record and the agent dir.

Built-in backends implement `agent_backend::Provider` plus whichever optional
capability traits (`rs/crates/agent-backend/src/provider.rs`, queried via
`Provider::as_<trait>`) they support; `launch_command` applies each one it
finds:

| Trait | claude | agy | aider |
|---|---|---|---|
| `HookProvider`: maps hook payloads to status | yes | yes | — |
| `GlobalHooker`: hooks from the CLI's user settings, for every session | `--session-id <id>`; spawn fails unless `agent hooks install` has registered this binary's hook | — | — |
| `SpawnHooker`: hooks wired into each spawn's launch command | — | `--add-dir <agent dir>` (or a symlink to it, below) with `.agents/hooks.json`, plus the exit wrapper | — |
| `InstructionsProvider`: the briefing; an unreadable one fails the spawn | `--append-system-prompt '<instructions.md>'`, inline: Claude Code's `/restart` refuses a session launched with `--append-system-prompt-file` (or any of `CLAUDE_RESTART_BLOCKING_FLAGS`) | `AGENTS.md` in the `--add-dir` directory | — |
| `CommandAllower`: pre-allowed commands | `--allowedTools 'Bash(agent status:*)' ...` | — | — |
| `PromptFlagProvider`: first message on the command line | — | `-i '<task>'` | — |
| `PromptHandover`: a long message typed as a short line, its text handed over by `UserPromptSubmit` | yes | — | — |
| `ModelChooser`: `spawn --model` | `--model '<model>'` | `--model '<model>'`, ahead of the exit wrapper | `--model '<model>'` |
| `SessionNamer`: names the CLI session after the agent | `--name '<agent>'` | — | — |
| `TrustPrompter`: reads and answers the trust prompt | moves to "Yes, I trust this folder" first (the default is "No, exit") | Enter ("Yes" is the default) | — |
| `NativeStatusProvider` | Claude Code's session registry | — | — |
| `PaneSessionProvider`: the session a pane runs | the registry, placed by the process tree | — | — |
| `PermissionResponder` | yes | yes | — |
| `TranscriptProvider` | `agent_transcript::Claude` | `agent_transcript::Agy` | — |
| `HookDeliverer`: held messages from a hook's output | `Stop` → `additionalContext` | `PostInvocation` → `injectSteps` + `force_continue` | — |
| `HookAllower`: approves a safe `agent` command from a hook | `PreToolUse` → `permissionDecision: allow` | — | — |
| `InputReader`: the input line, so typing never lands in a draft | after `❯` | after `>` | — |
| `SubmitChecker`: a typed message's Enter can arrive as a newline, so typing checks it submitted | yes | — | — |
| `TurnEndReader`: a turn that ended with no hook | — (native status) | the footer under the input box | — |
| `QuestionPrompter`: a question dialog with no hook | — (`PermissionRequest`) | `ask_question`'s `Question 1/N:` | — |
| `DialogReader`: any other dialog with no hook (offers, notices, pickers), and its keys | a footer of key hints under a border, at the bottom or right above the input box; Enter / Escape | — | — |

Notes on each:

- **Hooks** are used only when `Config::agent_binary` is set (it defaults to
  the running executable). Nothing is written into the agent's project.
  - **Claude** (`GlobalHooker`): the hooks are Claude Code's user settings'
    entry for every session, which `agent hooks install` writes (see [The
    global hook](#the-global-hook-agent-hook)). Spawn checks that every
    event in `CLAUDE_HOOK_EVENTS` runs this binary's `hook`, and fails with
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
- **Claude's launch command** carries `--settings
  '{"skillOverrides":{"auto-mode-setup":"off"}}'` (`CLAUDE_SPAWN_SETTINGS`),
  layered over the person's settings for that session only (hooks and the
  rest still come from theirs). It turns off `/auto-mode-setup` in the
  agent's session, and with it the offer Claude Code (v2.1.319) shows when
  a turn ends in auto mode once a person has had 5 classifier denials and
  5 startups ("Teach auto mode about your environment?", default "Yes").
  The offer fires no hook and the registry says `idle`, so a message
  typed then went into it, and its Enter started the setup wizard, which
  scans shell history and other repos (friction #29). Claude skips the
  offer when the command is off (`skillOverrides["auto-mode-setup"] ===
  "off"`, read in its eligibility check). It also passes `--name '<agent>'`
  (`SessionNamer`), which survives `/restart`.
- **agy's launch command** is prefixed with
  `env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY`: with those set (a daemon
  started over SSH passes them on) agy skips the OS keyring and comes
  up signed out.
- **The briefing** (`rs/crates/agent-service/src/service/instructions.rs`)
  combines a per-agent header (its name, host, who started it, how held
  messages arrive, and `{name}-<role>` helper naming) with the shared
  `skill_text` that `agent skill` also installs: how to message, wait on, and
  read other agents, fleet commands, and how to file friction. It uses the
  binary's full path, since the agent's shell may not have it on `PATH`.
- **Pre-allowed commands** (`AGENT_SAFE_COMMANDS`): `send`, `deliver`, `list`,
  `status`, `statuses`, `wait`, `events`, `capture`, `logs`, `sessions`,
  `transcript`, `state get`, `history`, `friction add`, `friction +1`,
  `friction list`, `friction show`, `friction link`, `friction links`. Each becomes two rules, `agent <cmd>` and
  `<full binary path> <cmd>`, for that Claude session only. Left out on
  purpose: `approve`, `deny`, `keys`, and `send --force` (one agent answering
  another's prompts), `spawn` and `cleanup` (a person approves each), and
  `friction accept`/`resolve` (for the people working on the tool) and
  `friction unlink` (it erases a link on both hosts). Rules
  match the literal command text, so `./bin/agent send` or `agent -q send`
  would still prompt; the PreToolUse hook covers those (next item).
- **The PreToolUse approval** (`AgentService::hook_approval`,
  `agent_allowcmd::Checker` in `rs/crates/agent-allowcmd/src/allowcmd.rs`):
  for a Bash call, the hook prints Claude's
  `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow",...}}`
  when the command is exactly one run of this binary with a subcommand in
  `AGENT_SAFE_COMMANDS` (the same list the rules come from), and nothing
  otherwise, so the normal permission flow decides. Instead of a full shell
  parser, `agent-allowcmd` uses a narrow hand-written lexer (`literal_words`)
  that accepts only shapes it understands completely and fails closed on
  anything else. The program may be the bare name (resolved on the hook's
  `PATH` against the payload's `cwd`, expanding leading `~` as bash does and
  refusing bash/zsh builtins and keywords in `SHELL_WORDS`), an absolute
  path, or a path relative to `cwd`; it must resolve, symlinks followed, to
  `Config::agent_binary`. Only `-q`/`--quiet` may come before the subcommand,
  and root persistent flags (`--backend`, `--backend-config`, `--termd-ns`)
  are refused after the subcommand too. Refused: more than one command,
  pipes, `&&`/`||`, `&`, `!`, subshells and compound commands, assignments
  (`A=1 agent ...`), `env` and other wrappers, any `$` expansion or
  substitution (so `$A send` still prompts: the hook can't see the shell's
  variables), globs, braces, tildes, unquoted backslashes (except plain
  `\\\n` line continuations between words), control characters other than
  `\n`/`\t` inside quotes, comments (zsh without `interactive_comments` reads
  `#` as text), and every redirection except `>/dev/null`, `2>/dev/null`,
  `&>/dev/null`, `>>/dev/null`, `2>>/dev/null`, `2>&1`, and `>&2` (`&>>` is
  refused because macOS `/bin/bash` 3.2 splits `&>>` into `&` + `>>`).
  Unquoted text is limited to `[A-Za-z0-9_@%+=:,./-]` with no leading `=`
  (even after an empty quote `""=cmd`, defending zsh's `=cmd`), so a word
  means the same in bash and zsh; anything else must be quoted. It never
  denies. Checked live (Claude Code 2.1.310): the allow skips the prompt in
  default mode, and a settings `deny` rule still wins over it. The decision
  is made before the status report is recorded, so a failed report doesn't
  lose it; an unknown agent or stale spawn id gets nothing.
- **Trust prompts** are answered at spawn only with `--trust-workspace`,
  since trusting a repo also enables its own hooks and settings. Spawn polls
  the screen up to 8 times, 500ms apart, and goes on if no prompt shows.
  Nothing is trusted ahead of the CLI: spawn answers the prompt the CLI
  draws, which names the folder as the CLI's process sees it, symlinks
  resolved (macOS's `/var/tmp/x` is `/private/var/tmp/x`), not necessarily
  the path spawn was given. After an answer it watches 3 polls more and
  answers another prompt too (3 in all) when its folder is the agent's
  under any name (`same_folder`, comparing resolved paths), as a CLI may ask
  about each name in turn; a prompt about another folder is left for
  `agent approve`.
  Without it, the agent is `waiting` at the prompt (see [Resolution](#resolution-native-report-scrape)),
  spawn says so, and `agent approve` answers it. Both CLIs draw the same
  dialog (`rs/crates/agent-backend/src/namedagents/trust.rs`): an
  `Accessing workspace:` header over the folder (wrapped at the screen's
  width), a question (Claude's `Quick safety check`, agy's `Do you trust the
  contents of this project?`), and the options `No, exit` and `Yes, I trust
  this folder` among the last six non-blank lines, the highlighted one marked
  (`❯` Claude, `>` agy). All of that must be there, options at the bottom, so
  text quoting the dialog higher up (a transcript, a file) isn't taken for
  it. The keys move from the highlighted option to the wanted one (Claude
  lists No first, agy Yes first); with neither marked, there are no keys
  rather than a guess.
- **Custom JSON backends** (`--backend-config`) have no provider: their
  launch command is typed as is, with no launch script, hooks, briefing, or
  dialog handling. The registry knows them only from the file, so it must be
  passed again to `status`, `wait`, and `watch`.

### spawned_by and cleanup

`spawned_by` builds the agent tree that `list` shows (helpers after their
parent) and that `cleanup` enforces (`rs/crates/agent/src/cli/cleanup.rs`). A
person may clean up anything. An agent may clean up only its descendants,
never itself (its own session would die partway), and a helper's own helpers
must be named in the same command, since once the helper's record is gone
nothing records whose they are.

An agent spawned over the bus (`spawn name@host`) gets `spawned_by`
`<caller>@<caller host>` on that host, or "" when a person spawned it.
Such a parent names no local agent, so its helpers nest under nothing in
`list` there, and only a person on that host can clean them up: `cleanup`
doesn't cross hosts, and no agent there is their ancestor. The hub doesn't
verify the name; it is the caller's claim, like a `[from name@host]` tag.
It must carry a host: the bus refuses a bare `spawned_by`, which would make
the new agent a helper of the serving host's agent of that name.

`cleanup --ended` picks the names itself, from every agent's
`status_info` (the resolved status, so an agent whose session is gone
counts though its last report said idle), all read before anything is
removed. The same rules then apply, with one difference: where named
cleanup refuses a helper whose own helpers would be left behind, `--ended`
skips it, since those helpers haven't ended and aren't its to remove. A
person gets every ended agent, with a note naming the helpers that stay.
Ended agents outside the caller's tree are counted, not listed. `--dry-run`
stops after the choice, printing each agent's end, what `cleanup` would do
to its session (kill it, leave an adopted one running, or nothing if it's
gone) and `StatusInfo::held`.

`AgentService::cleanup` types the backend's exit command (`/exit`) into an
idle agent and waits 1.5s, so the CLI exits cleanly and its hooks record it.
Then it snapshots terminal scrollback to `logs/<name>.log`, kills the
session, deletes the agent's messages and agent dir, and deletes the record;
state rows go with it through the foreign key's `ON DELETE CASCADE`. The same
transaction closes the agent's run (with its last `_status`, `_end_reason` and
`_transcript`) and records a `removed` agent event, so the run stays readable
(`export --all`). The terminal log and the transcripts list stay.


### Resuming an ended agent

`agent resume <name>` (`AgentService::resume`,
`rs/crates/agent-service/src/service/resume.rs`) starts an ended claude or
agy agent again on its last session, under its own record (`agent revive`
builds on this to re-brief from its handoff doc or take over a CLI restarted
in its pane; see [Fleet features](#fleet-features)). The CLI (`resume_agent`
in `rs/crates/agent/src/cli/resume.rs`, which the bus's `resume` op runs too)
finds that session as `agent transcript <name>` does (`resume_session`),
checks it can be continued (`check_resumable`, as `spawn --resume`), and
passes the directory it ran in as `ResumeOptions::cwd`, where the session
starts. A session that recorded no directory takes the agent record's `cwd`
(where its session started): agy ties a conversation to its workspace in
`history.jsonl` only in some lines (its prompts' carry no conversation id),
so a conversation can have none recorded yet.
It doesn't `chdir`: the bus serves requests concurrently in one process.
The service then:

1. Refuses an adopted agent (`adopted_resume_error`), and refuses unless
   the agent's resolved status is `ended`; then kills a leftover
   session (a CLI that quit leaves its pane's shell).
2. Writes `_status` `ended` and its end reason, so its run closes with how
   it really ended (a reboot fires no hook).
3. Spawns with `resume` set and the old record to replace: a new
   session, then `Store::replace_agent`, which in one transaction closes
   the old run (a `removed` event), rewrites the record in place, and opens
   a new run (a `spawned` event). The record is updated, not deleted:
   deleting it would cascade to the agent's state.
4. Deletes the reserved keys that described the ended run, keeping
   `_result`, `_transcript` and `_model` (the model `spawn --model` chose,
   which the relaunch passes again); the agent's own keys stay. The launch then
   writes a new `_spawn_id` and `_session_id`.
5. Starts a deliverer if messages were held for it: they never went out
   while it was ended.

Its name, backend and parent stay, so its helpers stay its helpers. A
failed resume leaves the agent ended (its record and files are kept).

An adopted agent (`agent adopt`) runs in a session the person made,
so resume refuses it: gomuxai doesn't type a launch into their shell, and
doesn't create an `agent-<name>` session to replace theirs. The error says
to start the CLI there (`claude -c` continues its last conversation), which
pane adoption then tracks; if their session is gone, to clean the agent up
and `spawn --continue` (or `--resume`) it in a session of gomuxai's own.
The CLI refuses before it looks for the session or enters its directory.
`spawn` therefore never runs in an existing session: `adopt` records the
agent itself (`store_agent` with `existing_session`).

`agent resume --all` (`run_resume_all`,
`rs/crates/agent/src/cli/resume_all.rs`) reads every agent's resolved status
first, so each is judged as things stood before any resume. It picks the ones
`ended (gone)` for which `resume_refusal` has no objection (claude or agy,
not adopted, a session from `last_session_id`); every other ended agent is
listed as skipped, with its end reason (ended on purpose) or the refusal.
Inside an agent only its descendants count (`Teams::ancestors`), as for
`resume <name>`. The picked agents are sorted by depth in the agent tree,
then name, and resumed one depth at a time, so a helper comes back to a
running parent: within a depth up to `RESUME_ALL_PARALLEL` (4) run at once,
each through `resume_agent`, with launches `RESUME_ALL_SPACING` (500ms)
apart. Each resume spends most of its time in `startup_delay` waiting for its
CLI, so overlapping them saves most of that, while the spacing keeps CLIs
from starting in the same instant (Claude Code rewrites its shared
`~/.claude.json` as it starts). Resumes share nothing but the store (SQLite,
with a busy timeout) and the `g3d-termd` daemon, and resume never `chdir`s.
Lines print in the sorted order as each finishes, so output is stable however
the resumes interleave. `--dry-run` runs `resume_session` (the session lookup
and `check_resumable`) for each pick and starts nothing.

### Continue: picking the session

`spawn --continue` (`continue_session` in `rs/crates/agent/src/cli/spawn.rs`,
`agent_discovery::r#continue`) picks the session the CLI's own `--continue`
would, then goes through `spawn --resume`'s path with its id, so the record
knows the session and a running one is refused (`check_resumable`). Passing
`-c` to the CLI instead would leave the agent not knowing which session it is
on. Claude's `-c` is "the most recent conversation in the current directory":
the session whose recorded cwd is the directory (Claude keeps a project's
sessions in one folder named for it) with the newest transcript. agy's is the
most recent conversation in its workspace, falling back (agy's changelog) to
one in a parent or child directory. Subagent sessions are never picked. The
directory is `--cwd`, or spawn's; the agent starts where the picked session
ran, as with `--resume`.

## Messages

`agent send` (`AgentService::send`, `rs/crates/agent-service/src/service/held.rs`)
types the message and Enter, or holds it (below). A message
from an agent (see [Who sends](#who-sends)) is prefixed `[from <name>@<host>] `, and
one from no agent `[from a person on <host>] `, or `[from a Claude
session on <host>] ` when the environment shows an AI CLI (`AI_AGENT`'s tool
name, the part before its first `_`, or `CLAUDECODE=1`; `unrecorded_sender` in
`rs/crates/agent/src/cli/send.rs`). Those name no agent to reply to. A slash
command stays unmarked from any sender, or it would no longer be one
(`mark_sender`): `agent_service::is_slash_command` takes `/name` followed by a
space or the end (`/model opus`, `/plugin:skill args`), and not a path
(`/tmp/out.txt is ready`, which has a second slash first). Who sent it is then
only in the sending host's command log (`agent history`). Claude's `/model`
also saves the model as the default for every new session in the user's
settings, so `spawn --model` is how to run one agent on another model. Before
typing, `check_can_send` refuses an `ended` agent (the text would run in the
shell left behind), a `waiting` agent, and any agent with a permission or
trust dialog on screen (the Enter would answer it; at Claude's trust
prompt it would pick "No, exit"). `--force` skips the
checks, and only works for a person at a terminal: it is refused when
`AGENT_NAME` is set or stdin isn't a TTY. Right before typing,
`send_unchecked` records `_sent_at` (spawn does too, for the first message),
for `wait --next`.

### Who sends

The sender (the `[from ...]` tag, a held message's `sender_id`, spawn's
`spawned_by`, the command log, and the rules on what an agent may clean up,
resume, or revive) is the agent the processes show, not `AGENT_NAME`
(`AgentService::caller_agent` in `rs/crates/agent-service/src/service/caller.rs`,
`Ctx::caller_agent` in `rs/crates/agent/src/cli/root.rs`). `AGENT_NAME` and
`AGENT_HOST` are set in an agent's session, and every process started from it
inherits them: a shell a tool opened, a terminal daemon it started, and
Claude Code's background-session daemon, one per user (per config dir),
started with the environment of whichever CLI first needs it. Claude Code
2.1.325 hands a session sent to the background (`/bg`) to a spare host
that daemon started ahead of time, so agent A's conversation ran its tools
with agent B's `AGENT_NAME`, and A's messages went out as `[from B@host]`
(friction #48). Walking up from the command's process:

- an agent's pane (its session's root process): that agent;
- a Claude Code background session's process (registry `"kind": "bg"`):
  the agent whose conversation it carries on (see [Sessions Claude moves
  into the background](#sessions-claude-moves-into-the-background)), or no
  agent, never the one whose environment it inherited;
- any other Claude Code CLI on the way (a `claude -p` a tool ran) is
  passed over: its pane's agent ran it.

Only where the walk reaches neither (a process detached from its session,
or under a pane this store records no agent for, such as another agent
binary's) does `AGENT_NAME` count, as before. A person typing at a terminal
(stdin a TTY, no `AGENT_NAME`) in an adopted agent's pane is a person.
`send --force`'s refusal still reads `AGENT_NAME`: anything that has it is
refused.

### Held messages

Typing into a busy Claude agent lands in its input box, mixed with
whatever a person is drafting there, and nothing says when it was read
(agy queues typed text for the turn's end, but typing into its
`ask_question` dialog answers it). So
for an agent that *holds messages*, `send` without `--now` does this
(`rs/crates/agent-service/src/service/held.rs`). An agent holds messages when
its backend is a `HookDeliverer` and a hook is known to reach its CLI
(`holds_messages` / `holds_messages_named`): spawn wired its hooks
(`_spawn_id` is set: agy), or the global hook has reported the agent's current
session (`_hooked_session` equals `_session_id`: Claude). A Claude CLI started
without the hook installed, say by hand in the agent's pane, never sets
`_hooked_session`, so messages to it are typed, not held for a `Stop` that
would never hand them over.

| Status `send` finds | What it does |
|---|---|
| `idle`, `error`, `unknown` | `type_held`: claims any messages still held, and types them and this one as one prompt, oldest first (after the dialog check) |
| `processing`, `waiting` | `hold`: stores a `pending` row in `messages`, records `_sent_at`, re-checks (below), and starts a deliverer |
| `waiting` at a dialog no hook reports (tool `dialog`) | `hold` too, `held_until` saying the dialog: no turn runs, so only the deliverer types it, once the dialog is gone |
| `ended` | refuses |

Slash commands are never held: a hook can only hand over text. Who
delivers a held message:

1. **The delivering hook.** The hook (`agent hook` for Claude, `report
   --hook` for agy) calls `handle_hook`, and for an event
   the backend `can_deliver` (Claude: `Stop`, with or without background
   tasks; agy: `PostInvocation`, below), `report_delivering` claims every pending row and writes the
   status report in one transaction. With messages claimed, the report is
   `processing`, event `delivered`, detail `N held messages delivered`,
   and it clears `_result` and sets `_turn_at` (the turn goes
   on, so `wait --next` waits for its real end and `_result` isn't the
   interim reply). The hook prints `delivery_output`: for Claude,
   `{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":
   "<N> messages arrived while you were working, oldest first:\n\n..."}}`.
   Claude continues the turn with it and shows it as `Stop hook feedback:`
   (a `{"decision":"block"}` works too but shows as `Stop hook error`). With
   nothing held, the report and output are as before: idle, no output.
   `report_hook` (status only) never claims.

   agy delivers from `PostInvocation`, which fires after each model call
   and its tools, including the turn's last, and prints
   `{"injectSteps":[{"userMessage":"[from lead@hub] ..."},...],"terminationBehavior":"force_continue"}`:
   one `userMessage` per message, each shown in the pane as a `> [from ...]`
   prompt of its own, which the model answers in the same turn
   (`exploration/agy-delivery-findings.md` compares the alternatives: Stop
   `continue` and the other step types work but show nothing). The output
   is serialized from a struct of exactly those two fields,
   since agy parses it strictly and drops a bad one silently. A message
   over 64 KB is split into labeled parts (`[part 2/3 of the message
   above]`), since agy cuts a step at ~192 KB without a word. Its
   `HookReport` has `delivery_only: true`: with nothing held, the hook prints
   nothing and records nothing (it says nothing `Stop` won't). A failed or
   `USER_CANCELED` invocation is left to `Stop`: `force_continue` would
   override the cancel. agy's other hooks never print.
2. **`hold`'s re-check.** The turn may have ended between `send`'s status
   check and the insert, its Stop finding nothing. The claim is the
   transaction's first statement, so it takes SQLite's write lock at once:
   a row inserted concurrently is either claimed by the hook, or committed
   after it, and then the hook's report is visible to `hold`'s re-read.
   If the last report says the turn is over (`turn_over`: `idle`, `error`,
   or a `Stop` that left background work running, while native status
   still says busy), `hold` types the held messages at once. The same
   path covers native status lagging a `Stop`, and a message sent during
   background work (the prompt takes input while the work runs).
3. **The deliverer.** A turn can end with no hook at all: observed on
   v2.1.310, Esc during a tool call or at a dialog, and a deny, fire
   nothing (no `Stop`, no `PostToolUseFailure`, and no `idle_prompt`
   Notification later either, though that follows a normal `Stop` after
   60s). Only native status sees the agent back at its prompt. So `hold`
   starts `agent deliver --wait <name>` (`start_deliverer`), detached with
   `setsid` so it outlives the sender's Bash tool call, logging to
   `agents/<name>/deliver.log`. It holds `agents/<name>/deliver.lock`
   (`flock`; one that finds it taken exits at once, and the holder looks
   again after releasing it, so a message held in between isn't missed).
   `deliver_held` checks every 2s; once the agent has been at its prompt
   (`idle`/`error`, or the background-work `Stop`) with an empty input
   line for 10s, it calls `type_held`. The grace gives a person who pressed
   Esc time to type first, and restarts whenever the input line has text.
   It exits when nothing is held, or the agent's session has ended, or the
   agent is gone. A
   `StopFailure` (rate limit) is delivered the same way. agy fires no hook
   on Esc or Ctrl+C either, and has no native status: its status turns
   `idle` from the screen (`TurnEndReader`, see Status resolution), and the
   deliverer goes on from there.
4. **The next `send`** that finds the agent at its prompt types the held
   messages ahead of its own; `agent deliver <name>` does it by hand.

**The input line.** `type_held` (every non-`--now` typing into an agent
that holds messages) first reads the input line off a styled capture
(`Ops::capture_styled`) through the backend's `InputReader` (`input_text`).
Claude's is the text between the last two `─` rules, after
the `❯` prompt (a named session, from `--name` or `/rename`, has its name
set into the top rule: `──── name ─` counts as a rule), with text drawn
faint left out: dim (SGR 2: the `Try "..."` placeholder and suggestions) or
in a grey (bright black, 90; a grey of the 256-color palette, 8 or
232-250; an RGB grey: agy's mode hint, `Auto mode: edits & safe commands
auto-approved (shift+tab to cycle)`, which agy 1.5.x shows in an empty
line in its auto, plan and accept-edits modes, and which, read as typed
text, held every message to an idle agy agent, friction #45 and #47). The style marks a placeholder,
whatever a release words it; typed text is drawn in the default
foreground. agy's is the same box after its `>` prompt. After
Esc, agy moves text a person queued during the turn back into it. If it has text in it, or isn't on screen, typing
would land in a person's draft, so `type_held` returns `InputBlockedError`:
`send` holds the message instead (`Held for X until its input line is
empty`), the deliverer keeps waiting, and `status -v` shows `held: N,
waiting for an empty input line (it has text in it)`. A line that isn't on
screen says something covers it, which `agent capture` shows: a dialog or
notice no status reads.
5. **`agent watch`**, if running, delivers pending rows as before.

Every deliverer claims rows before delivering (`claim_pending_messages`, or
`claim_message` per row for the watcher), so each message is delivered once;
`type_held` puts its rows back to `pending` if typing fails. Messages
held for an agent whose session ends stay `pending` (`status -v` says they won't
be delivered) until `cleanup` deletes them. `StatusInfo::held` counts the
pending rows, shown by `status -v`, `--json`, the orientation, and the
`status` RPC.

**Submitting a typed message.** Typing a message and pressing Enter are two
writes to the pane (the paste, then `send_special_key(..., "Enter")`), and Claude
handles whatever has arrived by the time it reads as one input. While its
event loop is busy (a long render, a paste it is still collecting), the
text and the Enter arrive in the same read; Claude takes that for a paste,
the Enter becomes a newline in it, and the message sits in the input box
until someone presses Enter. The first Enter after that is often eaten too.
So `send_unchecked`, for a backend that is a `SubmitChecker` (Claude), types
through `type_message` (`rs/crates/agent-service/src/service/submit.rs`):

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

If the box never shows the text in step 1, something else took the keys:
a dialog that came up after the sender's screen check (see [Dialogs no
hook reports](#dialogs-no-hook-reports)), whose highlighted option the
Enter would pick. If the screen shows one, no Enter is pressed and it
returns `DialogTookError` (naming the dialog and how to close it;
`type_held` holds the messages again). If it shows none, it presses Enter
as above, but if the box still never showed the message it doesn't call it
sent: `unchecked` says `its input box never showed the message`, as for a
box it couldn't read (below).

It presses no further Enter once the box is off screen (a dialog may have
replaced it) or shows anything other than what was typed (a person typing).
A box it can't read before typing (not on screen, or the capture failed)
gets type-then-Enter without the checks, and says so: `SendResult::unchecked`
(`unchecked` on the bus) names why, `send` prints `Typed into X without
checking it was submitted: <why>`, the deliverer logs it, and `_unchecked`
records it, so `status` prints an `unchecked:` line (`unchecked`,
`unchecked_at` in `--json`) until a turn starts after it.
`Config::submit_wait` (`submit_wait`, default 3s) bounds each wait; 0 turns
the checks off. A responsive Claude takes about 0.3s per message.

`type_held` leaves messages that were typed but not submitted claimed (they
sit in the box) rather than holding them again. While the box still shows
`_unsubmitted`, `status` prints an `unsent:` line (`unsubmitted_at` in
`--json`), and the input-line check says `a message typed into it wasn't
submitted: agent key <session> Enter submits it` instead of `it has text in
it`, so `send` and `status -v` name the cause. `send --now` types after it
and submits both.

Remote sends get this on the receiving host: `SendRequest.now` (ignored by
older hosts, which always type) and an optional `SendResponse` (older hosts
send none, and the CLI says just `Sent`); `SendResponse.held_until` is the
backend's `delivery_point` (`its turn ends`, `its current step ends`), for
the `Held for X until ...` line. Backends without `HookDeliverer` keep
typing.

### Queue

`agent queue` to an agent that holds messages is `send` (no watcher
needed). For other agents it stores a `pending` row in `messages`. `agent
watch` (`rs/crates/agent-service/src/watcher.rs`) delivers it: every
`--interval` (2s) it checks each agent that has pending messages and, when the
agent is `idle` or `error` (a failed turn leaves the CLI at its prompt, and no
hook fires again until the next prompt), claims the oldest and types it
(`send` with `now: true`). `processing` and `waiting` hold the message;
`ended` fails it, as do 5 `unknown` checks in a row or 3 failed sends
(backoff 1s, doubling). Each watcher holds a shared `flock` on
`.gomuxai/watch.lock`, which is how `queue` tells whether one is running.

## Status

### Statuses and end reasons

An agent and a session have the same statuses. The words and what each
means live in one place, `types::STATUSES` and `types::END_REASONS`
(`rs/crates/agent-base/src/types/status.rs`); `agent statuses` prints them, and
`agent status -v` prints the one that applies on a `means:` line.

| Status | Means |
|---|---|
| `idle` | at its prompt, its turn over: a message can be typed in (a background shell it started may still run) |
| `processing` | a turn is running (or a turn ended leaving background work, whose completion starts the next turn) |
| `waiting` | stopped on something only a person can answer: a permission prompt, a question, the CLI's workspace trust prompt, or another dialog of its own |
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
refuses one with any other status (`parse_end_reason`). A report writes
`_end_reason` every time, so a reason never outlives its end.

### Resolution: native, report, scrape

`AgentService::status_info` (`rs/crates/agent-service/src/service/status.rs`)
resolves status from three sources, most authoritative first:

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

Before any of these, an agent whose session doesn't exist is `ended
(gone)` whatever its last report said (an `ended` report stands, with its
reason): no session, no CLI.

Next, the **workspace trust prompt** (`trust_prompt_status`). A Claude or agy
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
for 30s after it (`TRUST_START_GRACE`) a session Claude's registry doesn't
list yet is starting, not `ended (gone)`. A reboot or a stopped `g3d-termd` daemon ends every
agent at once and fires no hook, and a backend with no native session list
(agy) would otherwise show its last report, `idle`, forever.

The rules between native and report are one function,
`agent_discovery::reconcile_native`, which `agent sessions` applies too:

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
  session's, so while there is one (`Snapshot::problems`, `Snapshot::err`)
  the readers don't conclude a session isn't running: native status, the
  pane's session and `agent sessions`' live view answer with the error.
  The report then stands, and `status -v` prints the reason on a `native:
  unread:` line (`--json`: `native_error`); `agent sessions` shows it as a
  hint.
- **After a deny**, Claude's session list still says `waiting` for a moment
  (under a second observed), and a deny fires no hook. Within 5s of a
  recorded `denied` report that left no pending prompt and no dialog on
  screen (`denied_since`), the report's status wins, so a `send` right after
  `deny` isn't refused.
- **agy dialog promotion.** agy fires `PreToolUse` before it shows a
  permission dialog and nothing while it's up. When the report is
  `processing` from `PreToolUse` and `AgyBackend::permission_prompt_visible`
  sees the dialog at the bottom of the screen, the status becomes `waiting`
  with detail `permission: ...`, so `approve`/`deny` (and `--expect`) work
  without `--force`. Its `ask_question` dialog works the same way
  (`screen_status`, `QuestionPrompter`): after its `PreToolUse`, the
  dialog's `Question 1/N:` heading or `↑/↓ Navigate · enter Select · esc
  Skip` hint above the `esc to cancel` footer makes it `waiting`, detail
  `question: ask_question: Which color? (red, green)` (from the tool's
  input), so `send` holds instead of typing (whose Enter would pick the
  highlighted option, its text becoming an untagged prompt), `send --now`
  and `type_held`'s dialog check refuse, and `approve`/`deny` refuse it as a
  question. A question whose options are Yes and No isn't taken for a
  permission prompt.
- **agy turns that end with no hook.** Esc and Ctrl+C fire nothing in agy
  (not even the running step's `PostToolUse`), so the report stays
  `processing`. When it has stood for `TURN_END_LAG` (3s, time for agy to
  redraw after a hook), is not a `Stop` (one that says `processing` left
  background tasks running), and `AgyBackend::at_prompt` sees the input box
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

Last, **a dialog no hook reports** (`dialog_status`, below) turns `idle`,
`error`, `unknown`, or a `waiting` with nothing to say on what into
`waiting` at that dialog.

`StatusInfo` also returns the last report as recorded (`reported_status`,
`reported_at`), which `wait --next` uses to spot a whole turn between checks,
and the times of the last send and turn start (`sent_at`, `turn_at`), which it
uses to spot a whole turn before the wait.

### Dialogs no hook reports

Claude Code shows dialogs of its own that fire no hook: offers and notices
when a turn ends or at startup (v2.1.319's auto-mode offer, "Teach auto
mode about your environment?"; a fullscreen-renderer offer; an auto-mode
nudge), and pickers a person opens (`/model`). While one is up, typed keys
go to it, and Enter picks its highlighted option. Its session registry says
`idle` for an offer and `waiting` (no detail) for a picker or wizard drawn
in the input box's place, so before this status said `idle`, and `send`
typed into it: the text was lost, its Enter accepted the offer, and `send`
reported `Sent (it was idle)` (friction #29). Spawn prevents the auto-mode
offer (see Claude's launch command, above); the rest are read off the
screen.

`ClaudeBackend::dialog`
(`rs/crates/agent-backend/src/namedagents/claude_dialog.rs`) reads one
generically, from how Claude draws every dialog: a border (a rule of `─`,
or a line of `▔` for a picker), the title on the next line, the body and
options (the highlighted one marked `❯`), and a footer of key hints joined
by ` · ` (`Enter to confirm · Esc to cancel`, `←/→ to change · Enter to
continue · Esc to cancel`). The footer is looked for where one can sit:
the screen's last non-blank line (a dialog under the input box, as the
classic renderer draws the offer, or in its place), and the last non-blank
line above the input box (the fullscreen renderer draws the offer above
it). Every segment must be a key hint (`<key> to <action>`), one for Enter
or Esc, none `to interrupt`: a running turn's footer (`⏵⏵ auto mode on
(shift+tab to cycle) · esc to interrupt`) and the idle status line aren't.
The border must be within 40 lines above the footer, with a title under
it, so a reply quoting a footer doesn't count.
`rs/crates/agent-backend/testdata` holds real captures of the offer in
both renderers, a busy and an idle screen.

`dialog_status` (`rs/crates/agent-service/src/service/dialog.rs`) reads the
screen when the resolved
status would otherwise be `idle`, `error` or `unknown`, or `waiting` with no
tool and no pending prompt. Permission, question and trust prompts are
left to their own handling (`dialog_in` checks them first). A dialog makes
the status `waiting`, source `scrape`, tool `dialog`, detail `dialog:
<title>; Enter picks: <highlighted option>`. Then:

- `send` to an agent that holds messages holds the message (`held_until`
  `its dialog is gone and it has been at its prompt for 10s`) and starts
  the deliverer, which types it once status is `idle` again. Nothing hooks
  the dialog's end, so the deliverer is the only way it goes out (or a
  later `send` to the idle agent).
- `send` to one that doesn't refuses (`check_can_send`), and so does every
  typing path's screen check (`check_screen`), naming the dialog, what
  Enter would pick, and how to get past it.
- spawn holds the first message (`hold_first_message`).
- `approve` presses Enter (the highlighted option), `deny` Escape (closes
  it; the offer takes it for "Not now"), both only while that dialog is on
  screen (`respond_dialog`); `--expect dialog` names it, `approve --always`
  is refused. Nothing is recorded: the screen says what happens next.
  `agent key <session> <key>` answers it otherwise.

### Claude Code's session registry

Claude Code keeps one file per running CLI process,
`<config dir>/sessions/<pid>.json` (`$CLAUDE_CONFIG_DIR`, else
`~/.claude`): it writes it at start, rewrites it as the status moves, and
removes it at exit. It is Claude's own record of which session each process
holds, and what `claude agents` is built on. `agent-claudereg`
(`rs/crates/agent-claudereg`) reads it.
The format isn't a published contract, so the reader takes only the fields
it names (`pid`, `sessionId`, `cwd`, `version`, `kind`, `entrypoint`,
`status`, `waitingFor`, `name`, `nameSource`, timestamps, `procStart` or
`procStartFt`, `pidDomain`, `tmux`), and refuses an entry with no pid or
session id, or a pid other than its file name's. A `kind` (`interactive`,
`bg`, `daemon`, `daemon-worker`) or `status` (`busy`, `idle`, `shell`,
`waiting`) outside that vocabulary is kept with `Entry::unknown` saying so: a
Claude Code that writes a new value needs gomuxai taught what it means, and
until then that session's status is `unknown`, never guessed (its process is
still proven and placed). A missing kind is `interactive` and a missing
status not yet known, as Claude reads them. Claude Code 2.1.316 ships a
switch, off, that writes the start time as `procStartFt` instead of
`procStart`; the reader takes either, and a `procStartFt` unlike this OS's
start time makes the entry unproven, not stale.

An entry is trusted only when proven (`proof`), the check Claude Code makes
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
exited, not yet reaped) counts as not running. `Snapshot::sessions` maps each
session to the live or unproven entry that holds it (when two do, a CLI
stopped with Ctrl-Z and the session resumed in another, the proven one, then
the one updated last).

An entry's `tmux` field is **not** used to place a session: Claude copies it
from its environment, so a `claude -p` run from inside an agent's pane
names that pane too. Where a session runs comes from the process tree
instead (`ps -A -o pid=,ppid=`): `Snapshot::under_pane` takes the live
entries whose process descends from the terminal's root process (`pane_pids`
from `g3d-termd`'s `status` document), and returns the shallowest. A `claude -p` that a tool
started inside the session is deeper, so it is never taken for the pane's
own. Two at the same depth (two CLIs side by side under one shell) are an
error naming both, unless one was updated last.

### Sessions Claude moves into the background

Claude Code can hand a conversation over to a background session that its
daemon runs (`claude daemon run`, one per user and config dir): `/bg` (alias
of `/background`, "Send this session to the background and free the
terminal") does; the daemon's log says `(slash)` for such a move. The daemon's `claude bg-pty-host` runs a new CLI
(registry `"kind": "bg"`) under a new session id, and the old transcript
gains a `{"type":"continued-in", "continuedInSessionId":"<new>"}` line.
Every hook now runs in the new process, under the new id. In 2.1.319 the
pane's CLI stayed, drawing the conversation and taking what is typed into
it, its registry entry keeping the old id and its last status for good;
in 2.1.325 `/bg` exits it (`backgrounded · <id>`), and `claude attach <id>`
in the pane opens the background session there again.

Where the new process runs says nothing about whose conversation it is.
The daemon is started by whichever CLI first needs it, with that CLI's
environment, and runs detached (under `systemd --user` on Linux); from
2.1.325 it starts a spare host ahead of time and hands the next session
sent to the background to it (`bg claimed-spare <id> (slash)` in
`daemon.log`). So agent A's conversation can run with agent B's
`AGENT_NAME`, under the daemon or B's CLI (friction #48-#50: the hook
placed nothing on A, so its long messages lost their text and its sends
went out as B's). What does say whose it is, is the conversation's own
record: the `continued-in` line its old transcript gained. A person's
`claude --resume <id> --fork-session` starts a separate conversation and
writes none, so the line proves the move. Four places follow it:

- **The hook** (`find_hook_session`): a background session's CLI (registry
  kind `bg`) is placed by lineage only (`background_owner`): the agent whose
  `_session_id` is already that session, or else the agent whose recorded
  `_transcript` continued in it, directly or through later moves (each
  session's transcript sits beside the one it continued from). Its place
  in the process tree is never used.
- **Who sends** (`caller_agent`): a command a background session's tool ran
  is that session's owner's, the same way (see [Who sends](#who-sends)).
- **`pane_session`** (Claude's): after `under_pane` finds the pane's CLI,
  `Snapshot::continuation` follows its `continued-in` lines
  (`agent_claudereg::find_transcript` finds each session's file under the
  config dir's `projects/`) to the running entry holding the last session,
  wherever it runs (a background session's own process before a person's
  resume of the same id), so pane adoption never moves an agent back onto
  the frozen old session. When no process holds it (the daemon retired the
  idle background session), the pane still answers with that session, its
  status unknown: the conversation is there, and the pane's CLI starts it
  again on its next input.
- **Status** (`follow_moved_session`): before asking native status, a Claude
  agent whose `_transcript` ends saying it continued in another session
  that its pane shows (per `pane_session`) is moved onto it, with an
  `EVENT_SESSION_MOVED` report: `_session_id` and `_transcript` (the new
  session's file beside the old) change, and `_result` stays, since it is
  the same conversation. Without it, status would read the old entry until
  the new session's next hook. Reading the transcript's last 64 KB for the
  line is the cost every status check of a Claude agent pays.

### Pane adoption

A person can quit an agent's Claude and start another in the same pane, or
`claude --resume` a different session there; the old CLI's last hook (if
any) says nothing about the new one, and a CLI started by hand may carry no
hooks at all. So when native status doesn't list `_session_id`,
`adopt_pane_session` asks the backend (`PaneSessionProvider`) what the
agent's pane runs. If it runs a session other than the one on record, that
session is recorded as the agent's: a report with event `adopted`, detail
`its pane runs session <id> now (pid <pid>, <status in words>); session
<old> before it ended`, the new `_session_id`, and `_result` and
`_transcript` cleared (another conversation's reply and file aren't this
one's). `agent sessions` then joins the agent to the session the scan found
under that id. A pane that runs nothing of the backend's, or the same
session, changes nothing, and status goes on as above. (For backends without
a session registry, such as an agy restarted by hand in its pane, `agent
revive` performs the equivalent takeover on demand; see [Fleet
features](#fleet-features).)

### Reports and the reserved state keys

`AgentService::report` writes these keys for the agent in one transaction
(`Store::set_states` / `Store::record_report`), so a reader never sees half of
two concurrent reports:

| Key | Holds |
|---|---|
| `_status` | `idle`, `processing`, `waiting`, `error`, `ended` (or `unknown`) |
| `_end_reason` | with `ended`: how the session ended (`quit`, `gone`, ...); empty otherwise, since every report writes it |
| `_event` | the hook event, `report` for `agent report`, `approved`/`denied`, `trusted`/`trust-declined` (approve/deny at a trust prompt), `delivered` (a hook handed over held messages), `adopted` (its pane runs another session), `revived` (`agent revive` took over a running CLI in its pane) |
| `_detail` | one line of context, e.g. `Bash: cargo test` or `permission: ...` |
| `_tool` | the tool the event concerns |
| `_tool_input` | the full JSON input of a call that may need permission (≤64 KiB); always written, so it never outlives its call |
| `_result` | the turn's final reply (Claude `Stop`'s `last_assistant_message`; agy `Stop` reads the closing `say` from its transcript); written only when set or cleared |
| `_session_id` | the CLI's session id; spawn sets it, hooks update it (`/clear`, `/resume`, agy's conversation id), and pane adoption or revive sets the session the pane runs |
| `_spawn_id` | agy only: the spawn's uuid, set once by spawn (cleared when `revive` takes over a hand-started agy without spawn hooks) |
| `_model` | the model `spawn --model` started the agent on; resume keeps it and relaunches on it; unset for the CLI's default |
| `_hooked_session` | Claude: the session the global hook last reported for the agent's main session; while it equals `_session_id`, the agent holds messages |
| `_turn_at` | when the last turn started: written with a report that is `processing` and clears `_result` (Claude `UserPromptSubmit`, agy's first `PreInvocation`) |
| `_sent_at` | when a message was last typed in; written by `send` (and queued deliveries and spawn's first message), not by `report` |
| `_unsubmitted` | what Claude's input box showed when Enter failed to submit a typed message; deleted by the next typing whose message doesn't stay in the box |
| `_unchecked` | why a message was typed into Claude without checking it was submitted (its input box couldn't be read); `status` shows it until a turn starts after it, and a checked typing deletes it |
| `_transcript` | the CLI's transcript path from the last hook payload that named one |
| `_named` | agy only: the conversation id already named via `/rename <agent>` after its first idle `Stop` |
| `_prompt.<nanos>` | one per unanswered Claude permission prompt: its call key, subagent, tool, input, and detail (JSON) |

**Pending prompts.** Claude raises one `PermissionRequest` per call, all at
once for parallel calls, and shows them one at a time ("1 of 4"), oldest
first; a background subagent's prompts fire in the main session too, with
`agent_id`, and can outlive the main turn's `Stop`. So the last report alone
can't say which prompt is on screen. Each `PermissionRequest` also stores a
`_prompt.<nanos>` key (its own key, so concurrent hook processes can't lose
each other's), and while the status is `waiting`, `status_info` describes the
oldest one, adding "; N more queued". A prompt ends when its call's
`PostToolUse`/`PostToolUseFailure` arrives (matched by a hash of tool and
input, and the subagent), when `approve`/`deny` answers it, when the main
agent takes a prompt (`UserPromptSubmit`, for its own prompts), or when a
turn ends with nothing running (`Stop` with no background tasks,
`StopFailure`, `SessionStart`, `SessionEnd`). Report writes and deletes in
one transaction (`Store::record_report`), which also appends a `status` agent
event (see [Export](#export)).

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

**Claude** (`ClaudeBackend::parse_hook_event`,
`rs/crates/agent-backend/src/namedagents/claude_hooks.rs`):

| Event | Status | Notes |
|---|---|---|
| `SessionStart` | idle | clears `_result`, except after `compact` |
| `UserPromptSubmit` | processing | clears `_result` |
| `PreToolUse`, `PostToolUse` | processing | detail `Tool: <its file, else its most telling input>`, from the tool table (`agent_transcript::describe_tool`), paths absolute; `PostToolUse` ends the call's pending prompt. `PreToolUse` for Bash carries the command and `cwd`, and prints an allow for a safe `agent` command (see [Spawning](#spawning-an-agent)) |
| `PostToolUseFailure` | processing | detail `failed: ...`; ends the call's pending prompt |
| `PermissionRequest` | waiting | detail `permission: ...` (plus `(from the <type> agent)` for a subagent's); stores `_tool_input` and a pending prompt |
| `PreCompact` | processing | |
| `Notification` | idle | only `idle_prompt`; `permission_prompt` arrives ~6s late and is ignored. `idle_prompt` also fires ~60s into background work, so `report_hook` drops it while the last report is a `Stop` that left the agent `processing` |
| `Stop` | idle | sets `_result`; stays processing while background tasks (shells, subagents) are running, since their completion starts a new turn. With messages held, processing, event `delivered`, and the hook prints them (see [Held messages](#held-messages)) |
| `StopFailure` | error | e.g. API error, rate limit |
| `SessionEnd` | ended (quit) | detail: Claude's reason (`prompt_input_exit`, `logout`, ...); ends every pending prompt. Ignored for `reason: clear` (a `SessionStart` follows); the session's own report is then `ended (replaced)`, detail `/clear` |
| `SubagentStart`, `SubagentStop` | — | not an agent report; the global hook records the subagent's session report (`processing`, then `ended (finished)`) |

**agy** (`AgyBackend::parse_hook_event`,
`rs/crates/agent-backend/src/namedagents/agy.rs`):

| Event | Status | Notes |
|---|---|---|
| `PreInvocation` | processing | clears `_result` on `invocationNum == 0` |
| `PreToolUse` | processing | stores `_tool_input` for dialog promotion; `ask_question`'s detail is its first question and options |
| `PostToolUse` | processing | `failed: ...` on error |
| `PostInvocation` | — | `delivery_only`: recorded (processing, `delivered`) only when it hands over held messages; ignored on an error or `USER_CANCELED` |
| `Stop` | idle, processing, or error | error for an error or `ERROR`/`MAX_*` termination; processing when `fullyIdle` is false; on idle, `report_hook_inner` reads the closing `say` from the transcript and stores it in `_result` |

agy has no `SessionEnd` hook and no session list, so its launch command ends
with `; <binary> report ended --end-reason quit --as <name> --spawn-id
<uuid> "agy exited with code $?"`: the shell records `ended (quit)`, with
agy's exit code as the detail, however agy ends (`/exit`, crash, `kill
-9`), and the spawn id keeps a stale wrapper from marking a respawn
`ended`. Arguments added later (`--add-dir`, `--model`, `-i`) are inserted
before that suffix.

agy launches with `--settings '{"showFeedbackSurvey":false}'` (settings
for that session only, never saved): agy 1.5.x shows a feedback survey
("How's the CLI experience so far?") after a turn now and then, in place of
its input box, where typed keys would answer it and messages waited while
status said idle (friction #44). agy refuses a setting it doesn't know, so
the launch fails loudly if a release drops the key.

agy sorts its workspace folders (the directory it runs in, and each
`--add-dir`) by path and makes the first its "Command Working Directory",
where commands run and which the model takes for the project. The agent's
dir beside the binary (`~/.local/bin/.gomuxai/agents/<name>`) sorts before
most projects (`/home/user/.local` before `/home/user/dev`), so agy worked there
(friction #46). agy takes a path as given, symlinks unresolved, so when the
agent's dir would sort first, `AgyBackend::workspace_dir` adds it through a
symlink whose path sorts after the spawn directory:
`<temp dir>/~gomuxai-<uid>/agy/<name>-<hash>` (`~` sorts after letters),
under `std::env::temp_dir()` or `/var/tmp`, in a directory that must be this
user's own and not writable by others. A project no such path sorts after
(under `/var/www`, say) gets the agent's dir as before. Spawn and resume
make the link each launch (a reboot clears `/tmp`).

### The global hook (`agent hook`)

`agent hooks install` (`rs/crates/agent/src/cli/hooks.rs`,
`rs/crates/agent-backend/src/namedagents/claude_settings.rs`) registers this
binary in Claude Code's user settings (`$CLAUDE_CONFIG_DIR/settings.json`,
else `~/.claude/settings.json`): for each of the 13 events in
`CLAUDE_HOOK_EVENTS`
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
time>`; the write is atomic (`util::write_file_atomic`: a temp file renamed
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
reads the payload and places itself (`find_hook_session`, `place_hook` in
`rs/crates/agent-service/src/service/hook.rs`):

1. **The CLI above it**: walking up its own ancestors (`/proc/<pid>/stat`
   on Linux, one `ps` listing elsewhere), the nearest process that is the
   CLI: its registry entry (`agent_claudereg::read_pid`) is live, or unproven
   but holding the payload's session id, or it is the pid Claude Code sets in
   `$CLAUDE_PID` for its hooks. Being an ancestor of the running hook is
   proof the process runs; the last two cover a CLI whose start time can't
   be compared, and the first event of a session, which can come before
   Claude writes its registry file (seen at `SessionStart`).
2. **The agent above that CLI**: walking on up, the first process that is
   a terminal's root process (`pane_pids` from `g3d-termd`), if its
   session is an agent's. Another CLI on the way first (any running
   process with a registry entry, proven or not) means this CLI was started
   from inside that one's session (a `claude -p` a tool ran), so no agent
   owns it. A CLI whose pane is gone (cleanup killed the session) reaches
   no pane, so no agent. A background session's CLI (registry kind `bg`)
   isn't placed by the walk at all, but by the conversation it carries on
   (see "Sessions Claude moves into the background").

Then `handle_claude_hook`:

- A CLI whose command line carries gomuxai's per-spawn hooks (`--settings`
  whose hooks run `report --hook`; a Claude agent spawned before the global
  hook) is skipped (`spawn_hooked`): its own hook reports it.
- **Every session's report** goes to `session_reports`, keyed by session
  id: the status the event maps to, as in the table above, with its end
  reason, event, detail, transcript and time (a stored newer one wins, since
  hook processes can finish out of order). A subagent's events (they carry
  `agent_id`) are recorded under `agent-<agent id>`, as `agent sessions`
  names it: `processing`, and `ended (finished)` at `SubagentStop`. A
  `SessionEnd` with reason `clear` is `ended (replaced)`. This is how
  `agent sessions` knows how a session no agent owns ended.
- **An agent's session** also gets the agent's report, through `handle_hook`
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

`AgentService::respond_permission` refuses unless the agent is `waiting`,
refuses if `--expect <tool>` names a different tool, and refuses dialogs that
are questions rather than yes/no prompts (Claude's `AskUserQuestion` and
`ExitPlanMode`, agy's `ask_question`). The status it checked can be hundreds
of milliseconds old, so right before the keypress it captures the screen and
requires the dialog to still be there: Claude's deny key is Escape, and
Escape on a running turn interrupts it. That narrows the race to a few socket
calls; it doesn't close it. Keys: `1` approve, `2` approve always, Escape
deny. It then records the expected outcome (`processing`/`approved`, or
`idle`/`denied`), because a denied prompt fires no hook, and drops the
answered pending prompt, so the next one is described while the approved
call runs. Deny drops the rest of that agent's batch too (Escape cancels
it), and denying a subagent's call records `processing`: only that call
ends, and the turn goes on.

A **workspace trust prompt** (status tool `trust`, which `--expect trust`
names) is answered by `respond_trust` instead: right before the keypress it
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

A **dialog no hook reports** (status tool `dialog`) is answered by
`respond_dialog`: right before the keypress the screen must show a dialog
with the title status described. Approve presses Enter, deny Escape
(`DialogReader::dialog_key`); `--always` is refused. If messages are held,
it starts the deliverer.

## Results and transcripts

**`_result`** holds the turn's final reply, stored by the `Stop` hook for both
Claude and agy (`agent state get <name> _result`). For Claude it comes directly
from `Stop`'s `last_assistant_message`. agy's `Stop` payload carries no reply
text, so on an idle agy `Stop` (not `USER_CANCELED` or background-task
`processing`), `AgentService::report_hook_inner`
(`rs/crates/agent-service/src/service/status.rs`) parses the agent's transcript
via `agent_transcript::read_file`, polling every 15ms up to 200ms if the
closing `PLANNER_RESPONSE` line has not flushed yet, and stores the turn's
final `say` (`say.final == true`). `_result` is emptied when a new turn starts
(Claude `UserPromptSubmit`, agy `PreInvocation` `invocationNum == 0`, `/clear`,
or a hook handing over held messages), so an empty value means the turn hasn't
finished. A turn that leaves background work running stays `processing` and
stores no result.

**`_transcript`** and `logs/<name>.transcripts`: hook payloads name the CLI's
own transcript file (Claude `transcript_path`, agy `transcriptPath`).
`report_hook` stores the path in `_transcript` and appends
`<backend>\t<path>` to `logs/<name>.transcripts` when it differs from the
last line. The list lives beside the log, so like the log it outlives
cleanup and spans `/clear` and respawns under the same name. The files:

- Claude: `~/.claude/projects/<encoded project>/<session id>.jsonl`
- agy: `~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript_full.jsonl`

**`agent-transcript`** (`rs/crates/agent-transcript`) reads both, and Kimi Code's
`~/.kimi-code/sessions/<workspace>/session_<uuid>/agents/<agent>/wire.jsonl`
(no hook reports it; `agent sessions` finds it), into the event stream
[`docs/EVENTS.md`](EVENTS.md) describes (types in `events.rs`): one event per
prompt, thinking block, reply, tool call and harness record, each tagged with
its turn, a tool's result arriving as an update of its call. A `Reader`'s
`Parser` takes a transcript's bytes in order and returns the events of the
whole lines among them, keeping what later lines need (the open turn, calls
waiting for results), and reports where it stopped (`pos`), so the same code
reads a whole file or what a live session appends. A parse always starts at
the file's start, because what a line means depends on the lines before it;
`resume_after` resumes after a line by reading the lines up to it silently
(`skip`), so what follows is numbered and turned as the whole stream is.
What only the files beside a transcript say (Kimi's `state.json`, which of
agy's two logs it is, agy's index of the subagents a conversation started,
the parent transcript that numbers a Claude subagent's turns) comes in
through `Options`, which `file_options` reads for a path; `read_file` and a
resumed parse take the same ones. `latest` folds a stream into each event's
latest state.
`Finals` (`rs/crates/agent-transcript/src/final.rs`) merges the final reply a
Stop hook reports (`last_assistant_message`) into a stream ahead of the
transcript, which writes it only after the hook ran: a provisional say that the
transcript's copy then updates.

The session's metadata (cwd, title, agent name, model, version, branch,
continued-in) follows one set of rules, `rs/crates/agent-transcript/src/meta.rs`,
which the parsers apply as they read and `agent sessions` applies to the
head and tail it reads (`harvest_claude`, `harvest_kimi` for a line;
`Session::harvest_agy`, `claude_subagent_session`, `file_options` for the files
beside a transcript). Each line's value is placed by its byte offset, not by
when it was read, so a reader going backwards from the end gets what a parse
from the start gets: the newest value wins, the oldest for the cwd.
Backends hand out their reader through `TranscriptProvider`.
`rs/crates/agent-transcript/testdata/corpus` holds the reference transcripts
with the exact events they must produce. `read_file` parses the whole file a
megabyte at a time but keeps only the last events it was asked for and their
updates, since transcripts run to tens of megabytes; the parser forgets calls
once they are done. The formats are the CLIs' internal files, not APIs: lines
that aren't JSON objects are counted in `Parser::skipped`, and unknown records
are ignored.

**`agent logs`** (`rs/crates/agent/src/cli/logs.rs`) reads the newest transcript, going
back to older ones until it has `--lines` events (each in its latest state),
and prints `--- session <id>` where each earlier session ends; `--json`
prints the stream itself, a session line then its events. It falls back to the
terminal log (with a hint saying why) when there's no transcript: another
backend, no hook fired yet, or the file is gone or unparseable. `--terminal`
asks for it directly (live scrollback when the session is up, else the saved
`logs/<name>.log` snapshot), cleaned by `util::terminal_lines` (escapes
dropped, cursor moves turned into spaces and line breaks, redraws collapsed).
`agent logs name@host` runs the same command on that host over the bus (`logs`
op, capped at `MAX_LOGS_REPLY` = 512 KB; see [RPC ops](#rpc-ops)).

**`agent transcript`** (`rs/crates/agent/src/cli/transcript.rs`) is the stream for
displays: one session's events, whole, as [`docs/EVENTS.md`](EVENTS.md)
frames them. It resolves a workgroup agent's name (the last transcript in
its list), a session id or a unique start of one, or a path, by the same
scan and `agent_discovery::resolve` as `agent sessions`, whose `Session` is the
stream's `session` line. The initial read stops at the size that `Session`
recorded, so the line describes the end of that read. Every read parses
from byte 0, through `Finals`: `--from` and `--until` only filter what is
written, by each event's `at`, so a window's events are numbered, turned
and merged as a whole read's. (`--from` is checked as `resume_after`
checks it, a line's start with its line whole, but not parsed with it,
since `Finals` must see the skipped lines' final replies.) `--follow`
polls the transcript every 250ms (there is no file watcher in the repo; a
poll is a `stat` of the path, an `fstat` of the open file and a 64-byte
`pread`) and feeds the parser what was appended, keeping a line not yet
whole for the next poll. A path that's gone, names another file
(device and inode mismatch), shrank, or no longer holds the last 64 bytes read
(a truncate and regrow between polls) ends the stream with an error.

A listed session's path may name no file yet: `agent_discovery::resolve` lists a
live agent's current transcript before its harness writes it (Claude Code
creates it at the first prompt). If that session hasn't ended, the target
is unwritten and `--follow` waits (`await_transcript`): it `stat`s the path
every 250ms and resolves the session again every 30s, and once the file is
there resolves it once more and streams it as any other. A session that has
ended with no file is an error, at once or when a wait sees it; so is a
path no session lists. Without `--follow` an unwritten transcript is an
error that says it isn't written yet.

Each poll also asks whether the reader has gone: `poll(2)` on stdout with
no events reports `POLLERR` (a Linux pipe whose reader closed) or `POLLHUP`
(a socket) without a write (`rs/crates/agent/src/cli/transcript_poll.rs`, Linux
and darwin; on darwin a pipe's reader closing may only show at the next write).
SIGPIPE is ignored, so a write to a closed stdout fails with `EPIPE` instead of
killing the process, and a notice to a closed stderr is just lost; SIGINT
and SIGTERM cancel the stream. All of these exit 0.

The `session` line's metadata (title, model, version, branch, agent name,
cwd) comes from the stream's own parser after each read, which has seen
every line, where the opening line had discovery's head-and-tail read; a
value the parser hasn't found leaves the known one. Status comes from
checks beside the poll, one at a time on a worker thread the stream waits for
before it returns, so a slow one never holds up the poll. For a session an
agent of this store owns, a report check every 2s reads that agent's
record and state from the store alone (`own_agent`: what export reads, no
termd, no other store) and resolves the probed session against it with no
harness list, counting only when the agent has reported since the status
was last set. A full check, `agent_discovery::resolve` over every store's agents,
Claude Code's session registry and the global hook's session reports, as
`agent sessions` does it, runs every 30s,
the wait doubling up to 5 minutes while the transcript doesn't grow. A
`session` line is written when anything but `size`, `mtime` and
`status_at` changed; its `size` is how far the stream has read.

For a Claude agent of this store whose `_transcript` is the file, each new
`_result` (its `updated_at` newer than the last one seen, and not older
than the current turn's first event) is offered to `Finals::provisional`
for the current turn, at the parser's `pos`: where the reply's line will
land. A Stop that delivers held messages empties `_result` (the turn goes
on), so it makes none.

## Export

`agent export` (`rs/crates/agent/src/cli/export.rs`,
`rs/crates/agent-export`) is the supported feed of the agent tree for displays:
what they would otherwise read from the database's tables, which aren't an
interface.

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
- **Status** is the last report, not `status_info`: export reads no session
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
- **Sessions and transcripts** are checked every second: `g3d-termd`
  `status` (terminal ids) and a stat of each live agent's
  transcript. A change is a `changed` line with `reason` `session` or
  `transcript`.
- **Other stores.** An agent spawned by another binary is recorded in that
  binary's store. For each `agent-*` session no read store records, export
  asks `g3d-termd` (op `env`, preferring `source == "request"`) for the
  session's `PATH` (spawn puts the binary's dir first),
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
  resume's own rules (`types::resumable`, which `AgentService::resume_refusal`
  gives the reasons for): a claude or agy agent gomuxai started (not
  adopted), with a session id, that has ended, and still has a record. For
  this host's agents export applies them to what it knows: the last report
  says `ended`, or the session is gone (where `agent status` says
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
{"name":"lead-t1","host":"laptop","kind":"termd","namespace":"agents","run":12,"parent":"lead","backend":"claude",
 "session":"agent-lead-t1","store":"/home/user/gomuxai/bin/.gomuxai","cwd":"/home/user/gomuxai",
 "created_at":"2026-10-24T14:00:00.5Z","live":true,"attached":true,"resumable":false,
 "status":"waiting","event":"PermissionRequest","detail":"permission: Bash: rm -rf build","tool":"Bash",
 "tool_input":"{\"command\":\"rm -rf build\"}","status_at":"2026-10-24T14:01:02.25Z",
 "session_id":"5f0c…","transcript":"/home/user/.claude/projects/-home-user-gomuxai/5f0c….jsonl",
 "dialect":"claude","transcript_size":48211,"transcript_mtime":"2026-10-24T14:01:02.3Z"}
```

`kind` is `termd` for every agent, and `namespace` names this host's
`g3d-termd` namespace (default `agents`). Absent fields are empty: `parent` for a person's agent,
`ended_at` while live, `namespace`, `store` and the transcript fields for another
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
the one its transcript's path names (`agent_transcript::session_id_from_path`).

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
{"type":"changed","id":4,"store":"/home/user/gomuxai/bin/.gomuxai","at":"2026-10-24T14:00:03.1Z","reason":"status","agent":{"name":"lead-t1","status":"processing","event":"PreToolUse","detail":"Bash: cargo test",…}}
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

`agent sessions` (`rs/crates/agent/src/cli/sessions.rs`,
`rs/crates/agent-discovery`) lists
the agent sessions on the host, the newest 25 by default (`--all`,
`--limit`, `--since`, `--project`, `--harness`, `--workgroup`, `--local`;
`agent help sessions` has the rules): workgroup agents' and the ones a person
ran, in one shape, the Session of [`docs/EVENTS.md`](EVENTS.md). It only
reads, and runs no backend CLI: for Claude it reads Claude Code's session
registry and the global hook's `session_reports`.

**Where.** One table (`agent_discovery::dirs`, ported from glyph3d-rs
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
are the readers' (`rs/crates/agent-transcript/src/meta.rs`, under "Results and
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
store and those its `g3d-termd` sessions point at), each with its name's
`logs/<name>.transcripts`. A transcript belongs to the run whose current or
last transcript it is; one only the history names, to that name's newest
run created before the file's last write. A subagent's session belongs to
its parent session's agent, at any depth. Every transcript an agent names
is listed first (stat only), wherever it lives, so the history rule knows
its last write.
Paths and stores are compared with symlinks resolved, so a symlinked
`~/.claude` lists a session once. A live agent whose current session no
hook has named a transcript for (one adopted from its pane) owns the
session the scan found under its `_session_id`. A live agent with no
transcript yet is listed with an empty `path` and its backend as `harness`.

**Cost: list, pick, then read** (`agent_discovery::Index`). `new_index` lists
every session from directory listings and stat alone: id, harness, path,
size and mtime, and a subagent's parent where its path says (Claude's
`<id>/subagents/`, Kimi's `agents/<agent>/`). agy conversations are read in
full there, since what they say is in agy's small index files, never the
transcript. The workgroup join needs only paths and mtimes, so it runs on
the listing too. `select` then picks groups (a session and the subagents
under it, ordered by the top session's mtime; a live agent's session with
no transcript yet counts as now) that match every filter (harness,
`--since` by mtime, workgroup or local by the join, project), keeps the
newest `limit`, and counts the rest; `resolve` reads just the picked
sessions and the sessions above any subagent among them, and sets their
status. A project filter needs a Claude session's cwd, which only its
transcript has, but Claude Code names the project folder for the cwd the
session started in with every character but a letter or digit made `-`: a
session whose folder lacks one of the filter's letter-and-digit runs
can't match and isn't read (a folder that isn't such a slug, or is 200
characters or more, is read). The rest are read to decide, so the count
left out stays exact. `agent transcript`, `resume` and `spawn --resume`
find a session by path (`lookup`) or by id or its start in the listing's
file names, then read it and its ancestors; `spawn --continue`
(`Index::r#continue`) reads the Claude sessions whose folder could be the
directory's, newest first, until one ran there. On a host with 2076
sessions (1.6 GB, warm cache) the default table went from 0.52s to 0.045s,
a lookup by id from 0.52s to 0.03s, and `--json` (every session, all read)
stays 0.5s with identical output.

**Status** (`agent_discovery::resolve`); every `ended` carries its end reason:
- a live agent's current session: its last report, reconciled with the
  session registry by `agent_discovery::reconcile_native`, the one rule `agent
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

## Fleet features

[`docs/FLEET.md`](FLEET.md) gives the design and wire shapes of the fleet
features, all governed by one rule: **decide from state, never from log lines**.
Where each piece lives in the code:

### `agent fleet`

Cross-host view of the agent tree, stuck flags (`waiting` > 10m, ≥ 3 held
messages or oldest held > 10m, `ended` still recorded, CLI session-name
mismatch), host focus, running `agent run` jobs, handoff docs older than their
repo's latest git commit, open friction counts, and 7-day / long-session
(`> 3d` with compaction counts) `--report` baselines (design:
[`docs/FLEET.md`](FLEET.md) "`agent fleet`"). `agent_fleet::fleet`
(`rs/crates/agent-fleet/src/fleet.rs`: `local`, `query_host`, `gather`,
`render`, `render_report`, `rpc_handler`) builds each host's `FleetHost` and
queries peers in parallel (`FLEET_TIMEOUT` = 3s, `RpcClient::fleet` in
`rs/crates/agent-bus/src/client.rs`); a peer that doesn't answer `fleet` (down,
not linked, or `fleet` left out of its `--remote-ops`) is `reachable: false`
with the error. `cli::fleet`
(`rs/crates/agent/src/cli/fleet.rs`: `run`, `local_host`) implements `agent
fleet` (`--host`, `--report`, `--json`), populates `Options::session_name` via
`AgentService::cli_session_name`, and backs `RpcExtras::fleet` in
`cli::remote_ops::bus_handler` (`rs/crates/agent/src/cli/remote_ops.rs`).

### `agent manage`

One document per host for a manager to draw (`agent_manage::HostView`), and the
data management behind it. `cli::manage` (`rs/crates/agent/src/cli/manage.rs`)
builds it in `local_host`, from what other commands already compute:

- **agents**: `agent_fleet::fleet::local`'s tree, flattened (`parent` keeps the
  tree), with `resumable` and `adopted`.
- **sessions**: the `agent sessions` index (`new_index`, `select`, `resolve`),
  each row given a `SessionState` by `agent_manage::classify` (a live status:
  `running`; no transcript on disk: `missing`; `ended (gone)`: `dormant`; any
  other `ended`: `ended`; `unknown` while agy holds the conversation open:
  `running`, else `unknown`) and `stale` by `is_stale`. A listed transcript is
  on disk by construction; only a transcript an agent's record names is
  checked.
- **usage**: `agent_transcript::usage::Scan` counts a transcript's model calls
  line by line: Claude's `message.usage` on `assistant` records (one call is
  several records with the same `message.id`: counted once, its newest counts
  standing; `<synthetic>` messages skipped; `compact_boundary` records counted
  as compactions), agy's `PLANNER_RESPONSE` steps, Kimi's `usage.record`s.
  `context_tokens` is the last call's prompt (input + cache read + cache
  write). A `Scan` remembers the byte it read up to and serializes, so
  `count_usage` keeps one per transcript path in `session_usage` and reads only
  what a transcript gained (a file shorter than the offset is counted again
  from its start). The first view of a host reads every transcript once (about
  13s for 2 GB in a debug build); `--usage-budget` stops counting at a
  deadline, leaving the rest for the next view. `sum_subagents` adds each
  subagent's tokens to every session above it.
- **friction** (every open entry, the newest 50 closed), **history** (the
  newest N of `command_log`), and **data**: file sizes of the database and
  `logs/`, the logs of agents no longer recorded, rows per table
  (`Store::table_counts`), transcripts per harness, and the backups.
- **llm_usage**: the host's LLM plan limits, as llm-exp's usage widget shows
  them (`agent_manage::llm_usage`): Claude's session, week and per-model week
  (share used and reset time) with its extra-usage credits, GLM, Kimi Code and
  agy quota windows, OpenRouter key spend, and the providers llm-exp couldn't
  ask. They are llm-exp's figures, not counted here: `read_llm_usage` reads
  only the snapshot `llm-exp usage --json` caches for its widget,
  `$HOME/.cache/llm-exp/usage-snapshot.json`, as it is (`captured_at` says how
  old), and never runs llm-exp, reads a credential or asks a provider. No file:
  the field is absent; a file that isn't a snapshot: `path` and `error` only.

`run_view` adds every trusted peer's answer to op `manage`, asked at once on
threads (`RpcClient::manage`, which sanitizes every string of the reply); a
peer is asked for its newest 300 sessions, 100 commands, and a 2s usage budget,
so the reply fits a bus message and the 10s RPC timeout. A peer that doesn't
answer is `reachable: false` with why. Serving a view logs no command.

`untrack` writes `untracked_sessions` rows (never for a running session unless
it is named with `--force`; a subagent goes with its top session), and
`local_host` drops those rows from the view unless asked. `backup` is SQLite's
`VACUUM INTO` (`Store::backup_to`), safe beside writers. `prune` runs one
transaction of `DELETE`s over the tables of finished things
(`agent_store::manage::PRUNE`, times compared with `julianday`, so a row whose
time can't be read stays), after a backup, then `VACUUM`s; `--logs` removes
`<name>.log` and `<name>.transcripts` of unrecorded agents by file age.

### Name at launch

`ClaudeBackend` implements `SessionNamer`
(`rs/crates/agent-backend/src/provider.rs`,
`rs/crates/agent-backend/src/namedagents/claude.rs`), passing
`--name '<agent>'` on `spawn` and `resume` (`AgentService::launch_command`,
`rs/crates/agent-service/src/service/agent_service.rs`; design:
[`docs/FLEET.md`](FLEET.md) "Name at launch"). `agy` has no CLI flag or setting
to title a conversation before its first prompt, so on an `agy` agent's first
clean idle `Stop` (when no messages are held, no dialog is up, and the input
line is empty), `maybe_name_agy_session`
(`rs/crates/agent-service/src/service/status.rs`) types `/rename <agent>`
without touching `_sent_at` and records `_named = <session_id>`.
`agent_backend::session_name` (`rs/crates/agent-backend/src/session_name.rs`,
exposed on `AgentService` as `cli_session_name`) reads a session's recorded
name or title back from Claude's live registry (`name_source != "derived"`) or
transcript (`custom-title`, `agent-name`, `ai-title`), agy's
`annotations/<id>.pbtxt`, and Kimi's `state.json`.

### Templates (`agent template`)

`<data dir>/templates.json` (`0600`) lists the named agent configurations
(`name`, `backend`, `model`, `cwd`, `brief`) this host agrees to start or
resume on request from a peer, without letting the caller supply a command,
`cwd`, model, or brief (design: [`docs/FLEET.md`](FLEET.md) "Templates").
`agent_fleet::templates` (`rs/crates/agent-fleet/src/templates.rs`: `load`,
`list`, `get`, `add`, `remove`, `validate`) reads and atomically writes the
file while preserving unknown JSON keys; `agent_fleet::template_op`
(`rs/crates/agent-fleet/src/template_op.rs`: `handle`, `rpc_handler`) serves
`list`, `start` (refusing running or ended-but-uncleaned records, and setting
`spawned_by` to `req.from`: `name@host` for a remote agent, `name` for a local
agent, `""` for a person), and `resume`. `cli::template`
(`rs/crates/agent/src/cli/template.rs`: `list`, `start`, `resume`, `add`,
`remove`, plus `serve_template` wired into `RpcExtras::template` in
`rs/crates/agent/src/cli/remote_ops.rs`) takes `--backend` (`claude`, `agy`,
`aider`, `shell`) from the global flag and requires `--cwd` on `add`, and
passes `resumed_by = ""` inside `serve_template` so any peer may resume a
host's template agent.

### Focus and follow (`agent focus`)

Each host stores at most one focused agent in SQLite `host_state` under key
`focus` (`rs/crates/agent-store/src/host_state.rs`) as a `FocusEvent` JSON
value, and publishes changes on `gomux.<host>.focus` (`publish_focus`,
`subscribe_focus`, `decode_focus_event` in `rs/crates/agent-bus/src/events.rs`;
design: [`docs/FLEET.md`](FLEET.md) "Focus and follow"). `agent_fleet::focus`
(`rs/crates/agent-fleet/src/focus.rs`: `get`, `set`, `clear`, `clear_if`,
`clear_if_with_publisher`, `follow`) manages local state, reads initial remote
focus via the `fleet` op, and streams bus updates until interrupted.
`cli::focus` (`rs/crates/agent/src/cli/focus.rs`) implements `agent focus`
(`[name]`, `--clear`, `--host`, `--follow`, `--json`), refusing remote
set/clear and falling back to polling `focus::get` every 1s (`STORE_POLL`) when
`--follow` runs locally without a bus; `cli::cleanup`
(`rs/crates/agent/src/cli/cleanup.rs`, `clear_focus`) calls
`focus::clear_if_with_publisher` when removing the focused agent.

### Heavy jobs (`agent run`)

`agent run` (`rs/crates/agent/src/cli/run.rs`, `agent_fleet::run` in
`rs/crates/agent-fleet/src/run.rs` and `src/run/`, tables `jobs` and `scripts`
in `rs/crates/agent-store/src/jobs.rs`; design: [`docs/FLEET.md`](FLEET.md)
"`agent run`: heavy jobs") runs a script file on disk with inherited stdio and
no concurrency lock. It reads the script once and records its path, SHA-256,
full content, caller, host, `cwd`, arguments and start time in `jobs`, writes
that content to `<data dir>/jobs/<id>/script` (0700) and runs that copy (`ran`),
then records the limits systemd applied, `ended_at` and `exit_code`, or
`not_run` for a job whose script never started. Orphaned rows are settled
against systemd unit or PID start-time liveness on `--list` / `--show`, with
`found_gone_at` set and `ended_at` left unknown. On Linux it wraps the process
in `systemd-run --user --scope --unit agent-job-<store>-<id>.scope` (`<store>`:
8 hex of the SHA-256 of the data dir's path) with `MemoryHigh` (default 75% of
RAM), `MemoryMax` (85% of RAM), and `MemorySwapMax=0`: systemd-run starts
`SCOPE_WRAPPER`, which stops itself; `run` waits for that stop with
`waitpid(WUNTRACED)`, checks `systemctl --user show` and the scope's cgroup
files against what was asked (an explicit limit not applied: refused, nothing
run, unless `--no-limit-ok`), then `SIGCONT`s it into the job, so a job runs
once and a scope that was never created is told from one that ended. SIGTERM
and SIGHUP (`JobSignals`, process-wide and counted) stop the scope. It injects
`TMPDIR=/var/tmp` and `CARGO_BUILD_JOBS=12` when unset, and asks systemd for
`ManagedOOMPreference=avoid` on loaded `g3d-termd-<ns>-<n>.service` user units
(`protect_termd`, reporting problems once via
`host_state["run.protect_termd"]`); on macOS it runs plainly and records
provenance. `--promote` copies reusable scripts to `<data dir>/scripts/<name>`;
each run checks the content read against the SHA-256 the `scripts` table
recorded.

### Shared skill and briefing (`agent skill`)

`agent_service::skill_text`
(`rs/crates/agent-service/src/service/instructions.rs`) holds the concise map
of `agent` commands; `AgentService::agent_instructions` prepends a per-agent
identity header to build spawned agents' `instructions.md` and `AGENTS.md` from
the same source (design: [`docs/FLEET.md`](FLEET.md) "`agent skill install`").
`agent_fleet::skill` (`rs/crates/agent-fleet/src/skill.rs`: `install`,
`install_with`, `show`, `skill_markdown`) writes
`<claude_config_dir>/skills/agent/SKILL.md` plus `install_claude_hooks`
(backing up `settings.json` to `<data dir>/backups/`) for Claude Code and
`~/.gemini/config/skills/agent/SKILL.md` for agy; `cli::skill`
(`rs/crates/agent/src/cli/skill.rs`) implements `agent skill install`
(`--harness claude|agy`) and `agent skill show`.

### Reviving an ended agent (`agent revive`)

`agent revive` (`rs/crates/agent/src/cli/revive.rs`,
`rs/crates/agent-service/src/service/revive.rs`, and `agent_fleet::handoff` in
`rs/crates/agent-fleet/src/handoff.rs`; design: [`docs/FLEET.md`](FLEET.md)
"Revive") brings an ended `claude` or `agy` agent back on its own conversation
and sends a re-brief pointing at its handoff doc (`--handoff`, else state key
`handoff`, else `<cwd>/docs/lanes/<name>.md`). `AgentService::revive_case`
distinguishes an empty/gone pane (resumed via `resume_agent`) from a pane where
a person already started the backend CLI again by hand (friction #51:
`take_over_pane_cli` adopts the running PID and session ID, opens a new run,
records `revived`, and clears stale `_spawn_id` without killing the pane).

### `_result` for agy and fleet friction kinds

agy's `Stop` hook populates `_result` from the transcript's closing `say` (see
[Results and transcripts](#results-and-transcripts)), and `agent friction add
--kind` (`rs/crates/agent/src/cli/friction.rs`) accepts `brief`, `handover`,
and `skill` alongside `help`, `error`, `missing`, `bug`, `docs`, and `other`
(design: [`docs/FLEET.md`](FLEET.md) "Friction kinds and `_result` for agy").

## The bus

The bus adds cross-host reach and push status and focus events. SQLite stays
each host's source of truth: an agent is a `g3d-termd` session on one host, so
that host owns its state. Everything on one host works without a bus.

Each host runs one `agent bus run` process (`rs/crates/agent/src/cli/bus.rs`,
`serve_bus`), which starts `agent-busd` (`busd/`, Go) as a child process via
`agent_bus::start_daemon` (`rs/crates/agent-bus/src/daemon.rs`). `agent-busd`
runs an embedded `nats-server` (no JetStream, no monitoring, no system account)
with a loopback client port for the host's own processes and optional hub/leaf
links to other hosts; `find_busd` locates it beside the `agent` executable,
else on absolute `PATH` entries (refusing empty or relative entries). On Linux,
`agent-busd` prints one ready JSON line (`{"client_url":"...","leaf_addr":"..."}`)
on stdout, answers `links\n` queries over stdin/stdout, and shuts down cleanly
(removing `bus.url`) on `SIGINT`/`SIGTERM`, stdin EOF, or parent exit
(`--parent-pid`, checked every 200ms); `Daemon` monitors child liveness and
forwards stderr. `agent bus run` then connects as a local client
(`Ctx::bus_connect`) and runs `agent_bus::serve_rpc`
(`rs/crates/agent-bus/src/rpc.rs`) over `agent_service::new_rpc_handler_with`
plus the `gomux.<host>.local.bus` status responder. Every other `agent` command
is only ever a client; `agent-bus` hides `tokio`, `async-nats`, and `rustls`
behind synchronous blocking calls.

### Subjects

All under `gomux.` (`rs/crates/agent-bus/src/contract.rs`, `busd/contract.go`):

| Subject | Use |
|---|---|
| `gomux.<host>.rpc.<op>` | requests for `<host>`'s bus |
| `gomux.<host>.agent.<name>.status` | status events (core NATS, not persisted) |
| `gomux.<host>.focus` | focus events (`FocusEvent`; `agent: ""` clears focus) |
| `gomux.<host>.local.>` | local clients only, never carried by links (`agent bus status` queries `gomux.<host>.local.bus`) |
| `_INBOX.<host>.>` | replies to `<host>`'s requests; each host has its own prefix |

Host and agent names share one pattern, `^[A-Za-z0-9][A-Za-z0-9_-]*$`, so
they can't contain `.`, `*`, `>`, or spaces. The host name is `$AGENT_HOST`
(or the app config's `host`; either naming one of the identity's former
names, in `host.previous`, means its current one), else the name in the
bus identity's `host` file (`.gomuxai/bus/host`), else
the machine's name with unsafe characters replaced: macOS's LocalHostName
(`scutil --get LocalHostName`), since its kernel hostname follows DHCP and
reverse DNS, and the short hostname elsewhere.

### Identity and pinned TLS

`agent bus init` writes the host's identity into `.gomuxai/bus/`
(`rs/crates/agent-bus/src/identity.rs`; `busd/identity.go` loads it read-only):

| File | What | Mode |
|---|---|---|
| `host` | the host name the identity is bound to | 0644 |
| `host.previous` | names it had before `agent bus rename`, oldest first | 0644 |
| `host.nk` | nkey seed the host signs leaf links with | 0600 |
| `local.nk` | nkey seed local CLIs and hooks authenticate with | 0600 |
| `tls.key`, `tls.crt` | self-signed ECDSA P-256 certificate (`tls.key` written as RFC 5915 SEC 1 `EC PRIVATE KEY`; loader accepts SEC 1 and PKCS#8) | 0600, 0644 |
| `peers/<host>.json` | one trusted peer bundle each (with `aka`, its former names) | |
| `<host>.pub.json` | this host's public bundle, to copy to peers | 0644 |

A public bundle is `{host, nkey, cert_sha256, url?}`: the public nkey, the
lowercase SHA-256 of the certificate's SPKI, and, for the hub, its `tls://` leaf
URL. Trust works like SSH `known_hosts`: no CA, nothing secret leaves a
host, and `agent bus trust` installs a peer's bundle (re-trusting replaces
it). `init_identity` refuses to replace an existing identity, `load_identity`
refuses secret files readable by group or others, and an identity loaded under a
different host name is refused (it would sign as a host the peers don't
know).

**Trust changes** (`rs/crates/agent/src/cli/bus_identity.rs`). `nats-server`'s
config reload refuses any change to the leafnode block, which holds the pinned
certificates (hub) and the remote (leaf), so a trust change applies on a
restart. `agent bus restart` stops the `gomux-bus` session and starts it
with what the running bus reports on its status subject (`listen`,
`client_port`, `remote_ops`; for a bus that predates the first two, its
leaf address and client URL's port), with any of `--listen`,
`--client-port` and `--remote-ops` given replacing that setting. `start`
and `restart` save what they ran with in `bus/settings.json`
(`BusSettings`), and `start` uses it for the flags it isn't given (the
defaults if nothing was saved); both check the op names first, so a typo
can't stop a bus that then fails to start. `untrust` and `trust --renames`
restart a running bus themselves (`--no-restart` leaves it); a plain
`trust` says to.

- **`untrust <host>`** (`untrust_peer`) removes the peer's bundle and its
  `ssh.json` entry; friction links and `spawned_by` naming it stay. One
  side is enough to cut the link: the hub no longer pins the peer, or the
  leaf no longer dials the hub.
- **`rename <new>`** (`rename_identity`) keeps `host.nk`, `local.nk` and
  `tls.key`. Neither the nkey nor the pin (the SPKI hash) names the host,
  so peers' pins still match, and a peer can check that the renamed host
  is the same machine. `tls.crt` is signed again with the same key under
  the new name, the old name is added to `host.previous`, and `host` is
  written last (each file written aside and renamed over), so a crash
  leaves the old name in force. `<new>.pub.json` replaces `<old>.pub.json`
  (its url from `--url`, else the old bundle's). A running bus is stopped
  first and started again as the new name; one run in the foreground is
  refused before anything changes. The new name may not be a trusted
  peer's, current or former.
- **`trust <bundle> --renames <old>`** (`rename_peer`) refuses a bundle
  without old's nkey and pin, then installs it with `aka` (old, plus old's
  own former names) in place of old's bundle, rewrites the store's
  references (`Store::rename_host`: agents' `spawned_by` and runs' `parent` of
  the form `name@old`, and friction links' `peer_host`) and the `ssh.json`
  entry. A plain `trust` of a bundle carrying another trusted peer's nkey
  or pin is refused (`SameKeysError`, pointing at `--renames`): the hub
  would refuse two users with one nkey, and two hosts can't be one
  machine. A bundle's own `aka` is never taken; only `--renames` sets it,
  and a plain re-trust of the same host keeps it.

Former names stay aliases: `canonical_host` maps a name in `host.previous`
to this host and a name in a peer's `aka` to that peer, so `name@<old>`
in held messages and briefings still reaches it, and config maps a former
`AGENT_HOST` (set in agents spawned before the rename) to the current
name. Until the hub re-trusts a renamed leaf, the leaf links (same keys)
but the hub's permissions for it are still the old name's, so requests to
`name@<new>` and its events don't cross. Hosts that reach the renamed one
only through the hub hold no bundle for it and keep their references to
the old name.

Every connection is TLS 1.3, pinned by key:

- **Leaf links.** The hub requires a client certificate and checks it
  against the pinned set of trusted peers (`TLSPinnedCerts` in `busd/daemon.go`);
  leaves check the hub's against its bundle (`pinVerifier`). TLS comes first
  (`TLSHandshakeFirst`), so unauthenticated dialers get no plaintext INFO.
  Peers then authenticate with their nkey (challenge-response).
- **Local clients.** The client port listens on `127.0.0.1` (default 7420)
  and serves the host's own certificate; `agent_bus::connect_local`
  (`rs/crates/agent-bus/src/client.rs`, with `ignore_discovered_servers()`)
  verifies the TLS 1.3 signature, pins `tls.crt`'s SPKI SHA-256, and
  authenticates with `local.nk`. A stale `bus.url` or another user holding the
  port can't impersonate the bus. With no `bus.url` (or `ECONNREFUSED`),
  `connect_local` returns `ErrNoBus` at once, since hooks call it on every
  event.

### Permissions and topology

A host started with `--listen` is the hub; the others dial every trusted
peer whose bundle has a URL, and `serverOptions` (`busd/daemon.go`) refuses to
start with more than one. On the hub, each peer's leaf connection gets
(`peerPermissions`):

- **publish**: requests `gomux.*.rpc.*` (minus the hub's own ops not in its
  `--remote-ops`), replies `_INBOX.>`, and status and focus events
  (`gomux.<peer>.agent.*.status`, `gomux.<peer>.focus`) for its own host only
  (every other host's are denied).
- **subscribe** (what the hub forwards to it): requests for itself, replies
  under its own `_INBOX.<peer>.`, and all status and focus events
  (`gomux.*.agent.*.status`, `gomux.*.focus`).

Each leaf also refuses, as `DenyImports`, requests for its own ops not in
its `--remote-ops` and anything under its own `local.>`. The bus serves
local clients every op (`ALL_OPS`); peers get `--remote-ops` (default
`DEFAULT_REMOTE_OPS`: `list,status,capture,send,respond,friction,logs,fleet,template,manage`;
`keys`, `spawn` and `resume` are local-only unless listed, since `keys` types
anything and `spawn` and `resume` start an arbitrary process on the host).
`logs` and `fleet` only read, like `capture` and `list`. `template` is safe to
serve by default because a host with no `templates.json` starts nothing, and
any start uses only the receiving host's own template parameters. `friction` is
on by default: what it lets a peer do (read entries, link them, resolve a
linked one) is far less than `send` already does.

### RPC ops

`rs/crates/agent-bus/src/rpc.rs` (`serve_rpc`) serves each op on its subject,
at most 32 requests in flight across ops (more get an immediate `"busy: 32
requests already in flight"` reply), with handler panics caught via
`catch_unwind` and turned into `"internal error in <op>: ..."` replies. Both
`serve_rpc` and `RpcServer::stop` perform an in-order NATS server round-trip
flush (`SUB` + `UNSUB 1` + `PUB`) so subscriptions are live before `serve_rpc`
returns and dropped before `stop` returns. Every reply is an
`RPCResponse{error, data}`. `RpcClient` (`rs/crates/agent-bus/src/client.rs`)
times out after 10s by default (`FLEET_TIMEOUT` = 3s for `fleet`,
`DELIVER_TIMEOUT` = 30s for `deliver`, `SPAWN_TIMEOUT` = 2m for `spawn`,
`resume`, and `template` `start`/`resume`) and strips terminal control
sequences from text another host sent (`util::sanitize_terminal`, including
strings nested inside `StatusResponse.tool_input` JSON); `capture --raw` keeps
only SGR sequences (`util::sanitize_styled`), so a hostile host's screen can't
set titles, write the clipboard, move the cursor, or switch modes in your
terminal.

| Op | Request → reply | CLI | Remote by default |
|---|---|---|---|
| `list` | `ListRequest` → `ListResponse` (`agent_binary`: the serving host's agent binary, absolute; agents: `name`, `session`, `backend`, `spawned_by`, `adopted`, `created_at`, resolved `status`, `end_reason`, `detail`, `session_id`, `cwd`, `attached`, `resumable`; older hosts send no `detail` onward) | `list --host H`, `export --hosts` | yes |
| `status` | `StatusRequest` → `StatusResponse` (mirrors `StatusInfo`) | `status`, `wait` | yes |
| `capture` | `CaptureRequest` → `CaptureResponse` | `capture` | yes |
| `send` | `SendRequest` (`now`) → `SendResponse` (status, held, typed); none from older hosts | `send` | yes |
| `respond` | `RespondRequest` (`approve`, `approve_always`, `deny`, `force`, `expect_tool`) → none; with `deliver`, → `DeliverResponse` (`typed`, `held`, `status`) | `approve`, `deny`, `deliver` | yes |
| `keys` | `KeysRequest` (text, then a key) → none | `keys name@host` | no |
| `spawn` | `SpawnRequest` (`agent`, `backend`, `cwd`, `task_file`, `task_content`, `spawned_by`, `model`) → `SpawnResponse` (`name`, `session`, `backend`, `spawned_by`, `cwd`, `task_file`, `model`, `status`, `end_reason`) | `spawn name@host`, `spawn --host H` | no |
| `logs` | `LogsRequest` (`agent`, `lines`, `full`, `json`, `terminal`) → `LogsResponse` (`output`: what `agent logs` prints there, `hint`: its stderr hint, `truncated`: bytes left out) | `logs name@host` | yes |
| `resume` | `ResumeRequest` (`agent`, `resumed_by`) → `ResumeResponse` (`name`, `session`, `backend`, `session_id`, `cwd`, `held`, `status`, `end_reason`) | `resume name@host` | no |
| `friction` | `FrictionRequest` (`action`: `get` `ids`; `link`/`unlink`/`resolve` `id`, with `from` (required), `by`, `note`; resolve's `uid` and `status`; resolve closes only an entry linked to `from`) → `FrictionResponse` (`entries` with their `links` and `requested`, `missing`, `resolved`) | `friction show`/`list`/`links`/`link`/`unlink`/`resolve` | yes |
| `fleet` | `FleetRequest` (`report`) → `FleetHost` (agent tree, flags, focus, running jobs, stale handoffs, friction, optional 7d `--report`) | `fleet`, `focus --host H` | yes |
| `manage` | `agent_manage::Request` (`sessions`, `history`, `untracked`, `stale_days`, `usage_budget_ms`) → `agent_manage::HostView` | `manage view` | yes |
| `template` | `TemplateRequest` (`action`: `list`/`start`/`resume`, `template`, `from`) → `TemplateResponse` (`templates`, `agent`) | `template list`/`start`/`resume` | yes |

`rpc::tests::test_all_ops_matches_rpc_ops` (`rs/crates/agent-bus/src/rpc.rs`)
keeps `ALL_OPS` and the server's op table in step. `logs`, `resume`, `fleet`,
and `template` run logic outside `agent-service` (`logs::serve_logs`,
`resume::resume_agent`, `fleet::local_host`, and `template::serve_template` in
`rs/crates/agent/src/cli/`, backed by `agent_fleet::fleet` and
`agent_fleet::template_op`), so `RpcHandler` takes them as
`agent_service::RpcExtras` (`rs/crates/agent-service/src/service/rpc.rs`).
`agent bus run` serves `cli::remote_ops::bus_handler`
(`rs/crates/agent/src/cli/remote_ops.rs`), which populates all four extras.
A handler without an extra entry refuses that op.

`logs` replies with what `agent logs` prints on the serving host, built as
chunks (`LogsChunk`: an event, or with `--full` its lines; a JSON line; a
session's separator or session line; a terminal log line) so a reply over
`MAX_LOGS_REPLY` (512 KB, encoded; the bus's message limit is 1 MB) drops
whole chunks from the start (`fit_logs`), puts back the session line or
separator the first kept chunk belongs to, and reports `truncated`. A single
text chunk too big alone keeps its end; a JSON line too big alone is left out,
since half a line isn't JSON. The client strips terminal controls from `output`
and `hint` as from `capture`, and says on stderr when the reply was cut.

`respond` with response `deliver` (`agent deliver name@host`) runs the
serving host's `deliver_now`: one check of `deliver_held`, typing the held
messages if the agent is at its prompt with an empty input line, and
replies how many it typed and how many are still held, and what the agent
was doing. Messages are held on the agent's host, so this is how a sender
elsewhere types them in after a turn that ended with no hook (friction
#42). It rides on `respond` because it acts for an agent the same way, on
messages already sent to it, after the same checks; a host that predates
it refuses it as an invalid permission response, and the CLI says to run
deliver there. `--wait` (the deliverer `send` starts) stays the agent's
host's own. The client waits up to `DELIVER_TIMEOUT` (30s): typing into
Claude waits for each message to be taken in.

`resume` runs the serving host's own `resume_agent`, so a refusal is that
host's resume's (adopted, not claude or agy, no session, not ended), and
an agent not found is not found there. `resumed_by` is the caller as
`name@host`: as on that host, an agent may resume only its own helpers
(the parent chain must contain it); a bare name, or one naming the serving
host, is refused. As for `spawn`, the caller refuses `--skip-permissions`
and `--trust-workspace` for another host's agent before sending, and
`ResumeRequest` has no field for them: a peer never starts a process on
another host with its permission checks bypassed. The client waits up to
`SPAWN_TIMEOUT`, as for spawn. The
remote side runs the same service code as a local call (`send` holds a
message for a busy Claude agent and refuses to type into dialogs;
`respond_permission` still checks the screen), and the CLI
adds the `[from ...]` prefix before the request leaves. For `spawn`, the
caller reads `--task` from its own store and sends the content
(`task_content`); `task_file` and `cwd` are paths on the remote host, `cwd`
absolute (empty: the home directory of the user the bus runs as), and a relative
`task_file` is relative to `cwd`. The caller refuses `--resume`,
`--attach`, `--backend-config`, `--skip-permissions` and
`--trust-workspace` for a remote spawn before sending; `SpawnRequest` has
no field for `--skip-permissions`, `--trust-workspace` or `--resume`, so a
peer can't ask for them either. `model` is checked against
`agent_service::validate_model` on both sides and shell-quoted on the launch line;
a host that predates it ignores it and replies with no `model`, and the
caller says the agent runs on its CLI's default. (Confirming a trust prompt is saved in the
host user's CLI config and would outlast the agent.) So `cwd` must be a
directory the CLI already trusts there, or a person there confirms the
prompt. A `spawned_by` naming the serving host itself (in any case) is
refused: that agent didn't ask over the bus. The client waits up to
`SPAWN_TIMEOUT` (2m) for the reply, since the far side sits out the CLI's
startup first. `rs/crates/agent/src/cli/remote.rs`
routes `name@host`: a bare name or `name@<this host>` stays local and needs
no bus.

Host names match ignoring case, but subjects don't: a request to
`gomux.mac-studio.rpc.list` reaches no bus when the host is `Mac-Studio`.
So the CLI spells a host as the bus does before it builds a subject
(`canonical_host`): this host's name, or a trusted peer's (`bus/peers/`),
whichever matches ignoring case; `name@host`, `list --host`, `spawn
--host`, `wait` and `export --hosts` all go through it. A hub trusts every
peer, so it knows every spelling; a leaf trusts only the hub, and reaches
other leaves by the exact name it is given. A request no bus answers fails
with `agent_bus::NoRespondersError`, to which the CLI adds the hosts it knows.

### Attach over ssh

The bus can't carry a terminal, so `attach name@host` and `resume
name@host --attach` (`rs/crates/agent/src/cli/ssh.rs`) run ssh themselves; a
display or agent never builds the ssh command or guesses a binary's path:

1. **Where:** the destination set per peer with `agent bus ssh <host>
   <dest>` (`bus/ssh.json`, plain config, shown by `bus status`), else the
   host's name. It may be a host, `user@host`, an alias or a bracketed
   address, never starting with `-`; `~/.ssh/config` still applies.
2. **What:** the host's `list` reply names its agent binary
   (`agent_binary`, `Config::agent_binary`, absolute with symlinks resolved).
   The caller refuses one that isn't an absolute path of plain characters
   (`agent_bus::validate_agent_binary`), refuses an agent the reply doesn't
   list, and says so for a host that predates the field, without running ssh.
3. **Run:** `ssh -t -- <dest> '<binary>' attach '<name>'` (or `resume
   '<name>' --attach`): ssh hands the command to a shell there, so both
   are quoted. It needs a terminal here, and says so without one.
4. **Exec:** `exec::exec_into` (`rs/crates/agent/src/cli/exec.rs`) replaces the
   agent process with ssh (`execvp`), so a signal to `agent attach` reaches ssh
   and nothing is orphaned; local attach does the same with `g3d`. The
   command-log row is written just before, since nothing runs after. Before the
   exec, a line on stderr says it is ssh, not the bus, and needs `ssh <dest>`
   to work: once ssh runs, its errors are its own.

Over ssh, the remote `resume --attach` is the person's own command on that
host, not the `resume` op: that host's `--remote-ops` doesn't apply, and
the `--skip-permissions` and `--trust-workspace` refusals are the caller's.

### Status events and wait

`report` publishes a `StatusEvent` (host, agent, status, `end_reason`,
event, detail, tool, time) for every report it records, through the service's
optional `EventPublisher` (`WarnPublisher` in `rs/crates/agent/src/cli/root.rs`).
Publishing is best-effort: the stored report is the source of truth, so a
missing or failing bus never fails a report. `Ctx::bus_publisher` connects on
the first report only, so commands that never report never touch the bus. A
publish only fills the client's buffer and a hook process lives for
milliseconds, so `execute_ctx` flushes before exit (`close_bus_publisher`).
`decode_status_event` rejects an event whose body names another host or agent
than its subject; `decode_focus_event` does the same for `gomux.<host>.focus`.

`agent_bus::wait` (`rs/crates/agent-bus/src/events.rs`), the core of `agent wait`:

1. Subscribe to the agent's status subject and round-trip flush, so `agent-busd`
   has the subscription before step 2.
2. Check the current status (`status_info` locally; the `status` RPC for a
   remote agent). A transition right after this check still arrives as an
   event.
3. Loop on events, a periodic re-check (5s with a bus, 2s polling without),
   and a 1.5s re-check after an event (native status can lag a hook).

Events are hints: the hub routes them and could forge one, so an event that
would end the wait only triggers a real check. `--next` ignores the `--for`
statuses until a check has seen the agent in another one, or seen a report
newer than the first check's that is already back in a `--for` status (a
whole turn between checks), or seen `sent_at < turn_at < reported_at` with the
report in a `--for` status (a whole turn after the last send, before the
wait). The send marker only counts a turn that started after the send, so a
turn already running when the message was typed (Claude queues it) doesn't
end the wait when it finishes. `WaitOptions::after` replaces the send marker
with "a `--for` report newer than this", which `--follow` uses to report each
arrival once. Unless `ended` is in `--for`, the wait fails as soon as the
agent is `ended`. A timeout exits 2.

`agent wait` with several names (`rs/crates/agent/src/cli/wait.rs`) runs one
`agent_bus::wait_observation` per agent, on threads that share the bus
connection, and prints each arrival as it comes. Local names are looked up
first, so a typo fails at once instead of waiting out the timeout. With no
names, `--any`/`--follow` list this host's agents (minus the caller), and
`--follow` lists them again every 10s to pick up new ones; there, an agent
whose session has ended or that has been cleaned up is dropped silently. A single named agent
without `--follow` keeps the original output (`name: status`) and errors.

### Security model, in short

Anything that can `send` to an agent can make it run commands, so the bus is
treated like SSH:

- Links: TLS 1.3, certificates pinned by key; nkey authentication per host;
  no shared secrets, and only public bundles are copied.
- Least privilege: fixed ops only, per-host `--remote-ops`, no raw keys
  and no open `spawn` remotely by default (use `template` with local
  `templates.json` to allow pre-approved cross-host starts), no general remote
  shell. Listing `spawn` on a host amounts to letting every linked peer run
  commands there as the bus's user, at least as much as `keys`: with `send` and
  `respond` (on by default; it also types an agent's held messages, `agent
  deliver name@host`) a peer can spawn a Claude agent and approve its Bash
  prompts, or spawn aider and send it `/run ...`. List `spawn` only on a host
  whose peers you'd give a shell. Peers can't publish status or focus events as
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
  characters (`validate_agent_binary`), and is shell-quoted, as the agent
  name is; the destination can't start with `-`, and `--` ends ssh's
  options. A host (or the hub, see below) that lies about its path can at
  most make ssh run another existing program there, as the person, with
  `attach <name>` as its arguments.
- **Known gap: replies are unsigned.** A peer may publish to any `_INBOX.`
  subject (narrower response permissions stop genuine replies from crossing
  the link), so a peer that learns another host's reply subject could
  answer in its place. Reply subjects are random but not secret.
  `test_security_peer_cant_forge_replies_to_others` documents it and runs with
  `GOMUXAI_KNOWN_BUGS=1`.

`rs/crates/agent-bus/tests/natsbus/security.rs`, `daemon.rs`, and `fleet.rs`
test each of these against a hub (`agent-busd`) with real leaves, including a
hostile one.

### macOS Local Network

macOS (15+) only lets a process reach the local network if its
*responsible* code holds the Local Network permission (Apple TN3179). A
`g3d-termd` daemon detaches from the terminal that started it, so for
processes inside `g3d-termd` responsibility resolves to nothing that can hold
the grant: dialing the hub over the LAN fails with `EHOSTUNREACH` ("no route
to host" in `bus.log`) and no alert. Loopback, the internet, and accepting
connections are unaffected. Because `agent-busd` is the process that dials the
LAN, it is the one that must hold the grant. The fix has two halves, and both
are needed:

1. **The build.** `make macos` (on a Mac) embeds
   `packaging/macos/Info.plist` (bundle identifier
   `io.github.tikimcfee.gomuxai` and `NSLocalNetworkUsageDescription`) as
   `__TEXT,__info_plist` into **both** `bin/agent` (`cargo rustc ... -C
   link-arg=-Wl,-sectcreate,...`) and `bin/agent-busd` (`CGO_ENABLED=1 go build
   -ldflags "-linkmode=external -extldflags=-Wl,-sectcreate,..."`), then signs
   both with an Apple-issued identity (`CODESIGN_IDENTITY`, default the first
   valid identity in the keychain; ad hoc signatures don't keep the grant).
   macOS keys the grant on the shared bundle identifier, so rebuilds keep it.
2. **The spawn.** On macOS, `agent_bus::start_daemon`
   (`rs/crates/agent-bus/src/daemon.rs`, `start_pipeless`) checks
   `agent_base::macos::has_info_plist` on `agent-busd` and, when present,
   spawns `agent-busd` with `responsibility_spawnattrs_setdisclaim`
   (`agent_base::macos::spawn_disclaimed`, setting `GOMUX_BUS_DISCLAIMED=1`) so
   `agent-busd` is its own responsible process. Because `posix_spawn` with
   disclaimed responsibility cannot inherit custom pipes cleanly, `agent-busd`
   runs with `--no-stdio --parent-pid <pid>`, writing `busd.ready`,
   `busd.links` (updated only when the link count changes), and `busd.err`
   atomically in `bus/` and exiting if its `agent bus run` parent dies.

On macOS, `bus_macos::warn_local_network`
(`rs/crates/agent/src/cli/bus_macos.rs`) prints a note at bus start when the
bus runs in `g3d-termd` (`$G3D_TERMD_ID` set) without an `Info.plist` on
`agent-busd` and will dial a hub.

### Signing over ssh

The login keychain is unlocked only in the GUI (Aqua) security session,
and every process keeps the security session of the process that started
it. A `g3d-termd` terminal is forked by the `g3d-termd` *daemon*, so it has
the session of whatever started the daemon, not of the client that asked for
the terminal. So a `make macos` sent over ssh into a `g3d-termd` session signs
when the daemon was started from a terminal on the Mac, and fails when the
daemon was started over ssh: securityd logs `CSSMERR_CSP_NO_USER_INTERACTION`
and codesign reports `errSecInternalComponent`, with a valid identity and the
screen unlocked. `launchctl managername` prints `Aqua` in the GUI session
and `Background` over ssh; `make macos` prints it when codesign fails.

Two things keep a Mac's daemon in the GUI session:

- A LaunchAgent in the `gui/<uid>` domain that runs `g3d-termd --ns agents
  --idle-exit 0` in the foreground (`LimitLoadToSessionType` Aqua; plist,
  probe script, and headless build-keychain alternative in
  [`tools/macsign/`](../tools/macsign/README.md)), loaded with
  `launchctl bootstrap gui/<uid> <plist>` while no `agents` daemon runs:
  launchd starts the daemon at login in the GUI session, and `--idle-exit 0`
  keeps `g3d-termd` up across empty terminal lists. The cost: any process
  running as the user can open a terminal there and use the unlocked login
  keychain, as from an open terminal tab.
- `agent bus restart` and `bus rename` keep the bus's session
  (`pause_bus_session` in `rs/crates/agent/src/cli/bus.rs`: Ctrl-C, then
  `respawn_pane` for a fresh shell under the same terminal id, preserving
  `cwd`, `env_base`, and request `env`, and the new `bus run` typed in).

### The gomux-bus session

`agent bus start` runs `env AGENT_HOST='<host>' '<binary>' bus run ...` in the
`g3d-termd` session `gomux-bus`, pinning the host so the bus runs as the
identity that started it, and waits for it to answer (`run_bus_in`): it
returns as soon as the bus does, prints `still starting...` once past 5s,
and gives up after 30s (`BUS_START_TIMEOUT`; the first launch of a newly
signed `agent` and `agent-busd` on macOS can take seconds of signature
checks, and the pipe-less launch's own wait for `busd.ready`,
`BUSD_READY_TIMEOUT`, is 25s, inside it). It tells a bus that exited (its
`bus run` process, found under the session's shell by its words `bus run`,
gone; or the screen showing the shell back at its prompt after the
command; or, never seen, 5s gone by) from one still starting at 30s, and
says which, with the session's output. The session
name is the same for every data dir in the namespace, while each bus records
its URL in its own data dir. So `start` and `stop` first check that this data
dir's bus is the one answering (`check_own_bus_session`), and otherwise leave
the session alone: it likely belongs to another binary (e.g. a dev build in
`bin/`). `stop` interrupts the bus so it removes `bus.url`, then kills the
session; `restart` and `rename` keep it (see "Signing over ssh").

## Storage

One SQLite database, `.gomuxai/gomuxai.db`, through `rusqlite` with bundled
SQLite (`rs/crates/agent-store/src/sqlite.rs`). Each `Store` holds one
`Connection` behind a `Mutex`; concurrent processes (`agent` commands, hooks,
`agent bus run`, `agent watch`) coordinate through SQLite's file locks and WAL.

### Schema

| Table | Columns | Notes |
|---|---|---|
| `agents` | `name` PK, `session_name`, `backend`, `task_file`, `log_file`, `created_at`, `spawned_by`, `cwd`, `adopted` | one row per live agent |
| `runs` | `id`, `name`, `parent`, `backend`, `session`, `cwd`, `created_at`, `ended_at`, `status`, `transcript`, `end_reason` | one per spawn; cleanup sets `ended_at` and the last status, transcript and end reason; indexed on `(name, id)` |
| `agent_events` | `id`, `at`, `kind`, `agent`, `run`, `status`, `event`, `detail`, `tool`, `tool_input`, `session_id`, `transcript`, `end_reason` | `spawned`/`status`/`removed`, for `export --follow` and `fleet`; the newest 20000 are kept |
| `session_reports` | `session_id` PK, `harness`, `status`, `end_reason`, `event`, `detail`, `transcript`, `at` | the global hook's last report for every Claude session it heard (a subagent's under `agent-<id>`), for `agent sessions`; a write older than the stored one is dropped; never pruned |
| `messages` | `id`, `sender_id`, `receiver_id`, `content`, `status`, `created_at` | held and queued messages: `pending`/`delivered`/`failed`, and `handover` for a long message typed as a short line whose `UserPromptSubmit` hook hands it over; indexed on `(receiver_id, status)`; claimed with `UPDATE ... WHERE status = 'pending'` (or `'handover'`, by id and receiver) |
| `tasks` | `name` PK, `content`, `source_file`, `created_at` | independent of agents |
| `state` | `agent_name`, `key`, `value`, `updated_at`; PK `(agent_name, key)` | FK to `agents` with `ON DELETE CASCADE`: state needs an agent record |
| `command_log` | `id`, `started_at`, `duration_ms`, `host`, `agent`, `parent`, `cwd`, `args` (JSON), `exit_code`, `error`, `build` | `agent history`; indexed on `(agent, id)` |
| `friction` | `id`, `uid`, `host`, `filed_by`, `parent`, `cwd`, `build`, timestamps, `kind`, `summary`, `detail`, `command` (JSON), `status`, accept and resolve fields, `duplicate_of` | the friction box |
| `friction_votes` | `id`, `friction_id` (FK, cascade), `uid`, `host`, `voter`, `parent`, `build`, `created_at`, `note`, `command` | `+1`s |
| `friction_links` | `id`, `friction_id` (FK, cascade), `peer_host` (`COLLATE NOCASE`), `peer_id`, `peer_uid`, `linked_by`, `linked_at`, `note`, `confirmed_at`, `peer_*` (status, kind, summary, votes, resolved_by, resolution, checked_at); unique `(friction_id, peer_host, peer_id)` | this host's half of a link to another host's entry, and what was last seen of it |
| `jobs` | `id`, `script`, `path`, `sha256`, `content`, `started_by`, `host`, `cwd`, `args` (JSON), `unit`, `memory_high`, `memory_max`, `memory_swap_max`, `note`, `started_at`, `ended_at`, `exit_code`, `pid`, `pid_start` | `agent run` job records (`rs/crates/agent-store/src/jobs.rs`); `ended_at` is `''` and `exit_code` `NULL` while running |
| `scripts` | `name` PK, `sha256`, `source`, `promoted_by`, `promoted_at` | scripts promoted to `<data dir>/scripts/<name>` by `agent run --promote` |
| `untracked_sessions` | `harness`, `session_id` (PK together), `path`, `title`, `by`, `at` | sessions a person told `agent manage` to stop tracking; the transcripts are untouched |
| `session_usage` | `path` PK, `size`, `scan` (JSON), `at` | each transcript's token count and where the count stopped (`agent_transcript::usage::Scan`); a cache, rebuilt from the transcript when missing |
| `host_state` | `key` PK, `value`, `updated_at` | host-scoped key-value entries (`rs/crates/agent-store/src/host_state.rs`): `focus` (`FocusEvent` JSON) and `run.protect_termd` |

There is no logs table: logs are files (see
[Results and transcripts](#results-and-transcripts)).

### Schema initialization

There is no numbered migration list. Every `Store::open` runs
`CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` for every table,
then idempotent `add_column` checks against `pragma_table_info` (tolerating
concurrent `ALTER TABLE` races) so older databases gain any missing columns in
place. Read-only opens (`open_read_only`) run no DDL and inspect
`sqlite_master` (`Schema`, `has_table`) before querying tables that an older
store may lack.

### Concurrency

Many short-lived processes write at once: every hook of every Claude session
on the machine is its own `agent hook`, and every hook of an agy agent its
own `agent report --hook`. `Store::open` sets `busy_timeout(5000)`,
`foreign_keys = 1`, and `synchronous = NORMAL` on the connection, plus
`PRAGMA journal_mode = WAL` (persisted in the database file). Multi-key writes
that must be seen together (`Store::set_states`) use a transaction. A
transaction that will write must not start with a plain read: once another
process commits, SQLite can't upgrade its read to a write (`SQLITE_BUSY`, which
`busy_timeout` doesn't wait out). So `Store::delete_agent` starts with the
run's `UPDATE ... RETURNING`.

Readers that only read (`export`) open the store with `open_read_only`
(`SQLITE_OPEN_READ_ONLY`, no schema writes) and use short transactions, so they
never hold up writers in WAL mode.

### Command log

`cli::root::execute_ctx` records every run in `command_log` when it ends
(`cli::history::record_command`, `rs/crates/agent/src/cli/history.rs`): args
(each capped at 300 bytes), duration, exit code, error, the calling agent
([Who sends](#who-sends)), cwd, and build id. Help is recorded too when a data
dir already exists (`record_command` opens the store itself when setup didn't);
`report --hook` and `agent hook` (every hook event of every session) and `bus
run` (a daemon) are not. `export --follow` is recorded once, when it ends
(Ctrl-C or SIGTERM; a SIGPIPE or SIGKILL leaves no row). Commands that `exec`
another binary (`attach`) record their row right before `exec_into`. The log
keeps the newest 20000 rows: every 100th insert deletes older ones, so most
inserts don't pay for pruning.

### The friction box

`agent friction` (`rs/crates/agent/src/cli/friction.rs`) is a request box for
the tool and fleet (`--kind`: `help`, `error`, `missing`, `bug`, `docs`,
`brief`, `handover`, `skill`, `other`). Entries move `open` → `accepted` →
`resolved`, `wontfix`, or `duplicate`; a duplicate's votes (and its own filing)
count toward the original. `add` and `+1` attach the caller's last failed
command from the command log within 15 minutes (copied, since the log is
pruned). `add` refuses a summary similar to an open or accepted entry's (word
overlap, `rs/crates/agent/src/cli/friction_similar.rs`), lists the matches, and
exits 3; `--new` files anyway. The box is per host; each entry and vote has a
`uid` so boxes can be merged later. Resolved entries, via `friction export`,
are the tool's changelog.

**Links across hosts** (`rs/crates/agent/src/cli/friction_links.rs`,
`rs/crates/agent-service/src/service/friction.rs`). The same problem filed on
two hosts is linked with `friction link 4 34@hub`. Unlike a same-store `--dup`,
both entries stay open and count their own votes (each may say why it happened
on its host). Each host stores its own half in `friction_links`, so `show 34`
on the other host knows about `#4` without asking. A half keeps the other
entry's `uid` (a reset store's new `#34` isn't taken for the old one), whether
the other host was seen to hold its half (`confirmed_at`), and what was last
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
  and `show` prints the other entry's kind in parentheses, `#34@hub
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
  (`Store::update_friction`), dropping any the original already has.

## CLI conventions

Shared by every command (`rs/crates/agent/src/cli/mod.rs`, `command.rs`,
`flags.rs`, `help.rs`, `output.rs`, `exit.rs`, `root.rs`):

- **Command frame.** Each command is a `command::Def` combining a `command::Doc`
  (`use_line`, `short`, `long`, `example`, `args: ArgRule`, `required`,
  `exclusive`, `skip_setup`), a `clap::Args` struct, and `fn(&Ctx, &Args) ->
  anyhow::Result<()>`. Adding a module name to `commands!` in `cli/mod.rs`
  declares the module and places it in its `agent help` task group; subcommands
  hang off their parent's `Def::group`. `root::find` walks non-flag words down
  the tree, `flags.rs` parses command and global flags anywhere on the line
  with pflag-compatible syntax and error messages, and `help.rs` renders help.
- **Context and lazy opening.** `root::Ctx` holds global flags (`Globals`),
  `changed(flag)`, stdout/stderr writers, `hint()`/`notice()`, and `OnceLock`
  handles. `Ctx::setup` builds `Config` (validating `--termd-ns`) and the
  backend `Registry` (including `--backend-config`) for every command except
  `help` and `version`; `store()`, `termd()`, `service()`, `bus_connect()`, and
  `bus_publisher()` open lazily on first call, so commands that need no store
  (`help`, `version`, `statuses`) create no `.gomuxai/` directory.
- **stdout is the result, stderr the hint.** `Ctx::hint` prints at most one
  suggested next step per command, only after a real check showed it's
  needed; `-q` drops it. Commands whose stdout is a data stream (`events`,
  `watch`) say what they're doing once with `Ctx::notice`.
- **Errors say what failed, what was found, and what to try.** Unknown
  commands, subcommands, and flags get "did you mean" suggestions
  (`util::similar`: one edit per three letters, so a typo finds its command
  and an unrelated word finds none) or point at `agent help`. `root_args` and
  `run_help` apply that rule at the root; argument errors add the usage line
  and the command's first example (`usage_error`). Group commands without a
  leaf run `run_group`.
- **Exit codes.** 0 done, 1 failed, 2 `wait` timed out, 3 `friction add`
  found a similar entry (and `agent run` exits with its script's exit code).
  Commands return `ExitError` (`cli/exit.rs`) for non-1 codes instead of
  calling `process::exit`, so `execute_ctx` still records the run and flushes
  the bus publisher; `main.rs` exits with `exit::exit_code(&err)`.
- **`--json`** (on `list`, `status`, `state get`/`list`, `task get`/`list`,
  `session list`, `bus status`, `logs`, `history`, `friction list`/`show`,
  `run --list`/`--show`, `events`, `export`, `fleet`, `focus`, `template list`)
  goes through `Ctx::print_json`: indented, with `&`, `<`, `>` unescaped.
  Streams (`events --json`, `export --json --follow`, `focus --follow --json`)
  print one compact object per line instead.
- **Help is grouped by task** (`commands!` in `cli/mod.rs`), and tested:
  `help_examples_parse` (`rs/crates/agent/src/cli/commands_test.rs`) parses
  every example line in every command's help and fails on an unknown command or
  flag or a wrong argument count; `help_layout` requires a one-line `short`, a
  group, and an `example` for every visible command; `help_fixtures.rs` checks
  rendered help and usage errors against `rs/crates/agent/testdata/`. When you
  change a command, update its help and examples with it.
- **`agent` alone** prints an orientation (`rs/crates/agent/src/cli/orient.rs`):
  who you are, the host, your helpers (or all agents), the bus, queued messages.

## Testing

```bash
make test          # cargo test --workspace (in rs/) + go test ./... (in busd/)
make lint          # cargo clippy --workspace --all-targets -- -D warnings + go vet in busd/
make fmt           # cargo fmt --all (in rs/)
```

- **Unit and crate tests (`rs/crates/*/src/*_test.rs`, `rs/crates/*/tests/*.rs`)**:
  - `agent`: CLI behaviour (`TestEnv` in `src/cli/testenv.rs` gives each
    parallel test its own store, isolated `HOME`/`PATH` with `exec cat`
    stand-ins for `claude`/`agy`, private `g3d-termd` namespace, and optional
    throwaway `agent-busd`), subprocess E2E (`tests/binary.rs`, `tests/e2e_*.rs`),
    and help/usage fixtures (`tests/help_fixtures.rs`).
  - `agent-service`: `AgentService` spawn, adopt, resume, revive, send,
    handover, submit checks, held delivery, dialog/trust/permission handling,
    RPC ops (`tests/service/`), and `watcher` (`tests/watcher.rs`).
  - `agent-fleet`: fleet view and thresholds (`tests/fleet_view_test.rs`),
    templates and handoff staleness (`tests/fleet_lib_test.rs`), `run` scopes
    and script promotion (`tests/run_test.rs`), and `skill` installation
    (`tests/skill_test.rs`).
  - `agent-bus` & `busd/`: real `agent-busd` hub and leaves on ephemeral
    loopback ports (`127.0.0.1:0`), identity/TLS pinning, RPC across links,
    status and focus events, `wait`, and hostile-peer security tests
    (`rs/crates/agent-bus/tests/natsbus/`, `busd/busd_test.go`).
  - `agent-backend`, `agent-transcript` (against fixtures in
    `rs/crates/agent-transcript/testdata/`), `agent-discovery`, `agent-export`,
    `agent-claudereg`, `agent-allowcmd`, `agent-termd`, `agent-store`, and
    `agent-base`.
- **`g3d-termd`** is used by `agent-termd`, `agent-service`, and CLI tests that
  open terminals (`require_termd!()` skips when `g3d-termd` isn't installed).
  No test needs `claude` or `agy` installed, and temp dirs go under `/var/tmp`
  (never `/tmp`, which is `tmpfs` on Linux hosts).
- **Runtime `termd` test guard.** Tests never touch the live `g3d-termd` socket
  directory. Every `agent-termd` connection checks at run time whether the
  current executable is a Cargo test binary (`is_cargo_test_exe` in
  `rs/crates/agent-termd/src/paths.rs`, matching `<profile>/deps/<crate>-<hash>`
  and verified by `tests/guard_test.rs`). In a test binary, `check_test_server`
  refuses any socket unless `agent_termd::testing::{require, init, setup}`
  created a private `0700` socket directory under `/var/tmp` (within the Unix
  `sun_path` limit), set `GOMUXAI_TEST_TERMD_DIR` equal to `G3D_TERMD_DIR`, and
  unset `G3D_TERMD_SOCKET`; `atexit` and `Drop` stop every test daemon on exit.
- **Waiting.** Tests wait for state, never sleep for it: `agent_base::await_`
  polls a condition (a shell's printed marker, a screen, a status, a log
  line) and on timeout says what it waited for and what it last saw. A
  check that something did *not* happen waits for a later event that
  proves the moment has passed (a probe message from the same connection,
  a logged decision, a later line on the pane).
- **Opt-ins.** `GOMUXAI_KNOWN_BUGS=1` runs tests for known open bugs (the
  unsigned-replies one). `GOMUX_REAL_TRANSCRIPTS=<path>[:<path>...]` parses
  real transcripts on your machine as a smoke test. `AGENT_STORE_COMPAT_DB=<db>`
  runs `agent-store`'s manual database comparison test (`--ignored`).

## Known gaps

- Replies over the bus are unsigned (see the security model).
- One hub per bus network; the hub is trusted and can forge events.
- `state`, `queue`, and `revive` are per host and not served over the bus:
  `state get name@host` and `revive name@host` are refused (use `resume
  name@host` or run `revive` on that host), and `queue` refuses other hosts in
  favor of `send`.
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
- `agy` has no CLI flag or `--settings` key to name a conversation before its
  first prompt, so `agy` conversations start unnamed until their first clean
  idle `Stop` types `/rename <agent>` (deferred while messages are held, a
  dialog is up, or text sits on the input line).
- If `agy` has not flushed its closing `PLANNER_RESPONSE` transcript line
  within 200ms of firing `Stop`, `_result` for that turn stays empty; `agent
  logs` still reads it once flushed.
- An `agy` CLI started by hand in an existing agent pane and adopted by `agent
  revive` runs without spawn-wired hooks until restarted through `agent`: its
  status comes from screen scraping and messages are typed rather than held.
- On Linux systems where `systemd-oomd` monitors the root-owned user slice
  rather than user-owned cgroups, `ManagedOOMPreference=avoid` on user
  `g3d-termd` units is not honoured by `oomd` (`agent run` notes this once; the
  job's own `MemoryMax` + `MemorySwapMax=0` scope is what protects the
  terminal). On macOS, `agent run` records provenance only, without memory
  limits.
- A message over 10,000 characters that a hook hands Claude (a long typed
  one, or held ones) reaches the model as a file Claude saved, with a 2 KB
  preview: it reads the rest itself. A long message to a Claude CLI no hook
  reaches is typed whole and arrives as pasted content.
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
- A deliverer polls native status every 2s for as long as messages are
  held, e.g. while an agent sits at a prompt nobody answers.
- Custom backends aren't remembered after spawn: pass `--backend-config`
  again.
- The PreToolUse approval resolves bare `agent` on the hook's `PATH` (against
  `cwd`, refusing relative or `~` `PATH` entries, shell builtins, and root flags
  after the subcommand), which can still differ from an interactive shell if a
  shell profile aliases `agent` or defines a shell function by that name;
  commands through a variable (`$A send`) still prompt.
- Session discovery (`agent-discovery`) skips non-UTF-8 transcript filenames.
- On Linux, `agent-claudereg` reads `/proc/<pid>/stat` as UTF-8; if the
  kernel's 15-byte `comm` truncation splits a multi-byte UTF-8 process name on
  a reused PID, that registry entry falls back to `unproven` rather than
  `stale`.
- On macOS, starting a `g3d-termd`-hosted leaf bus that dials a hub using an
  `agent-busd` built without `make macos` (no `Info.plist`) currently prints
  the Local Network warning from both `agent_bus::daemon` and `cli::bus_macos`.
- `agent capture` reads managed agents only, not raw sessions.
- `approve`/`deny` can still hit a prompt answered in the milliseconds
  between the screen check and the keypress.
- Two data dirs on one machine can't both `bus start`: they share the
  `gomux-bus` session name and the default client port.
- `export` shows reported, not resolved, status (see [Export](#export));
  a store from a binary built before it has no events, and other hosts
  come only from `list`. An old binary writing into a store that has
  `agent_events` (a downgrade) records no events, and followers miss its
  changes.
- `runs`, `session_reports` (one row per Claude session and subagent the
  global hook heard) and `jobs` are pruned only by `agent manage prune`, which
  a person runs; nothing prunes them on its own.
- No harness records its model's context window, so `agent manage` reports
  context as a token count, not a share of the window.
- `agent manage`'s actions are local: op `manage` is read-only, so untracking,
  backing up and pruning another host's data means running the command there.
- Claude Code's session registry and hook payloads are its internal files,
  not a contract. A registry entry with a kind or status gomuxai doesn't
  know reads as `unknown` for that session until `agent-claudereg` learns it;
  a file that can't be read at all stops the readers from calling any
  session gone, and status falls back to the reports, saying why. Pane
  adoption needs a proven entry, so it waits while Claude records no
  comparable start time.
- `/bg` (Claude Code 2.1.325) exits the CLI in the agent's pane, whose
  `SessionEnd` makes the agent `ended (quit)`: `send` refuses it until a
  person runs `claude attach <id>` there (a pane client with no registry
  entry) and a hook from the background session reports. Hooks and
  senders follow the background session by its lineage; typing reaches it
  only through such a client.
- The sender falls back on `AGENT_NAME` where the processes show no agent
  (a process detached from its session, or under another agent binary's
  pane), so a name leaked into such a process still counts there.
- A Claude agent needs `agent hooks install` (spawn refuses without it). A
  Claude CLI that started before the install, or with the hooks removed,
  reports nothing and holds no messages until it is restarted; native
  status and pane adoption still see it.
- Pane adoption happens in a status check (`status`, `list`, `wait`, a
  `send`); `export` and `sessions` show the new session only after one.
- The open friction entries: `agent friction list`.

## History

In November 2026, `agent` was rewritten from Go to Rust (`rs/crates/*`) to sit
alongside `g3d-termd` and `g3d` and fold fleet status (`fleet`, `focus`,
`template`), memory-scoped heavy jobs (`run`), `revive`, and `skill` into the
one tool ([`docs/FLEET.md`](FLEET.md)), while preserving the CLI, SQLite store,
hook payloads, and bus wire contract. The embedded NATS server stayed in Go as
the `agent-busd` sidecar (`busd/`) because `nats-server/v2` (leafnode hub/spoke
routing with SPKI-pinned TLS and per-peer nkey accounts) has no pure-Rust
server equivalent.

The Go implementation the Rust tool replaced was removed once every host ran
the Rust build, along with the tests that proved a Go host and a Rust host work
together (a mixed bus, bus identities, a shared database, export JSON parity,
an allowlist differential). Both are in git history: commit `0c2eab5` is the
last with `legacy/`. What stays from the Go is the formats: the store's
schema, the bus's subjects and payloads, and the Go-style JSON, time and path
encoding in `agent_base::gostd`, which is what is on disk and on the wire.

- [`docs/FLEET.md`](FLEET.md): design of `fleet`, `focus`, `template`, `run`,
  `skill`, `revive`, session naming at launch, and `_result` for agy.
- [`docs/history/NATS_INTEGRATION_PLAN.md`](history/NATS_INTEGRATION_PLAN.md):
  the bus's design and revision record. The March JetStream design it
  describes (server, coordinator, streams, subjects) was removed; this file
  describes what was built.
- [`exploration/claude-hooks-findings.md`](../exploration/claude-hooks-findings.md),
  [`exploration/agy-hooks-findings.md`](../exploration/agy-hooks-findings.md):
  the probes behind the hook mapping, with payloads and firing order.
- [`exploration/tmux-tui-connector-ideation.md`](../exploration/tmux-tui-connector-ideation.md):
  mechanisms for sharing a TUI between agents and people.
- [`docs/DEVELOPMENT.md`](DEVELOPMENT.md): pre-commit checks, parallel
  worktree worker setup, rolling fleet installs, and public-repo hygiene.
- [`docs/SURVEY.md`](SURVEY.md): comparison with native CLI orchestration,
  peer session managers and protocols, local-model setup via
  `ANTHROPIC_BASE_URL`, and open design candidates.

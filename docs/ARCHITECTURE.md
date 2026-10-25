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
├── gomuxai.db               # agents, messages, tasks, state, command_log, friction
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
| `internal/tmux` | `TmuxOps` over `gotmux`: sessions, send keys, capture, pipe-pane. |
| `internal/natsbus` | The bus: contract (subjects, ops, wire types), identity and trust, the daemon's server config, RPC server and client, status events and `Wait`. `natstest` starts throwaway NATS servers for tests. |
| `internal/transcript` | Readers for Claude Code and agy session transcripts, into one `Session`/`Turn`/`Step` shape. |
| `internal/watcher` | The `agent watch` loop: delivers queued messages when their agent is idle. |
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
   `AGENT_NAME` ("" for a person). The record exists before the CLI starts,
   because hooks report against it immediately.
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

Notes on each:

- **Hooks** are wired only when `Config.AgentBinary` is set (it defaults to
  the running executable). Each hook runs
  `<binary> report --hook --as <name> --spawn-id <uuid>`. The uuid is made
  per spawn and stored as both `_spawn_id` and `_session_id`; Claude gets it
  as `--session-id`. Nothing is written into the agent's project. Claude
  registers 11 events (`claudeHookEvents`); agy registers `PreInvocation`,
  `PreToolUse`, `PostToolUse`, and `Stop`, each with `--event <Event>`
  because agy's payload omits the event name and runs hooks with the bundle
  dir as cwd. Each hook has a 10s timeout.
- **agy's launch command** is prefixed with
  `env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY`: with those set (a tmux
  server started over SSH passes them on) agy skips the OS keyring and comes
  up signed out.
- **The briefing** (`service/instructions.go`) tells the agent its name,
  host, and who started it, how to message, wait on, and read other agents,
  how to run helpers, and how to file friction. It uses the binary's full
  path, since the agent's shell may not have it on `PATH`.
- **Pre-allowed commands** (`agentSafeCommands`): `send`, `list`, `status`,
  `wait`, `events`, `state get`, `history`, `friction add`, `friction +1`,
  `friction list`, `friction show`. Each becomes two rules, `agent <cmd>` and
  `<full binary path> <cmd>`, for that Claude session only. Left out on
  purpose: `approve`, `deny`, `keys`, and `send --force` (one agent answering
  another's prompts), `spawn` and `cleanup` (a person approves each), and
  `friction accept`/`resolve` (for the people working on the tool). Rules
  match the literal command text, so `$A send` still prompts.
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
foreign key's `ON DELETE CASCADE`. The terminal log and the transcripts list
stay.

## Messages

`agent send` (`AgentService.Send`) types the message and Enter. A message
from an agent (`AGENT_NAME` set) is prefixed `[from <name>@<host>] `. Before
typing, `checkCanSend` refuses an `exited` agent (the text would run in the
shell left behind), a `waiting` agent, and any agent with a permission or
trust dialog on screen (the Enter would answer it). `--force` skips the
checks, and only works for a person at a terminal: it is refused when
`AGENT_NAME` is set or stdin isn't a TTY.

`agent queue` stores a `pending` row in `messages`. `agent watch`
(`internal/watcher`) delivers it: every `--interval` (2s) it checks each
agent that has pending messages and calls `Send` when the agent is `idle` or
`error` (a failed turn leaves the CLI at its prompt, and no hook fires again
until the next prompt). `processing` and `waiting` hold the message;
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
- **agy dialog promotion.** agy fires `PreToolUse` before it shows a
  permission dialog and nothing while it's up. When the report is
  `processing` from `PreToolUse` and `AgyBackend.PermissionPromptVisible`
  sees the dialog at the bottom of the screen, the status becomes `waiting`
  with detail `permission: ...`, so `approve`/`deny` (and `--expect`) work
  without `--force`.

`StatusInfo` also returns the last report as recorded (`ReportedStatus`,
`ReportedAt`), which `wait --next` uses to spot a whole turn between checks.

### Reports and the reserved state keys

`AgentService.Report` writes these keys for the agent in one transaction
(`SetStates`), so a reader never sees half of two concurrent reports:

| Key | Holds |
|---|---|
| `_status` | `idle`, `processing`, `waiting`, `error`, `exited` (or `unknown`) |
| `_event` | the hook event, `report` for `agent report`, `approved`/`denied` |
| `_detail` | one line of context, e.g. `Bash: go test ./...` or `permission: ...` |
| `_tool` | the tool the event concerns |
| `_tool_input` | the full JSON input of a call that may need permission (≤64 KiB); always written, so it never outlives its call |
| `_result` | Claude's last reply (`Stop`'s `last_assistant_message`); written only when set or cleared |
| `_session_id` | the CLI's session id; spawn sets it, hooks update it (`/clear`, `/resume`, agy's conversation id) |
| `_spawn_id` | the spawn's uuid, set once by spawn |
| `_transcript` | the CLI's transcript path from the last hook payload that named one |

Timestamps are stored with nanoseconds: a whole turn can fit in one second,
and `wait --next` tells reports apart by time. Every recorded report also
publishes a status event (see [Status events](#status-events-and-wait)).

`--spawn-id` guards against stale writers: a report or hook whose spawn id
isn't the agent's current `_spawn_id` is dropped. Without it, the
`SessionEnd` of the Claude that `cleanup` just killed could land on a
respawn of the same name.

Hook mode (`report --hook`) reads the payload on stdin, never writes stdout,
and exits 0 even when the report fails (errors go to stderr), so a reporting
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
| `PreToolUse`, `PostToolUse` | processing | detail `Tool: <most telling input>` |
| `PostToolUseFailure` | processing | detail `failed: ...` |
| `PermissionRequest` | waiting | detail `permission: ...`; stores `_tool_input` |
| `PreCompact` | processing | |
| `Notification` | idle | only `idle_prompt`; `permission_prompt` arrives ~6s late and is ignored |
| `Stop` | idle | sets `_result`; stays processing while background tasks (shells, subagents) are running, since their completion starts a new turn |
| `StopFailure` | error | e.g. API error, rate limit |
| `SessionEnd` | exited | ignored for `reason: clear` (a `SessionStart` follows) |

**agy** (`AgyBackend.ParseHookEvent`, `namedagents/agy.go`):

| Event | Status | Notes |
|---|---|---|
| `PreInvocation` | processing | |
| `PreToolUse` | processing | stores `_tool_input` for dialog promotion |
| `PostToolUse` | processing | `failed: ...` on error |
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
`idle`/`denied`), because a denied prompt fires no hook.

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

**`internal/transcript`** reads both into `Session` → `Turn` (a prompt) →
`Step` (one of thinking, reply, tool call with its result), and
`Session.Events` flattens that into the lines `agent logs` prints. Backends
hand out their reader through `backend.TranscriptProvider`. It was ported
from glyph3d-rs `native/src/agent_transcript/` (types, Claude's two-pass
`tool_use`/`tool_result` pairing, the agy step parser, format detection),
which itself replicates glyph3d-js's session adapter; the tail-window read
follows glyph3d-js `cli/sessions.go`. `ReadFile` reads the last 1 MiB first
and doubles the window until it holds enough events, plus 256 KiB of the
head for metadata, since transcripts run to tens of megabytes. The formats
are the CLIs' internal files, not APIs: lines that aren't JSON objects are
counted in `Session.Skipped`, and unknown records are ignored.

**`agent logs`** (`internal/cli/logs.go`) reads the newest transcript, going
back to older ones until it has `--lines` events, and prints
`--- session <id>` where each earlier session ends. It falls back to the
terminal log (with a hint saying why) when there's no transcript: another
backend, no hook fired yet, or the file is gone or unparseable. `--terminal`
asks for it directly. The terminal log is the raw pipe-pane stream, cleaned by
`util.TerminalLines` (escapes dropped, cursor moves turned into spaces and
line breaks, redraws collapsed). Logs are per host; `logs name@host` is
refused in favor of `capture`.

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
keeps them, since you asked.

| Op | Request → reply | CLI | Remote by default |
|---|---|---|---|
| `list` | `ListRequest` → `ListResponse` (agents with status and parent) | `list --host H` | yes |
| `status` | `StatusRequest` → `StatusResponse` (mirrors `StatusInfo`) | `status`, `wait` | yes |
| `capture` | `CaptureRequest` → `CaptureResponse` | `capture` | yes |
| `send` | `SendRequest` → none | `send` | yes |
| `respond` | `RespondRequest` (`approve`, `approve_always`, `deny`, `force`, `expect_tool`) → none | `approve`, `deny` | yes |
| `keys` | `KeysRequest` (text, then a key) → none | `keys name@host` | no |

`natsbus/ops_test.go` keeps `AllOps` and the server's op table in step. The
remote side runs the same service code as a local call (`Send` still
refuses dialogs; `RespondPermission` still checks the screen), and the CLI
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
whole turn between checks). Unless `exited` is in `--for`, the wait fails
as soon as the agent is `exited`. A timeout exits 2.

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
| `agents` | `name` PK, `session_name`, `backend`, `task_file`, `log_file`, `created_at`, `spawned_by` | one row per live agent |
| `messages` | `id`, `sender_id`, `receiver_id`, `content`, `status`, `created_at` | `pending`/`delivered`/`failed`; indexed on `(receiver_id, status)` |
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
`agents.spawned_by`). `addColumn` checks `pragma_table_info` first and
tolerates another process adding the column between the check and the
`ALTER TABLE`. A new column goes in both the `CREATE TABLE` (new databases)
and an `addColumn` call (old ones).

### Concurrency

Many short-lived processes write at once: every hook of every agent is its
own `agent report --hook`. The DSN sets `busy_timeout(5000)`,
`foreign_keys(1)`, and `synchronous(NORMAL)` on every pooled connection (a
one-off `PRAGMA` would reach only one), and `journal_mode = WAL` is set once,
since it's stored in the file. Multi-key writes that must be seen together
(`SetStates`) use a transaction.

### Command log

`Execute` records every run in `command_log` when it ends: args (each capped
at 300 bytes), duration, exit code, error, `AGENT_NAME`, cwd, and build id.
Help is recorded too (`recordCommand` opens the store itself when setup
didn't); `report --hook` (every tool call of every agent) and `bus run` (a
daemon) are not. Recording is best effort, bounded at 500ms. The log keeps
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
  `events`) goes through `printJSON`: indented, with `&`, `<`, `>` not
  HTML-escaped.
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
  pruning and friction queries; transcript parsing against fixtures in
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
- No multi-agent wait, and `wait --next` misses a turn that ends before the
  wait starts.
- `_result` is Claude-only; agy agents' replies are only in their
  transcripts.
- Long messages typed into a Claude TUI arrive as pasted content.
- Custom backends aren't remembered after spawn: pass `--backend-config`
  again.
- `agent capture` reads managed agents only, not raw sessions.
- `approve`/`deny` can still hit a prompt answered in the milliseconds
  between the screen check and the keypress.
- Two data dirs on one machine can't both `bus start`: they share the
  `gomux-bus` session name and the default client port.
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

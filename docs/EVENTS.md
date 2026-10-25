# Agent session events

gomuxai reads every agent session the host can see, whether gomuxai spawned it or not,
and serves what each one did as one stream of events. It is the only parser of the
harnesses' transcripts: `agent logs`, `agent transcript` and any display (a 3D viewer,
a script) read these events and never a transcript itself.

Events are one per thing that happened: a prompt, a thinking block, a reply, a tool call.
They are never pre-grouped. Each carries the turn (prompt) it belongs to, so a reader may
group them; none has to.

The Go types are in `internal/transcript/events.go`; the Session is
`discovery.Session` (`internal/discovery/sessions.go`).

## Commands

- `agent sessions [--json] [--harness h] [--workgroup|--local]`: every agent session on the
  host, newest first. That includes workgroup agents (spawned by gomuxai, with a name, parent
  and status) and local transcripts that no workgroup owns (a plain Claude Code, agy or Kimi
  session). Both kinds have the same fields; `workgroup` says which one a session is.
  `--json` prints an array of Sessions. `docs/ARCHITECTURE.md` ("Sessions") says where each
  field comes from and how status is decided.
- `agent transcript <name|session-id|path> [--follow] [--from <byte>] [--until <byte>]
  [--requests]`: one session's events, as JSON lines. The session is a workgroup agent's
  name (its current transcript), a session id or the start of only one, or a transcript's
  path. (`agent events` is the bus's status feed, not this.)
  - `--from` resumes after the line at a byte offset: the `at` of the last event the reader
    holds that isn't a provisional reply (see `say`). That line's events are not sent again
    (a line's events are always sent together); what follows is numbered and turned as the
    whole stream is, and updates to events of earlier lines still come. Such an update is
    sent whole even though its add lies before `--from`, where the reader may never have
    seen it: it is the event's current state either way. The parser reads the lines before
    to know them.
  - Under `--from`, `at` and `line` are absolute in the file, as in a whole read (the
    parser reads from byte 0 anyway), and `seq` is the whole stream's: the first event sent
    has the `seq` a whole read gives it, not 0.
  - `--until` leaves out the lines that start at or after a byte offset, so `--from A
    --until B` reads the lines after A's and before B: a history window. Not with
    `--follow`.
  - `--follow` keeps reading as the transcript grows, polling every 250ms and never
    reading a line that isn't whole yet, until interrupted or the reader closes the pipe
    (both exit 0).
  - `--requests` adds the model-request channel; it is off by default. Request events
    number their own `seq` from 0, so a conversation event has the same `seq` with or
    without it.
- `agent export` is unchanged: the set of agents as recorded now, plus `--follow` for changes
  to that set.

## Framing

JSON lines. The stream opens with `{"type":"session", …Session}`, describing the session as
of the end of the initial read. After that come `{"type":"event", …Event}` lines, plus a new
`session` line whenever the session's metadata changes after that (title, model, status:
any field but `size`, `mtime` and `status_at`; `--follow` checks every 2s).

Nothing is ever reordered. An event that changes later (a tool's result lands, a provisional
reply is confirmed) is sent again with the same `id` and `"op":"update"`: the whole event,
with the `at`, `line` and `ts` of the line that changed it. When one line adds an event and
updates an earlier one, the add comes first.

## Session

```jsonc
{ "session_id": "…", "harness": "claude|agy|kimi", "path": "…", "size": 0, "mtime": "…",
  "cwd": "…", "project": "…",  // the first cwd the session recorded, never the storage folder name
  "title": "…",                // the latest title the harness recorded (Claude: a custom title wins over an AI one)
  "agent_name": "…",           // the name the harness recorded for the session (Claude's agent-name)
  "model": "…", "version": "…", "git_branch": "…",
  "workgroup": { "name": "…", "parent": "…", "store": "…", "host": "…" },  // null if no workgroup owns it
  "status": "idle|processing|waiting|error|exited|unknown", "status_at": "…",
  "parent_session": { "session_id": "…", "tool_id": "…" },  // set on a subagent's session
  "continued_in": "…" }
```

- `session_id` is the harness's id: Claude's session uuid, agy's conversation id, Kimi's
  uuid (without `session_`). A subagent's is its own: Claude's `agent-<agent id>` (its
  transcript's name), agy's conversation id, Kimi's `<uuid>:<agent>`.
- `tool_id` is the call that started the subagent, where the harness records it (Claude's
  meta.json `toolUseId`); absent for agy and Kimi.
- `workgroup.parent` is absent when a person spawned the agent.
- A live workgroup agent whose CLI hasn't written a transcript yet (or keeps none) is listed
  with an empty `path` and `harness` set to its backend.

## Event

```jsonc
{ "id": "…",                      // stable: a tool's tool_use id; otherwise "<record uuid>#<block index>"
                                  // (no uuid: agy "step_<step_index>#<n>", Kimi "L<line>#<n>")
  "op": "add|update",
  "seq": 0,                       // 0-based position in the transcript's stream, the same in every
                                  // read of it (the request channel numbers its own)
  "at": 0, "line": 0,             // byte offset and line index of the source line
  "ts": "…",
  "turn": { "id": "…", "index": 0, "derived": false },  // the prompt this event belongs to
  "agent": "…",                   // the subagent id, on a subagent's own events (Claude: its agentId;
                                  // agy and Kimi, which record none: its session_id)
  "kind": "prompt|think|say|tool|system|request",
  "<kind>": { … } }               // exactly one, matching kind
```

### Kinds

- `prompt` `{text, origin, command?, images?}`: every message into the agent.
  - `origin` is one of `human`, `queued` (sent mid-turn), `delivered` (gomuxai's held
    delivery), `task-notification`, `peer`, `command` or `system` (the harness's own: a
    scheduled wakeup, an idle notice, a hook's feedback).
  - One message can hold several task notifications; each completes its own call.
  - A mid-turn message carries the current turn's id.
  - A slash command has `command: "/name"` and `text: "/name args"`. `images` is
    `[{media_type, data}]`, with the base64 data whole.
- `think` `{text?, redacted?}`: a redacted block (signature only) is still an event.
- `say` `{text, final?, provisional?}`: text the agent wrote.
  - `final` marks a turn's closing reply. A reply the harness gave no stop reason (older
    Claude Code) is marked final by an update once its turn ends with nothing more from the
    model.
  - A provisional final reply comes from the Stop hook before the transcript holds it. When
    the transcript's own copy lands, it is sent as an update of the same `id`. That id is
    `<promptId>#final`, and `<promptId>#final<n>` for the nth Stop of a turn (a held
    delivery continues it), which pairs with the turn's nth final reply
    (`transcript.Finals`).
  - `agent transcript` sends one for a Claude agent of the store it runs from (the hook's
    text is the agent's `_result`). It isn't in the transcript, so it takes no `seq` of its
    own: it carries the `seq` of the event after it. Its `at` and `line` are where the
    transcript's copy will land, so a reader doesn't resume `--from` it. A read that
    starts after the copy landed has no provisional reply, just the copy, as an add.
- `tool` `{name, input, result?, action, target?, detail?, meta?, sub_sessions?}`: one event
  per call.
  - `result` is `{status: pending|ok|error|interrupted|background, output?, structured?}`. A
    call is added as `pending`, with any `meta` the table reads from its input (an edit's
    line counts); its result, and the `meta` read from that, arrive as an update.
  - `action`, `target`, `detail` and `meta` come from the one tool table,
    `transcript.SummarizeTool` in `internal/transcript/tools.go`, whose doc lists each
    action's `meta`. The Claude and agy hooks' status details and `agent logs` read the same table.
  - Tools the table calls noise keep `action: "noise"`; readers may hide them.
  - A call that starts subagents names their sessions in `sub_sessions`, in the order its
    result gives them: one for Claude's Agent, one or more for agy's `invoke_subagent`.
    Each one's `parent_session` names the call.
- `system` `{subtype, text?, data?}`: harness records such as turn timing, compaction, API
  errors, interrupts and model fallbacks (`model_fallback`, `data: {from, to}`).
- `request` (only with `--requests`) `{model, params, base?, tail, blobs}`: what one model call
  was sent. Claude Code's `api-request` lines name message hashes that its `api-request-blob`
  lines hold, so this channel is indexable and searchable, and it is a snapshot of what the
  agent saw before it changed anything.

## Rules

- Turn ids come from the transcript (Claude's `promptId` and `turnPosition.promptIndex`) where
  it records them. Otherwise they are derived and marked `derived`; never a silent mix.
  - `promptIndex` counts human prompts, so a turn a task notification or a peer opens has
    its own id but the index of the human prompt before it. A record with a `promptId` and
    no `promptIndex` takes the last index seen in its file. A file that records no
    `promptIndex` at all (Claude Code 2.1.25x-26x) numbers its turns by counting human
    prompts, as a derived turn does.
  - A subagent's records hold the parent's current `promptId`: its events carry that turn,
    with the index the parent's transcript gives it, or the last index known.
  - An index of 0 means the transcript doesn't say (a subagent whose parent's transcript is
    gone, records before any prompt); a known turn's index is 1 or more.
  - A derived turn's id is the id of the prompt event that opened it, and its index counts
    such prompts from 1.
- `api-request*` lines are never conversation events, only `request` events, and only when asked.
- A subagent's transcript is its own session, linked both ways: `parent_session` on it, and
  `sub_sessions` on the call that started it.
- No text field is ever truncated. Readers decide what to show.
- `internal/transcript/testdata/corpus/` holds the conformance corpus: transcripts with the
  events they must produce. Every parser change passes it. Its README lists the cases and
  the decisions this page leaves open.

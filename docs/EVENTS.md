# Agent session events

gomuxai reads every agent session the host can see, whether gomuxai spawned it or not,
and serves what each one did as one stream of events. It is the only parser of the
harnesses' transcripts: `agent logs`, `agent events` and any display (a 3D viewer, a
script) read these events and never a transcript itself.

Events are one per thing that happened: a prompt, a thinking block, a reply, a tool call.
They are never pre-grouped. Each carries the turn (prompt) it belongs to, so a reader may
group them; none has to.

The Go types are in `internal/transcript/events.go`.

## Commands

- `agent sessions [--json]`: every agent session on the host. That includes workgroup
  agents (spawned by gomuxai, with a name, parent and status) and local transcripts that no
  workgroup owns (a plain Claude Code, agy or Kimi session). Both kinds have the same fields;
  `workgroup` says which one a session is.
- `agent events <name|session-id|path> [--json] [--follow] [--from <byte>] [--requests]`: one
  session's events, as JSON lines.
  - `--from` resumes at a byte offset: the `at` of the last event the reader holds.
  - `--requests` adds the model-request channel; it is off by default.
- `agent export` is unchanged: the set of agents as recorded now, plus `--follow` for changes
  to that set.

## Framing

JSON lines. The stream opens with `{"type":"session", …Session}`, describing the session as
of the end of the initial read. After that come `{"type":"event", …Event}` lines, plus a new
`session` line whenever the session's metadata changes after that (title, model, status).

Nothing is ever reordered. An event that changes later (a tool's result lands, a provisional
reply is confirmed) is sent again with the same `id` and `"op":"update"`: the whole event,
with the `at`, `line` and `ts` of the line that changed it. When one line adds an event and
updates an earlier one, the add comes first.

## Session

```jsonc
{ "session_id": "…", "harness": "claude|agy|kimi", "path": "…", "size": 0, "mtime": "…",
  "cwd": "…", "project": "…",  // from the cwd the session recorded, never the storage folder name
  "title": "…",                // the latest title the harness recorded (Claude: a custom title wins over an AI one)
  "agent_name": "…",           // the name the harness recorded for the session (Claude's agent-name)
  "model": "…", "version": "…", "git_branch": "…",
  "workgroup": { "name": "…", "parent": "…", "store": "…", "host": "…" },  // null if no workgroup owns it
  "status": "idle|processing|waiting|error|exited|unknown", "status_at": "…",
  "parent_session": { "session_id": "…", "tool_id": "…" },  // set on a subagent's session
  "continued_in": "…" }
```

## Event

```jsonc
{ "id": "…",                      // stable: a tool's tool_use id; otherwise "<record uuid>#<block index>"
                                  // (no uuid: agy "step_<step_index>#<n>", Kimi "L<line>#<n>")
  "op": "add|update",
  "seq": 0,                       // 0-based position in this stream (--requests numbers its own)
  "at": 0, "line": 0,             // byte offset and line index of the source line
  "ts": "…",
  "turn": { "id": "…", "index": 0, "derived": false },  // the prompt this event belongs to
  "agent": "…",                   // the subagent id, on a subagent's own events
  "kind": "prompt|think|say|tool|system|request",
  "<kind>": { … } }               // exactly one, matching kind
```

### Kinds

- `prompt` `{text, origin, command?, images?}`: every message into the agent.
  - `origin` is one of `human`, `queued` (sent mid-turn), `delivered` (gomuxai's held
    delivery), `task-notification`, `peer`, `command` or `system`.
  - A mid-turn message carries the current turn's id.
  - A slash command has `command: "/name"` and `text: "/name args"`. `images` is
    `[{media_type, data}]`, with the base64 data whole.
- `think` `{text?, redacted?}`: a redacted block (signature only) is still an event.
- `say` `{text, final?, provisional?}`: text the agent wrote.
  - `final` marks a turn's closing reply.
  - A provisional final reply comes from the Stop hook before the transcript holds it. When
    the transcript's own copy lands, it is sent as an update of the same `id`.
- `tool` `{name, input, result?, action, target?, detail?, meta?, sub_session?}`: one event
  per call.
  - `result` is `{status: pending|ok|error|interrupted|background, output?, structured?}`. A
    call is added as `pending`; its result, and the `meta` read from it, arrive as an update.
  - `action`, `target`, `detail` and `meta` come from the one tool table.
  - Tools the table calls noise keep `action: "noise"`; readers may hide them.
  - A call that starts a subagent names its session in `sub_session`.
- `system` `{subtype, text?, data?}`: harness records such as turn timing, compaction, API
  errors and interrupts.
- `request` (only with `--requests`) `{model, params, base?, tail, blobs}`: what one model call
  was sent. Claude Code's `api-request` lines name message hashes that its `api-request-blob`
  lines hold, so this channel is indexable and searchable, and it is a snapshot of what the
  agent saw before it changed anything.

## Rules

- Turn ids come from the transcript (Claude's `promptId` and `turnPosition.promptIndex`) where
  it records them. Otherwise they are derived and marked `derived`; never a silent mix.
  - `promptIndex` counts human prompts, so a turn a task notification or a peer opens has
    its own id but the index of the human prompt before it. A record with a `promptId` and
    no `promptIndex` takes the last index seen in its file, or 0.
  - A derived turn's id is the id of the prompt event that opened it, and its index counts
    such prompts from 1.
- `api-request*` lines are never conversation events, only `request` events, and only when asked.
- A subagent's transcript is its own session, linked both ways: `parent_session` on it, and
  `sub_session` on the call that started it.
- No text field is ever truncated. Readers decide what to show.
- `internal/transcript/testdata/corpus/` holds the conformance corpus: transcripts with the
  events they must produce. Every parser change passes it. Its README lists the cases and
  the decisions this page leaves open.

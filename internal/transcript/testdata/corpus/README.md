# Event stream conformance corpus

Transcripts, and the exact events [`docs/EVENTS.md`](../../../../docs/EVENTS.md) says
each one must produce. Every parser change passes it.

The transcripts are synthesized. Their record shapes (record types, field names, key
order, nesting) are modelled on real Claude Code 2.1.310, agy and Kimi Code transcripts.
All content is made up. The expected events were written by hand from the contract, not
taken from any parser's output. Only the bookkeeping was computed: `seq`, `at`, `line`,
and the session line's `size`.

## Layout

```
<harness>/<case>.events.jsonl            the expected stream: a session line, then event lines
<harness>/<case>.requests.events.jsonl   the same stream with --requests (only where it differs)
claude/<case>.jsonl                      a Claude Code transcript
claude/<case>/subagents/agent-<id>.jsonl     a subagent's own transcript, plus its .meta.json
claude/<case>/subagents/agent-<id>.events.jsonl
agy/<case>/<conversation-id>/.system_generated/logs/transcript_full.jsonl
kimi/<case>/session_<id>/agents/main/wire.jsonl, and state.json beside agents/
```

agy and Kimi cases keep the harness's own directory layout below `<case>/`, because
those harnesses record the session id only in the path. The session line's `path` names
the transcript, relative to this directory.

## Comparing

Compare parsed JSON values, not bytes:

- A field that is absent equals its zero value (`false`, `0`, `""`, `null`, `{}`, `[]`).
  So `"derived": false` and a missing `derived` are the same.
- Session lines leave out `mtime`, `status` and `status_at`. They come from the file
  system and the workgroup, not the transcript, so the test does not compare them.

## Cases

| Case | What it exercises | Modelled on |
|---|---|---|
| `claude/basic-turn` | A typed prompt, a redacted thinking block (signature only) and one with text, a reply, a noise tool (ToolSearch), a Bash call and its result, the final reply, turn timing. `ai-title` then `custom-title` then `ai-title`: the custom title wins. | `user` with `promptId`, `promptSource: typed`, `origin.kind`, `turnOrigin`, `turnPosition{promptIndex,turnIndex}`; `assistant` one block per record sharing `message.id`, `thinking{thinking,signature}`, `tool_use{id,name,input,caller}`; `user` `tool_result` with `toolUseResult{stdout,stderr,interrupted,isImage,noOutputExpected}` and `sourceToolAssistantUUID`; `system` `turn_duration`; `file-history-snapshot`, `last-prompt`, `ai-title`, `custom-title`, `mode`, `permission-mode`. |
| `claude/mid-turn-queued` | A message sent mid-turn and taken at a tool boundary: a `queued` prompt in the current turn. A second message queued as the turn ends, which opens its own turn: a `queued` prompt with its own turn id. An Edit with a `structuredPatch`. | `queue-operation` `enqueue`/`remove`/`dequeue` with `content`; `attachment` `queued_command{prompt,commandMode: prompt,origin{kind: human},humanTurn}` with `rendered`; `user` with `promptSource: queued`. |
| `claude/held-delivery` | gomuxai's held delivery: Stop's `additionalContext` after a final reply is a `delivered` prompt in the same turn, and the reply to it is a second final reply. Two `stop_hook_summary` records. | `attachment` `hook_success` and `hook_additional_context{content[],hookEvent: Stop,toolUseID}`; `system` `stop_hook_summary{hookCount,hookInfos,hookErrors,hookAdditionalContext,preventedContinuation,stopReason,hasOutput,toolUseID}`. |
| `claude/notification-and-commands` | A background Bash (`background`), then its task notification, which opens a turn (`task-notification`) and completes the call (`ok`). A local slash command (`/context`) with its output record. A slash command that expands into a model turn (`/review internal/server`). | `toolUseResult.backgroundTaskId`; `user` with `origin{kind: task-notification,producer}`, `promptSource: system`, `turnOrigin: task_notification`, `<task-notification>` tags `task-id`, `tool-use-id`, `output-file`, `status`, `summary`; `<local-command-caveat>` (`isMeta`); `<command-name>`/`<command-message>`/`<command-args>` in both tag orders; `system` `local_command{commandRun}`; the skill body as an `isMeta` `turnCompanion` user record; `attachment` `command_permissions`. |
| `claude/requests` | `api-request-shape`, `api-request-blob` and `api-request` lines produce nothing in the plain stream. With `--requests` each `api-request` is a `request` event that carries the blobs its `tail` names. | `api-request{id,base,params,tail,keep,shapeHash,querySource,cacheControl,requestDigest}`, `api-request-blob{hash,message}`, `api-request-shape{shapeHash,shape{system,tools}}`. |
| `claude/subagent` (+ `subagent/subagents/agent-a3f9c2e17b4d5e608`) | An Agent call that launches a background subagent: `background`, with `sub_session`. The subagent's own transcript as its own session, with `parent_session` and `agent` on every event. The task notification that ends it fills the call's result and `meta`. | `toolUseResult{isAsync,status: async_launched,agentId,outputFile,...}`; `<task-notification>` with `result`, `note` and `usage{subagent_tokens,tool_uses,duration_ms}`; subagent records with `isSidechain: true`, `agentId`, the parent's `sessionId` and `promptId`; `agent-<id>.meta.json{agentType,toolUseId,...}`; WebFetch `toolUseResult{bytes,code,codeText,result,durationMs,url}`. |
| `claude/tool-results` | MultiEdit with `edits[]`. Three calls in one message whose results land after all three calls (pending, then updates). A partial Read (`meta.range`). A tool error. An interrupted call that never gets a result. | MultiEdit's input and `toolUseResult{filePath,edits,originalFileContents,structuredPatch,userModified}` (from the versions that had MultiEdit; 2.1.310 has none); Read `toolUseResult{type,file{filePath,content,numLines,startLine,totalLines}}`; an error `tool_result` with `is_error: true` and a string `toolUseResult` (`Error: ...`); the `[Request interrupted by user for tool use]` text record, with no `tool_result` for the call. |
| `claude/system-records` | Auto-compaction mid-turn: the boundary and the summary, both in the current turn. `stop_hook_summary`, `turn_duration`. An API error. | `system` `compact_boundary{compactMetadata,logicalParentUuid}` with `parentUuid: null`; `user` with `isCompactSummary` and `isVisibleInTranscriptOnly`; an `assistant` record with `message.model: <synthetic>`, `isApiErrorMessage`, `error`, `apiErrorStatus`. |
| `claude/derived-turns` | An older transcript with no `promptId`, `turnPosition`, `origin` or `promptSource`: turn ids are derived (`derived: true`). One record with two blocks (`#0`, and the tool's id). A plain interrupt. | The same envelope as 2.1.310 minus the turn fields (version 2.0.x); a leading `summary` record. |
| `claude/session-meta` | A prompt with an image. `ai-title` written twice (the latest wins), `agent-name`, `continued-in`. | A `user` content array `[text, image{source{type: base64,media_type,data}}]` with `imagePasteIds`; `agent-name{agentName}`; `continued-in{continuedInSessionId}`. |
| `agy/conversation` | A USER_INPUT prompt. A step with three tool calls, each paired with its own GENERIC result in order (the existing parsers keep only the last). A command sent to the background (`RUNNING`), whose completion arrives later in a SYSTEM_MESSAGE. `ask_question`. `send_message`, and a peer's reply. | `transcript_full.jsonl` steps `{step_index,source,type,status,created_at,content,thinking,tool_calls[{name,args}],input_tokens,cache_read_tokens,output_tokens}`; `<USER_REQUEST>` with `<ADDITIONAL_METADATA>` and `<USER_SETTINGS_CHANGE>` (Model Selection); GENERIC content led by `Created At:`/`Completed At:`; view_file's `File Path:`/`Total Lines:`/`Showing lines A to B` header; `[Message] timestamp=… sender=… priority=… content=…`; `Task id "…" finished with result:`. |
| `kimi/interleaved` | One turn whose thinking, text and tool calls interleave across steps. Events come out in wire order (glyph3d-rs groups all thinking, then tools, then text: the bug this case catches). | `wire.jsonl`: `metadata{protocol_version,created_at}`, `turn.prompt{input[]}`, `context.append_message`, `llm.request{provider,model,modelAlias}`, `context.append_loop_event{time,event}` with event types `step.begin`, `content.part{part{type: think|text, think|text}}`, `tool.call{toolCallId,name,args,display}`, `tool.result{toolCallId,result{output}}` and `step.end`, `usage.record`; `state.json{title,workDir,lastPrompt}`. Shapes come from the format survey in glyph3d-rs `kimi.rs` and glyph3d-js's Kimi tests; no Kimi sessions were on the survey host. |

## Decisions

Where EVENTS.md left a choice open, the corpus makes one. The ones that change what a
parser emits are also written into EVENTS.md. The tool-table agent and the coordinator
should check the rest.

**Stream**

- `seq` is the event's 0-based position in the stream it is in. The `--requests` stream
  numbers its own events, so the same event can have a different `seq` in each.
- An update is the whole event again with the same `id`, `turn` and `kind`. Its `at`,
  `line` and `ts` are those of the line that caused it.
- When one line both adds an event and updates an earlier one, the add comes first: a
  task notification, then the background call it completes; an interrupt, then the call
  it stops.
- The opening session line describes the session as of the end of the initial read. A
  static file therefore has exactly one session line. Later session lines come only from
  `--follow`.

**Ids and turns**

- Event ids: Claude's are `<record uuid>#<block index>`, and a tool's is its `tool_use`
  id. A `request`'s id is its `api-request` line's `id`, which `base` refers to. agy
  records have no uuid, so its ids are `step_<step_index>#<n>`, with `n` counting from 0
  over the parts the step has, thinking before content; agy tool ids are `step_<step_index>_tool_<j>`, as the
  existing parsers make them. Kimi's ids are `L<line index>#0`; its tool ids are
  `toolCallId`.
- Claude turns: the id is the record's `promptId`, and the index is
  `turnPosition.promptIndex`. Records without a `promptId` (assistant, attachment and
  system records) take the current turn. A record whose `promptId` has no `promptIndex`
  (a local slash command, a task notification from older versions, a subagent's records)
  takes the last `promptIndex` seen in the same file, or 0 if there is none.
- `promptIndex` counts human prompts. A turn opened by a task notification or a peer gets
  its own `promptId`, but shares the index of the human prompt before it.
- Derived turns (no `promptId`: older Claude, agy, Kimi): the id is the `id` of the prompt
  event that opened the turn, and the index counts turn-opening prompts from 1, as
  `promptIndex` does. Prompts that open a derived turn are a typed prompt, a slash command
  and an idle-time task notification (Claude), `USER_INPUT` (agy), and `turn.prompt`
  (Kimi). Messages that arrive mid-turn stay in the current turn.
- A subagent's events carry the parent's turn id (its records hold the parent's
  `promptId`) with index 0, since its file holds no `promptIndex`.

**Prompts**

- `queued_command` attachments (`commandMode: prompt`) are mid-turn messages: origin
  `queued`, or `peer` when their `origin.kind` is `peer` or `channel`. A `user` record with
  `promptSource: queued` is `queued` too. `queue-operation` lines are never events: they
  repeat what the prompt record holds.
- `hook_additional_context` from Stop is `delivered` when its text starts with gomuxai's
  head line (`A message arrived while you were working:` or `N messages arrived while you
  were working, oldest first:`); other hook context is origin `system`. The text is the
  attachment's `content` joined with `\n`, head line included.
- Slash commands: `command` is `/name`, and `text` is `/name args` (just `/name` with no
  args). The command markup wins over the record's `origin.kind: human`. The
  `<local-command-caveat>` record and a skill's expanded body (`isMeta`, `turnCompanion`)
  are not events. A `user` record with `isMeta` and no `origin` is never an event; one
  with an `origin` (a peer or channel message) is.
- A subagent's first prompt, the task its parent gave it, has origin `peer`.
- `images` is `[{media_type, data}]`, with the base64 data whole.
- agy: the prompt is the text inside `<USER_REQUEST>`, trimmed. Each `[Message]` entry
  of a SYSTEM_MESSAGE is a prompt: `task-notification` when the sender is a `…/task-N`,
  `peer` when it is another conversation, `system` with no header. The text is the
  entry's `content=` part, trimmed.

**Replies and thinking**

- `final` is set on the last text block of an assistant message whose `stop_reason` is
  `end_turn`. A turn that a held delivery continues has two final replies, one per Stop.
- agy: a PLANNER_RESPONSE with content and no tool calls is final.
- Kimi: `final` is never set. Its wire records no stop reason we have seen; `step.end`
  of a step without tool calls is a candidate, to be confirmed on real Kimi sessions.
- Claude thinking with an empty `thinking` is `{redacted: true}`. Empty agy or Kimi
  thinking emits nothing.

**Tools**

- `action`, `target`, `detail` and `meta` are what the tool table
  (`internal/transcript/tools.go`, `SummarizeTool`) says for the call's name, input and
  structured response. Every tool event in the corpus was checked against it. Where the
  table differs from glyph3d-js's `toolRegistry.js` on purpose, the corpus follows the
  table. Earlier drafts of this corpus followed glyph3d-js, and changed here:
  - An edit's add carries `meta: {added, removed}` counted from its input (old_string and
    new_string, or a MultiEdit's every edit). The result's `structuredPatch` replaces it
    in the update.
  - A call with nothing to count has no `meta`, never zeros. A `background` Bash has none:
    while a call runs, the reader hands the table no response.
  - Noise tools (`ToolSearch` in `claude/basic-turn`) keep `action: "noise"` and their
    detail.
- A call is added with `result: {status: "pending"}`. Its result comes in the update, with
  the `meta` the table reads from that result.
- `output` is the `tool_result` content (a string, or its text blocks joined with `\n`).
  `structured` is the record's `toolUseResult` when that is an object; a string
  `toolUseResult` (`Error: …`) repeats the output and is left out.
- Status: `is_error` makes `error`. `toolUseResult.status: async_launched` or a
  `backgroundTaskId` makes `background`. A call still pending at an
  `[Request interrupted by user for tool use]` record becomes `interrupted`, with no
  output.
- A task notification whose `<tool-use-id>` names a `background` call completes it.
  `<status>` `completed` makes `ok`, `failed` makes `error`, `killed` or `stopped` makes
  `interrupted`. The output is the `<result>` text, or the `<summary>` when there is no
  result. `structured` is dropped. For an Agent call, `meta` is `{tools, tokens, ms}` from
  `<usage>`, the same keys the table's `taskMeta` gives a finished Task.
- `sub_session` is set from the Agent result's `agentId`, so it arrives with the first
  update. A subagent's `session_id` is its `agentId` (the file is `agent-<agentId>.jsonl`).
- The response a reader hands the table: Claude's `toolUseResult` when it is an object
  (a string one reads as none, so a failed Bash has no `meta`). For an Agent completed
  by a task notification, `{totalToolUseCount, totalTokens, totalDurationMs}` from
  `<usage>`. Kimi's `result`, plus `file: {numLines, startLine}` for Read, as glyph3d-js's
  `kimiResponse` builds it. For agy, what glyph3d-js's `agyEnrich` reconstructs:
  `{output, stdout}` for `run_command`, and `{output, file}` from the `Showing lines` /
  `Total Lines` header for `view_file`. `target` is relative to the session cwd; Kimi's
  `path` names the file, and `input` keeps Kimi's own key.
- agy: output is the GENERIC content without its leading `Created At:`/`Completed At:`
  lines, trimmed; there is no `structured`. A `RUNNING` result is `background`. Its
  completion (`Task id "…" finished with result:`) makes it `ok`, with that text as output.
- agy: the corpus models `transcript_full.jsonl`, whose tool args are plain JSON (strings
  as strings, `AllowMultiple` a boolean), so `input` is the args as recorded. agy's other
  log, `transcript.jsonl`, JSON-encodes every string arg a second time and writes
  `AllowMultiple` as `"true"`/`"false"`. But it also truncates long fields
  (`truncated_fields`; 34 of 49 conversations on the survey host). Readers take
  `transcript_full.jsonl`. A reader that meets a string arg that is itself a JSON string
  should decode it once, so `input` carries the decoded value either way.

**System events**

- Every Claude `type: system` record is a `system` event. `subtype` is the record's;
  `text` is its `content` string; `data` is every other field except the envelope
  (`parentUuid`, `logicalParentUuid`, `isSidechain`, `uuid`, `timestamp`, `userType`,
  `entrypoint`, `cwd`, `sessionId`, `session_id`, `version`, `gitBranch`, `slug`,
  `isMeta`, `level`, `sessionKind`).
- Other system events: a compaction summary (`isCompactSummary`) is
  `{subtype: compact_summary, text}`; an API error (`isApiErrorMessage`) is
  `{subtype: api_error, text, data: {error, apiErrorStatus}}`, not a `say`; and
  `[Request interrupted by user]` / `[Request interrupted by user for tool use]` is
  `{subtype: interrupt, text}`.
- Attachments other than `queued_command` and `hook_additional_context` are not events.

**Requests**

- `request` is `{model: params.model, params, base?, tail, blobs}`. `blobs` maps each
  hash in `tail` to the message of the `api-request-blob` line that defined it.
  `api-request-shape` produces nothing; the shape is not part of `request`.

**Session**

- `title`: Claude's latest `custom-title` if there is one, else its latest `ai-title`.
  Kimi's comes from `state.json` (`title`, else `lastPrompt`). agy records none.
- `model`, `version`, `git_branch` and `cwd` are the latest values recorded; Claude's
  `<synthetic>` model is skipped. `project` is the last element of `cwd`.
- agy: `session_id` is the conversation directory's name, `cwd` is the first
  `run_command`'s `Cwd`, and `model` is the latest `Model Selection` change in a
  USER_INPUT. Kimi: `session_id` is the `session_<id>` directory's id, `cwd` is
  `state.json`'s `workDir`, and `model` is the latest `llm.request` model.
- `agent_name` comes from Claude's `agent-name` record (latest), and `continued_in` from
  `continued-in`.
- `ts` is the record's own timestamp string. Kimi's epoch milliseconds are written as
  RFC 3339 UTC with milliseconds.

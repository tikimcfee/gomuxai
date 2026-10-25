# Delivering messages to a busy `agy` agent through its hooks

**Date:** 2026-10-24
**CLI version tested:** `Antigravity CLI 1.4.7`, model Gemini 4 Argon (High),
global `toolPermission: always-proceed`.
**Question:** can `deliver` hand held messages to a busy agy agent from its
hooks, the way Claude's Stop hook `additionalContext` does, and what does a
person watching the pane see?

Companion to `agy-hooks-findings.md` (hook events, payloads, firing order).

---

## Summary

| Hook output | Model acts on it? | Shown in the TUI? | Mid-turn? | Verdict |
| :--- | :--- | :--- | :--- | :--- |
| `Stop` `{"decision":"continue","reason":"..."}` | Yes | **No.** Only the model's answer appears | n/a (turn end) | Works, but invisible to a person |
| `PreInvocation` `injectSteps` `userMessage` | Yes | **Yes**, as a normal `> ...` prompt | Yes (`invocationNum >= 1`) | Good, but misses messages that arrive during the last model call |
| `PreInvocation` `injectSteps` `systemMessage` | Yes | No | Yes | Invisible |
| `PreInvocation` `injectSteps` `ephemeralMessage` | Yes | No | Yes | Invisible, and dropped after the turn |
| `SessionStart` `injectSteps` `userMessage` | Yes | Yes, as a prompt | First prompt of a session only | Not useful for delivery |
| `PostInvocation` `injectSteps` alone | **No.** The turn ends with the message unanswered | Yes, as a prompt | | Trap: looks delivered but isn't answered until the next turn |
| **`PostInvocation` `injectSteps` + `terminationBehavior: force_continue`** | **Yes, in the same turn** | **Yes**, as a prompt | **Yes** | **Best: the recommended delivery point** |
| `PostInvocation` `force_continue` alone | The model invents output | (extra replies) | | Harmful: never use it without injected steps |
| `PostToolUse` / `Stop` with `injectSteps` | | | | Rejected (unknown field), logged only |

Turns that end without `Stop`: **Esc and Ctrl+C fire no hook at all**, not
even `PostToolUse`/`PostInvocation` for the running step, so gomuxai stays
`processing`. The only signals are the screen and agy's own log
(`Cancelling conversation <id>`). See section 4.

---

## Setup

- `make build`; agents spawned with `./bin/agent spawn agyprobe-tN --backend agy`
  from a scratch git repo `/tmp/agyprobe-ws`.
- **Probe hooks next to gomuxai's:** the workspace's own
  `/tmp/agyprobe-ws/.agents/hooks.json` registers all six events under the
  name `probe`. agy merges it with the `--add-dir` bundle gomuxai writes
  (`cli.log`: `loaded 2 named hooks from 2 hooks.json file(s)`), so both run on
  every event and gomuxai's status reports keep working. Nothing in the repo is
  touched.
- **Probe script** (`probe.sh <Event>`): appends the event and stdin payload to
  a log; if `/tmp/agyprobe-ctl/<Event>@<invocationNum>.json` or
  `<Event>.json` exists, prints it once and deletes it (one-shot, like CF's
  Claude probe), else prints nothing. A `<Event>.sticky.json` is printed
  without being deleted.
- agy's own log, `~/.gemini/antigravity-cli/log/cli-<timestamp>.log` (one per
  agy process; `cli.log` links to the newest), records every hook failure.
- The true output schema is in agy's embedded hook docs (`strings` on the
  binary). Abridged:
  - `injectSteps` (SessionStart, PreInvocation, PostInvocation): a list of
    `{"systemMessage": {"systemMessage": "...", "metadata": {...}}}`
    (nested object), `{"userMessage": "..."}` (plain string), or
    `{"ephemeralMessage": "..."}` (plain string; "transient message dropped
    after the current turn").
  - PostInvocation also takes `terminationBehavior`: `"force_continue"`,
    `"terminate"`, or `""`.
  - Stop takes only `decision` (`"continue"` blocks the stop) and `reason`
    ("Injected as a system message if continuing").
  - PostToolUse "expects an empty JSON object `{}`".

Screens below are `agent capture` output, trimmed to the turn, with the host
replaced by `hub`.

---

## 1. Stop `{"decision":"continue","reason":"..."}`

**Hook output:**
```json
{"decision":"continue","reason":"[from agyprobe@hub] New message while you were busy: please reply with the codeword PINEAPPLE-7 on its own line."}
```

**What the model did:** it acted on it. After its first reply (`READY`), agy
ran another `PreInvocation` → `PostInvocation` → `Stop` cycle
(`invocationNum: 2`, `executionNum: 1`) and the model replied `PINEAPPLE-7`.

**Screen:** the reason text **never appears**. A person sees two replies
in a row and no hint why the second one happened:
```text
> [from agyprobe@hub] Reply with just the word READY.

  READY

▸ Thought for 4s, 143 tokens

  PINEAPPLE-7
```
Expanding the thought (ctrl+o) shows only the model's reasoning ("A new
message interrupts the current operation; the codeword `PINEAPPLE-7` is the
required response..."). It doesn't read as an error. It doesn't read as
anything: the message is invisible.

**Transcript:** it is stored as a system message, not a user prompt:
```json
{"source":"SYSTEM","type":"SYSTEM_MESSAGE","content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user. ...\n<SYSTEM_MESSAGE>\nStop hook blocked termination: [from agyprobe@hub] New message while you were busy: ...\n</SYSTEM_MESSAGE>"}
```

**gomuxai status:** the continued cycle's `PreInvocation` came 10 ms after
the first `Stop`. Polling `agent
status` every 0.4 s showed `processing` throughout, then `idle` after the
second Stop. gomuxai's own Stop hook does report `idle` at the first Stop,
so an event-driven `wait` can see a ~10 ms false idle.

**Other Stop output?** No cleaner option exists. Stop's output is parsed
strictly (protojson): adding any other field rejects the whole output. With
`{"decision":"continue","reason":"stop-reason-text","injectSteps":[...]}`
the turn simply ended, nothing was shown, and `cli.log` said:
```text
E... stophooks.go:62] failed to call custom stop hook jsonhook__probe_Stop_0_0: failed to unmarshal result from hook jsonhook__probe_Stop_0_0 via protojson: {...}: proto: (line 1:52): unknown field "injectSteps"
```

---

## 2. `injectSteps` from `PreInvocation` and `SessionStart`

All three tests ran on a turn whose prompt was "Reply with just the word
READYn", with the probe on `PreInvocation@0` (the turn's first model call).

### `ephemeralMessage`
```json
{"injectSteps":[{"ephemeralMessage":"[from agyprobe@hub] EPHEMERAL: also include the codeword MANGO-11 in your reply."}]}
```
The model replied `READY2` / `MANGO-11`. **Not shown** in the TUI. Transcript:
`{"source":"SYSTEM_SDK","type":"EPHEMERAL_MESSAGE","content":"[from agyprobe@hub] EPHEMERAL: ..."}`.

### `systemMessage`
```json
{"injectSteps":[{"systemMessage":{"systemMessage":"[from agyprobe@hub] SYSMSG: also include the codeword LIME-31 in your reply."}}]}
```
The model replied `READY4` / `LIME-31`. **Not shown.** Transcript: a
`SYSTEM_MESSAGE` wrapped as "The following system message was injected by a
PreInvocation hook: ...".

### `userMessage`: visible
```json
{"injectSteps":[{"userMessage":"[from agyprobe@hub] USERMSG: also include the codeword KIWI-23 in your reply."}]}
```
The model replied `READY3` / `KIWI-23`. **Shown as a prompt of its own**:
```text
> [from agyprobe@hub] Do not run any commands; answer right here. Reply
  with just the word READY3.

────────────────────────────────────────────────────────────
> [from agyprobe@hub] USERMSG: also include the codeword KIWI-23 in your reply.

  READY3
  KIWI-23
```
Transcript: `{"source":"SYSTEM_SDK","type":"USER_INPUT","content":"<USER_REQUEST>\n[from agyprobe@hub] USERMSG: ...\n</USER_REQUEST>..."}`.
From then on, hook payloads carry it as `lastUserInput`. `agent logs` lists
it as a `user` line.

### Mid-turn (between tool calls)
Probe on `PreInvocation@1`, the model call after a tool result:
```json
{"injectSteps":[{"userMessage":"[from agyprobe@hub] MIDTURN: when you finish, also include the codeword PLUM-47 in your reply."}]}
```
```text
> [from agyprobe@hub] Run the shell command 'sleep 2; echo step-one' with
  your run_command tool (not agent send), then reply with DONE5.

● Bash(sleep 2; echo step-one)
  ⎿  Command exited with code 0.
            Command output:
            step-one

────────────────────────────────────────────────────────────
> [from agyprobe@hub] MIDTURN: when you finish, also include the codeword
  PLUM-47 in your reply.

  DONE5
  PLUM-47
```
It works mid-turn and is visible. The same holds for the other two step
types (they're invisible either way).

### `SessionStart`
Fires only once, on a session's first prompt (not before every turn, despite
what `agy-hooks-findings.md` says; it didn't fire on later turns here). Its
`userMessage` was acted on (`DONE MELON-5`). It's useless for delivery.

### `PostToolUse`
`{"injectSteps":[...]}` from PostToolUse is rejected: `post-tool hook
"jsonhook__probe_PostToolUse_0_0" failed: ... unknown field`. The tool's
result is unaffected and nothing reaches the model.

---

## 3. `PostInvocation` and `terminationBehavior: force_continue`

`PostInvocation` fires after every model call and its tools, **including the
last one before `Stop`**, so it covers both mid-turn and end-of-turn
delivery.

**`injectSteps` alone** (on a turn with no tool call):
```json
{"injectSteps":[{"userMessage":"[from agyprobe@hub] POSTINV: reply with the codeword DATE-59."}]}
```
The message is shown and stored, but **the turn ends right after it**. Stop
fires, gomuxai goes idle, and the model doesn't answer:
```text
> [from agyprobe@hub] Do not run any commands; ... READY7.

  READY7

────────────────────────────────────────────────────────────
> [from agyprobe@hub] POSTINV: reply with the codeword DATE-59.

────────────────────────────────────────────────────────────────────────────────
>
```
The model only got to it in the next turn, when it replied `READY8` / `DATE-59`.

**`injectSteps` + `force_continue`:** clean delivery, in the same turn:
```json
{"injectSteps":[{"userMessage":"[from agyprobe@hub] POSTINV-FC: reply with the codeword PEAR-61."}],"terminationBehavior":"force_continue"}
```
```text
> [from agyprobe@hub] Do not run any commands; ... READY8.

  READY8
  DATE-59

────────────────────────────────────────────────────────────
> [from agyprobe@hub] POSTINV-FC: reply with the codeword PEAR-61.

  PEAR-61
```
Hooks: `PostInvocation(0)` → `PreInvocation(1)` → `PostInvocation(1)` →
**one** `Stop`. Unlike Stop `continue`, there's no intermediate Stop, so
gomuxai stays `processing` the whole time.

Repeated live on a fresh agent (`agyprobe-t3`), after a tool call and with a
two-line message:
```json
{"injectSteps":[{"userMessage":"[from agyprobe@hub] Two-line held message.\nSecond line: when you finish, include the codeword QUINCE-12."}],"terminationBehavior":"force_continue"}
```
```text
> [from agyprobe@hub] Run the shell command 'echo tool-a' with your
  run_command tool, then reply DONE18. Answer here, do not use agent send.

● Ran (echo tool-a) (ctrl+o to expand)

────────────────────────────────────────────────────────────
> [from agyprobe@hub] Two-line held message.
  Second line: when you finish, include the codeword QUINCE-12.

▸ Thought for 36s, 221 tokens

  DONE18 QUINCE-12
```
Status polled every 0.4 s stayed `processing` until the single Stop.

**`force_continue` alone:** harmful. Asked for "READY9", the model replied
`READY9`, then on the forced extra call invented `FIG-71`, a made-up
codeword in the pattern of earlier messages. A sticky `force_continue` (every
PostInvocation) gave 8 forced calls of `READY14` (one with an invented
`APRICOT-83`). The 9th call then became a **10-minute thinking loop**
("The repeated generation of `READY14` continues...") that only Esc ended.
`cli.log` logged `request would have ended on a model turn` at the start.
The documented `MAX_FORCED_INVOCATIONS` cap was never reached.

---

## 4. Turns that end abnormally

| How the turn ended | Hooks after the last one that fired | Screen | gomuxai status |
| :--- | :--- | :--- | :--- |
| Esc while a tool runs (`sleep 30`) | **None** (no PostToolUse, PostInvocation, or Stop; nothing later either, even after the sleep would have ended) | `⎿  Interrupted · What should Antigravity CLI do instead?`, idle `>` prompt, footer `? for shortcuts` | Stuck at `processing` (`PreToolUse` detail) |
| Esc during generation | **None** after PreInvocation | same | Stuck at `processing` |
| Ctrl+C during a tool | **None** (same as Esc; agy keeps running) | same | Stuck at `processing` |
| PreToolUse hook `{"decision":"deny","reason":"..."}` | No PostToolUse; turn continues → PostInvocation → normal Stop | `⎿  Error: tool call denied by pre-tool hook: probe denied this command` | Normal |
| `ask_question` dialog skipped with Esc | PostToolUse (`error: ""`) → normal Stop | `(skipped)` / `User Skipped (no option selected).` | Normal (but `processing`, not `waiting`, while the dialog was up) |
| Hook prints non-JSON / unknown fields (PreInvocation, PostInvocation, PostToolUse, Stop) | Hook is ignored and the turn continues normally; only `cli.log` records it | Nothing | Normal |
| `/exit`, `agent cleanup` | **None** (no SessionEnd in agy; gomuxai's `report exited` in the launch command covers it) | shell | `exited` / removed |
| Real permission dialog, denied | **Not reproduced**, see below | | |

The transcript has no marker for an interrupt either: the cancelled tool step
is simply missing.

**Permission dialog:** this machine's global settings
(`toolPermission: always-proceed`, `permissions.allow: command(*)`) never
prompt, and a PreToolUse `{"decision":"force_ask"}` was **ignored** (the
command just ran). agy has no per-session or workspace permission override:
there's no CLI flag for it, `.agents/settings.json` isn't read, and
`agy -p /permissions` lists only `global` rules. Changing global settings
would affect the person's other agy sessions, so I didn't. To test it: set
`toolPermission` to `request-review` in `~/.gemini/antigravity-cli/settings.json`
for one run, trigger a command, and press Esc / choose "3. No".

**Is there any signal when agy goes idle without Stop?** No hook. Two
signals exist:
1. **agy's log**, one line pair per interrupt, with the conversation ID
   (6 interrupts gave exactly 6 pairs, and no normal turn produced one):
   ```text
   I... conversation_manager.go:1655] Cancelling in-progress response for conversation <conversationId>
   I... server.go:2425] Cancelling conversation <conversationId> (killBackgroundTasks=false)
   ```
   Every agy process writes its own `log/cli-<timestamp>.log`, and
   `--log-file <path>` overrides the path, so gomuxai could give each agent
   its own log. The format is internal and may change.
2. **The screen:** the `⎿  Interrupted · What should Antigravity CLI do
   instead?` line, and the footer `? for shortcuts` with no spinner. A busy
   agy shows `esc to cancel` instead.

### agy's own queue for typed messages
Text typed while agy is busy isn't lost or mixed into the turn. agy queues
it and shows it above the input box:
```text
● Bash(sleep 15; echo slept3) (ctrl+o to collapse)
⣷  Running command...
▸ [from agyprobe@hub] TYPED-WHILE-BUSY: als... (alt+m to send immediately)
────────────────────────────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────────────────────────────
  Press up to edit queued messages    Gemini 4 Argon · high · 1 task(s) · /tasks
```
It's submitted as a new turn 34 ms after Stop (`PreInvocation`
`invocationNum: 0`), and the model answered it (`DONE16` / `BERRY-8`).
(`/config` → "Queued Messages" can switch this to "Send Immediately", which
interrupts the turn.)

Two hazards:
- **Esc moves queued messages back into the input box, unsent.** After an
  Esc, `QUEUED-THEN-ESC: ...` sat in the `>` line until Enter was pressed, so
  anything typed next would be appended to it.
- **Typing into an `ask_question` dialog answers it.** gomuxai shows
  `processing` there (its dialog check only knows the `1. Yes` permission
  dialog), so `send` doesn't refuse. The dialog swallowed the
  `[from agyprobe@hub] ` tag, the Enter picked the highlighted option (`red`),
  and the rest (`TYPED-AT-DIALOG: 2 please`) became a queued, untagged prompt.

---

## 5. Limits

- **Size:** a 60 KB `userMessage` arrived whole (the model quoted the codeword
  at its end). A 400 KB one was **truncated** to ~192 KB, keeping the head:
  `...xxxx\n<truncated 208273 bytes>\n\nNOTE: The output was truncated because it was too long. ...`.
  There was no error, and the model never saw the tail. Every later hook
  payload carries the injected text as `lastUserInput`, so a big message
  inflates each hook's stdin too.
- **Multi-line text:** newlines, quotes, tabs, unicode and backticks arrive
  intact. The model quoted the line exactly, and the TUI shows the lines as a
  multi-line prompt.
- **Several messages at once:** one `injectSteps` list with two
  `userMessage`s, a `systemMessage` and an `ephemeralMessage`: the model got
  all four, with their senders. Each `userMessage` is its own prompt.
- **Strict output:** output is parsed with protojson. Unknown fields, a
  string where an object is expected, or non-JSON stdout reject the whole
  output. Except on PreToolUse, where it blocks the tool (see
  `agy-hooks-findings.md`), the failure is **silent**: the turn goes on and
  only `cli.log` has the error. A delivery hook that prints bad JSON loses
  the message without anyone seeing it.
- **Hooks run synchronously** in the agent loop (default timeout 30 s), so a
  delivery hook must be fast.
- **Nothing broke the session** in these tests except `force_continue`
  without new input (section 3).

### Side findings for gomuxai (not delivery)
- A read-only tmux client attached to an agent's session (`tmux attach -r`,
  e.g. a viewer) makes every `send-keys` to it fail with tmux's `client is
  read-only`. `agent send`, `key`, and `spawn` then fail with only `sending
  keys: failed to run command`. Filed as friction #24.
- `spawn --backend agy` in an untrusted directory without `--trust-workspace`
  stopped at agy's trust prompt and reported status `unknown`.
- agy's Stop payload has no last-reply field, so `_result` stays empty for
  agy agents. The reply is in the transcript (`PLANNER_RESPONSE`).

---

## Recommendation

**Deliver held messages from a `PostInvocation` hook**, with
`injectSteps` `userMessage` + `terminationBehavior: "force_continue"`:

```json
{"injectSteps":[{"userMessage":"[from lead@hub] first held message"},{"userMessage":"[from lead@hub] second"}],"terminationBehavior":"force_continue"}
```

- PostInvocation fires after every model call and its tools, including the
  final one, so a held message reaches the model at the next step boundary,
  whether the turn has more work or was about to end. A tool that runs
  for minutes holds it until it ends, as Claude's would.
- The message shows in the pane as an ordinary `> [from ...]` prompt and in
  `agent logs` as a `user` line, so a person watching sees what arrived and
  when. Stop `continue`, `systemMessage` and `ephemeralMessage` are all
  invisible.
- Only one Stop fires, so gomuxai's status stays `processing`, without the
  flicker of Stop `continue`.
- Keep the `[from sender@host]` tag. One `userMessage` per held message.
- **Print nothing when nothing is held.** Never print `force_continue` without
  injected steps.
- Build the output with `encoding/json`, using only these fields: any mistake
  loses the message silently. Mark messages delivered only after writing the
  output. To confirm delivery, look for a `SYSTEM_SDK` `USER_INPUT` step in
  the transcript (`transcriptPath`) at the next hook.
- Keep each message well under ~190 KB (say 64 KB). Above that, agy cuts it
  without telling anyone: truncate it yourself with a note, or leave the rest
  held.
- This hook prints JSON, while gomuxai's status hook must print nothing. Give
  delivery its own handler or command, or make `report --hook --event
  PostInvocation` print the delivery JSON for agy and nothing otherwise.
- Optionally deliver from `PreInvocation` too. It sees the same boundaries
  except the turn's end, so PostInvocation alone is enough.

**When agy ends a turn without a usable hook** (Esc, Ctrl+C), no hook fires,
so gomuxai's status stays `processing` and nothing triggers delivery:
- Detect it some other way: (a) give each agent `--log-file
  .gomuxai/agents/<name>/agy.log` and treat `Cancelling conversation <id>` as
  "turn ended, idle"; or (b) when the last hook report is `processing` and
  older than a few seconds, check the screen for `Interrupted · What should
  Antigravity CLI do instead?` or the idle footer `? for shortcuts` with no
  spinner, and report `idle`. (b) fits how gomuxai already reads agy's
  permission dialog from the screen. (a) is more exact but depends on agy's
  internal log format.
- Once it's idle, deliver held messages by typing them (agy is at its
  prompt). But first check that the input line is empty: after Esc, agy puts
  any queued message back in the input box, and typed text would be appended
  to it.
- For typing generally: agy queues text typed during a turn by itself and
  submits it at turn end, which is an acceptable fallback. But don't type while
  an `ask_question` dialog is up. gomuxai should learn to see that dialog
  (footer `esc to cancel`, a `Question 1/N:` line, `↑/↓ Navigate · enter
  Select · esc Skip`) and report it as `waiting`.

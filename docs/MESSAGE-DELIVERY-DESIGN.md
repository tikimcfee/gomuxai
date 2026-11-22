# Fast, Coalesced, and Interruptible Inter-Agent Message Delivery

Unified design and implementation on branch `msg-delivery` (combining the independent designs and prototypes from `msg-neon` and `msg-carbon` after cross-review).

---

## 1. Problem & Diagnosis

Inter-agent messages previously arrived late and piecemeal due to four coupled gaps:

1. **Turn-end-only delivery on Claude:** `ClaudeBackend` only delivered held messages on `Stop`. During a 5–15 tool-call turn, messages sat unseen in SQLite for minutes while the agent finished executing an outdated plan.
2. **Piecemeal & uncoalesced delivery:** Multiple held messages were concatenated with raw `\n\n` on Claude (spilling >10,000 chars to a file whose 2,000-char preview showed only the *oldest* messages), injected as N separate stacked `userMessage` prompts on `agy`, and delivered 1 message per 2s poll on `agent watch`. Senders had no way to mark an earlier pending message superseded while keeping its context.
3. **`agent send --now` bypassed SQLite & had no real interrupt:** `opts.now` bypassed `send_held`, leaving earlier `pending` rows in SQLite to arrive *after* `--now` at `Stop` and overwrite it, recorded no message ID, and on default `agy` queued for *turn end* (`queuedMessages: "queue"`, slower than `PostInvocation`). Neither harness had a safe, ancestor-scoped interrupt.
4. **Zero sender visibility:** `send` returned no message ID, `rpc.rs` hardcoded `from: "remote"`, and senders could not tell whether a revision caught a message while still held or arrived after delivery.

---

## 2. Harness Findings (`claude` 2.1.330 & `agy` 1.5.14)

- **Hold in SQLite during tool execution, deliver at the boundary:** While a tool executes, no model call can read a message unless the tool is aborted. Holding in SQLite until the next step/batch hook achieves next-tool latency while keeping the coalescing and superseding window open.
- **Claude `PostToolBatch`:** Fires **once** after all parallel, sequential, failed, or denied tool calls in a batch resolve, right before the next model call. Filtering `r.agent.is_empty()` prevents subagent tool batches from stealing the main session's inbox. Pairing `hookSpecificOutput.additionalContext` with top-level `systemMessage` makes mid-turn delivery visible in the pane. `Stop` remains the backstop for 0-tool turns and messages arriving during final text generation.
- **Why Claude's UDS peer socket (`CLAUDE_CODE_MESSAGING_SOCKET`) is avoided:** Verified in Claude 2.1.330's bundle that external callers authenticate as `peerProtocol: 1` (`origin.kind = "peer"`), whose system-prompt framing instructs Claude *not* to execute commands or external actions requested by peer notifications, and once pushed into Claude's internal queue messages can no longer be coalesced or superseded.
- **Antigravity (`agy`):** `PostInvocation` (`injectSteps` + `force_continue`) delivers a **single** coalesced `userMessage` step (split into labeled 64 KB parts when >64 KB). Passing `{"queuedMessages":"send-immediately","showFeedbackSurvey":false}` in `--settings` fixes `--now` and human mid-turn typing so `agy` delivers at the next step rather than turn end.
- **`PreToolUse`-deny interrupt gate:**
  - **Claude:** Main-session `PreToolUse` returns `permissionDecision: "deny"` + `continue: false` + `stopReason`, blocking the tool before side effects and halting the turn cleanly without prompt-injection retries; gomuxai marks the turn interrupted and triggers immediate 0s-grace prompt delivery.
  - **`agy`:** `PreToolUse` returns strict `{"decision":"deny","reason":"..."}`, aborting the tool and falling straight into `PostInvocation`, which injects the coalesced urgent message in the same turn.

---

## 3. Unified Architecture Implemented on `msg-delivery`

### 3.1 Four Urgency Levels (`soon` as Default)

| Level | CLI Flag / `--urgency` | Behavior | Who May Use |
|---|---|---|---|
| **`later`** | `--later` / `--urgency later` / `agent queue` | Held strictly until the current turn ends (`Stop` / idle), skipping mid-turn `PostToolBatch` and `PostInvocation` (`ClaimFilter::NonLater`). | Any agent or person |
| **`soon`** *(default)* | *(default)* / `--soon` / `--urgency soon` | Coalesced hook delivery at the next tool-batch / step boundary (`PostToolBatch` + `Stop` on Claude; `PostInvocation` on `agy`). | Any agent or person |
| **`now`** | `--now` / `--urgency now` | Inserts into SQLite first (recording a receipt ID and applying `--replace`/`--supersede`), claims and coalesces all pending rows when the input line is empty (fixing out-of-order delivery), refuses dialogs, and types into the harness mid-turn queue (`send-immediately` on `agy`). | Any agent or person |
| **`interrupt`** | `--interrupt` / `--urgency interrupt` | Enforces `check_can_interrupt` (person or host-aware `spawned_by` ancestor), arms `PreToolUse`-deny to block the next tool call before side effects, marks `[URGENT INTERRUPT]`, and bypasses the 10s human grace (`0s` grace, fast poll) in `deliver_held`. Never touches dialogs. | Person, or ancestor in `spawned_by` chain |

### 3.2 Coalescing & Context-Preserving Superseding

- **SQLite `messages` schema (`agent-store`):** Idempotent migrations add `urgency`, `topic`, `superseded_by`, `delivered_at`, `delivery_via`, and `delivery_batch`, plus `status = 'superseded'`.
  - Moving replaced rows to `status = 'superseded'` (`delivery_batch = 0`) keeps `get_pending_messages` / `held_count` counting only **active** pending messages while `claim_pending_batch_matching` pulls unreported superseded rows once alongside the batch that delivers their replacement (and `requeue_batch` restores both cleanly on retryable typing failure).
- **Explicit (`--replace`, `--supersede <int>`, `--key <topic>`) & implicit coalescing (`agent-backend/src/coalesce.rs`):**
  - Single active message with 0 superseded rows uses the exact minimal fast path.
  - Multi-message or superseded batches render as **one** chronological block, **oldest first, newest last**.
  - **Supersede keeps the text:** Superseded messages retain their full body fenced under `--- #41 (2m ago) [SUPERSEDED by #43 — context only, do NOT act on this] ---` so relative follow-ups (*"actually keep Option<T> in step 2"*) keep their referents. Only if a Claude batch exceeds `CLAUDE_INLINE_SOFT_CAP` (9,500 chars) do superseded bodies collapse to 140-char previews alongside a top manifest inside Claude's 2,000-char file preview.
  - Multiple active messages from the same sender carry recency hints (`earlier from <sender> — newer #<id> follows`) without dropping additive instructions.

### 3.3 Sender Receipts (Synchronous, Pull, and Bus Events)

1. **Synchronous receipt on `agent send` (human output + `--json` + bus `SendResponse`):**
   - Every `send` records a SQLite `Message` row (`id > 0`), returning `message_id`, `superseded_ids`, `already_delivered_id`, `already_delivered_via`, and `interrupted`.
   - If `--supersede <id>` or `--replace` misses because the prior message was already delivered, `send` immediately prints a `Note:` naming the delivered message `#id` and channel (`via PostToolBatch`, `via Stop`, etc.).
2. **Pull inspection (`agent status <name> --receipt <int> [--json]`):**
   - Pre-allowed for agents via `status` in `AGENT_SAFE_COMMANDS`; reports `pending`, `superseded`, `delivered` (with timestamp, `delivery_via`, and `delivery_batch`), or `failed` (if the target agent ended with the message still pending).
3. **Bus `StatusEvent` extension:**
   - `delivered` status events include optional `delivered_ids` and `superseded_ids` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`), propagating receipts across hosts with zero `agent-busd` daemon changes.

# Field Survey, Local Models, and Open Directions

Synthesized design notes comparing `gomuxai` with native CLI orchestration,
peer session managers, local-model delegation, and remaining roadmap candidates.

## 1. Where `gomuxai` fits alongside native CLI features

Modern coding CLIs now ship single-vendor subagent fan-out, background modes,
and workflows inside one session (for example, Claude Code's `Agent` tool,
forked background subagents, workflows, and cloud-relayed cross-session
messaging).

- **When to use the CLI's native subagents/workflows:** short single-host,
  single-vendor fan-out inside one task where a human does not need to attach
  to individual panes.
- **What `gomuxai` uniquely provides:**
  1. **Real attachable terminals (`g3d-termd`) across multiple vendors**
     (`claude`, `agy`, `aider`, custom JSON backends, and local-model sessions)
     without idle auto-retirement.
  2. **A self-hosted cross-machine bus with pinned TLS 1.3 and nkeys**—no
     vendor cloud relay or subscription login required, so API-key, enterprise,
     and custom `ANTHROPIC_BASE_URL` (local-model) sessions participate on the
     same footing.
  3. **Remote permission inspection and `--expect`-guarded answering** across
     vendors after viewing the full tool input (`agent status -v`).
  4. **Recursive spawning with parent-scoped cleanup** (`agent cleanup` only
     allows an agent to remove its own descendants) and **agent-filed friction**
     (`agent friction`) as an executable changelog.

## 2. Peer CLIs, orchestrators, and protocols

### Hook convergence across agent CLIs

Lifecycle hooks across major coding CLIs have largely converged on JSON over
stdin with events matching `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PermissionRequest`, `PostToolUse`, and `Stop` (including Claude Code, `agy`,
Codex CLI, Copilot CLI, Cursor CLI, Gemini CLI, Kimi Code, Qwen Code, and
Factory Droid). Several also allow `PreToolUse` or `PermissionRequest` hooks to
return structured allow/deny decisions directly on stdout.

This makes a single **field-mapped generic hook backend** practical for adding
new CLIs without screen scraping, leaving regex scraping only for hook-less
tools such as `aider`.

### Peer patterns worth borrowing

From permissive (MIT / Apache-2.0) session managers in the space (`herdr`,
`agent-deck`, `Gas Town`, `OpenRig`, `AWS CAO`, `hcom`):

- **Hook freshness windows:** let hook status win while fresh (e.g., 2 minutes),
  reconciling against process/registry state thereafter so crashed harnesses do
  not stay `processing`.
- **Draft-aware terminal fallback:** when falling back to keystroke delivery on
  an idle TUI, hold automatic delivery while a human has partially typed text
  in the composer.
- **Declarative screen-rule corpora:** keep a golden pane-capture corpus for
  scraped dialogs and backends so monthly TUI layout tweaks are caught by unit
  tests.
- **File-reservation leases:** optional advisory path leases for parallel
  workers sharing a repository without separate worktrees.

*(License note: several TUI session managers in the ecosystem use AGPL, GPL,
BSL, or ELv2—only permissive MIT/Apache designs are suitable for code porting.)*

### Protocol fit

- **MCP server (stateless 2026-07-28 spec):** high leverage as an optional
  server (`list`, `send`, `status`, `wait`, `friction`) so non-Bash or
  strictly sandboxed CLIs get typed coordination without shell allow-rule
  parsing.
- **ACP (Agent Client Protocol):** useful later as a client backend for CLIs
  that expose structured streaming and `session/request_permission` over
  JSON-RPC, at the cost of having no interactive TUI pane unless bridged.
- **NATS subject alignment:** Synadia's NATS agent conventions (`agents.prompt`,
  `agents.hb`, `agents.status`) are a candidate reference if bus subjects are
  versioned in the future.

## 3. Running local models through `gomuxai`

The lowest-friction way to run a local open-weights coding model (for example,
dense 27B–35B Q4 GGUF models on a 32 GB GPU) as a first-class `gomuxai` worker
is pointing **Claude Code** at **`llama.cpp`'s `llama-server`** (`--jinja` for
template-derived PEG tool-call parsing and native Anthropic `/v1/messages`
support):

- **Full `gomuxai` fidelity with zero new parser code:** hooks (`agent hook`),
  `--session-id`, JSONL transcripts (`agent sessions`, `agent transcript`,
  `agent logs`), session registry status, permission prompts, and cross-host
  NATS bus routing all work unchanged—even though vendor cloud relays disable
  sessions that set a custom `ANTHROPIC_BASE_URL`.
- **Two required configuration details for Claude Code + `llama-server`:**
  1. Set `"CLAUDE_CODE_ATTRIBUTION_HEADER": "0"` inside the settings `"env"`
     block alongside `"ANTHROPIC_BASE_URL"`. Without this, per-turn attribution
     header changes invalidate `llama-server`'s exact-prefix KV cache and force
     full prompt re-evaluation every turn.
  2. Trim default tool schemas on launch:
     `claude --model <alias> --tools "Bash,Read,Edit,Write,Glob,Grep" --strict-mcp-config --disallowedTools "mcp__*"`.
     Untrimmed default sessions send ~29k tokens on turn 1 (~80% tool schemas),
     slowing initial prefill and confusing smaller models with harness-specific
     meta-tools.
- **Trust boundaries for local delegates:** keep permission prompts enabled
  (pre-allowing read-only commands and `go test`), run in an isolated worktree,
  and scope tasks to summarization, call-site inventories, test triage,
  boilerplate, and single-file edits with explicit specs—leaving cross-package
  refactors, concurrency, and security/bus changes to frontier models, with the
  coordinator always re-running tests and reviewing the diff.

## 4. Open candidates

Capabilities already shipped from earlier rounds include hook-boundary held
message delivery (`Stop` / `PostInvocation`), PreToolUse AST auto-approval of
safe `agent` commands (`internal/allowcmd`), multi-agent `wait` (`--any`,
`--follow`), unified transcript event streaming (`agent sessions`,
`agent transcript`, `docs/EVENTS.md`), and the pure `g3d-termd` backend.

Remaining candidates, roughly by leverage:

1. **Hook-returned permission decisions:** let `agent hook` / `report --hook`
   wait briefly for an `agent approve` / `deny` decision on `PermissionRequest`
   (Claude) and `PreToolUse` (`agy`) and return the structured decision JSON on
   stdout, closing the screen-check keystroke race while keeping keystrokes as
   a fallback. Probe first whether a bounded wait delays the local TUI prompt
   for an attached human.
2. **Pass native flags through `agent spawn`:** forward `--worktree`,
   `--permission-mode`, `--model`, `--agent`, and `--effort` to backends that
   support them rather than re-implementing worktree lifecycle in `gomuxai`.
3. **Built-in `claude-local` preset and persisted custom backends:** allow
   configuring `ANTHROPIC_BASE_URL` / model / trimmed tools cleanly per backend
   and store `--backend-config` definitions in SQLite at spawn time so later
   `status`/`wait` calls do not require repeating `--backend-config`.
4. **Generic hook backend:** one configurable event/field map covering
   Claude-shaped hook CLIs (Codex, Copilot, Cursor, Gemini, Kimi, Qwen Code).
5. **`agy` `_result` capture:** populate `_result` when `agy`'s `Stop` hook
   carries final reply text (`FinalModelOutput`), matching Claude's `_result`.
6. **Bus hardening and store housekeeping:** sign RPC replies over NATS so a
   compromised leaf cannot spoof another host's `_INBOX` reply; add size-bounded
   rotation for `<store>/logs/<name>.log` (keeping the file path stable for
   tailers) and retention pruning for `runs` and `session_reports`.

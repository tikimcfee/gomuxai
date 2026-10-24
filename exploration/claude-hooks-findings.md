# Claude Code hooks & process signals — empirical findings

Probed 2026-10-24 on a Linux dev box, Claude Code v2.1.310, model haiku. Counterpart: `agy-hooks-findings.md` (agy side).

Method: a logging hook (`hook.sh` → one JSON line per call: time, `$AGENT_NAME`, `CLAUDE*` env names, stdin payload) registered on 13 events, injected with `--settings '<json>'`. Probed headless (`-p`, tool call allowed) and interactive in tmux (permission prompt, deny with Esc, `/exit`, kill-session).

## Headline findings

1. **Hooks can be injected at launch, no files written.** `claude --settings '<json-string>'` accepted the full hooks block. Spawn never needs to touch the project's `.claude/`.
2. **`$AGENT_NAME` reaches hooks.** Env set on the tmux session is inherited by hook processes, so `agent report` can identify itself with zero config.
3. **Claude already tracks status natively.** `claude agents --json` lists every interactive + background session with `pid`, `sessionId`, `cwd`, and `status`. Observed: `idle` → `waiting` (permission prompt) → `idle` (after Esc). `busy` while working. Its `pid` equals tmux `#{pane_pid}` when claude is the pane command, so pane ↔ session mapping is trivial. (Background sessions use a `state` field instead, e.g. `"blocked"`.)
4. **`Stop` carries `last_assistant_message`.** The agent's final reply arrives in the hook payload — results without `capture`.
5. **Current scraping is already broken.** The idle prompt is now `❯`, which `claudeIdlePattern` (`^>\s*$`, `internal/backend/namedagents/claude.go:15`) can never match.

## Event → payload → suggested status

| Event | Fired when (observed) | Useful payload keys | gomuxai status |
|---|---|---|---|
| `SessionStart` | launch (`source: startup`) | `session_id`, `transcript_path`, `cwd`, `source` | `ready` |
| `UserPromptSubmit` | prompt submitted | `prompt`, `prompt_id`, `permission_mode` | `busy` |
| `PreToolUse` | before each tool | `tool_name`, `tool_input`, `tool_use_id` | `busy` (detail: tool) |
| `PermissionRequest` | permission dialog shown, **immediately** (+13ms after PreToolUse) | `tool_name`, `tool_input` | `waiting` (needs human) |
| `Notification` | ~6s after the dialog, `notification_type: permission_prompt`, `message: "Claude needs your permission"` | `notification_type`, `message` | `waiting` (redundant w/ above, delayed) |
| `PostToolUse` | after tool succeeds | `tool_response`, `duration_ms` | `busy` |
| `Stop` | turn finished | `last_assistant_message`, `background_tasks`, `session_crons`, `stop_hook_active` | `idle` (+ result) |
| `SessionEnd` | `/exit` (`reason: prompt_input_exit`); also fired on tmux kill-session | `reason` | `exited` |

Registered but not triggered by these probes (settings accepted them without error): `PostToolUseFailure`, `StopFailure`, `SubagentStart`, `SubagentStop`, `PreCompact`.

Every payload carries `session_id`, `transcript_path`, `cwd`, `hook_event_name`.

## The gap: interrupts fire nothing

Pressing Esc at the permission prompt cancelled the turn with **no** `Stop`, `PostToolUseFailure`, or other hook. Hook-only status would stay stuck at `waiting` until the next prompt. `claude agents --json` correctly went back to `idle`. So hooks give push-based transitions, and the native status is the reconciler.

## Process-level options (beyond hooks)

- `--session-id <uuid>`: gomuxai can pick the session id at spawn, then join hooks, `claude agents --json`, and the transcript by id.
- `-p --output-format stream-json --verbose --include-hook-events`: headless runs emit `system/init`, `hook_started`/`hook_response` per event, assistant/user messages, and `result`. For task-style agents, no tmux or hooks needed: the process stdout *is* the event stream. `--resume <id>` continues it.
- `claude --bg` / `claude agents` / `claude logs <id>` / `claude attach <id>`: native background sessions with their own lifecycle commands.
- Hook env includes `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`, `CLAUDE_PROJECT_DIR`, and `CLAUDE_CODE_MESSAGING_SOCKET`/`_TOKEN` (native cross-session messaging; not explored here).

## Suggested design for the claude backend

1. Spawn adds `--session-id <uuid> --settings '<hooks json>'`; every hook runs `agent report --hook` (reads the stdin payload, maps event → status, publishes to the bus; `Stop` also publishes `last_assistant_message` as the result).
2. The bus daemon reconciles with `claude agents --json` (keyed by pane pid / session id) on a slow tick and on demand, which covers interrupts, crashes, and missed hooks.
3. Delete the claude regexes. Keep scraping only as a labeled fallback for backends with neither hooks nor a status command.
4. For one-shot/task agents, consider spawning headless `-p stream-json` processes instead of TUIs.

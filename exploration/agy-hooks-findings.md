# Antigravity CLI (`agy`) Hooks, Headless Mode & MCP Empirical Findings

**Date:** 2026-10-24  
**CLI Version Tested:** `Antigravity CLI 1.4.7` (`~/.local/bin/agy`)  
**Test Environment:** Isolated scratch git repositories under `/tmp/agy-test` and `/tmp/agy-*` (zero tracked files modified in `gomuxai`).

---

## Executive Summary: Event → Payload → Suggested `gomuxai` Status Table

| `agy` Lifecycle Event / Moment | Hook Config Structure | Key `stdin` JSON Fields (all `camelCase`) | Required `stdout` / Exit Code (Non-Blocking) | Blocking / Control Capability | Suggested `gomuxai` Status / Event |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Process Spawn (TUI idle before 1st prompt)** | *(None — no hook fires until 1st prompt is submitted)* | N/A | N/A | N/A | **`idle`** *(set on launch, because `SessionStart` is lazy)* |
| **`SessionStart`** *(fires before each invocation)* | Flat array of handler objects | `conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath`, `modelName`, `lastUserInput` | `exit 0` + `{}` or empty `""` *(optional `{"injectSteps":[...]}`)* | Non-zero exit or invalid JSON logs/fails hook; can inject `systemMessage` / `userMessage` / `ephemeralMessage` | *(Ignored in favor of `PreInvocation`)* |
| **`PreInvocation`** *(fires before each model call in a turn; `invocationNum` = 0, 1, ...)* | Flat array of handler objects | Common fields + `invocationNum` (0-indexed per turn), `initialNumSteps` | `exit 0` + `{}` or empty `""` *(optional `{"injectSteps":[...]}`)* | Can inject steps before model inference | **`processing`** *(fires on every turn, including Turn 2+ and `--conversation` resumes)* |
| **`PreToolUse`** *(fires before each tool execution, **prior** to permission prompt)* | Grouped: `[{"matcher": "*", "hooks": [...]}]` | Common fields + `stepIdx`, `toolCall: {"name": "<tool>", "args": {...}}` | `exit 0` + `{}` or empty `""` (defers to CLI permission policy), or `{"decision": "allow"}` (auto-approves tool!) | `{"decision": "deny", "reason": "..."}` blocks tool; `{"decision": "ask" \| "force_ask"}` forces user approval prompt; `overwrite` rewrites args; `exit != 0` or non-JSON stdout blocks tool | **`processing`** *(detail `<toolCall.name>: <arg>`; Note: if permission prompt is triggered in TUI, `agy` waits between `PreToolUse` and `PostToolUse`)* |
| **`PostToolUse`** *(fires after tool execution completes, if not denied)* | Grouped: `[{"matcher": "*", "hooks": [...]}]` | Common fields + `stepIdx`, `toolCall: {"name": "<tool>", "args": {...}}`, `error` (`""` on success, error string on tool error) | `exit 0` + `{}` or empty `""` | Post-observation only (cannot undo tool execution) | **`processing`** *(with `"failed: ..."` detail when `error != ""`)* |
| **`PostInvocation`** *(fires after model + any tool steps in that invocation finish)* | Flat array of handler objects | Common fields + `invocationNum`, `initialNumSteps` | `exit 0` + `{}` or empty `""` *(optional `terminationBehavior`)* | `{"terminationBehavior": "force_continue" \| "terminate"}` forces loop continuation or immediate stop | *(No status change needed — wait for `Stop`)* |
| **`Stop`** *(fires when turn execution loop terminates)* | Flat array of handler objects | Common fields + `executionNum`, `terminationReason` (`"NO_TOOL_CALL"`, `"ERROR"`, `"USER_CANCELED"`, `"MAX_INVOCATIONS"`, etc.), `error`, `fullyIdle` (`bool`) | `exit 0` + `{}` or empty `""` | `{"decision": "continue", "reason": "..."}` blocks stop, injects `reason`, and triggers another `PreInvocation` cycle | If `error != ""` or error `terminationReason` → **`error`**;<br>Else if `!fullyIdle` → **`processing`**;<br>Else → **`idle`** |
| **Session Exit (`/exit`, `/quit`, `Ctrl+D`, EOF)** | *(None — no `SessionEnd` hook exists)* | N/A | N/A | N/A | Detected via tmux pane / process exit |

---

## 1. Hook Registration, Environment Variables & Exact Payloads

### 1.1 Supported Hook Events & `hooks.json` Schema
`agy` supports **6 lifecycle hook events** in `.agents/hooks.json`:
1. `SessionStart` *(flat list of handler objects)*
2. `PreInvocation` *(flat list of handler objects)*
3. `PreToolUse` *(grouped by `matcher` regex over lowercased tool name, e.g. `"*"` or `"run_command|call_mcp_tool"`)*
4. `PostToolUse` *(grouped by `matcher` regex)*
5. `PostInvocation` *(flat list of handler objects)*
6. `Stop` *(flat list of handler objects)*

**Unknown event names:** Adding Claude Code hook keys such as `UserPromptSubmit` or `SessionEnd` to `.agents/hooks.json` was tested empirically; `agy` **silently ignores** unknown event keys without error.

Because `agy` hook payloads do not carry the event name in `stdin` JSON, `gomuxai` passes `--event <Event>` on each hook command:

```json
{
  "gomuxai": {
    "PreInvocation": [
      { "type": "command", "command": "/path/to/agent report --hook --as worker --event PreInvocation", "timeout": 10 }
    ],
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "/path/to/agent report --hook --as worker --event PreToolUse", "timeout": 10 }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "/path/to/agent report --hook --as worker --event PostToolUse", "timeout": 10 }
        ]
      }
    ],
    "Stop": [
      { "type": "command", "command": "/path/to/agent report --hook --as worker --event Stop", "timeout": 10 }
    ]
  }
}
```

### 1.2 Environment Variables & Working Directory Passed to Hooks
When tested with a stripped parent environment (`env -u ANTIGRAVITY_* AGENT_NAME=worker-clean agy ...`), every hook subprocess receives:
- **`AGENT_NAME`**: Inherited untouched from the environment in which `agy` was launched (`"worker-clean"`).
- **`ANTIGRAVITY_CONVERSATION_ID`**: Automatically injected by `agy` (e.g. `"00000000-0000-0000-0000-000000000001"`), matching `stdin.conversationId`.
- **`TMUX` / `TMUX_PANE`**: Inherited when running inside a `tmux` session.
- **Working Directory (`cwd` / `PWD`)**: Set to **the directory containing `.agents/hooks.json`** (e.g. `.gomuxai/agents/<name>`), **not** the repository root. The repository/workspace root paths are passed in `stdin.workspacePaths`.

### 1.3 Captured `stdin` Payloads (Sanitized Samples)

#### `SessionStart`
```json
{
  "artifactDirectoryPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001",
  "conversationId": "00000000-0000-0000-0000-000000000001",
  "lastUserInput": "Run 'echo hello-hook' using run_command, then reply with DONE.",
  "modelName": "gemini-3.8-flash-low",
  "transcriptPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001/.system_generated/logs/transcript_full.jsonl",
  "workspacePaths": [
    "/tmp/agy-test"
  ]
}
```

#### `PreInvocation` & `PostInvocation`
```json
{
  "artifactDirectoryPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001",
  "conversationId": "00000000-0000-0000-0000-000000000001",
  "initialNumSteps": 1,
  "invocationNum": 0,
  "lastUserInput": "Run 'echo hello-hook' using run_command, then reply with DONE.",
  "modelName": "gemini-3.8-flash-low",
  "transcriptPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001/.system_generated/logs/transcript_full.jsonl",
  "workspacePaths": [
    "/tmp/agy-test"
  ]
}
```
*Note:* Within a single user turn that makes 1 tool call, `PreInvocation`/`PostInvocation` fire twice: first with `invocationNum: 0, initialNumSteps: 1` (producing the tool call), and again after the tool completes with `invocationNum: 1, initialNumSteps: 3` (producing the final text response). On a second user turn in the same session, `invocationNum` resets to `0` while `initialNumSteps` is `5`.

#### `PreToolUse`
```json
{
  "artifactDirectoryPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001",
  "conversationId": "00000000-0000-0000-0000-000000000001",
  "lastUserInput": "Run 'echo hello-hook' using run_command, then reply with DONE.",
  "modelName": "gemini-3.8-flash-low",
  "stepIdx": 2,
  "toolCall": {
    "args": {
      "CommandLine": "echo hello-hook",
      "IsDaemon": false,
      "WaitMsBeforeAsync": 5000,
      "toolSummary": "Echo hello hook"
    },
    "name": "run_command"
  },
  "transcriptPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001/.system_generated/logs/transcript_full.jsonl",
  "workspacePaths": [
    "/tmp/agy-test"
  ]
}
```

#### `PostToolUse` (Success vs. Tool Error)
- **When `run_command` executes a shell command that exits `42`**: `PostToolUse` receives `"error": ""` because the tool execution itself succeeded and returned `"Command exited with code 42."` to the model.
- **When a tool itself errors** (tested by calling `read_file` on `/tmp/definitely_missing_file_987654321.txt`): `PostToolUse` receives `"error": "file does not exist"`:
```json
{
  "conversationId": "00000000-0000-0000-0000-000000000002",
  "error": "file does not exist",
  "lastUserInput": "Call read_file on /tmp/definitely_missing_file_987654321.txt, and when it errors reply TOOL_ERROR_SEEN.",
  "modelName": "gemini-3.8-flash-low",
  "stepIdx": 2,
  "toolCall": {
    "args": { "Path": "/tmp/definitely_missing_file_987654321.txt" },
    "name": "read_file"
  }
}
```

#### `Stop`
```json
{
  "artifactDirectoryPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001",
  "conversationId": "00000000-0000-0000-0000-000000000001",
  "error": "",
  "executionNum": 0,
  "fullyIdle": true,
  "lastUserInput": "Run 'echo hello-hook' using run_command, then reply with DONE.",
  "modelName": "gemini-3.8-flash-low",
  "terminationReason": "NO_TOOL_CALL",
  "transcriptPath": "/home/user/.gemini/antigravity-cli/brain/00000000-0000-0000-0000-000000000001/.system_generated/logs/transcript_full.jsonl",
  "workspacePaths": [
    "/tmp/agy-test"
  ]
}
```

---

## 2. Firing Order, Permission Waits & Stdout / Exit-Code Requirements

### 2.1 Observed Firing Order in Interactive (`tmux`) and Headless (`-p`) Sessions

1. **Interactive TUI Startup (`agy` launched in `tmux` session):**
   - If the workspace directory is not in `~/.gemini/antigravity-cli/settings.json` `trustedWorkspaces`, `agy` displays a modal trust prompt (`Do you trust the contents of this project? > Yes, I trust this folder`). Sending `Enter` confirms trust.
   - Once at the main `>` prompt (before the first user message is submitted), **0 hooks have fired**. `SessionStart` is lazy and only fires when the first prompt is submitted.

2. **Turn 1 (Prompt with 1 Tool Call + Permission Wait in `tmux`):**
   ```text
   1. 14:39:06.780  SessionStart    (lastUserInput = Turn 1 prompt)
   2. 14:39:06.807  PreInvocation   (invocationNum = 0, initialNumSteps = 1)
   3. 14:39:08.608  PreToolUse      (stepIdx = 2, toolCall.name = "run_command")
      [TUI blocks here waiting on user approval: "Run this command? > 1. Yes, run command"]
      [User presses 1 or Enter at 14:39:27]
   4. 14:39:27.876  PostToolUse     (stepIdx = 2, error = "")
   5. 14:39:27.903  PostInvocation  (invocationNum = 0, initialNumSteps = 1)
   6. 14:39:27.928  PreInvocation   (invocationNum = 1, initialNumSteps = 3)
   7. 14:39:29.770  PostInvocation  (invocationNum = 1, initialNumSteps = 3)
   8. 14:39:29.801  Stop            (executionNum = 0, terminationReason = "NO_TOOL_CALL", fullyIdle = true)
   ```

3. **Turn 2 (Follow-Up Prompt in Same Session, or Resumed via `--conversation <id>`):**
   ```text
   9.  14:39:31.947  PreInvocation  (invocationNum = 0, initialNumSteps = 5)
   10. 14:39:33.442  PostInvocation (invocationNum = 0, initialNumSteps = 5)
   11. 14:39:33.473  Stop           (executionNum = 0, terminationReason = "NO_TOOL_CALL", fullyIdle = true)
   ```

4. **Session Exit (`/exit`, `/quit`, `Ctrl+D`):**
   - Exits cleanly (`tmux` session terminates), with **no additional hook fired** after the last turn's `Stop`.

### 2.2 Permission / Approval Wait & TUI Dialog Keys
- `agy` has **no separate `PermissionRequest` or `Notification` hook**.
- In the TUI, the permission dialog renders a numbered menu:
  ```text
  Run this command?
  > 1. Yes, run command
    2. Yes, and always allow running "<binary>" in this conversation
    3. No, tell the agent what to do differently
  Press Enter to confirm, Esc to cancel
  ```
  - Pressing `"1"` approves once.
  - Pressing `"2"` approves for the conversation.
  - Pressing `"Escape"` cancels/denies.
- Additionally, **`PreToolUse` runs before `agy`'s permission prompt** and can control the permission gate via stdout JSON (`{"decision": "allow"}`, `{"decision": "deny", "reason": "..."}`, or `{"decision": "force_ask", "reason": "..."}`). Returning `{}` or empty `""` defers to `agy`'s standard permission check (`--dangerously-skip-permissions` flag and `settings.json`).

### 2.3 Stdout & Exit-Code Contract (Empirical Test Matrix)

| Hook Return Behavior | Tested Result in `agy` |
| :--- | :--- |
| **`exit 0` + valid JSON (`{}` or `{"decision":"allow"}`)** | **Non-blocking.** Normal execution across all 6 events. |
| **`exit 0` + empty stdout (`""`)** | **Non-blocking across all 6 events!** `agy` treats empty stdout as `{}` (`PreToolUse` defers to standard permission rules; `PostToolUse`, `PreInvocation`, `PostInvocation`, `Stop` succeed). |
| **`exit 0` + non-JSON stdout (e.g. `"not-json"`)** | **BLOCKS tool calls!** On `PreToolUse`, `agy` aborts the tool call (`PostToolUse` never fires) and reports a JSON hook parse failure to the model. **Crucial rule for `gomuxai`:** `agent report --hook` must emit nothing on stdout and exit `0`. |
| **`exit 1` (non-zero exit) on `PreToolUse`** | **BLOCKS tool calls!** Model receives: `Encountered error in tool execution: JSON hook "jsonhook__..." failed: command failed: exit status 1, stderr: ...` (`PostToolUse` does not fire). |
| **`PreToolUse` outputs `{"decision": "deny", "reason": "X"}`** | **BLOCKS tool calls cleanly.** Model receives: `tool call denied by pre-tool hook: X` (`PostToolUse` does not fire). |
| **`Stop` outputs `{"decision": "continue", "reason": "Y"}`** | **BLOCKS turn completion.** `agy` injects `Y` into the conversation and immediately runs another `PreInvocation` → `PostInvocation` → `Stop` cycle with `executionNum: 1`. |

---

## 3. Launch-Time Hook Injection & Merging (Zero Repo Footprint)

### 3.1 Can Hooks Be Injected at Launch Without Writing `.agents/hooks.json` in the Project?
**Yes — via `--add-dir <path>`!**
- While `agy` does not have a dedicated `--hooks-file` flag or `AGY_HOOKS` env var, its `--add-dir <dir>` flag adds `<dir>` to `workspacePaths` and runs full `.agents/` customization discovery on `<dir>`.
- **Verified empirically:** We created an empty git repo `/tmp/agy-clean-repo` (containing only `.git`, no `.agents/` directory) and an external directory `/tmp/gomuxai-injected-hooks/.agents/hooks.json`, then ran:
  ```bash
  agy --add-dir /tmp/gomuxai-injected-hooks -p "Reply PURE_INJECT_OK."
  ```
  `agy` discovered `/tmp/gomuxai-injected-hooks/.agents/hooks.json` and fired `SessionStart`, `PreInvocation`, and `Stop` with `/tmp/agy-clean-repo` completely untouched.
- `gomuxai` generates a per-agent bundle under `.gomuxai/agents/<name>/.agents/hooks.json` and passes `--add-dir .gomuxai/agents/<name>` when spawning `agy`.

### 3.2 Do Project, Subdirectory, Plugin, `--add-dir`, and Global Hooks Merge?
**Yes — all discovered `hooks.json` files merge and execute sequentially:**
1. **Different hook names across files** (tested with repo root `.agents/hooks.json` + subdirectory `sub/.agents/hooks.json` + plugin `.agents/plugins/p1/hooks.json` + `--add-dir` `.agents/hooks.json`):
   - All 4 hooks fired sequentially on `SessionStart` and `Stop`, each executing with its own `cwd` set to the directory where its `hooks.json` lives.
2. **Same top-level hook name across files** (tested with `"gomuxai-probe"` defined in both the project's `.agents/hooks.json` and the `--add-dir` `.agents/hooks.json`):
   - **Both handlers still merged and executed sequentially** — one does **not** clobber the other.
3. **Global `~/.gemini/config/hooks.json`**: Also merges into the active hook set alongside workspace and plugin hooks.

---

## 4. Non-Interactive Mode (`-p`, JSON Streaming, Resume) & MCP Client Support

### 4.1 Flag Ordering Gotcha with `-p` / `--print`
In `agy`, `-p` / `--print` takes the prompt string as its own argument. Placing another flag immediately after `-p` fails:
```text
Error: -p took "--output-format" as its prompt, so the intended prompt was left as an argument and ignored.
Attach the prompt to the flag (-p='your prompt') and move --output-format elsewhere on the command line.
```
Always put `--output-format`, `--conversation`, `--model`, `--add-dir`, etc. **before** `-p "<prompt>"` or `-i "<prompt>"`.

### 4.2 Single-Turn Headless JSON (`--output-format json -p "..."`)
Returns a single JSON summary object on stdout when the turn finishes:
```json
{
  "conversation_id": "00000000-0000-0000-0000-000000000003",
  "status": "SUCCESS",
  "response": "READY\n",
  "duration_seconds": 1.858801,
  "num_turns": 1,
  "usage": {
    "input_tokens": 13704,
    "output_tokens": 1,
    "thinking_tokens": 0,
    "cache_read_tokens": 0,
    "total_tokens": 13705
  }
}
```

### 4.3 Streaming NDJSON Output (`--output-format stream-json -p "..."`)
Streams newline-delimited JSON events (`init` → `step_update` → `result`) in real time:
```json
{"event":"init","conversation_id":"00000000-0000-0000-0000-000000000003","init":{"model":"gemini-3.8-flash-low","cwd":"...","tools":[...],"permission_mode":"request-review"}}
{"event":"step_update","step_update":{"conversation_id":"00000000-0000-0000-0000-000000000003","step_index":2,"state":"DONE","step_type":"user_input"}}
{"event":"step_update","step_update":{"conversation_id":"00000000-0000-0000-0000-000000000003","step_index":4,"state":"ACTIVE","step_type":"agent_response","text_delta":"The codeword was BANANA_42."}}
{"event":"step_update","step_update":{"conversation_id":"00000000-0000-0000-0000-000000000003","step_index":4,"state":"DONE","step_type":"agent_response","text_delta":"\n","duration_seconds":2.845563,"usage":{"input_tokens":13910,"output_tokens":9,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":13919}}}
{"event":"result","result":{"conversation_id":"00000000-0000-0000-0000-000000000003","status":"SUCCESS","response":"The codeword was BANANA_42.\n","duration_seconds":7.052717,"num_turns":2,"usage":{"input_tokens":27614,"output_tokens":10,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":27624}}}
```

### 4.4 Multi-Turn Headless Streaming (`--input-format stream-json --output-format stream-json -p ""`)
Reads NDJSON user turns from `stdin`, executing a full turn and emitting a `{"event":"result",...}` line after each input line while keeping the process and prompt cache warm:
- **Required `stdin` NDJSON schema:**
  ```json
  {"event":"user","message":{"content":"Your prompt text here"}}
  ```
- **Verified 2-turn run:** Fired `SessionStart` → `PreInvocation` → `PostInvocation` → `Stop` on line 1, and `PreInvocation` → `PostInvocation` → `Stop` on line 2 (with `cache_read_tokens: 8135`).

### 4.5 Resuming a Session (`--conversation <id>` or `-c` / `--continue`)
- `--conversation <id>` resumes an existing conversation by ID in both interactive TUI mode (`agy --conversation <id>`) and headless mode (`agy --conversation <id> --output-format json -p "..."`). (Note: passing a new UUID that does not exist yet logs `warning: conversation "<uuid>" not found`; `agy` assigns its own `conversationId` on Turn 1 and reports it via hooks.)
- `-i "<prompt>"` (`--prompt-interactive`) launches the interactive TUI and immediately submits the initial prompt.

### 4.6 MCP Client Support
`agy` supports **Stdio** and **Streamable HTTP** MCP servers via:
1. **Global CLI / Config (`~/.gemini/config/mcp_config.json`)**: Managed via `agy mcp add [--env KEY=val] <name> <cmd> [args...]`, `agy mcp list`, `agy mcp remove <name>`.
2. **Zero-Footprint Launch Injection via Plugin in `--add-dir` (Verified Empirically)**:
   - Placing `.agents/plugins/gomuxai/plugin.json` and `.agents/plugins/gomuxai/mcp_config.json` inside an external directory passed via `--add-dir /path/to/gomuxai-bundle` automatically launches the stdio MCP server for that `agy` session without modifying the repo or global `~/.gemini/config/mcp_config.json`:
     ```json
     {
       "mcpServers": {
         "gomuxai": {
           "command": "agent",
           "args": ["mcp", "serve"],
           "env": {
             "AGENT_NAME": "worker-1",
             "NATS_URL": "nats://127.0.0.1:4222"
           }
         }
       }
     }
     ```
   - When the model calls an MCP tool, `agy` first reads the cached tool schema file (`~/.gemini/antigravity-cli/mcp/<plugin>_<server>/<tool>.json` via `read_file`) and then invokes `call_mcp_tool`:
     ```json
     {
       "name": "call_mcp_tool",
       "args": {
         "ServerName": "gomuxai_gomuxai",
         "ToolName": "gomuxai_ping",
         "Arguments": { "msg": "hello-nats" }
       }
     }
     ```
     Both `PreToolUse` and `PostToolUse` fire for `call_mcp_tool`.

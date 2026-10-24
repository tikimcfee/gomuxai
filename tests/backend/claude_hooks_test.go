package backend_test

import (
	"encoding/json"
	"os/exec"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// Payloads trimmed from real Claude Code v2.1.310 hook invocations
// (see exploration/claude-hooks-findings.md).
func TestClaudeParseHookEvent(t *testing.T) {
	provider := namedagents.NewClaudeBackend()

	tests := []struct {
		name       string
		payload    string
		wantOK     bool
		wantStatus backend.Status
		wantDetail string
		wantResult string
	}{
		{
			name:       "session start",
			payload:    `{"hook_event_name":"SessionStart","session_id":"s1","source":"startup"}`,
			wantOK:     true,
			wantStatus: backend.StatusIdle,
			wantDetail: "session startup",
		},
		{
			name:       "prompt submitted",
			payload:    `{"hook_event_name":"UserPromptSubmit","session_id":"s1","prompt":"hi"}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "prompt received",
		},
		{
			name:       "pre tool use summarizes the command",
			payload:    `{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command":"echo probe","description":"Echo"}}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "Bash: echo probe",
		},
		{
			name:       "permission request waits on a human",
			payload:    `{"hook_event_name":"PermissionRequest","session_id":"s1","tool_name":"Edit","tool_input":{"file_path":"/tmp/x.go"}}`,
			wantOK:     true,
			wantStatus: backend.StatusWaiting,
			wantDetail: "permission: Edit: /tmp/x.go",
		},
		{
			name:    "delayed permission notification is ignored",
			payload: `{"hook_event_name":"Notification","session_id":"s1","notification_type":"permission_prompt","message":"Claude needs your permission"}`,
			wantOK:  false,
		},
		{
			name:       "idle notification",
			payload:    `{"hook_event_name":"Notification","session_id":"s1","notification_type":"idle_prompt"}`,
			wantOK:     true,
			wantStatus: backend.StatusIdle,
			wantDetail: "waiting for input",
		},
		{
			name:       "stop carries the result",
			payload:    `{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"done","stop_hook_active":false}`,
			wantOK:     true,
			wantStatus: backend.StatusIdle,
			wantDetail: "turn finished",
			wantResult: "done",
		},
		{
			// Observed: the agent launched a background shell, replied
			// "started", and Stop fired while the shell kept running.
			name:       "stop with a background shell running is not idle",
			payload:    `{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"started","background_tasks":[{"id":"berqn6wy0","type":"shell","status":"running","description":"Sleep 45 seconds then echo finished","command":"sleep 45; echo finished"}],"session_crons":[]}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "turn finished, 1 background task(s) running",
		},
		{
			name:       "stop with a background subagent running is not idle",
			payload:    `{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"launched","background_tasks":[{"id":"adfca3363a3dc3bba","type":"subagent","status":"running","description":"Run sleep 30 and reply ok","agent_type":"general-purpose"}],"session_crons":[]}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "turn finished, 1 background task(s) running",
		},
		{
			name:       "stop after background work finished",
			payload:    `{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"The subagent replied ok.","background_tasks":[],"session_crons":[]}`,
			wantOK:     true,
			wantStatus: backend.StatusIdle,
			wantDetail: "turn finished",
			wantResult: "The subagent replied ok.",
		},
		{
			name:       "session end",
			payload:    `{"hook_event_name":"SessionEnd","session_id":"s1","reason":"prompt_input_exit"}`,
			wantOK:     true,
			wantStatus: backend.StatusExited,
			wantDetail: "prompt_input_exit",
		},
		{
			// Observed: /clear fires SessionEnd (reason clear), then SessionStart
			// (source clear). Reporting exited in between would fail queued messages.
			name:    "session end on /clear is not an exit",
			payload: `{"hook_event_name":"SessionEnd","session_id":"s1","reason":"clear"}`,
			wantOK:  false,
		},
		{
			name:    "unknown events are ignored",
			payload: `{"hook_event_name":"SubagentStart","session_id":"s1"}`,
			wantOK:  false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			r, ok, err := provider.ParseHookEvent("", []byte(tt.payload))
			if err != nil {
				t.Fatalf("ParseHookEvent: %v", err)
			}
			if ok != tt.wantOK {
				t.Fatalf("ok = %v, want %v", ok, tt.wantOK)
			}
			if !ok {
				return
			}
			if r.Status != tt.wantStatus {
				t.Errorf("status = %v, want %v", r.Status, tt.wantStatus)
			}
			if r.Detail != tt.wantDetail {
				t.Errorf("detail = %q, want %q", r.Detail, tt.wantDetail)
			}
			if r.Result != tt.wantResult {
				t.Errorf("result = %q, want %q", r.Result, tt.wantResult)
			}
			if r.SessionID != "s1" {
				t.Errorf("session id = %q, want s1", r.SessionID)
			}
		})
	}
}

// A new prompt or conversation makes the stored result stale.
func TestClaudeParseHookEvent_ClearResult(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	for payload, want := range map[string]bool{
		`{"hook_event_name":"UserPromptSubmit","prompt":"hi"}`:                         true,
		`{"hook_event_name":"UserPromptSubmit","prompt":"<task-notification>..."}`:     true,
		`{"hook_event_name":"SessionStart","source":"clear"}`:                          true,
		`{"hook_event_name":"SessionStart","source":"compact"}`:                        false,
		`{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{}}`:          false,
		`{"hook_event_name":"Stop","last_assistant_message":"done"}`:                   false,
		`{"hook_event_name":"Stop","last_assistant_message":"","background_tasks":[]}`: false,
	} {
		r, ok, err := provider.ParseHookEvent("", []byte(payload))
		if err != nil || !ok {
			t.Fatalf("%s: ok=%v err=%v", payload, ok, err)
		}
		if r.ClearResult != want {
			t.Errorf("%s: ClearResult = %v, want %v", payload, r.ClearResult, want)
		}
	}
}

func TestClaudeParseHookEvent_InvalidJSON(t *testing.T) {
	_, _, err := namedagents.NewClaudeBackend().ParseHookEvent("", []byte("not json"))
	if err == nil {
		t.Fatal("expected error for invalid payload")
	}
}

func TestClaudeParseHookEvent_TruncatesLongDetail(t *testing.T) {
	payload := `{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"` + strings.Repeat("é", 300) + `"}}`
	r, _, err := namedagents.NewClaudeBackend().ParseHookEvent("", []byte(payload))
	if err != nil {
		t.Fatalf("ParseHookEvent: %v", err)
	}
	if n := len([]rune(r.Detail)); n != 120 {
		t.Errorf("detail is %d runes, want 120", n)
	}
	if !strings.HasSuffix(r.Detail, "…") {
		t.Errorf("detail should end with an ellipsis: %q", r.Detail)
	}
}

// The launch command is typed into a shell, so its --settings JSON must
// survive shell quoting, including awkward characters in the binary path.
func TestClaudeLaunchCommandWithHooks(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	reportCmd := `'/opt/my tools/it'\''s/agent' report --hook --as 'helper'`

	cmd := provider.LaunchCommandWithHooks(reportCmd, "11111111-2222-3333-4444-555555555555")
	if !strings.HasPrefix(cmd, "claude --session-id ") {
		t.Fatalf("unexpected launch command: %s", cmd)
	}

	// Let a real shell split the arguments.
	out, err := exec.Command("sh", "-c", `printf '%s\n' `+strings.TrimPrefix(cmd, "claude ")).Output()
	if err != nil {
		t.Fatalf("shell parsing launch command: %v", err)
	}
	args := strings.Split(strings.TrimRight(string(out), "\n"), "\n")
	if len(args) != 4 || args[0] != "--session-id" || args[2] != "--settings" {
		t.Fatalf("unexpected args: %q", args)
	}
	if args[1] != "11111111-2222-3333-4444-555555555555" {
		t.Errorf("session id = %q", args[1])
	}

	var settings struct {
		Hooks map[string][]struct {
			Hooks []struct {
				Type    string `json:"type"`
				Command string `json:"command"`
			} `json:"hooks"`
		} `json:"hooks"`
	}
	if err := json.Unmarshal([]byte(args[3]), &settings); err != nil {
		t.Fatalf("settings JSON: %v\n%s", err, args[3])
	}
	for _, event := range []string{"SessionStart", "UserPromptSubmit", "PreToolUse", "PermissionRequest", "Stop", "SessionEnd"} {
		matchers := settings.Hooks[event]
		if len(matchers) != 1 || len(matchers[0].Hooks) != 1 {
			t.Errorf("%s: expected one hook, got %+v", event, matchers)
			continue
		}
		if got := matchers[0].Hooks[0].Command; got != reportCmd {
			t.Errorf("%s: command = %q, want %q", event, got, reportCmd)
		}
	}
}

func TestClaudeLaunchCommandWithHooks_KeepsSkipPermissions(t *testing.T) {
	provider := namedagents.NewClaudeBackendWithConfig(namedagents.ClaudeConfig{SkipPermissions: true})
	cmd := provider.LaunchCommandWithHooks("agent report --hook", "sid")
	if !strings.HasPrefix(cmd, "claude --dangerously-skip-permissions --session-id ") {
		t.Errorf("unexpected launch command: %s", cmd)
	}
}

func TestParseClaudeAgentsJSON(t *testing.T) {
	// Shape from `claude agents --json`: interactive sessions use "status",
	// background sessions use "state".
	data := []byte(`[
		{"pid":1,"kind":"interactive","sessionId":"idle-1","status":"idle"},
		{"pid":2,"kind":"interactive","sessionId":"busy-1","status":"busy"},
		{"pid":3,"kind":"interactive","sessionId":"wait-1","status":"waiting"},
		{"id":"x","kind":"background","sessionId":"bg-1","state":"blocked"},
		{"pid":4,"kind":"interactive","sessionId":"new-1","status":"something-new"}
	]`)

	tests := []struct {
		sessionID  string
		wantStatus backend.Status
		wantFound  bool
	}{
		{"idle-1", backend.StatusIdle, true},
		{"busy-1", backend.StatusProcessing, true},
		{"wait-1", backend.StatusWaiting, true},
		{"bg-1", backend.StatusWaiting, true},
		{"new-1", backend.StatusUnknown, true},
		{"missing", backend.StatusUnknown, false},
	}
	for _, tt := range tests {
		st, found, err := namedagents.ParseClaudeAgentsJSON(data, tt.sessionID)
		if err != nil {
			t.Fatalf("%s: %v", tt.sessionID, err)
		}
		if st != tt.wantStatus || found != tt.wantFound {
			t.Errorf("%s: got (%v, %v), want (%v, %v)", tt.sessionID, st, found, tt.wantStatus, tt.wantFound)
		}
	}
}

func TestClaudePermissionKey(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	want := map[backend.PermissionResponse]string{
		backend.PermissionApprove:       "1",
		backend.PermissionApproveAlways: "2",
		backend.PermissionDeny:          "Escape",
	}
	for resp, key := range want {
		got, err := provider.PermissionKey(resp, "Bash")
		if err != nil || got != key {
			t.Errorf("PermissionKey(%v) = %q, %v; want %q", resp, got, err, key)
		}
	}
}

// Question dialogs also raise PermissionRequest, but "1" would pick an answer.
func TestClaudePermissionKey_RefusesQuestionDialogs(t *testing.T) {
	provider := namedagents.NewClaudeBackend()
	for _, tool := range []string{"AskUserQuestion", "ExitPlanMode"} {
		if key, err := provider.PermissionKey(backend.PermissionApprove, tool); err == nil {
			t.Errorf("PermissionKey(approve, %s) = %q; want an error", tool, key)
		}
	}
	if _, err := provider.PermissionKey(backend.PermissionApprove, ""); err != nil {
		t.Errorf("empty tool should be treated as a plain prompt: %v", err)
	}
}

// Screens trimmed from Claude Code v2.1.310 (visible pane, ANSI stripped).
const (
	claudeBashDialog = `● Bash(touch fx-a.txt)

────────────────────────────────────────────────────────────────────────────────
 Bash command
 touch fx-a.txt
 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and always allow access to fx1 from this project
   3. Yes, and switch to auto mode · auto mode handles these prompts for you
   4. No

 Esc to cancel · Tab to amend

`
	claudeWriteDialog = `● Write(fx-b.txt)

────────────────────────────────────────────────────────────────────────────────
 Create file
 fx-b.txt
 Do you want to create fx-b.txt?
 ❯ 1. Yes
   2. Yes, and switch to accept edits (auto-approve file edits and common file
      commands) for this session (shift+tab)
   3. No

 Esc to cancel · Tab to amend
`
	claudeRunning = `● Bash(touch fx-a.txt)
  ⎿  Running…

✻ Cogitating… (3s · esc to interrupt)

────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  ⏸ manual mode on · ? for shortcuts
`
	claudeIdleAfterList = `● Options:
  1. Yes, refactor it
  2. No, leave it
  Esc to cancel is what the dialog says.

────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  ⏸ manual mode on · ? for shortcuts
`
)

// deny's Escape cancels a running turn, so it is only sent while the dialog
// is actually on screen.
func TestClaudePermissionPromptVisible(t *testing.T) {
	b := namedagents.NewClaudeBackend()
	for name, tt := range map[string]struct {
		screen string
		want   bool
	}{
		"bash dialog, No is 4":         {claudeBashDialog, true},
		"write dialog, No is 3":        {claudeWriteDialog, true},
		"running turn":                 {claudeRunning, false},
		"idle, reply mentions options": {claudeIdleAfterList, false},
		"empty":                        {"", false},
	} {
		if got := b.PermissionPromptVisible(tt.screen); got != tt.want {
			t.Errorf("%s: visible = %v, want %v", name, got, tt.want)
		}
	}
}

func TestClaudeWithInstructions(t *testing.T) {
	cmd := namedagents.NewClaudeBackend().WithInstructions("claude --session-id x", "/data/agents/it's/instructions.md")
	out, err := exec.Command("sh", "-c", `printf '%s\n' `+strings.TrimPrefix(cmd, "claude ")).Output()
	if err != nil {
		t.Fatal(err)
	}
	args := strings.Split(strings.TrimSpace(string(out)), "\n")
	if len(args) != 4 || args[2] != "--append-system-prompt-file" || args[3] != "/data/agents/it's/instructions.md" {
		t.Errorf("args = %q", args)
	}
}

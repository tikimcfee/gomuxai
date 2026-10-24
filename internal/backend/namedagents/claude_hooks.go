package namedagents

import (
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"regexp"
	"strings"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// claudeHookEvents are the Claude Code hook events wired to `agent report`.
// Payload shapes and firing order are documented in
// exploration/claude-hooks-findings.md.
var claudeHookEvents = []string{
	"SessionStart",
	"UserPromptSubmit",
	"PreToolUse",
	"PermissionRequest",
	"PostToolUse",
	"PostToolUseFailure",
	"PreCompact",
	"Notification",
	"Stop",
	"StopFailure",
	"SessionEnd",
}

// claudeHookTimeoutSec bounds each hook; `agent report` is a local DB write.
const claudeHookTimeoutSec = 10

// maxDetailLen keeps status details to a single readable line.
const maxDetailLen = 120

// LaunchCommandWithHooks injects status hooks via --settings, so nothing is
// written into the agent's project directory.
func (b *ClaudeBackend) LaunchCommandWithHooks(reportCmd, sessionID string) string {
	return fmt.Sprintf("%s --session-id %s --settings %s",
		b.LaunchCommand(), util.ShellQuote(sessionID), util.ShellQuote(claudeHookSettings(reportCmd)))
}

// claudeHookSettings returns the --settings JSON registering reportCmd on every hook event.
func claudeHookSettings(reportCmd string) string {
	type hookCmd struct {
		Type    string `json:"type"`
		Command string `json:"command"`
		Timeout int    `json:"timeout"`
	}
	type matcher struct {
		Matcher string    `json:"matcher"`
		Hooks   []hookCmd `json:"hooks"`
	}
	hooks := make(map[string][]matcher, len(claudeHookEvents))
	for _, event := range claudeHookEvents {
		hooks[event] = []matcher{{
			Hooks: []hookCmd{{Type: "command", Command: reportCmd, Timeout: claudeHookTimeoutSec}},
		}}
	}
	data, _ := json.Marshal(map[string]any{"hooks": hooks})
	return string(data)
}

// claudeHookPayload holds the hook payload fields we use. Every payload
// carries hook_event_name and session_id; the rest depend on the event.
type claudeHookPayload struct {
	Event                string          `json:"hook_event_name"`
	SessionID            string          `json:"session_id"`
	Source               string          `json:"source"`
	ToolName             string          `json:"tool_name"`
	ToolInput            json.RawMessage `json:"tool_input"`
	NotificationType     string          `json:"notification_type"`
	LastAssistantMessage string          `json:"last_assistant_message"`
	Reason               string          `json:"reason"`
	Error                string          `json:"error"`
}

// ParseHookEvent maps a Claude Code hook payload to a status report.
func (b *ClaudeBackend) ParseHookEvent(event string, payload []byte) (types.HookReport, bool, error) {
	var p claudeHookPayload
	if err := json.Unmarshal(payload, &p); err != nil {
		return types.HookReport{}, false, fmt.Errorf("decoding hook payload: %w", err)
	}
	if p.Event == "" {
		p.Event = event
	}

	r := types.HookReport{Event: p.Event, SessionID: p.SessionID, Tool: p.ToolName}
	switch p.Event {
	case "SessionStart":
		r.Status = types.StatusIdle
		r.Detail = "session " + p.Source
	case "UserPromptSubmit":
		r.Status = types.StatusProcessing
		r.Detail = "prompt received"
	case "PreToolUse", "PostToolUse":
		r.Status = types.StatusProcessing
		r.Detail = toolSummary(p.ToolName, p.ToolInput)
	case "PostToolUseFailure":
		r.Status = types.StatusProcessing
		r.Detail = "failed: " + toolSummary(p.ToolName, p.ToolInput)
	case "PermissionRequest":
		r.Status = types.StatusWaiting
		r.Detail = "permission: " + toolSummary(p.ToolName, p.ToolInput)
	case "PreCompact":
		r.Status = types.StatusProcessing
		r.Detail = "compacting"
	case "Notification":
		// permission_prompt arrives ~6s after PermissionRequest and can land
		// after the prompt is already answered, so only idle_prompt counts.
		if p.NotificationType != "idle_prompt" {
			return types.HookReport{}, false, nil
		}
		r.Status = types.StatusIdle
		r.Detail = "waiting for input"
	case "Stop":
		r.Status = types.StatusIdle
		r.Detail = "turn finished"
		r.Result = p.LastAssistantMessage
	case "StopFailure":
		r.Status = types.StatusError
		r.Detail = truncate(firstNonEmpty(p.Error, "turn failed"))
	case "SessionEnd":
		// /clear ends the session but not the CLI; SessionStart follows.
		if p.Reason == "clear" {
			return types.HookReport{}, false, nil
		}
		r.Status = types.StatusExited
		r.Detail = p.Reason
	default:
		return types.HookReport{}, false, nil
	}
	return r, true, nil
}

// toolSummary renders a tool call as "Tool: <most telling input>".
func toolSummary(tool string, input json.RawMessage) string {
	var fields map[string]any
	_ = json.Unmarshal(input, &fields)
	for _, key := range []string{"command", "file_path", "pattern", "url", "query", "description"} {
		if v, ok := fields[key].(string); ok && v != "" {
			return truncate(tool + ": " + v)
		}
	}
	return tool
}

// NativeStatus reads the session's status from `claude agents --json`.
func (b *ClaudeBackend) NativeStatus(ctx context.Context, sessionID string) (types.Status, bool, error) {
	ctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, "claude", "agents", "--json").Output()
	if err != nil {
		return types.StatusUnknown, false, fmt.Errorf("claude agents --json: %w", err)
	}
	return ParseClaudeAgentsJSON(out, sessionID)
}

// ParseClaudeAgentsJSON finds sessionID in `claude agents --json` output.
// Interactive sessions report "status", background sessions report "state".
func ParseClaudeAgentsJSON(data []byte, sessionID string) (types.Status, bool, error) {
	var sessions []struct {
		SessionID string `json:"sessionId"`
		Status    string `json:"status"`
		State     string `json:"state"`
	}
	if err := json.Unmarshal(data, &sessions); err != nil {
		return types.StatusUnknown, false, fmt.Errorf("decoding claude agents output: %w", err)
	}
	for _, s := range sessions {
		if s.SessionID != sessionID {
			continue
		}
		switch firstNonEmpty(s.Status, s.State) {
		case "idle":
			return types.StatusIdle, true, nil
		case "busy":
			return types.StatusProcessing, true, nil
		case "waiting", "blocked":
			return types.StatusWaiting, true, nil
		default:
			return types.StatusUnknown, true, nil
		}
	}
	return types.StatusUnknown, false, nil
}

// claudeDialogTools raise a PermissionRequest whose dialog is a question to
// the user rather than a yes/no prompt; "1" would pick an answer.
var claudeDialogTools = map[string]bool{
	"AskUserQuestion": true,
	"ExitPlanMode":    true,
}

// PermissionKey returns the key that answers Claude's permission dialog:
// "1" is "Yes", "2" is the "don't ask again" variant, Escape cancels the turn.
func (b *ClaudeBackend) PermissionKey(resp types.PermissionResponse, tool string) (string, error) {
	if claudeDialogTools[tool] {
		return "", fmt.Errorf("%s is a question for the user, not a permission prompt; answer it with send/keys (or --force)", tool)
	}
	switch resp {
	case types.PermissionApprove:
		return "1", nil
	case types.PermissionApproveAlways:
		return "2", nil
	case types.PermissionDeny:
		return "Escape", nil
	default:
		return "", fmt.Errorf("unsupported permission response %d", resp)
	}
}

// claudeDialogNoOption matches the "No" line of a permission dialog. Its
// number varies with the dialog (3 for Write, 4 for Bash in v2.1.310), and
// it behaves exactly like Escape, so it is only used for detection.
var claudeDialogNoOption = regexp.MustCompile(`(?m)^[\s❯]*\d+\.\s+No\b`)

// claudeDialogTailLines bounds the detection to the bottom of the screen,
// where the dialog sits, so a numbered "No" in an older reply can't match.
const claudeDialogTailLines = 12

// PermissionPromptVisible reports whether Claude's permission dialog is on
// screen: a numbered "No" option above the "Esc to cancel" footer. A running
// turn shows "esc to interrupt" instead, and the idle prompt neither.
func (b *ClaudeBackend) PermissionPromptVisible(screen string) bool {
	var tail []string
	lines := strings.Split(screen, "\n")
	for i := len(lines) - 1; i >= 0 && len(tail) < claudeDialogTailLines; i-- {
		if strings.TrimSpace(lines[i]) != "" {
			tail = append(tail, lines[i])
		}
	}
	if len(tail) == 0 || !strings.Contains(tail[0], "Esc to cancel") {
		return false
	}
	return claudeDialogNoOption.MatchString(strings.Join(tail, "\n"))
}

func truncate(s string) string {
	runes := []rune(strings.Join(strings.Fields(s), " "))
	if len(runes) > maxDetailLen {
		return string(runes[:maxDetailLen-1]) + "…"
	}
	return string(runes)
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

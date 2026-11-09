package namedagents

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/claudereg"
	"github.com/tikimcfee/gomuxai/internal/transcript"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Payload shapes and firing order are documented in
// exploration/claude-hooks-findings.md.

// LaunchCommandWithSession pins a new session's id; a resumed one keeps its
// own (Claude refuses --session-id with --resume unless it forks). The hooks
// come from Claude Code's user settings (agent hooks install), so nothing is
// added for them.
func (b *ClaudeBackend) LaunchCommandWithSession(sessionID string) string {
	cmd := b.LaunchCommand()
	if b.config.Resume == "" {
		cmd += " --session-id " + util.ShellQuote(sessionID)
	}
	return cmd
}

// HooksInstalled checks Claude Code's user settings for binary's hook on
// every event.
func (b *ClaudeBackend) HooksInstalled(binary string) error {
	path, err := ClaudeSettingsPath()
	if err != nil {
		return err
	}
	r, err := ClaudeHooksStatus(path, binary)
	if err != nil {
		return err
	}
	var missing []string
	for _, e := range ClaudeHookEvents {
		if r.Events[e] != HookCurrent {
			missing = append(missing, e)
		}
	}
	if live := liveOthers(r.Others); len(missing) > 0 && len(live) > 0 {
		return fmt.Errorf("Claude Code's hook (%s) runs %s, not this binary, so this binary's store would never hear from a Claude agent it spawned: spawn with that binary, or point the hook here with agent hooks install --force (which stops that binary's agents reporting)", path, strings.Join(live, ", "))
	}
	if len(missing) > 0 {
		return fmt.Errorf("Claude Code's settings (%s) don't run this binary's hook on %s, so a Claude agent couldn't report its status, take held messages, or run pre-approved agent commands: agent hooks install registers it", path, strings.Join(missing, ", "))
	}
	return nil
}

// WithInstructions appends the briefing to Claude's system prompt. Unlike a
// CLAUDE.md, nothing is written into the agent's project.
func (b *ClaudeBackend) WithInstructions(launchCmd, path string) string {
	return launchCmd + " --append-system-prompt-file " + util.ShellQuote(path)
}

// claudeHookPayload holds the hook payload fields we use. Every payload
// carries hook_event_name and session_id; the rest depend on the event.
type claudeHookPayload struct {
	Event                string          `json:"hook_event_name"`
	SessionID            string          `json:"session_id"`
	TranscriptPath       string          `json:"transcript_path"`
	Cwd                  string          `json:"cwd"`
	Source               string          `json:"source"`
	ToolName             string          `json:"tool_name"`
	ToolInput            json.RawMessage `json:"tool_input"`
	NotificationType     string          `json:"notification_type"`
	LastAssistantMessage string          `json:"last_assistant_message"`
	Reason               string          `json:"reason"`
	Error                string          `json:"error"`
	// AgentID and AgentType name the subagent a tool event came from;
	// subagents' tool hooks fire in the main session, without them.
	AgentID   string `json:"agent_id"`
	AgentType string `json:"agent_type"`
	// BackgroundTasks (Stop) lists work the session started that outlives
	// the turn. Observed entries: {"id", "type": "shell" | "subagent",
	// "status": "running", "description", "command" | "agent_type"};
	// finished tasks drop out of the list.
	BackgroundTasks []claudeBackgroundTask `json:"background_tasks"`
}

type claudeBackgroundTask struct {
	Status string `json:"status"`
}

// runningBackgroundTasks counts the tasks still at work. When one finishes,
// Claude starts a new turn on its own (a UserPromptSubmit whose prompt is a
// <task-notification>), so the session isn't idle yet. session_crons are
// left out on purpose: a cron fires later as a fresh turn, and between fires
// the agent really is idle.
func runningBackgroundTasks(tasks []claudeBackgroundTask) int {
	n := 0
	for _, t := range tasks {
		if t.Status == "running" || t.Status == "pending" {
			n++
		}
	}
	return n
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

	r := types.HookReport{Event: p.Event, SessionID: p.SessionID, Tool: p.ToolName, Transcript: p.TranscriptPath, Agent: p.AgentID}
	switch p.Event {
	case "SessionStart":
		r.Status = types.StatusIdle
		r.Detail = "session " + p.Source
		// startup, resume, clear: a different conversation. compact keeps it.
		r.ClearResult = p.Source != "compact"
		r.Prompt = types.PromptsAllEnded
	case "UserPromptSubmit":
		r.Status = types.StatusProcessing
		r.Detail = "prompt received"
		r.ClearResult = true
		r.Prompt = types.PromptsMainEnded
	case "PreToolUse":
		r.Status = types.StatusProcessing
		r.Detail = toolSummary(p.ToolName, p.ToolInput)
		if p.ToolName == "Bash" {
			var in struct {
				Command string `json:"command"`
			}
			_ = json.Unmarshal(p.ToolInput, &in)
			r.Command, r.Cwd = in.Command, p.Cwd
		}
	case "PostToolUse":
		r.Status = types.StatusProcessing
		r.Detail = toolSummary(p.ToolName, p.ToolInput)
		r.Prompt, r.CallKey = types.PromptResolved, callKey(p.ToolName, p.ToolInput)
	case "PostToolUseFailure":
		r.Status = types.StatusProcessing
		r.Detail = "failed: " + toolSummary(p.ToolName, p.ToolInput)
		r.Prompt, r.CallKey = types.PromptResolved, callKey(p.ToolName, p.ToolInput)
	case "PermissionRequest":
		r.Status = types.StatusWaiting
		r.Detail = "permission: " + toolSummary(p.ToolName, p.ToolInput)
		switch {
		case p.AgentType != "":
			// As Claude's dialog says: "Bash command · from the general-purpose agent".
			r.Detail += fmt.Sprintf(" (from the %s agent)", p.AgentType)
		case p.AgentID != "":
			r.Detail += " (from a subagent)"
		}
		r.ToolInput = toolInput(p.ToolInput)
		r.Prompt, r.CallKey = types.PromptOpened, callKey(p.ToolName, p.ToolInput)
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
		if n := runningBackgroundTasks(p.BackgroundTasks); n > 0 {
			// The reply is interim ("started it, will report back"); the
			// real one comes from the turn the task's completion starts.
			r.Status = types.StatusProcessing
			r.Detail = fmt.Sprintf("turn finished, %d background task(s) running", n)
			break
		}
		// Nothing left running, so nothing can be asking. (While background
		// subagents run, their prompts outlive the turn.)
		r.Status = types.StatusIdle
		r.Detail = "turn finished"
		r.Result = p.LastAssistantMessage
		r.Prompt = types.PromptsAllEnded
	case "StopFailure":
		r.Status = types.StatusError
		r.Detail = truncate(firstNonEmpty(p.Error, "turn failed"))
		r.Prompt = types.PromptsAllEnded
	case "SessionEnd":
		// /clear ends the session but not the CLI; SessionStart follows.
		if p.Reason == "clear" {
			return types.HookReport{}, false, nil
		}
		r.Status, r.EndReason = types.StatusEnded, types.EndQuit
		r.Detail = p.Reason
		r.Prompt = types.PromptsAllEnded
	default:
		return types.HookReport{}, false, nil
	}
	return r, true, nil
}

// CanDeliver: Claude's Stop, with or without background work running. Its
// additionalContext continues the turn. Nothing fires when a turn is
// interrupted (Esc, a deny), and later hooks of a running turn
// (PostToolUse) only add context the model was seen to ignore.
func (b *ClaudeBackend) CanDeliver(r types.HookReport) bool {
	return r.Event == "Stop"
}

func (b *ClaudeBackend) DeliveryPoint() string { return "its turn ends" }

// DeliveryOutput returns the messages as Stop's additionalContext, under a
// line saying what they are. Claude goes on with it and shows it as "Stop
// hook feedback: ...". A {"decision":"block"} works too, but shows as "Stop
// hook error" with an error footer.
func (b *ClaudeBackend) DeliveryOutput(messages []string) []byte {
	head := "A message arrived while you were working:"
	if len(messages) > 1 {
		head = fmt.Sprintf("%d messages arrived while you were working, oldest first:", len(messages))
	}
	out, _ := json.Marshal(map[string]any{
		"hookSpecificOutput": map[string]string{
			"hookEventName":     "Stop",
			"additionalContext": head + "\n\n" + strings.Join(messages, "\n\n"),
		},
	})
	return out
}

// AllowOutput approves PreToolUse's tool call: Claude runs it without a
// permission prompt, and shows reason to the person, not the model.
func (b *ClaudeBackend) AllowOutput(reason string) []byte {
	out, _ := json.Marshal(map[string]any{
		"hookSpecificOutput": map[string]string{
			"hookEventName":            "PreToolUse",
			"permissionDecision":       "allow",
			"permissionDecisionReason": reason,
		},
	})
	return out
}

// TranscriptReader reads the session JSONL whose path hooks report as
// transcript_path.
func (b *ClaudeBackend) TranscriptReader() transcript.Reader {
	return transcript.Claude{}
}

// NativeStatus reads the session's status from Claude Code's session
// registry (internal/claudereg): found is false when no running process
// holds the session.
func (b *ClaudeBackend) NativeStatus(ctx context.Context, sessionID string) (types.Status, bool, error) {
	snap, err := claudereg.ReadSystem()
	if err != nil {
		return types.StatusUnknown, false, err
	}
	e, ok := snap.Sessions()[sessionID]
	if !ok {
		// Not listed means not running only if every file could be read.
		return types.StatusUnknown, false, snap.Err()
	}
	st, detail := RegistryStatus(e.Entry)
	if e.Unknown != "" {
		// Listed, running, in a state gomuxai can't name: say which.
		return st, true, errors.New(detail)
	}
	return st, true, nil
}

// ClaudeSessions returns the status of every Claude Code session running on
// this host, by session id, from the session registry.
func ClaudeSessions(ctx context.Context) (map[string]types.Status, error) {
	snap, err := claudereg.ReadSystem()
	if err == nil {
		err = snap.Err() // an unread file could be any session: none can be called gone
	}
	if err != nil {
		return nil, err
	}
	out := map[string]types.Status{}
	for id, e := range snap.Sessions() {
		out[id], _ = RegistryStatus(e.Entry)
	}
	return out, nil
}

// RegistryStatus maps a registry entry's status to an agent status, with a
// line saying what it means.
func RegistryStatus(e claudereg.Entry) (types.Status, string) {
	if e.Unknown != "" {
		return types.StatusUnknown, e.Unknown
	}
	switch e.Status {
	case claudereg.StatusBusy:
		return types.StatusProcessing, "a turn is running"
	case claudereg.StatusIdle:
		return types.StatusIdle, "at its prompt"
	case claudereg.StatusShell:
		// The turn is over, but a shell it started still runs in the
		// background, and its end starts the next turn (a task
		// notification): not done, as a Stop with background tasks isn't.
		// Claude Code's own `claude agents` calls it busy too.
		return types.StatusProcessing, "turn over; a background shell it started is still running, and its end starts the next turn"
	case claudereg.StatusWaiting:
		if e.WaitingFor != "" {
			return types.StatusWaiting, "waiting for " + e.WaitingFor
		}
		return types.StatusWaiting, "waiting for a person"
	}
	// No status recorded yet (a CLI still starting).
	return types.StatusUnknown, "the CLI has not recorded a status yet"
}

// PaneSession finds the Claude session a pane runs: of the running
// processes the registry proves (pid and start time), the one nearest the
// pane's process in the process tree. A pane whose CLI was replaced (an
// exit, then `claude` typed at the shell) answers with the one running now;
// a `claude -p` started from inside the session is deeper, so it is never
// taken for it.
func (b *ClaudeBackend) PaneSession(ctx context.Context, panePID int) (types.PaneSession, bool, error) {
	snap, err := claudereg.ReadSystem()
	if err != nil {
		return types.PaneSession{}, false, err
	}
	tree, err := util.ReadProcTree(ctx)
	if err != nil {
		return types.PaneSession{}, false, err
	}
	e, ok, err := snap.UnderPane(tree, panePID)
	if err == nil && !ok {
		err = snap.Err() // nothing found means nothing runs only if every file could be read
	}
	if err != nil || !ok {
		return types.PaneSession{}, false, err
	}
	st, detail := RegistryStatus(e.Entry)
	return types.PaneSession{ID: e.SessionID, PID: e.PID, Status: st, Detail: detail}, true, nil
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
	return permissionKey(resp, tool, claudeDialogTools)
}

// claudeDialogNoOption matches the "No" line of a permission dialog. Its
// number varies with the dialog (3 for Write, 4 for Bash in v2.1.310), and
// it behaves exactly like Escape, so it is only used for detection.
var claudeDialogNoOption = regexp.MustCompile(`(?m)^[\s❯]*\d+\.\s+No\b`)

// PermissionPromptVisible reports whether Claude's permission dialog is on
// screen: a numbered "No" option above the "Esc to cancel" footer. A running
// turn shows "esc to interrupt" instead, and the idle prompt neither.
func (b *ClaudeBackend) PermissionPromptVisible(screen string) bool {
	tail := screenTail(screen)
	if len(tail) == 0 || !strings.Contains(tail[0], "Esc to cancel") {
		return false
	}
	return claudeDialogNoOption.MatchString(strings.Join(tail, "\n"))
}

// InputText reads Claude's input box: the lines between the last two rules
// of "─" at the bottom of the screen, the first starting with the "❯"
// prompt. Placeholder text ("Try ...") and suggestions are drawn dim, so
// dim text is left out; what remains was typed.
func (b *ClaudeBackend) InputText(screen string) (string, bool) {
	return inputBoxText(screen, "❯", 0)
}

// EnterCanBeANewline: Claude takes input that arrives in one read with more
// than a keystroke in it for a paste, where "\r" is a newline. A message
// typed while its event loop is busy (a long render, a paste it is still
// collecting) arrives in one read with the Enter typed after it.
func (b *ClaudeBackend) EnterCanBeANewline() {}

// AllowCommands pre-approves Bash commands by prefix for this session only,
// via --allowedTools rules like Bash(agent status:*).
func (b *ClaudeBackend) AllowCommands(launchCmd string, prefixes []string) string {
	if len(prefixes) == 0 {
		return launchCmd
	}
	rules := make([]string, len(prefixes))
	for i, p := range prefixes {
		rules[i] = util.ShellQuote("Bash(" + p + ":*)")
	}
	return launchCmd + " --allowedTools " + strings.Join(rules, " ")
}

// claudeTrust is Claude's folder trust dialog. Its highlighted option is
// "No, exit", so a bare Enter would quit.
var claudeTrust = trustDialog{question: "Quick safety check", marker: "❯", footer: "Enter to confirm", below: 1}

// TrustPrompt reads Claude's folder trust dialog off the screen.
func (b *ClaudeBackend) TrustPrompt(screen string) (string, bool) {
	folder, _, ok := claudeTrust.read(screen)
	return folder, ok
}

// TrustKeys accepts Claude's folder trust dialog, moving to "Yes" first if
// "No, exit" is highlighted.
func (b *ClaudeBackend) TrustKeys(screen string) []string {
	return claudeTrust.keys(screen, true)
}

// DeclineTrustKeys picks "No, exit" at Claude's folder trust dialog.
func (b *ClaudeBackend) DeclineTrustKeys(screen string) []string {
	return claudeTrust.keys(screen, false)
}

package namedagents

import (
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/transcript"
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

// LaunchCommandWithHooks injects status hooks via --settings, so nothing is
// written into the agent's project directory. A new session takes sessionID
// as its id; a resumed one keeps its own (Claude refuses --session-id with
// --resume unless it forks).
func (b *ClaudeBackend) LaunchCommandWithHooks(reportCmd, sessionID string) string {
	cmd := b.LaunchCommand()
	if b.config.Resume == "" {
		cmd += " --session-id " + util.ShellQuote(sessionID)
	}
	return cmd + " --settings " + util.ShellQuote(claudeHookSettings(reportCmd))
}

// WithInstructions appends the briefing to Claude's system prompt. Unlike a
// CLAUDE.md, nothing is written into the agent's project.
func (b *ClaudeBackend) WithInstructions(launchCmd, path string) string {
	return launchCmd + " --append-system-prompt-file " + util.ShellQuote(path)
}

// claudeHookSettings returns the --settings JSON registering reportCmd on every hook event.
func claudeHookSettings(reportCmd string) string {
	hooks := make(map[string][]hookMatcher, len(claudeHookEvents))
	for _, event := range claudeHookEvents {
		hooks[event] = []hookMatcher{{Hooks: []hookCommand{newHookCommand(reportCmd)}}}
	}
	data, _ := json.Marshal(map[string]any{"hooks": hooks})
	return string(data)
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
		r.Status = types.StatusExited
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

// NativeStatus reads the session's status from `claude agents --json`.
func (b *ClaudeBackend) NativeStatus(ctx context.Context, sessionID string) (types.Status, bool, error) {
	out, err := claudeAgents.get(ctx)
	if err != nil {
		return types.StatusUnknown, false, err
	}
	return ParseClaudeAgentsJSON(out, sessionID)
}

// claudeAgentsTTL is how long one `claude agents --json` answer serves
// other agents' status checks in the same process, so listing several
// agents' status runs the CLI once rather than once per agent. Well under
// the 2s and 5s re-check intervals of wait and the watcher.
const claudeAgentsTTL = 500 * time.Millisecond

// claudeAgents caches `claude agents --json` for claudeAgentsTTL.
var claudeAgents claudeAgentsCache

type claudeAgentsCache struct {
	mu  sync.Mutex
	at  time.Time
	out []byte
	err error // a failure is cached too: a hung CLI shouldn't cost 3s per agent
}

func (c *claudeAgentsCache) get(ctx context.Context) ([]byte, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if !c.at.IsZero() && time.Since(c.at) < claudeAgentsTTL {
		return c.out, c.err
	}
	ctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, "claude", "agents", "--json").Output()
	if err != nil {
		err = fmt.Errorf("claude agents --json: %w", err)
	}
	c.out, c.err, c.at = out, err, time.Now()
	return out, err
}

// ClaudeSessions returns the status of every Claude Code session running on
// this host, by session id, from `claude agents --json`.
func ClaudeSessions(ctx context.Context) (map[string]types.Status, error) {
	out, err := claudeAgents.get(ctx)
	if err != nil {
		return nil, err
	}
	return parseClaudeAgents(out)
}

// ParseClaudeAgentsJSON finds sessionID in `claude agents --json` output.
func ParseClaudeAgentsJSON(data []byte, sessionID string) (types.Status, bool, error) {
	sessions, err := parseClaudeAgents(data)
	if err != nil {
		return types.StatusUnknown, false, err
	}
	status, found := sessions[sessionID]
	if !found {
		return types.StatusUnknown, false, nil
	}
	return status, true, nil
}

// parseClaudeAgents reads `claude agents --json` output into each session's
// status. Interactive sessions report "status", background sessions report
// "state".
func parseClaudeAgents(data []byte) (map[string]types.Status, error) {
	var sessions []struct {
		SessionID string `json:"sessionId"`
		Status    string `json:"status"`
		State     string `json:"state"`
	}
	if err := json.Unmarshal(data, &sessions); err != nil {
		return nil, fmt.Errorf("decoding claude agents output: %w", err)
	}
	out := make(map[string]types.Status, len(sessions))
	for _, s := range sessions {
		switch firstNonEmpty(s.Status, s.State) {
		case "idle":
			out[s.SessionID] = types.StatusIdle
		case "busy":
			out[s.SessionID] = types.StatusProcessing
		case "waiting", "blocked":
			out[s.SessionID] = types.StatusWaiting
		default:
			out[s.SessionID] = types.StatusUnknown
		}
	}
	return out, nil
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
	return inputBoxText(screen, "❯")
}

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

// TrustKeys accepts Claude's folder trust dialog. Its highlighted option is
// "No, exit", so a bare Enter would quit: move to "Yes" first if needed.
func (b *ClaudeBackend) TrustKeys(screen string) []string {
	if !strings.Contains(screen, "Yes, I trust this folder") || !strings.Contains(screen, "No, exit") {
		return nil
	}
	if strings.Contains(screen, "❯ Yes, I trust this folder") {
		return []string{"Enter"}
	}
	return []string{"Down", "Enter"}
}

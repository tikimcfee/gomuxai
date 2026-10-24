package namedagents

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Agy status detection patterns (fallback when hooks have not reported yet).
var (
	// Idle: prompt at end of output
	agyIdlePattern = regexp.MustCompile(`(?m)^>\s*$`)

	// Processing: spinner characters, thinking indicators, tool use
	agyProcessingPattern = regexp.MustCompile(`(⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Thinking|Running|Reading|Editing)`)

	// Error: permission denied, rate limits, API errors
	agyErrorPattern = regexp.MustCompile(`(?i)(Permission denied|Rate limit|Error:|API Error|failed|could not)`)
)

// agyHookTimeoutSec bounds each hook; `agent report` is a local DB write.
const agyHookTimeoutSec = 10

// AgyConfig holds configuration options for the Antigravity CLI (agy) backend.
type AgyConfig struct {
	// SkipPermissions passes --dangerously-skip-permissions to agy CLI.
	SkipPermissions bool

	// AgentDir is the per-agent directory (e.g. .gomuxai/agents/<name>) where
	// the generated .agents/hooks.json bundle is written for --add-dir.
	AgentDir string
}

// AgyBackend implements Provider, HookProvider, PermissionResponder, and
// PromptFlagProvider for the Antigravity CLI (agy).
type AgyBackend struct {
	config     AgyConfig
	exitSuffix string
}

// NewAgyBackend creates a new Agy backend with default config.
func NewAgyBackend() *AgyBackend {
	return &AgyBackend{}
}

// NewAgyBackendWithConfig creates an Agy backend with the given config.
func NewAgyBackendWithConfig(cfg AgyConfig) *AgyBackend {
	return &AgyBackend{config: cfg}
}

// Identity

func (b *AgyBackend) Name() string {
	return "agy"
}

func (b *AgyBackend) SessionPrefix() string {
	return "agent-"
}

// Lifecycle

func (b *AgyBackend) Initialize(ctx context.Context) error {
	return nil
}

func (b *AgyBackend) Cleanup() error {
	if b.config.AgentDir != "" {
		_ = os.RemoveAll(b.config.AgentDir)
	}
	return nil
}

// Status detection (fallback when no hook report is recorded)

func (b *AgyBackend) GetStatus(ctx context.Context, output string) (types.Status, error) {
	if agyErrorPattern.MatchString(output) {
		return types.StatusError, nil
	}

	if agyProcessingPattern.MatchString(output) {
		return types.StatusProcessing, nil
	}

	lines := strings.Split(strings.TrimRight(output, "\n"), "\n")
	if len(lines) > 0 {
		lastLine := lines[len(lines)-1]
		if agyIdlePattern.MatchString(lastLine) || strings.TrimSpace(lastLine) == ">" {
			return types.StatusIdle, nil
		}
	}

	if agyIdlePattern.MatchString(output) {
		return types.StatusIdle, nil
	}

	return types.StatusUnknown, nil
}

// GetIdlePatternForLog returns a regex pattern for file watcher optimization.
func (b *AgyBackend) GetIdlePatternForLog() string {
	return `(?m)^>\s*$`
}

// ExtractLastMessage extracts the last assistant message from output.
func (b *AgyBackend) ExtractLastMessage(output string) string {
	output = strings.TrimRight(output, "\n\r\t ")
	if strings.HasSuffix(output, "> ") {
		output = strings.TrimSuffix(output, "> ")
	}
	if strings.HasSuffix(output, ">") {
		output = strings.TrimSuffix(output, ">")
	}
	output = strings.TrimRight(output, "\n\r\t ")
	return output
}

// Commands

func (b *AgyBackend) LaunchCommand() string {
	if b.config.SkipPermissions {
		return "agy --dangerously-skip-permissions"
	}
	return "agy"
}

// AppendPromptFlag delivers the initial task via agy's -i (--prompt-interactive)
// flag so the session starts interactively and runs the initial prompt once
// workspace trust is confirmed. If LaunchCommandWithHooks attached an exit
// report command ("; <agent> report exited ..."), -i is placed before it.
func (b *AgyBackend) AppendPromptFlag(launchCmd, prompt string) string {
	if prompt == "" {
		return launchCmd
	}
	if b.exitSuffix != "" && strings.HasSuffix(launchCmd, b.exitSuffix) {
		base := strings.TrimSuffix(launchCmd, b.exitSuffix)
		return fmt.Sprintf("%s -i %s%s", base, util.ShellQuote(prompt), b.exitSuffix)
	}
	return fmt.Sprintf("%s -i %s", launchCmd, util.ShellQuote(prompt))
}

func (b *AgyBackend) InitMessage(taskFile string) string {
	if taskFile != "" {
		return fmt.Sprintf("Please read and execute the task in %s", taskFile)
	}
	return ""
}

func (b *AgyBackend) ExitCommand() string {
	return "/exit"
}

// Legacy Backend interface methods (for backwards compatibility)

func (b *AgyBackend) IdlePattern() string {
	return `(?m)^>\s*$`
}

func (b *AgyBackend) ProcessingPattern() string {
	return `(⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Thinking)`
}

// HookProvider implementation

// LaunchCommandWithHooks writes a per-agent .agents/hooks.json bundle under
// the gomuxai data dir (.gomuxai/agents/<name>/.agents/hooks.json) and injects
// it via --add-dir so nothing is written into the project directory.
// Because agy runs hooks with cwd set to the bundle dir and omits the event
// name from stdin JSON, each hook command uses the absolute reportCmd and
// passes --event <Event>. Since agy has no SessionEnd hook or native session
// list, the command is followed by "; <agent> report exited ..." so the parent
// shell records exit (including crashes and SIGKILL) with --spawn-id guarding
// against stale respawns.
func (b *AgyBackend) LaunchCommandWithHooks(reportCmd, sessionID string) string {
	bundleDir := b.bundleDir(sessionID)
	if err := WriteAgyHooksBundle(bundleDir, reportCmd); err != nil {
		return b.LaunchCommand()
	}
	b.exitSuffix = "; " + agyExitReportCmd(reportCmd)
	return fmt.Sprintf("%s --add-dir %s%s", b.LaunchCommand(), util.ShellQuote(bundleDir), b.exitSuffix)
}

func agyExitReportCmd(reportCmd string) string {
	return strings.Replace(reportCmd, " report --hook", " report exited", 1)
}

func (b *AgyBackend) bundleDir(sessionID string) string {
	if b.config.AgentDir != "" {
		return b.config.AgentDir
	}
	id := sessionID
	if id == "" {
		id = "default"
	}
	return filepath.Join(".gomuxai", "agents", id)
}

// WriteAgyHooksBundle writes <bundleDir>/.agents/hooks.json registering
// PreInvocation, PreToolUse, PostToolUse, and Stop hooks that invoke reportCmd.
func WriteAgyHooksBundle(bundleDir, reportCmd string) error {
	hooksDir := filepath.Join(bundleDir, ".agents")
	if err := os.MkdirAll(hooksDir, 0755); err != nil {
		return fmt.Errorf("creating agy hooks dir: %w", err)
	}
	data, err := agyHooksJSON(reportCmd)
	if err != nil {
		return err
	}
	hooksFile := filepath.Join(hooksDir, "hooks.json")
	if err := os.WriteFile(hooksFile, data, 0644); err != nil {
		return fmt.Errorf("writing agy hooks file: %w", err)
	}
	return nil
}

// agyHooksJSON builds the .agents/hooks.json document for agy.
// PreInvocation and Stop take flat handler arrays; PreToolUse and PostToolUse
// take matcher-grouped arrays.
func agyHooksJSON(reportCmd string) ([]byte, error) {
	type hookCmd struct {
		Type    string `json:"type"`
		Command string `json:"command"`
		Timeout int    `json:"timeout"`
	}
	type matcherGroup struct {
		Matcher string    `json:"matcher"`
		Hooks   []hookCmd `json:"hooks"`
	}
	cmdFor := func(event string) hookCmd {
		return hookCmd{
			Type:    "command",
			Command: fmt.Sprintf("%s --event %s", reportCmd, event),
			Timeout: agyHookTimeoutSec,
		}
	}
	doc := map[string]any{
		"gomuxai": map[string]any{
			"PreInvocation": []hookCmd{cmdFor("PreInvocation")},
			"PreToolUse": []matcherGroup{{
				Matcher: "*",
				Hooks:   []hookCmd{cmdFor("PreToolUse")},
			}},
			"PostToolUse": []matcherGroup{{
				Matcher: "*",
				Hooks:   []hookCmd{cmdFor("PostToolUse")},
			}},
			"Stop": []hookCmd{cmdFor("Stop")},
		},
	}
	return json.MarshalIndent(doc, "", "  ")
}

// agyHookPayload holds the camelCase fields delivered by agy hooks on stdin.
// Payload shapes and firing order are documented in
// exploration/agy-hooks-findings.md.
type agyHookPayload struct {
	Event             string      `json:"hook_event_name"`
	ConversationID    string      `json:"conversationId"`
	LastUserInput     string      `json:"lastUserInput"`
	InvocationNum     int         `json:"invocationNum"`
	ExecutionNum      int         `json:"executionNum"`
	TerminationReason string      `json:"terminationReason"`
	Error             string      `json:"error"`
	FullyIdle         *bool       `json:"fullyIdle"`
	StepIdx           int         `json:"stepIdx"`
	ToolCall          agyToolCall `json:"toolCall"`
}

type agyToolCall struct {
	Name string          `json:"name"`
	Args json.RawMessage `json:"args"`
}

// ParseHookEvent maps an agy hook event and payload to a status report.
// Note: agy has no SessionEnd or PermissionRequest/waiting hook event;
// neither status is faked here (process exit is detected via tmux pane liveness).
func (b *AgyBackend) ParseHookEvent(event string, payload []byte) (types.HookReport, bool, error) {
	var p agyHookPayload
	if err := json.Unmarshal(payload, &p); err != nil {
		return types.HookReport{}, false, fmt.Errorf("decoding hook payload: %w", err)
	}
	ev := firstNonEmpty(event, p.Event)
	r := types.HookReport{Event: ev, SessionID: p.ConversationID, Tool: p.ToolCall.Name}

	switch ev {
	case "PreInvocation":
		r.Status = types.StatusProcessing
		r.Detail = "prompt received"
		r.ClearResult = p.InvocationNum == 0
	case "PreToolUse":
		r.Status = types.StatusProcessing
		r.Detail = agyToolSummary(p.ToolCall.Name, p.ToolCall.Args)
	case "PostToolUse":
		r.Status = types.StatusProcessing
		summary := agyToolSummary(p.ToolCall.Name, p.ToolCall.Args)
		if p.Error != "" {
			r.Detail = "failed: " + summary
		} else {
			r.Detail = summary
		}
	case "Stop":
		if p.Error != "" || isAgyErrorTermination(p.TerminationReason) {
			r.Status = types.StatusError
			r.Detail = truncate(firstNonEmpty(p.Error, p.TerminationReason, "turn failed"))
		} else if p.FullyIdle != nil && !*p.FullyIdle {
			r.Status = types.StatusProcessing
			r.Detail = "background tasks running"
		} else {
			r.Status = types.StatusIdle
			if p.TerminationReason != "" && p.TerminationReason != "NO_TOOL_CALL" {
				r.Detail = p.TerminationReason
			} else {
				r.Detail = "turn finished"
			}
		}
	default:
		return types.HookReport{}, false, nil
	}
	return r, true, nil
}

func isAgyErrorTermination(reason string) bool {
	switch reason {
	case "ERROR", "MAX_INVOCATIONS", "MAX_FORCED_INVOCATIONS", "MAX_TOKEN_BUDGET_EXCEEDED":
		return true
	default:
		return false
	}
}

// agyToolSummary renders an agy tool call as "tool: <most telling arg>".
func agyToolSummary(tool string, args json.RawMessage) string {
	if tool == "" {
		return "tool"
	}
	var fields map[string]any
	_ = json.Unmarshal(args, &fields)
	for _, key := range []string{
		"CommandLine", "Path", "TargetFile", "SearchPath", "Query", "Pattern", "Url", "ToolName", "toolSummary",
		"command", "file_path", "pattern", "url", "query", "description",
	} {
		if v, ok := fields[key].(string); ok && v != "" {
			return truncate(tool + ": " + v)
		}
	}
	return tool
}

// agyDialogTools present a multiple-choice question dialog to the user rather
// than a yes/no permission prompt; pressing "1" would pick an answer.
var agyDialogTools = map[string]bool{
	"ask_question": true,
}

// PermissionKey returns the key that answers agy's numbered permission dialog:
// "1" approves once, "2" approves always in the conversation, Escape cancels/denies.
func (b *AgyBackend) PermissionKey(resp types.PermissionResponse, tool string) (string, error) {
	if agyDialogTools[tool] {
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

// agyDialogYesOption and agyDialogNoOption match the numbered "1. Yes" and
// "N. No" lines of agy's permission dialog.
var (
	agyDialogYesOption = regexp.MustCompile(`(?m)^[\s>❯]*1\.\s+Yes\b`)
	agyDialogNoOption  = regexp.MustCompile(`(?m)^[\s>❯]*\d+\.\s+No\b`)
)

// agyDialogTailLines bounds detection to the bottom of the screen so a
// numbered list in an older reply cannot match.
const agyDialogTailLines = 12

// PermissionPromptVisible reports whether agy's permission dialog is on
// screen: numbered "1. Yes" and "N. No" options above the "esc to cancel"
// statusline footer. An idle prompt shows "? for shortcuts" instead, and
// a question dialog (ask_question) shows "esc to discard".
func (b *AgyBackend) PermissionPromptVisible(screen string) bool {
	var tail []string
	lines := strings.Split(screen, "\n")
	for i := len(lines) - 1; i >= 0 && len(tail) < agyDialogTailLines; i-- {
		if strings.TrimSpace(lines[i]) != "" {
			tail = append(tail, lines[i])
		}
	}
	if len(tail) == 0 || !strings.Contains(strings.ToLower(tail[0]), "esc to cancel") {
		return false
	}
	joined := strings.Join(tail, "\n")
	return agyDialogYesOption.MatchString(joined) && agyDialogNoOption.MatchString(joined)
}

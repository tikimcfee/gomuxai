package namedagents

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"unicode/utf8"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/transcript"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// AgyConfig holds configuration options for the Antigravity CLI (agy) backend.
type AgyConfig struct {
	// SkipPermissions passes --dangerously-skip-permissions to agy CLI.
	SkipPermissions bool

	// AgentDir is the per-agent directory (e.g. .gomuxai/agents/<name>) where
	// the generated .agents/hooks.json bundle is written for --add-dir.
	AgentDir string
}

// AgyBackend implements Provider, and most optional backend interfaces, for
// the Antigravity CLI (agy).
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

func (b *AgyBackend) Name() string {
	return "agy"
}

// agyEnv clears the variables agy uses to decide it's in a remote SSH session.
// When they're set, agy skips the OS keyring and every launch comes up "not
// signed in", even with a valid saved login. A tmux session created from an
// SSH connection inherits them, so spawned agents would too.
const agyEnv = "env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY "

func (b *AgyBackend) LaunchCommand() string {
	if b.config.SkipPermissions {
		return agyEnv + "agy --dangerously-skip-permissions"
	}
	return agyEnv + "agy"
}

// TrustKeys accepts agy's workspace trust prompt, whose default is "Yes".
func (b *AgyBackend) TrustKeys(screen string) []string {
	if strings.Contains(screen, "Do you trust the contents of this project?") {
		return []string{"Enter"}
	}
	return nil
}

// WithInstructions delivers the briefing as AGENTS.md in a directory passed
// with --add-dir: agy loads AGENTS.md (and GEMINI.md) from the root of added
// directories, not from .agents/rules. The briefing already lives in the
// agent's own directory, which doubles as the hooks bundle, so usually the
// --add-dir is already there.
func (b *AgyBackend) WithInstructions(launchCmd, path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		return launchCmd
	}
	dir := filepath.Dir(path)
	if err := os.WriteFile(filepath.Join(dir, "AGENTS.md"), data, 0600); err != nil {
		return launchCmd
	}
	addDir := " --add-dir " + util.ShellQuote(dir)
	if strings.Contains(launchCmd, addDir) {
		return launchCmd
	}
	return b.insertBeforeExit(launchCmd, addDir)
}

// AppendPromptFlag delivers the initial task via agy's -i (--prompt-interactive)
// flag so the session starts interactively and runs the initial prompt once
// workspace trust is confirmed. If LaunchCommandWithHooks attached an exit
// report command ("; <agent> report exited ..."), -i is placed before it.
func (b *AgyBackend) AppendPromptFlag(launchCmd, prompt string) string {
	if prompt == "" {
		return launchCmd
	}
	return b.insertBeforeExit(launchCmd, " -i "+util.ShellQuote(prompt))
}

// insertBeforeExit appends args to launchCmd, ahead of the exit report that
// LaunchCommandWithHooks attached, so they go to agy rather than the report.
func (b *AgyBackend) insertBeforeExit(launchCmd, args string) string {
	if b.exitSuffix != "" && strings.HasSuffix(launchCmd, b.exitSuffix) {
		return strings.TrimSuffix(launchCmd, b.exitSuffix) + args + b.exitSuffix
	}
	return launchCmd + args
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

// Backend interface: status scraping, used only before the first hook report.

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
	if err := writeAgyHooksBundle(bundleDir, reportCmd); err != nil {
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

// writeAgyHooksBundle writes <bundleDir>/.agents/hooks.json registering
// PreInvocation, PreToolUse, PostToolUse, PostInvocation, and Stop hooks
// that invoke reportCmd.
func writeAgyHooksBundle(bundleDir, reportCmd string) error {
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
// PreInvocation, PostInvocation, and Stop take flat handler arrays;
// PreToolUse and PostToolUse take matcher-grouped arrays. PostInvocation
// reports no status: it hands over held messages (DeliveryOutput).
func agyHooksJSON(reportCmd string) ([]byte, error) {
	cmdFor := func(event string) hookCommand {
		return newHookCommand(fmt.Sprintf("%s --event %s", reportCmd, event))
	}
	doc := map[string]any{
		"gomuxai": map[string]any{
			"PreInvocation": []hookCommand{cmdFor("PreInvocation")},
			"PreToolUse": []hookMatcher{{
				Matcher: "*",
				Hooks:   []hookCommand{cmdFor("PreToolUse")},
			}},
			"PostToolUse": []hookMatcher{{
				Matcher: "*",
				Hooks:   []hookCommand{cmdFor("PostToolUse")},
			}},
			"PostInvocation": []hookCommand{cmdFor("PostInvocation")},
			"Stop":           []hookCommand{cmdFor("Stop")},
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
	TranscriptPath    string      `json:"transcriptPath"`
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
// agy has no SessionEnd or permission hook event, so neither status comes
// from here: the launch command reports the exit, and the service tells a
// permission dialog from the screen (PermissionPromptVisible), as it does a
// question dialog (QuestionPromptVisible) and a turn that Esc or Ctrl+C
// ended, which fires no hook (AtPrompt).
func (b *AgyBackend) ParseHookEvent(event string, payload []byte) (types.HookReport, bool, error) {
	var p agyHookPayload
	if err := json.Unmarshal(payload, &p); err != nil {
		return types.HookReport{}, false, fmt.Errorf("decoding hook payload: %w", err)
	}
	ev := firstNonEmpty(event, p.Event)
	r := types.HookReport{Event: ev, SessionID: p.ConversationID, Tool: p.ToolCall.Name, Transcript: p.TranscriptPath}

	switch ev {
	case "PreInvocation":
		r.Status = types.StatusProcessing
		r.Detail = "prompt received"
		r.ClearResult = p.InvocationNum == 0
	case "PreToolUse":
		r.Status = types.StatusProcessing
		r.Detail = agyToolSummary(p.ToolCall.Name, p.ToolCall.Args)
		// agy shows its permission dialog after PreToolUse, with no hook of
		// its own: keep the input for the status that promotes this to
		// waiting.
		r.ToolInput = toolInput(p.ToolCall.Args)
	case "PostToolUse":
		r.Status = types.StatusProcessing
		summary := agyToolSummary(p.ToolCall.Name, p.ToolCall.Args)
		if p.Error != "" {
			r.Detail = "failed: " + summary
		} else {
			r.Detail = summary
		}
	case "PostInvocation":
		// Fires after each model call and its tools, the last one too, so
		// it is where held messages are handed over. A failed or cancelled
		// call is left to Stop: force_continue would override the cancel.
		if p.Error != "" || isAgyErrorTermination(p.TerminationReason) || p.TerminationReason == "USER_CANCELED" {
			return types.HookReport{}, false, nil
		}
		r.Status = types.StatusProcessing
		r.DeliveryOnly = true
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

// TranscriptReader reads the conversation transcript whose path hooks
// report as transcriptPath.
func (b *AgyBackend) TranscriptReader() transcript.Reader {
	return transcript.Agy{}
}

func isAgyErrorTermination(reason string) bool {
	switch reason {
	case "ERROR", "MAX_INVOCATIONS", "MAX_FORCED_INVOCATIONS", "MAX_TOKEN_BUDGET_EXCEEDED":
		return true
	default:
		return false
	}
}

// agyToolArgKeys are the toolCall.args fields that best describe a tool
// call, most telling first; Claude-style names follow agy's own.
var agyToolArgKeys = append([]string{
	"CommandLine", "Path", "TargetFile", "SearchPath", "Query", "Pattern", "Url", "ToolName", "toolSummary",
}, claudeToolInputKeys...)

// agyToolSummary renders an agy tool call as "tool: <most telling arg>".
func agyToolSummary(tool string, args json.RawMessage) string {
	if tool == "" {
		return "tool"
	}
	if tool == "ask_question" {
		return agyQuestionSummary(tool, args)
	}
	return toolSummary(tool, args, agyToolArgKeys)
}

// agyQuestionSummary renders an ask_question call as "ask_question: <first
// question> (<its options>)", and how many more questions it asks.
func agyQuestionSummary(tool string, args json.RawMessage) string {
	var call struct {
		Questions []struct {
			Question string   `json:"question"`
			Options  []string `json:"options"`
		} `json:"questions"`
	}
	if json.Unmarshal(args, &call) != nil || len(call.Questions) == 0 || strings.TrimSpace(call.Questions[0].Question) == "" {
		return tool
	}
	q := call.Questions[0]
	s := tool + ": " + strings.TrimSpace(q.Question)
	if len(q.Options) > 0 {
		s += " (" + strings.Join(q.Options, ", ") + ")"
	}
	if n := len(call.Questions) - 1; n == 1 {
		s += "; 1 more question"
	} else if n > 1 {
		s += fmt.Sprintf("; %d more questions", n)
	}
	return s
}

// agyDialogTools present a multiple-choice question dialog to the user rather
// than a yes/no permission prompt; pressing "1" would pick an answer.
var agyDialogTools = map[string]bool{
	"ask_question": true,
}

// PermissionKey returns the key that answers agy's numbered permission dialog:
// "1" approves once, "2" approves always in the conversation, Escape cancels/denies.
func (b *AgyBackend) PermissionKey(resp types.PermissionResponse, tool string) (string, error) {
	return permissionKey(resp, tool, agyDialogTools)
}

// agyDialogYesOption and agyDialogNoOption match the numbered "1. Yes" and
// "N. No" lines of agy's permission dialog.
var (
	agyDialogYesOption = regexp.MustCompile(`(?m)^[\s>❯]*1\.\s+Yes\b`)
	agyDialogNoOption  = regexp.MustCompile(`(?m)^[\s>❯]*\d+\.\s+No\b`)
)

// PermissionPromptVisible reports whether agy's permission dialog is on
// screen: numbered "1. Yes" and "N. No" options above the "esc to cancel"
// statusline footer, and not a question dialog whose options happen to be
// Yes and No. An idle prompt shows "? for shortcuts" instead.
func (b *AgyBackend) PermissionPromptVisible(screen string) bool {
	tail := screenTail(screen)
	if len(tail) == 0 || !strings.Contains(strings.ToLower(tail[0]), "esc to cancel") {
		return false
	}
	joined := strings.Join(tail, "\n")
	return agyDialogYesOption.MatchString(joined) && agyDialogNoOption.MatchString(joined) && !agyQuestionDialog.MatchString(joined)
}

// agyQuestionDialog matches what ask_question's dialog shows: its
// "Question 1/N:" heading, or its "↑/↓ Navigate · enter Select · esc Skip"
// hint (agy 1.4.7).
var agyQuestionDialog = regexp.MustCompile(`(?m)^\s*Question \d+/\d+:|Navigate\b.*\besc Skip\b`)

// QuestionPromptVisible reports whether ask_question's dialog is on screen:
// its heading or hint above the "esc to cancel" footer.
func (b *AgyBackend) QuestionPromptVisible(screen string) bool {
	tail := screenTail(screen)
	if len(tail) == 0 || !strings.Contains(strings.ToLower(tail[0]), "esc to cancel") {
		return false
	}
	return agyQuestionDialog.MatchString(strings.Join(tail, "\n"))
}

// agyFooterBlankCols is how far a footer's text must start from the left for
// its left part to count as blank: agy right-aligns the model name there.
const agyFooterBlankCols = 16

// agyInterrupted is the line agy prints above its prompt after Esc or Ctrl+C.
const agyInterrupted = "Interrupted · What should Antigravity CLI do instead?"

// AtPrompt reports whether agy is back at its prompt: the input box ("> ")
// at the bottom of the screen, under its footer only, and the footer an
// idle one ("? for shortcuts", or nothing on the left while text is typed).
// A turn shows "esc to cancel" there instead, or "Press up to edit queued
// messages", and a dialog replaces the box. Esc and Ctrl+C fire no hook, so
// this is how gomuxai sees that the turn ended: how is "interrupted" when
// agy's line saying so is right above the box.
func (b *AgyBackend) AtPrompt(screen string) (bool, string) {
	lines := strings.Split(strings.TrimRight(screen, "\n"), "\n")
	for len(lines) > 0 && strings.TrimSpace(lines[len(lines)-1]) == "" {
		lines = lines[:len(lines)-1]
	}
	start, end, ok := inputBox(lines)
	if !ok || end != len(lines)-2 || !strings.HasPrefix(strings.TrimLeft(lines[start], " "), ">") {
		return false, ""
	}
	footer := lines[end+1]
	text := strings.TrimLeft(footer, " ")
	if !strings.HasPrefix(text, "? for shortcuts") && len(footer)-len(text) < agyFooterBlankCols {
		return false, ""
	}
	for i := start - 2; i >= 0; i-- {
		if strings.TrimSpace(lines[i]) == "" {
			continue
		}
		if strings.Contains(lines[i], agyInterrupted) {
			return true, "interrupted"
		}
		break
	}
	return true, ""
}

// InputText reads agy's input box: the lines between the last two rules of
// "─" at the bottom of the screen, the first starting with the ">" prompt.
// Text drawn dim, if any, is left out as for Claude.
func (b *AgyBackend) InputText(screen string) (string, bool) {
	return inputBoxText(screen, ">")
}

// CanDeliver: agy's PostInvocation, which fires after every model call and
// its tools, including the turn's last one. With injectSteps and
// force_continue, the model answers the messages in the same turn, and each
// shows in the pane as a prompt of its own. Stop's "continue" works too, but
// shows nothing, and Esc fires no hook at all.
func (b *AgyBackend) CanDeliver(r types.HookReport) bool {
	return r.Event == "PostInvocation"
}

// DeliveryPoint: after its current model call and tools, mid-turn or last.
func (b *AgyBackend) DeliveryPoint() string { return "its current step ends" }

// agyMaxStepBytes bounds each injected userMessage: agy cuts a longer one to
// about 192 KB, keeping the head, and tells no one.
const agyMaxStepBytes = 64 << 10

// agyPartLabelRoom is room left in each part for its label.
const agyPartLabelRoom = 128

type agyInjectStep struct {
	UserMessage string `json:"userMessage"`
}

// agyPostInvocationOutput is PostInvocation's output. agy parses it
// strictly: an unknown field or a wrong type drops all of it, silently.
type agyPostInvocationOutput struct {
	InjectSteps         []agyInjectStep `json:"injectSteps"`
	TerminationBehavior string          `json:"terminationBehavior"`
}

// DeliveryOutput returns the messages as PostInvocation's injectSteps, one
// userMessage each (a long one split into labeled parts, so agy never cuts
// it), with force_continue so the model answers them now rather than at
// the next prompt. Never force_continue without a step: the model would
// invent a reply. nil if there is nothing to deliver.
func (b *AgyBackend) DeliveryOutput(messages []string) []byte {
	var steps []agyInjectStep
	for _, m := range messages {
		for _, part := range agyMessageParts(m) {
			steps = append(steps, agyInjectStep{UserMessage: part})
		}
	}
	if len(steps) == 0 {
		return nil
	}
	out, err := json.Marshal(agyPostInvocationOutput{InjectSteps: steps, TerminationBehavior: "force_continue"})
	if err != nil {
		return nil
	}
	return out
}

// agyMessageParts splits a message longer than agyMaxStepBytes into parts
// that fit, each labeled with where it stands.
func agyMessageParts(m string) []string {
	if len(m) <= agyMaxStepBytes {
		return []string{m}
	}
	parts := splitText(m, agyMaxStepBytes-agyPartLabelRoom)
	n := len(parts)
	for i := range parts {
		if i > 0 {
			parts[i] = fmt.Sprintf("[part %d/%d of the message above]\n", i+1, n) + parts[i]
		}
		if i < n-1 {
			parts[i] += fmt.Sprintf("\n[part %d/%d: the message goes on in the next prompt]", i+1, n)
		}
	}
	return parts
}

// splitText splits s into pieces of at most max bytes, at a line break in
// a piece's second half if there is one, and never inside a UTF-8 sequence.
func splitText(s string, max int) []string {
	var parts []string
	for len(s) > max {
		cut := max
		for cut > 0 && !utf8.RuneStart(s[cut]) {
			cut--
		}
		if nl := strings.LastIndexByte(s[:cut], '\n'); nl >= cut/2 {
			cut = nl + 1
		}
		parts = append(parts, s[:cut])
		s = s[cut:]
	}
	return append(parts, s)
}

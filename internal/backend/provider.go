// Package backend provides backend definitions for different AI CLI tools.
package backend

import (
	"context"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// Re-export Status type and constants for convenience
type Status = types.Status

const (
	StatusUnknown    = types.StatusUnknown
	StatusIdle       = types.StatusIdle
	StatusProcessing = types.StatusProcessing
	StatusError      = types.StatusError
	StatusWaiting    = types.StatusWaiting
	StatusExited     = types.StatusExited
)

type HookReport = types.HookReport
type PermissionResponse = types.PermissionResponse
type PromptChange = types.PromptChange

const (
	PromptNone       = types.PromptNone
	PromptOpened     = types.PromptOpened
	PromptResolved   = types.PromptResolved
	PromptsMainEnded = types.PromptsMainEnded
	PromptsAllEnded  = types.PromptsAllEnded
)

const (
	PermissionApprove       = types.PermissionApprove
	PermissionApproveAlways = types.PermissionApproveAlways
	PermissionDeny          = types.PermissionDeny
)

// HookProvider is implemented by backends whose CLI reports its own status
// through lifecycle hooks. Hooks run reportCmd with the event payload on stdin.
type HookProvider interface {
	// LaunchCommandWithHooks returns the launch command with hooks wired to
	// reportCmd and the backend session pinned to sessionID.
	LaunchCommandWithHooks(reportCmd, sessionID string) string

	// ParseHookEvent maps a hook payload to a status report.
	// event is the hook event name when passed on the command line (for
	// backends whose stdin payload omits it); backends that embed the event
	// name in payload may prefer the payload field.
	// ok is false for events that carry no status change.
	ParseHookEvent(event string, payload []byte) (report HookReport, ok bool, err error)
}

// HookDeliverer is implemented by hook backends whose CLI takes messages
// from a hook's output and goes on with them, so messages sent while the
// agent is busy can be held and handed over from a hook (at the turn's end,
// or between model calls), rather than typed into its input box mid-turn.
type HookDeliverer interface {
	// CanDeliver reports whether the hook that produced r can hand messages
	// to the agent through its output (Claude's Stop, agy's PostInvocation).
	CanDeliver(r HookReport) bool

	// DeliveryOutput is what that hook prints to hand the agent messages
	// (oldest first, each with its "[from ...]" tag), which it takes as its
	// next step.
	DeliveryOutput(messages []string) []byte

	// DeliveryPoint says, for people, when that hook runs: "its turn ends"
	// (Claude's Stop), "its current step ends" (agy's PostInvocation).
	DeliveryPoint() string
}

// HookAllower is implemented by hook backends whose hook can approve a
// shell command before it runs (HookReport.Command), skipping the
// permission prompt.
type HookAllower interface {
	// AllowOutput is what that hook prints to approve the command; reason
	// says why, for the person.
	AllowOutput(reason string) []byte
}

// InputReader is implemented by backends that can read the CLI's input line
// off the screen, so held messages are never typed into a person's
// half-written input.
type InputReader interface {
	// InputText returns what is typed in the input line of screen (the
	// visible pane, with its styling escapes), leaving out placeholder and
	// suggestion text. found is false if no input line is on screen.
	InputText(screen string) (text string, found bool)
}

// TurnEndReader is implemented by backends whose CLI can end a turn without
// firing any hook (agy on Esc or Ctrl+C), so the screen has to tell that it
// is back at its prompt.
type TurnEndReader interface {
	// AtPrompt reports whether screen (the visible pane, ANSI-stripped)
	// shows the CLI at its prompt with no turn running. how describes the
	// turn's end when the screen shows it, e.g. "interrupted".
	AtPrompt(screen string) (atPrompt bool, how string)
}

// QuestionPrompter is implemented by backends whose CLI asks the user a
// question in a dialog with no hook of its own (agy's ask_question), after
// the PreToolUse of the tool that asks it.
type QuestionPrompter interface {
	// QuestionPromptVisible reports whether a question dialog is on screen
	// (the visible pane, ANSI-stripped), where Enter would pick an answer.
	QuestionPromptVisible(screen string) bool
}

// InstructionsProvider is implemented by backends that can give an agent
// standing instructions at launch without writing into its project.
type InstructionsProvider interface {
	// WithInstructions returns launchCmd changed to load the instructions in
	// the file at path (absolute).
	WithInstructions(launchCmd, path string) string
}

// CommandAllower is implemented by backends that can pre-approve shell
// commands for one agent at launch, without changing user or project settings.
type CommandAllower interface {
	// AllowCommands returns launchCmd changed so shell commands starting with
	// any of prefixes run without a permission prompt.
	AllowCommands(launchCmd string, prefixes []string) string
}

// TrustPrompter is implemented by backends whose CLI asks whether to trust a
// new workspace before it starts.
type TrustPrompter interface {
	// TrustKeys returns the keys that answer "trust" to the prompt on screen
	// (visible pane, ANSI-stripped), or nil if no trust prompt is showing.
	TrustKeys(screen string) []string
}

// PromptFlagProvider is implemented by backends whose CLI can accept the
// initial task prompt via a launch flag (e.g. agy -i <prompt>).
type PromptFlagProvider interface {
	AppendPromptFlag(launchCmd, prompt string) string
}

// NativeStatusProvider is implemented by backends whose CLI can report the
// status of a running session directly (no terminal scraping).
type NativeStatusProvider interface {
	// NativeStatus returns the session's status. found is false when the CLI
	// doesn't know the session (not started yet, exited, or unsupported).
	NativeStatus(ctx context.Context, sessionID string) (status Status, found bool, err error)
}

// PermissionResponder is implemented by backends that can answer a pending
// permission prompt with a keypress.
type PermissionResponder interface {
	// PermissionKey returns the key that answers the prompt for tool. It
	// errors for tools whose dialogs aren't yes/no permission prompts (e.g. a
	// question to the user), where a "yes" key would pick an answer instead.
	// An empty tool means "treat it as a plain permission prompt".
	PermissionKey(resp PermissionResponse, tool string) (string, error)

	// PermissionPromptVisible reports whether a permission prompt is showing
	// on screen (the visible pane, ANSI-stripped). Checked right before the
	// keypress, since reported status can lag a prompt that was just answered.
	PermissionPromptVisible(screen string) bool
}

// TranscriptProvider is implemented by backends whose CLI keeps a transcript
// of each session, at the path its hooks report (HookReport.Transcript).
type TranscriptProvider interface {
	// TranscriptReader returns the reader for the CLI's transcript format.
	TranscriptReader() transcript.Reader
}

// Provider defines the interface for the built-in AI CLI tool backends.
// What a backend can do beyond launching (hooks, native status, permission
// prompts, ...) it declares by implementing the optional interfaces above.
type Provider interface {
	Name() string
	LaunchCommand() string
	InitMessage(taskFile string) string

	// ExitCommand is typed into an idle agent before cleanup kills its
	// session, so the CLI can exit cleanly ("" for none).
	ExitCommand() string
}

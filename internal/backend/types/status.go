// Package types provides shared types for the backend package.
package types

// Status represents the current state of an AI backend.
type Status int

const (
	StatusUnknown Status = iota
	StatusIdle
	StatusProcessing
	StatusError
	StatusWaiting // blocked on a human, e.g. a permission prompt
	StatusEnded   // no CLI runs the session any more (EndReason says how it ended)
)

// String returns a human-readable representation of the status.
func (s Status) String() string {
	switch s {
	case StatusIdle:
		return "idle"
	case StatusProcessing:
		return "processing"
	case StatusError:
		return "error"
	case StatusWaiting:
		return "waiting"
	case StatusEnded:
		return "ended"
	default:
		return "unknown"
	}
}

// Statuses lists every status with what it means, in the order a person
// reads them. Help text and docs are built from it.
var Statuses = []Meaning{
	{"idle", "at its prompt, its turn over, nothing it started still running: a message can be typed in"},
	{"processing", "a turn is running (or a turn ended leaving background work, whose completion starts the next turn)"},
	{"waiting", "stopped on something only a person can answer: a permission prompt, a question, or the CLI's workspace trust prompt"},
	{"error", "its last turn failed (an API error, a rate limit); it is at its prompt"},
	{"ended", "no CLI runs the session any more; the end reason says how it ended"},
	{"unknown", "nothing reliable says yet (a CLI still starting, or a harness with no status of its own)"},
}

// EndReason says how a session ended, beside a status of ended.
type EndReason string

const (
	// EndQuit: the CLI ended the session and said so (Claude's SessionEnd
	// hook), or the shell it ran in saw it exit (agy's exit wrapper, with
	// the exit code in the detail).
	EndQuit EndReason = "quit"
	// EndGone: its process isn't running and nothing recorded it ending: a
	// crash, kill -9, OOM, or a session from before reports were kept.
	EndGone EndReason = "gone"
	// EndReplaced: the agent went on in a newer session (a /clear, a resume,
	// a respawn, or a CLI started anew in its pane).
	EndReplaced EndReason = "replaced"
	// EndFinished: a subagent that recorded finishing its task.
	EndFinished EndReason = "finished"
	// EndParent: a subagent whose parent session ended.
	EndParent EndReason = "parent"
	// EndClosed: its agent was cleaned up, which closed its pane.
	EndClosed EndReason = "closed"
)

// EndReasons lists every end reason with what it means.
var EndReasons = []Meaning{
	{string(EndQuit), "the CLI ended the session and said so (Claude's SessionEnd hook; the detail has Claude's reason), or the shell it ran in saw it exit (agy; the detail has the exit code)"},
	{string(EndGone), "its process isn't running and nothing recorded it ending: a crash, kill -9, OOM, or a session from before reports were kept"},
	{string(EndReplaced), "the agent went on in a newer session: a /clear, a resume, a respawn, or a CLI started anew in its pane"},
	{string(EndFinished), "a subagent that recorded finishing its task"},
	{string(EndParent), "a subagent whose parent session ended"},
	{string(EndClosed), "its agent was cleaned up, which closed its pane"},
}

// Meaning is a status or end reason and what it means.
type Meaning struct {
	Name, Means string
}

// MeaningOf returns what name means in list, or "".
func MeaningOf(list []Meaning, name string) string {
	for _, m := range list {
		if m.Name == name {
			return m.Means
		}
	}
	return ""
}

// HookReport is a status update derived from a backend lifecycle hook payload.
type HookReport struct {
	Status Status
	Event  string // backend's event name, e.g. "PreToolUse"
	Detail string // human-readable context, e.g. "Bash: go test ./..."
	Tool   string // tool the event concerns, if any
	// ToolInput is the tool call's full input (JSON) when the event may
	// raise a permission prompt, so whoever answers it can see all of it.
	ToolInput string
	Result    string // final assistant message, when the event carries one
	SessionID string // backend's session id, when the event carries one
	// Transcript is the path of the session's transcript file (the CLI's
	// own JSONL record of the conversation), when the event carries one.
	Transcript string
	// EndReason says how the session ended, with a status of ended.
	EndReason EndReason
	// ClearResult marks the start of a new turn or conversation, after which
	// the previous turn's Result no longer answers anything.
	ClearResult bool
	// DeliveryOnly marks an event that is wired only to hand the agent its
	// held messages (agy's PostInvocation): it says nothing new about the
	// status, so it is recorded only when it delivers.
	DeliveryOnly bool

	// Prompt is how the event changes the queue of pending permission
	// prompts. Claude raises one PermissionRequest per call, all at once
	// for parallel calls, and shows them one at a time ("1 of 4"), so the
	// last report alone doesn't say which prompt is on screen.
	Prompt PromptChange
	// CallKey identifies the tool call (its tool and input), matching a
	// call's result to its pending prompt.
	CallKey string
	// Agent is the subagent the event came from ("" for the main agent).
	Agent string

	// Command is the shell command line a tool call is about to run, on an
	// event whose output can approve it (Claude's PreToolUse for Bash), and
	// Cwd the directory it runs in.
	Command string
	Cwd     string
}

// PromptChange is how a hook event changes the pending permission prompts.
type PromptChange int

const (
	PromptNone       PromptChange = iota
	PromptOpened                  // a call asks for permission
	PromptResolved                // a call finished: its prompt, if any, was answered
	PromptsMainEnded              // the main agent takes a prompt, so none of its own are pending
	PromptsAllEnded               // the turn or session ended: none are pending
)

// PermissionResponse is an answer to a pending permission prompt.
type PermissionResponse int

const (
	PermissionApprove PermissionResponse = iota
	PermissionApproveAlways
	PermissionDeny
)

// PaneSession is a backend session found running in an agent's pane.
type PaneSession struct {
	ID     string
	PID    int    // the CLI process holding it
	Status Status // what the CLI's own record says
	Detail string // that status in words
}

// Resumable reports whether agent resume would continue an agent: one
// gomuxai started (not adopted), on a claude or agy backend, with a
// session to continue, whose session has ended. service.Resume refuses the
// rest with the reason; export and the bus's list carry this answer, so a
// display offers resume only where it would work.
func Resumable(backend string, adopted bool, sessionID string, ended bool) bool {
	return !adopted && ResumableBackend(backend) && sessionID != "" && ended
}

// ResumableBackend reports whether agent resume continues backend's
// sessions: claude and agy name their sessions, so the CLI can be told
// which to continue.
func ResumableBackend(backend string) bool {
	return backend == "claude" || backend == "agy"
}

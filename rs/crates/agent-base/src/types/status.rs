use std::borrow::Cow;
use std::fmt;

/// The current state of an AI backend.
///
/// The discriminants are Go's iota values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(i32)]
pub enum Status {
    #[default]
    Unknown = 0,
    Idle = 1,
    Processing = 2,
    Error = 3,
    /// Blocked on a human, e.g. a permission prompt.
    Waiting = 4,
    /// No CLI runs the session any more (EndReason says how it ended).
    Ended = 5,
}

impl Status {
    /// The human-readable name of the status (Go's `String`).
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Processing => "processing",
            Status::Error => "error",
            Status::Waiting => "waiting",
            Status::Ended => "ended",
            Status::Unknown => "unknown",
        }
    }

    /// The status a name means; anything else is Unknown, as Go's String
    /// prints every other value as "unknown".
    pub fn from_name(name: &str) -> Status {
        match name {
            "idle" => Status::Idle,
            "processing" => Status::Processing,
            "error" => Status::Error,
            "waiting" => Status::Waiting,
            "ended" => Status::Ended,
            _ => Status::Unknown,
        }
    }

    /// The status for Go's integer value; out of range is Unknown.
    pub fn from_i32(n: i32) -> Status {
        match n {
            1 => Status::Idle,
            2 => Status::Processing,
            3 => Status::Error,
            4 => Status::Waiting,
            5 => Status::Ended,
            _ => Status::Unknown,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A status or end reason and what it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meaning {
    pub name: &'static str,
    pub means: &'static str,
}

/// Every status with what it means, in the order a person reads them. Help
/// text and docs are built from it.
pub static STATUSES: &[Meaning] = &[
    Meaning {
        name: "idle",
        means: "at its prompt, its turn over, nothing it started still running: a message can be typed in",
    },
    Meaning {
        name: "processing",
        means: "a turn is running (or a turn ended leaving background work, whose completion starts the next turn)",
    },
    Meaning {
        name: "waiting",
        means: "stopped on something only a person can answer: a permission prompt, a question, the CLI's workspace trust prompt, or another dialog of its own",
    },
    Meaning {
        name: "error",
        means: "its last turn failed (an API error, a rate limit); it is at its prompt",
    },
    Meaning {
        name: "ended",
        means: "no CLI runs the session any more; the end reason says how it ended",
    },
    Meaning {
        name: "unknown",
        means: "nothing reliable says yet (a CLI still starting, or a harness with no status of its own)",
    },
];

/// How a session ended, beside a status of ended. Like Go's `type EndReason
/// string`, it holds any text (a stored or remote value may be one this
/// build doesn't know); the known ones are the `END_*` constants, and the
/// default is "" (none).
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct EndReason(pub Cow<'static, str>);

impl EndReason {
    pub const fn from_static(s: &'static str) -> Self {
        EndReason(Cow::Borrowed(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for EndReason {
    fn from(s: String) -> Self {
        EndReason(Cow::Owned(s))
    }
}

impl From<&str> for EndReason {
    fn from(s: &str) -> Self {
        EndReason(Cow::Owned(s.to_string()))
    }
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for EndReason {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for EndReason {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// The CLI ended the session and said so (Claude's SessionEnd hook), or the
/// shell it ran in saw it exit (agy's exit wrapper, with the exit code in the
/// detail).
pub const END_QUIT: EndReason = EndReason::from_static("quit");
/// Its process isn't running and nothing recorded it ending: a crash, kill
/// -9, OOM, or a session from before reports were kept.
pub const END_GONE: EndReason = EndReason::from_static("gone");
/// The agent went on in a newer session (a /clear, a resume, a respawn, or a
/// CLI started anew in its pane).
pub const END_REPLACED: EndReason = EndReason::from_static("replaced");
/// A subagent that recorded finishing its task.
pub const END_FINISHED: EndReason = EndReason::from_static("finished");
/// A subagent whose parent session ended.
pub const END_PARENT: EndReason = EndReason::from_static("parent");
/// Its agent was cleaned up, which closed its pane.
pub const END_CLOSED: EndReason = EndReason::from_static("closed");

/// Every end reason with what it means.
pub static END_REASONS: &[Meaning] = &[
    Meaning {
        name: "quit",
        means: "the CLI ended the session and said so (Claude's SessionEnd hook; the detail has Claude's reason), or the shell it ran in saw it exit (agy; the detail has the exit code)",
    },
    Meaning {
        name: "gone",
        means: "its process isn't running and nothing recorded it ending: a crash, kill -9, OOM, or a session from before reports were kept",
    },
    Meaning {
        name: "replaced",
        means: "the agent went on in a newer session: a /clear, a resume, a respawn, or a CLI started anew in its pane",
    },
    Meaning {
        name: "finished",
        means: "a subagent that recorded finishing its task",
    },
    Meaning {
        name: "parent",
        means: "a subagent whose parent session ended",
    },
    Meaning {
        name: "closed",
        means: "its agent was cleaned up, which closed its pane",
    },
];

/// What `name` means in `list`, or "".
pub fn meaning_of(list: &[Meaning], name: &str) -> &'static str {
    list.iter()
        .find(|m| m.name == name)
        .map(|m| m.means)
        .unwrap_or("")
}

/// A status update derived from a backend lifecycle hook payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookReport {
    pub status: Status,
    /// The backend's event name, e.g. "PreToolUse".
    pub event: String,
    /// Human-readable context, e.g. "Bash: go test ./...".
    pub detail: String,
    /// The tool the event concerns, if any.
    pub tool: String,
    /// The tool call's full input (JSON) when the event may raise a
    /// permission prompt, so whoever answers it can see all of it.
    pub tool_input: String,
    /// The final assistant message, when the event carries one.
    pub result: String,
    /// The backend's session id, when the event carries one.
    pub session_id: String,
    /// The path of the session's transcript file (the CLI's own JSONL record
    /// of the conversation), when the event carries one.
    pub transcript: String,
    /// How the session ended, with a status of ended.
    pub end_reason: EndReason,
    /// Marks the start of a new turn or conversation, after which the
    /// previous turn's Result no longer answers anything.
    pub clear_result: bool,
    /// Marks an event that is wired only to hand the agent its held messages
    /// (agy's PostInvocation): it says nothing new about the status, so it is
    /// recorded only when it delivers.
    pub delivery_only: bool,

    /// How the event changes the queue of pending permission prompts. Claude
    /// raises one PermissionRequest per call, all at once for parallel calls,
    /// and shows them one at a time ("1 of 4"), so the last report alone
    /// doesn't say which prompt is on screen.
    pub prompt: PromptChange,
    /// Identifies the tool call (its tool and input), matching a call's
    /// result to its pending prompt.
    pub call_key: String,
    /// The subagent the event came from ("" for the main agent).
    pub agent: String,

    /// The prompt a UserPromptSubmit-like event submitted, as typed.
    pub prompt_text: String,

    /// The shell command line a tool call is about to run, on an event whose
    /// output can approve it (Claude's PreToolUse for Bash), and `cwd` the
    /// directory it runs in.
    pub command: String,
    pub cwd: String,
}

/// How a hook event changes the pending permission prompts.
///
/// The discriminants are Go's iota values (PromptNone, PromptOpened, ...).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PromptChange {
    #[default]
    None = 0,
    /// A call asks for permission.
    Opened = 1,
    /// A call finished: its prompt, if any, was answered.
    Resolved = 2,
    /// The main agent takes a prompt, so none of its own are pending.
    MainEnded = 3,
    /// The turn or session ended: none are pending.
    AllEnded = 4,
}

/// An answer to a pending permission prompt.
///
/// The discriminants are Go's iota values (PermissionApprove, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PermissionResponse {
    Approve = 0,
    ApproveAlways = 1,
    Deny = 2,
}

/// A dialog a CLI shows that no hook reports, as read off its screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dialog {
    /// Its first line, e.g. "Teach auto mode about your environment?".
    pub title: String,
    /// The option Enter picks, e.g. "1. Yes"; "" if none shows.
    pub highlighted: String,
}

/// A backend session found running in an agent's pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneSession {
    pub id: String,
    /// The CLI process holding it.
    pub pid: i32,
    /// What the CLI's own record says.
    pub status: Status,
    /// That status in words.
    pub detail: String,
}

/// Whether agent resume would continue an agent: one gomuxai started (not
/// adopted), on a claude or agy backend, with a session to continue, whose
/// session has ended. service.Resume refuses the rest with the reason;
/// export and the bus's list carry this answer, so a display offers resume
/// only where it would work.
pub fn resumable(backend: &str, adopted: bool, session_id: &str, ended: bool) -> bool {
    !adopted && resumable_backend(backend) && !session_id.is_empty() && ended
}

/// Whether agent resume continues backend's sessions: claude and agy name
/// their sessions, so the CLI can be told which to continue.
pub fn resumable_backend(backend: &str) -> bool {
    backend == "claude" || backend == "agy"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_names() {
        for (s, name) in [
            (Status::Unknown, "unknown"),
            (Status::Idle, "idle"),
            (Status::Processing, "processing"),
            (Status::Error, "error"),
            (Status::Waiting, "waiting"),
            (Status::Ended, "ended"),
        ] {
            assert_eq!(s.to_string(), name);
            assert_eq!(Status::from_name(name), s);
            assert_eq!(Status::from_i32(s as i32), s);
        }
        assert_eq!(Status::from_i32(99), Status::Unknown);
        // Every status has a meaning.
        for m in STATUSES {
            assert_eq!(Status::from_name(m.name).as_str(), m.name);
        }
    }

    #[test]
    fn end_reasons() {
        for r in [
            END_QUIT,
            END_GONE,
            END_REPLACED,
            END_FINISHED,
            END_PARENT,
            END_CLOSED,
        ] {
            assert!(!meaning_of(END_REASONS, r.as_str()).is_empty(), "{r}");
        }
        assert_eq!(meaning_of(END_REASONS, "bogus"), "");
        assert_eq!(EndReason::from("quit".to_string()), END_QUIT);
        assert_eq!(END_GONE, "gone");
        assert!(EndReason::default().is_empty());
        assert_eq!(serde_json::to_string(&END_CLOSED).unwrap(), r#""closed""#);
    }

    #[test]
    fn resumable_cases() {
        assert!(resumable("claude", false, "s1", true));
        assert!(resumable("agy", false, "s1", true));
        assert!(!resumable("aider", false, "s1", true));
        assert!(!resumable("claude", true, "s1", true));
        assert!(!resumable("claude", false, "", true));
        assert!(!resumable("claude", false, "s1", false));
    }
}

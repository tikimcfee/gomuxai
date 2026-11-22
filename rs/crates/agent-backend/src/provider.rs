//! Go `internal/backend/provider.go`: the [`Provider`] trait and the
//! optional capabilities a backend declares.
//!
//! Go finds a capability by type assertion (`if h, ok := p.(HookProvider)`).
//! Here each optional interface is a trait, and [`Provider`] has one accessor
//! per trait, `None` by default, that a backend implementing it overrides:
//! `if let Some(h) = p.as_hook_provider() { ... }`.

pub use agent_base::types::{
    Dialog, END_CLOSED, END_FINISHED, END_GONE, END_PARENT, END_QUIT, END_REPLACED, EndReason,
    HookReport, PaneSession, PermissionResponse, PromptChange, Status,
};

use agent_transcript as transcript;

/// Implemented by backends whose CLI reports its own status through
/// lifecycle hooks, with the event payload on stdin. How the hooks are
/// registered is [`SpawnHooker`]'s or [`GlobalHooker`]'s.
pub trait HookProvider {
    /// Maps a hook payload to a status report. `event` is the hook event
    /// name when passed on the command line (for backends whose stdin
    /// payload omits it); backends that embed the event name in the payload
    /// may prefer the payload field. `Ok(None)` for events that carry no
    /// status change.
    fn parse_hook_event(&self, event: &str, payload: &[u8]) -> anyhow::Result<Option<HookReport>>;
}

/// Implemented by hook backends whose hooks are wired into each spawn's
/// launch command (agy): `report_cmd` runs on every event.
pub trait SpawnHooker {
    /// Returns the launch command with hooks wired to `report_cmd` and the
    /// backend session pinned to `session_id`.
    fn launch_command_with_hooks(&self, report_cmd: &str, session_id: &str) -> String;
}

/// Implemented by hook backends whose hooks are registered once for every
/// session of the CLI (Claude: agent hooks install), and find their
/// session's agent from the running processes (agent hook).
pub trait GlobalHooker {
    /// Returns `Ok` when the CLI's settings run `binary`'s hook on every
    /// event, else an error saying what is missing and how to fix it.
    fn hooks_installed(&self, binary: &str) -> anyhow::Result<()>;
    /// Returns the launch command with the backend session pinned to
    /// `session_id` (a resumed one keeps its own).
    fn launch_command_with_session(&self, session_id: &str) -> String;
}

/// Implemented by hook backends whose CLI takes messages from a hook's
/// output and goes on with them, so messages sent while the agent is busy
/// can be held and handed over from a hook (at the turn's end, or between
/// model calls), rather than typed into its input box mid-turn.
pub trait HookDeliverer {
    /// Reports whether the hook that produced `r` can hand messages to the
    /// agent through its output (Claude's Stop, agy's PostInvocation).
    fn can_deliver(&self, r: &HookReport) -> bool;

    /// What that hook prints to hand the agent messages (oldest first, each
    /// with its "[from ...]" tag), which it takes as its next step. Empty
    /// where Go returns nil.
    fn delivery_output(&self, messages: &[String]) -> Vec<u8>;

    /// Says, for people, when that hook runs: "its turn ends" (Claude's
    /// Stop), "its current step ends" (agy's PostInvocation).
    fn delivery_point(&self) -> String;
}

/// Implemented by hook backends whose CLI takes typed text that is long or
/// spans several lines as a paste, and shows it to the model as pasted
/// material rather than as the prompt itself, while a hook that runs when a
/// prompt is submitted can hand text to the model with it. A message the
/// CLI would take as a paste is then typed as one short line, and that hook
/// hands over its whole text (see service/handover.go).
pub trait PromptHandover {
    /// Reports whether the CLI takes `text`, typed in at once, as a paste.
    fn typed_as_paste(&self, text: &str) -> bool;

    /// Returns the prompt `r`'s event submitted, when `r` is the hook that
    /// can hand over text with it (Claude's UserPromptSubmit).
    fn submitted_prompt(&self, r: &HookReport) -> Option<String>;

    /// What that hook prints to hand `text` to the model with the prompt.
    fn handover_output(&self, text: &str) -> Vec<u8>;
}

/// Implemented by hook backends whose hook can approve a shell command
/// before it runs (`HookReport::command`), skipping the permission prompt.
pub trait HookAllower {
    /// What that hook prints to approve the command; `reason` says why, for
    /// the person.
    fn allow_output(&self, reason: &str) -> Vec<u8>;
}

/// Implemented by backends that can read the CLI's input line off the
/// screen, so held messages are never typed into a person's half-written
/// input.
pub trait InputReader {
    /// Returns what is typed in the input line of `screen` (the visible
    /// pane, with its styling escapes), leaving out placeholder and
    /// suggestion text. `None` if no input line is on screen.
    fn input_text(&self, screen: &str) -> Option<String>;
}

/// Implemented by backends whose CLI can take the Enter that follows a typed
/// message for a newline: when both reach a busy CLI in one read, Claude
/// reads them as a paste, and the message sits in its input box. Typing a
/// message into one waits for its input box to show the text before
/// pressing Enter, and checks that the box emptied after.
pub trait SubmitChecker: InputReader {
    /// A marker: the CLI behaves as described above.
    fn enter_can_be_a_newline(&self);
}

/// Implemented by backends whose CLI can end a turn without firing any hook
/// (agy on Esc or Ctrl+C), so the screen has to tell that it is back at its
/// prompt.
pub trait TurnEndReader {
    /// Reports whether `screen` (the visible pane, ANSI-stripped) shows the
    /// CLI at its prompt with no turn running: `Some(how)`, where `how`
    /// describes the turn's end when the screen shows it, e.g.
    /// "interrupted" (else ""). `None` when it isn't at its prompt.
    fn at_prompt(&self, screen: &str) -> Option<String>;
}

/// Implemented by backends whose CLI asks the user a question in a dialog
/// with no hook of its own (agy's ask_question), after the PreToolUse of the
/// tool that asks it.
pub trait QuestionPrompter {
    /// Reports whether a question dialog is on screen (the visible pane,
    /// ANSI-stripped), where Enter would pick an answer.
    fn question_prompt_visible(&self, screen: &str) -> bool;
}

/// Implemented by backends that can give an agent standing instructions at
/// launch without writing into its project.
pub trait InstructionsProvider {
    /// Returns `launch_cmd` changed to load the instructions in the file at
    /// `path` (absolute), or why it can't: an agent is never started without
    /// its briefing.
    fn with_instructions(&self, launch_cmd: &str, path: &str) -> anyhow::Result<String>;
}

/// Implemented by backends that can pre-approve shell commands for one agent
/// at launch, without changing user or project settings.
pub trait CommandAllower {
    /// Returns `launch_cmd` changed so shell commands starting with any of
    /// `prefixes` run without a permission prompt.
    fn allow_commands(&self, launch_cmd: &str, prefixes: &[String]) -> String;

    /// allow_commands, plus the backend's own tool rules: `allow` run without
    /// a prompt (e.g. "Read(//srv/app/**)") and `deny` are refused (e.g.
    /// "Edit"). A backend that has no such rules ignores them.
    fn allow_tools(
        &self,
        launch_cmd: &str,
        prefixes: &[String],
        _allow: &[String],
        _deny: &[String],
    ) -> String {
        self.allow_commands(launch_cmd, prefixes)
    }
}

/// Implemented by backends whose CLI asks whether to trust a new workspace
/// before it starts. Each method reads `screen`, the visible pane,
/// ANSI-stripped. The CLI runs no hooks until the folder is trusted, so only
/// the screen shows the prompt.
pub trait TrustPrompter {
    /// Reports whether the trust prompt is on screen: `Some(folder)`, the
    /// folder it asks about ("" if the screen doesn't show it).
    fn trust_prompt(&self, screen: &str) -> Option<String>;
    /// Returns the keys that answer "trust" to the prompt on screen, or
    /// `None` if no trust prompt is showing or its highlighted option can't
    /// be told.
    fn trust_keys(&self, screen: &str) -> Option<Vec<String>>;
    /// Returns the keys that answer "no" to it (the CLI then exits), or
    /// `None` likewise.
    fn decline_trust_keys(&self, screen: &str) -> Option<Vec<String>>;
}

/// Implemented by backends whose CLI takes the model to run on at launch
/// (spawn --model).
pub trait ModelChooser {
    /// Returns `launch_cmd` changed to run on `model`.
    fn with_model(&self, launch_cmd: &str, model: &str) -> String;
}

/// Implemented by backends whose CLI takes a reasoning effort at launch
/// (spawn --effort: claude's and agy's --effort low|medium|high|xhigh|max).
pub trait EffortChooser {
    /// Returns `launch_cmd` changed to run at `effort`.
    fn with_effort(&self, launch_cmd: &str, effort: &str) -> String;
}

/// Implemented by backends whose CLI takes a display name for the session at
/// launch (Claude's --name).
pub trait SessionNamer {
    /// Returns `launch_cmd` changed to name the session `name`.
    fn with_session_name(&self, launch_cmd: &str, name: &str) -> String;
}

/// Implemented by backends whose CLI can show a dialog that no hook reports
/// and its session status doesn't tell from idle: Claude's offers, notices
/// and setup wizards, which come up at startup or after a turn. While one is
/// up, typed keys go to it, and Enter picks its highlighted option.
pub trait DialogReader {
    /// Reports whether such a dialog is on screen (the visible pane,
    /// ANSI-stripped). Permission, question and trust prompts count too:
    /// callers check for those first.
    fn dialog(&self, screen: &str) -> Option<Dialog>;
    /// Returns the key that answers it: approve picks the highlighted
    /// option, deny closes it.
    fn dialog_key(&self, resp: PermissionResponse) -> anyhow::Result<String>;
}

/// Implemented by backends whose CLI can accept the initial task prompt via
/// a launch flag (e.g. agy -i <prompt>).
pub trait PromptFlagProvider {
    fn append_prompt_flag(&self, launch_cmd: &str, prompt: &str) -> String;
}

/// Implemented by backends whose CLI can report the status of a running
/// session directly (no terminal scraping).
pub trait NativeStatusProvider {
    /// Returns the session's status, whether the CLI knows the session, and
    /// an error. With found true, an error says why the status is unknown (a
    /// value the backend doesn't know). found is false when the CLI doesn't
    /// know the session (not started yet, exited, or unsupported); an error
    /// then says why that can't be told.
    fn native_status(&self, session_id: &str) -> (Status, bool, Option<anyhow::Error>);
}

/// Implemented by backends that can say which of their sessions runs under
/// a process: the CLI in an agent's pane, including one a person started
/// there by hand, which carries no hooks and so never reports.
pub trait PaneSessionProvider {
    /// Returns the session whose CLI runs under `pane_pid` (the pane's own
    /// process) in the process tree, the one nearest it. `Ok(None)` when no
    /// CLI of this backend runs there.
    fn pane_session(&self, pane_pid: i32) -> anyhow::Result<Option<PaneSession>>;
}

/// Implemented by backends that can answer a pending permission prompt with
/// a keypress.
pub trait PermissionResponder {
    /// Returns the key that answers the prompt for `tool`. It errors for
    /// tools whose dialogs aren't yes/no permission prompts (e.g. a question
    /// to the user), where a "yes" key would pick an answer instead. An
    /// empty tool means "treat it as a plain permission prompt".
    fn permission_key(&self, resp: PermissionResponse, tool: &str) -> anyhow::Result<String>;

    /// Reports whether a permission prompt is showing on screen (the visible
    /// pane, ANSI-stripped). Checked right before the keypress, since
    /// reported status can lag a prompt that was just answered.
    fn permission_prompt_visible(&self, screen: &str) -> bool;
}

/// Implemented by backends whose CLI keeps a transcript of each session, at
/// the path its hooks report (`HookReport::transcript`).
pub trait TranscriptProvider {
    /// Returns the reader for the CLI's transcript format.
    fn transcript_reader(&self) -> Box<dyn transcript::Reader>;
}

/// The built-in AI CLI tool backends. What a backend can do beyond
/// launching (hooks, native status, permission prompts, ...) it declares by
/// implementing the optional traits above, and overriding the accessor for
/// each.
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn launch_command(&self) -> String;
    fn init_message(&self, task_file: &str) -> String;

    /// Typed into an idle agent before cleanup kills its session, so the
    /// CLI can exit cleanly ("" for none).
    fn exit_command(&self) -> String;

    fn as_hook_provider(&self) -> Option<&dyn HookProvider> {
        None
    }
    fn as_spawn_hooker(&self) -> Option<&dyn SpawnHooker> {
        None
    }
    fn as_global_hooker(&self) -> Option<&dyn GlobalHooker> {
        None
    }
    fn as_hook_deliverer(&self) -> Option<&dyn HookDeliverer> {
        None
    }
    fn as_prompt_handover(&self) -> Option<&dyn PromptHandover> {
        None
    }
    fn as_hook_allower(&self) -> Option<&dyn HookAllower> {
        None
    }
    fn as_input_reader(&self) -> Option<&dyn InputReader> {
        None
    }
    fn as_submit_checker(&self) -> Option<&dyn SubmitChecker> {
        None
    }
    fn as_turn_end_reader(&self) -> Option<&dyn TurnEndReader> {
        None
    }
    fn as_question_prompter(&self) -> Option<&dyn QuestionPrompter> {
        None
    }
    fn as_instructions_provider(&self) -> Option<&dyn InstructionsProvider> {
        None
    }
    fn as_command_allower(&self) -> Option<&dyn CommandAllower> {
        None
    }
    fn as_trust_prompter(&self) -> Option<&dyn TrustPrompter> {
        None
    }
    fn as_model_chooser(&self) -> Option<&dyn ModelChooser> {
        None
    }
    fn as_effort_chooser(&self) -> Option<&dyn EffortChooser> {
        None
    }
    fn as_session_namer(&self) -> Option<&dyn SessionNamer> {
        None
    }
    fn as_dialog_reader(&self) -> Option<&dyn DialogReader> {
        None
    }
    fn as_prompt_flag_provider(&self) -> Option<&dyn PromptFlagProvider> {
        None
    }
    fn as_native_status_provider(&self) -> Option<&dyn NativeStatusProvider> {
        None
    }
    fn as_pane_session_provider(&self) -> Option<&dyn PaneSessionProvider> {
        None
    }
    fn as_permission_responder(&self) -> Option<&dyn PermissionResponder> {
        None
    }
    fn as_transcript_provider(&self) -> Option<&dyn TranscriptProvider> {
        None
    }
}

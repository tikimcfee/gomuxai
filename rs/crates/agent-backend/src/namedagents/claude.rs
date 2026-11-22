//! Go `namedagents/claude.go`.

use agent_base::util::shell_quote;

use crate::backend::Backend;
use crate::provider::*;

/// Holds configuration options for the Claude backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeConfig {
    /// Passes --dangerously-skip-permissions to claude CLI.
    pub skip_permissions: bool,
    /// The session id claude continues (--resume), or "".
    pub resume: String,
    /// --permission-mode, or "" for the person's default.
    pub permission_mode: String,
}

/// Implements [`Provider`] for Claude Code CLI.
#[derive(Debug, Clone, Default)]
pub struct ClaudeBackend {
    pub(crate) config: ClaudeConfig,
}

/// Creates a new Claude backend with default config.
pub fn new_claude_backend() -> ClaudeBackend {
    ClaudeBackend::default()
}

/// Creates a Claude backend with the given config.
pub fn new_claude_backend_with_config(cfg: ClaudeConfig) -> ClaudeBackend {
    ClaudeBackend { config: cfg }
}

/// The settings every Claude agent is launched with (--settings, layered
/// over the person's own for that session only). They turn off
/// /auto-mode-setup, whose offer ("Teach auto mode about your
/// environment?") comes up when a turn ends in auto mode, fires no hook, and
/// takes the keys of the next message typed in: its Enter starts the setup
/// wizard, which reads the person's shell history and other repos.
pub const CLAUDE_SPAWN_SETTINGS: &str = r#"{"skillOverrides":{"auto-mode-setup":"off"}}"#;

/// The launch flags after which Claude Code's /restart refuses to restart
/// the session in place ("it has restrictions a restart can't carry over
/// (launch flags: a custom system prompt, a tool allowlist, or restricted
/// settings)"), so an agent on an old version could only be ended and
/// resumed by hand. No launch command gomuxai builds may use one:
/// --append-system-prompt, --settings, --model, --allowedTools rules and
/// --resume all ride the restart.
pub const CLAUDE_RESTART_BLOCKING_FLAGS: &[&str] = &[
    "--system-prompt",
    "--system-prompt-file",
    "--append-system-prompt-file",
    "--permission-prompt-tool",
    "--setting-sources",
    "--tools",
];

impl ClaudeBackend {
    pub fn name(&self) -> &str {
        "claude"
    }

    pub fn launch_command(&self) -> String {
        let mut cmd = String::from("claude");
        if self.config.skip_permissions {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        if !self.config.permission_mode.is_empty() {
            cmd.push_str(" --permission-mode ");
            cmd.push_str(&shell_quote(&self.config.permission_mode));
        }
        cmd.push_str(" --settings ");
        cmd.push_str(&shell_quote(CLAUDE_SPAWN_SETTINGS));
        if !self.config.resume.is_empty() {
            cmd.push_str(" --resume ");
            cmd.push_str(&shell_quote(&self.config.resume));
        }
        cmd
    }

    pub fn init_message(&self, task_file: &str) -> String {
        if !task_file.is_empty() {
            return format!("Please read and execute the task in {task_file}");
        }
        String::new()
    }

    pub fn exit_command(&self) -> String {
        "/exit".to_string()
    }

    // Backend: status scraping, used only for sessions without hooks.

    pub fn idle_pattern(&self) -> &str {
        r"(?m)^>\s*$"
    }

    pub fn processing_pattern(&self) -> &str {
        "(⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Thinking)"
    }
}

impl ModelChooser for ClaudeBackend {
    /// Runs claude on `model` (--model).
    fn with_model(&self, launch_cmd: &str, model: &str) -> String {
        format!("{launch_cmd} --model {}", shell_quote(model))
    }
}

impl EffortChooser for ClaudeBackend {
    /// Runs claude at `effort` (--effort).
    fn with_effort(&self, launch_cmd: &str, effort: &str) -> String {
        format!("{launch_cmd} --effort {}", shell_quote(effort))
    }
}

impl SessionNamer for ClaudeBackend {
    /// Names the session `name` (--name).
    fn with_session_name(&self, launch_cmd: &str, name: &str) -> String {
        if name.is_empty() {
            return launch_cmd.to_string();
        }
        format!("{launch_cmd} --name {}", shell_quote(name))
    }
}

impl Backend for ClaudeBackend {
    fn name(&self) -> &str {
        ClaudeBackend::name(self)
    }
    fn launch_command(&self) -> String {
        ClaudeBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        ClaudeBackend::init_message(self, task_file)
    }
    fn idle_pattern(&self) -> &str {
        ClaudeBackend::idle_pattern(self)
    }
    fn processing_pattern(&self) -> &str {
        ClaudeBackend::processing_pattern(self)
    }
}

impl Provider for ClaudeBackend {
    fn name(&self) -> &str {
        ClaudeBackend::name(self)
    }
    fn launch_command(&self) -> String {
        ClaudeBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        ClaudeBackend::init_message(self, task_file)
    }
    fn exit_command(&self) -> String {
        ClaudeBackend::exit_command(self)
    }

    fn as_hook_provider(&self) -> Option<&dyn HookProvider> {
        Some(self)
    }
    fn as_global_hooker(&self) -> Option<&dyn GlobalHooker> {
        Some(self)
    }
    fn as_hook_deliverer(&self) -> Option<&dyn HookDeliverer> {
        Some(self)
    }
    fn as_prompt_handover(&self) -> Option<&dyn PromptHandover> {
        Some(self)
    }
    fn as_hook_allower(&self) -> Option<&dyn HookAllower> {
        Some(self)
    }
    fn as_input_reader(&self) -> Option<&dyn InputReader> {
        Some(self)
    }
    fn as_submit_checker(&self) -> Option<&dyn SubmitChecker> {
        Some(self)
    }
    fn as_instructions_provider(&self) -> Option<&dyn InstructionsProvider> {
        Some(self)
    }
    fn as_command_allower(&self) -> Option<&dyn CommandAllower> {
        Some(self)
    }
    fn as_trust_prompter(&self) -> Option<&dyn TrustPrompter> {
        Some(self)
    }
    fn as_model_chooser(&self) -> Option<&dyn ModelChooser> {
        Some(self)
    }
    fn as_effort_chooser(&self) -> Option<&dyn EffortChooser> {
        Some(self)
    }
    fn as_session_namer(&self) -> Option<&dyn SessionNamer> {
        Some(self)
    }
    fn as_dialog_reader(&self) -> Option<&dyn DialogReader> {
        Some(self)
    }
    fn as_native_status_provider(&self) -> Option<&dyn NativeStatusProvider> {
        Some(self)
    }
    fn as_pane_session_provider(&self) -> Option<&dyn PaneSessionProvider> {
        Some(self)
    }
    fn as_permission_responder(&self) -> Option<&dyn PermissionResponder> {
        Some(self)
    }
    fn as_transcript_provider(&self) -> Option<&dyn TranscriptProvider> {
        Some(self)
    }
}

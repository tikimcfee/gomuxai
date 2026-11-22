//! Go `namedagents/agy.go`.

use std::sync::{LazyLock, Mutex};

use agent_base::gostd::{filepath, os};
use agent_base::util::{shell_quote, strip_ansi};
use anyhow::Context;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use super::hooks::{
    HookCommand, HookMatcher, first_non_empty, input_box, input_box_text, is_rule, marshal,
    new_hook_command, permission_key, screen_tail, tool_input, tool_summary, truncate,
};
use super::trust::TrustDialog;
use crate::backend::Backend;
use crate::gojson::{marshal_indent, raw_value, unmarshal_struct};
use crate::provider::*;

/// Holds configuration options for the Antigravity CLI (agy) backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgyConfig {
    /// Passes --dangerously-skip-permissions to agy CLI.
    pub skip_permissions: bool,

    /// The per-agent directory (e.g. .gomuxai/agents/<name>) where the
    /// generated .agents/hooks.json bundle is written for --add-dir.
    pub agent_dir: String,

    /// The absolute directory agy runs in: the workspace it works in.
    pub cwd: String,

    /// The conversation id agy continues (--conversation), or "".
    pub resume: String,

    /// The agent is in a confined role (a dream's): its PreToolUse hook runs
    /// with --confined, so a call it has no allow for is blocked, and it
    /// never launches without its hooks.
    pub confined: bool,
}

/// Implements [`Provider`], and most optional backend traits, for the
/// Antigravity CLI (agy).
#[derive(Debug, Default)]
pub struct AgyBackend {
    config: AgyConfig,
    /// The exit report launch_command_with_hooks attached ("; <agent>
    /// report ended ..."), or "".
    exit_suffix: Mutex<String>,
    /// The path launch_command_with_hooks passed agy for the agent's dir
    /// (workspace_dir).
    added_dir: Mutex<String>,
}

/// Creates a new Agy backend with default config.
pub fn new_agy_backend() -> AgyBackend {
    AgyBackend::default()
}

/// Creates an Agy backend with the given config.
pub fn new_agy_backend_with_config(cfg: AgyConfig) -> AgyBackend {
    AgyBackend {
        config: cfg,
        ..Default::default()
    }
}

/// Clears the variables agy uses to decide it's in a remote SSH session.
/// When they're set, agy skips the OS keyring and every launch comes up "not
/// signed in", even with a valid saved login. A terminal session created
/// from an SSH connection inherits them, so spawned agents would too.
const AGY_ENV: &str = "env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY ";

/// Session-only settings (--settings, never saved): no feedback survey, and
/// mid-turn typed messages (`send --now`, or a person typing) deliver at the
/// next step boundary instead of agy's turn-end `"queue"` default.
const AGY_SETTINGS: &str = r#"{"queuedMessages":"send-immediately","showFeedbackSurvey":false}"#;

fn lock(m: &Mutex<String>) -> String {
    m.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

impl AgyBackend {
    pub fn name(&self) -> &str {
        "agy"
    }

    pub fn launch_command(&self) -> String {
        let mut cmd = format!("{AGY_ENV}agy --settings {}", shell_quote(AGY_SETTINGS));
        if self.config.skip_permissions {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        if !self.config.resume.is_empty() {
            cmd.push_str(" --conversation ");
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

    // Backend: status scraping, used only before the first hook report.

    pub fn idle_pattern(&self) -> &str {
        r"(?m)^>\s*$"
    }

    pub fn processing_pattern(&self) -> &str {
        "(⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|Thinking)"
    }

    /// Appends `args` to `launch_cmd`, ahead of the exit report that
    /// launch_command_with_hooks attached, so they go to agy rather than the
    /// report.
    fn insert_before_exit(&self, launch_cmd: &str, args: &str) -> String {
        let suffix = lock(&self.exit_suffix);
        if !suffix.is_empty()
            && let Some(head) = launch_cmd.strip_suffix(suffix.as_str())
        {
            return format!("{head}{args}{suffix}");
        }
        format!("{launch_cmd}{args}")
    }

    /// Returns the path to pass agy (--add-dir) for `dir`, a directory of
    /// gomuxai's (the agent's: hooks bundle, briefing), so that agy still
    /// works in the directory it runs in.
    ///
    /// agy (1.5.x) sorts its workspace folders, the directory it runs in and
    /// each --add-dir, by path, and makes the first its "Command Working
    /// Directory", where commands run and which the model takes for the
    /// project. The agent's dir, .gomuxai/agents/<name> beside the binary
    /// (~/.local/bin/.gomuxai/...), sorts before most projects
    /// ("/home/u/.local" before "/home/u/dev"), so agy worked there instead
    /// (friction #46). agy takes a path as given, symlinks unresolved, so a
    /// symlink to `dir` at a path that sorts after Cwd leaves Cwd first:
    /// under the temp dirs, in a folder named "~gomuxai-<uid>" ("~" sorts
    /// after the letters). If none of those sorts after Cwd (a project under
    /// /var/www, say), or one can't be made, `dir` itself is passed.
    fn workspace_dir(&self, dir: &str) -> String {
        let cwd = &self.config.cwd;
        if cwd.is_empty() || !filepath::is_abs(dir) {
            return dir.to_string();
        }
        let clean_cwd = filepath::clean(cwd);
        if filepath::clean(dir) > clean_cwd {
            return dir.to_string();
        }
        let sum = Sha256::digest(filepath::clean(dir).as_bytes());
        let hex: String = sum[..4].iter().map(|b| format!("{b:02x}")).collect();
        let name = format!("{}-{hex}", filepath::base(dir));
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        for base in [temp_dir(), "/var/tmp".to_string()] {
            let home = filepath::join(&[base, format!("~gomuxai-{uid}"), "agy".to_string()]);
            let link = filepath::join(&[home.clone(), name.clone()]);
            if link <= clean_cwd {
                continue;
            }
            if own_dir(&home, uid).is_err() {
                continue;
            }
            if let Ok(cur) = std::fs::read_link(&link)
                && cur.to_str() == Some(dir)
            {
                return link;
            }
            let _ = std::fs::remove_file(&link);
            if std::os::unix::fs::symlink(dir, &link).is_ok() {
                return link;
            }
        }
        dir.to_string()
    }

    fn bundle_dir(&self, session_id: &str) -> String {
        if !self.config.agent_dir.is_empty() {
            return self.config.agent_dir.clone();
        }
        let id = if session_id.is_empty() {
            "default"
        } else {
            session_id
        };
        filepath::join(&[".gomuxai", "agents", id])
    }
}

/// Go's `os.TempDir`: $TMPDIR, else /tmp.
fn temp_dir() -> String {
    match std::env::var("TMPDIR") {
        Ok(d) if !d.is_empty() => d,
        _ => "/tmp".to_string(),
    }
}

/// Makes `dir` (and its parent) if missing, and checks the parent, in a
/// shared temp dir, is this user's own and private: another user's would
/// choose where the symlink points.
fn own_dir(dir: &str, uid: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let top = filepath::dir(dir);
    os::mkdir_all(dir, 0o700)?;
    for d in [top.as_str(), dir] {
        let info = std::fs::symlink_metadata(d).map_err(|e| os::PathError::new("lstat", d, e))?;
        if !info.is_dir() || info.uid() != uid || info.mode() & 0o022 != 0 {
            anyhow::bail!("{d} isn't a directory of this user's own");
        }
    }
    Ok(())
}

fn agy_exit_report_cmd(report_cmd: &str) -> String {
    // $? is agy's exit status: the shell runs this right after it.
    format!(
        "{} \"agy exited with code $?\"",
        report_cmd.replacen(" report --hook", " report ended --end-reason quit", 1)
    )
}

/// Writes <bundle_dir>/.agents/hooks.json registering PreInvocation,
/// PreToolUse, PostToolUse, PostInvocation, and Stop hooks that invoke
/// `report_cmd`.
fn write_agy_hooks_bundle(bundle_dir: &str, report_cmd: &str) -> anyhow::Result<()> {
    let hooks_dir = filepath::join(&[bundle_dir, ".agents"]);
    os::mkdir_all(&hooks_dir, 0o755).context("creating agy hooks dir")?;
    let data = agy_hooks_json(report_cmd)?;
    let hooks_file = filepath::join(&[hooks_dir.as_str(), "hooks.json"]);
    os::write_file(&hooks_file, &data, 0o644).context("writing agy hooks file")?;
    Ok(())
}

/// The .agents/hooks.json document, its keys in the order Go writes a map's.
#[derive(Serialize)]
struct AgyHooksDoc {
    gomuxai: AgyHooks,
}

#[derive(Serialize)]
struct AgyHooks {
    #[serde(rename = "PostInvocation")]
    post_invocation: Vec<HookCommand>,
    #[serde(rename = "PostToolUse")]
    post_tool_use: Vec<HookMatcher>,
    #[serde(rename = "PreInvocation")]
    pre_invocation: Vec<HookCommand>,
    #[serde(rename = "PreToolUse")]
    pre_tool_use: Vec<HookMatcher>,
    #[serde(rename = "Stop")]
    stop: Vec<HookCommand>,
}

/// Builds the .agents/hooks.json document for agy. PreInvocation,
/// PostInvocation, and Stop take flat handler arrays; PreToolUse and
/// PostToolUse take matcher-grouped arrays. PostInvocation reports no
/// status: it hands over held messages (delivery_output).
fn agy_hooks_json(report_cmd: &str) -> anyhow::Result<Vec<u8>> {
    let cmd_for = |event: &str| new_hook_command(format!("{report_cmd} --event {event}"));
    let doc = AgyHooksDoc {
        gomuxai: AgyHooks {
            pre_invocation: vec![cmd_for("PreInvocation")],
            pre_tool_use: vec![HookMatcher {
                matcher: "*".to_string(),
                hooks: vec![cmd_for("PreToolUse")],
            }],
            post_tool_use: vec![HookMatcher {
                matcher: "*".to_string(),
                hooks: vec![cmd_for("PostToolUse")],
            }],
            post_invocation: vec![cmd_for("PostInvocation")],
            stop: vec![cmd_for("Stop")],
        },
    };
    marshal_indent(&doc, "  ")
}

impl SpawnHooker for AgyBackend {
    /// Writes a per-agent .agents/hooks.json bundle under the gomuxai data
    /// dir (.gomuxai/agents/<name>/.agents/hooks.json) and injects it via
    /// --add-dir so nothing is written into the project directory. Because
    /// agy runs hooks with cwd set to the bundle dir and omits the event name
    /// from stdin JSON, each hook command uses the absolute report_cmd and
    /// passes --event <Event>. Since agy has no SessionEnd hook or native
    /// session list, the command is followed by "; <agent> report ended ..."
    /// so the parent shell records the end however agy exits (including
    /// crashes and SIGKILL), with agy's exit code as the detail, and
    /// --spawn-id guarding against stale respawns.
    fn launch_command_with_hooks(&self, report_cmd: &str, session_id: &str) -> String {
        let bundle_dir = self.bundle_dir(session_id);
        let hook_cmd = if self.config.confined {
            format!("{report_cmd} --confined")
        } else {
            report_cmd.to_string()
        };
        if write_agy_hooks_bundle(&bundle_dir, &hook_cmd).is_err() {
            if self.config.confined {
                // Without its hooks, agy's own settings would decide every
                // call (always-proceed on a person's machine): don't start it.
                return "echo 'gomuxai: no hooks for a confined agy agent, so it is not started' >&2; exit 1"
                    .to_string();
            }
            return self.launch_command();
        }
        let suffix = format!("; {}", agy_exit_report_cmd(report_cmd));
        let added = self.workspace_dir(&bundle_dir);
        *self.exit_suffix.lock().unwrap_or_else(|e| e.into_inner()) = suffix.clone();
        *self.added_dir.lock().unwrap_or_else(|e| e.into_inner()) = added.clone();
        format!(
            "{} --add-dir {}{suffix}",
            self.launch_command(),
            shell_quote(&added)
        )
    }
}

/// agy's workspace trust prompt, whose default is "Yes".
const AGY_TRUST: TrustDialog = TrustDialog {
    question: "Do you trust the contents of this project?",
    marker: ">",
    yes_first: true,
    footer: "Navigate",
    below: 2,
};

impl TrustPrompter for AgyBackend {
    /// Reads agy's workspace trust prompt off the screen.
    fn trust_prompt(&self, screen: &str) -> Option<String> {
        AGY_TRUST.read(screen).map(|(folder, _)| folder)
    }

    /// Accepts agy's workspace trust prompt.
    fn trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        AGY_TRUST.keys(screen, true)
    }

    /// Picks "No, exit" at agy's workspace trust prompt.
    fn decline_trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        AGY_TRUST.keys(screen, false)
    }
}

impl InstructionsProvider for AgyBackend {
    /// Delivers the briefing as AGENTS.md in a directory passed with
    /// --add-dir: agy loads AGENTS.md (and GEMINI.md) from the root of added
    /// directories, not from .agents/rules. The briefing already lives in
    /// the agent's own directory, which doubles as the hooks bundle, so
    /// usually the --add-dir is already there.
    fn with_instructions(&self, launch_cmd: &str, path: &str) -> anyhow::Result<String> {
        let data = os::read_file(path).context("reading the instructions for agy's AGENTS.md")?;
        let dir = filepath::dir(path);
        os::write_file(&filepath::join(&[dir.as_str(), "AGENTS.md"]), &data, 0o600)
            .context("writing agy's AGENTS.md")?;
        if !lock(&self.added_dir).is_empty()
            && filepath::clean(&dir) == filepath::clean(&self.bundle_dir(""))
        {
            return Ok(launch_cmd.to_string()); // launch_command_with_hooks added it
        }
        let add_dir = format!(" --add-dir {}", shell_quote(&self.workspace_dir(&dir)));
        if launch_cmd.contains(&add_dir) {
            return Ok(launch_cmd.to_string());
        }
        Ok(self.insert_before_exit(launch_cmd, &add_dir))
    }
}

impl PromptFlagProvider for AgyBackend {
    /// Delivers the initial task via agy's -i (--prompt-interactive) flag so
    /// the session starts interactively and runs the initial prompt once
    /// workspace trust is confirmed. If launch_command_with_hooks attached
    /// an exit report command ("; <agent> report ended ..."), -i is placed
    /// before it.
    fn append_prompt_flag(&self, launch_cmd: &str, prompt: &str) -> String {
        if prompt.is_empty() {
            return launch_cmd.to_string();
        }
        self.insert_before_exit(launch_cmd, &format!(" -i {}", shell_quote(prompt)))
    }
}

impl ModelChooser for AgyBackend {
    /// Runs agy on `model` (--model).
    fn with_model(&self, launch_cmd: &str, model: &str) -> String {
        self.insert_before_exit(launch_cmd, &format!(" --model {}", shell_quote(model)))
    }
}

impl EffortChooser for AgyBackend {
    /// Runs agy at `effort` (--effort), on the model it would pick.
    fn with_effort(&self, launch_cmd: &str, effort: &str) -> String {
        self.insert_before_exit(launch_cmd, &format!(" --effort {}", shell_quote(effort)))
    }

    /// agy names each effort of a model as its own id, `<model>-<effort>`
    /// (gemini-4-carbon-high, gemini-4-neon-xhigh): the id its transcripts
    /// record, and one --model takes as is. A base name with --effort is
    /// documented to pick the same variant, but an id agy doesn't know only
    /// warns and runs its default model, so spawn passes the id it records.
    fn model_at_effort(&self, model: &str, effort: &str) -> Option<String> {
        Some(format!("{model}-{effort}"))
    }
}

/// Holds the camelCase fields delivered by agy hooks on stdin. Payload
/// shapes and firing order are documented in
/// exploration/agy-hooks-findings.md.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
#[allow(dead_code)] // fields kept as the Go struct has them
struct AgyHookPayload {
    #[serde(rename = "hook_event_name")]
    event: String,
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "transcriptPath")]
    transcript_path: String,
    #[serde(rename = "lastUserInput")]
    last_user_input: String,
    #[serde(rename = "invocationNum")]
    invocation_num: i64,
    #[serde(rename = "executionNum")]
    execution_num: i64,
    #[serde(rename = "terminationReason")]
    termination_reason: String,
    error: String,
    #[serde(rename = "fullyIdle")]
    fully_idle: Option<bool>,
    #[serde(rename = "stepIdx")]
    step_idx: i64,
    #[serde(rename = "toolCall")]
    tool_call: AgyToolCall,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AgyToolCall {
    name: String,
    #[serde(deserialize_with = "raw_value")]
    args: Option<Box<RawValue>>,
}

impl AgyToolCall {
    fn args(&self) -> &[u8] {
        self.args
            .as_ref()
            .map(|r| r.get().as_bytes())
            .unwrap_or_default()
    }
}

impl HookProvider for AgyBackend {
    /// Maps an agy hook event and payload to a status report. agy has no
    /// SessionEnd or permission hook event, so neither status comes from
    /// here: the launch command reports the exit, and the service tells a
    /// permission dialog from the screen (permission_prompt_visible), as it
    /// does a question dialog (question_prompt_visible) and a turn that Esc
    /// or Ctrl+C ended, which fires no hook (at_prompt).
    fn parse_hook_event(&self, event: &str, payload: &[u8]) -> anyhow::Result<Option<HookReport>> {
        let p: AgyHookPayload = unmarshal_struct(payload, "namedagents.agyHookPayload")
            .context("decoding hook payload")?;
        let ev = first_non_empty(&[event, &p.event]).to_string();
        let mut r = HookReport {
            event: ev.clone(),
            session_id: p.conversation_id.clone(),
            tool: p.tool_call.name.clone(),
            transcript: p.transcript_path.clone(),
            ..Default::default()
        };

        match ev.as_str() {
            "PreInvocation" => {
                r.status = Status::Processing;
                r.detail = "prompt received".to_string();
                r.clear_result = p.invocation_num == 0;
            }
            "PreToolUse" => {
                r.status = Status::Processing;
                if p.tool_call.name == "run_command" {
                    r.command = agy_arg(p.tool_call.args(), "CommandLine");
                }
                r.detail = agy_tool_summary(&p.tool_call.name, p.tool_call.args());
                // agy shows its permission dialog after PreToolUse, with no hook of
                // its own: keep the input for the status that promotes this to
                // waiting.
                r.tool_input = tool_input(p.tool_call.args());
            }
            "PostToolUse" => {
                r.status = Status::Processing;
                let summary = agy_tool_summary(&p.tool_call.name, p.tool_call.args());
                r.detail = if !p.error.is_empty() {
                    format!("failed: {summary}")
                } else {
                    summary
                };
            }
            "PostInvocation" => {
                // Fires after each model call and its tools, the last one too, so
                // it is where held messages are handed over. A failed or cancelled
                // call is left to Stop: force_continue would override the cancel.
                if !p.error.is_empty()
                    || is_agy_error_termination(&p.termination_reason)
                    || p.termination_reason == "USER_CANCELED"
                {
                    return Ok(None);
                }
                r.status = Status::Processing;
                r.delivery_only = true;
            }
            "Stop" => {
                if !p.error.is_empty() || is_agy_error_termination(&p.termination_reason) {
                    r.status = Status::Error;
                    r.detail = truncate(first_non_empty(&[
                        &p.error,
                        &p.termination_reason,
                        "turn failed",
                    ]));
                } else if p.fully_idle == Some(false) {
                    r.status = Status::Processing;
                    r.detail = "background tasks running".to_string();
                } else {
                    r.status = Status::Idle;
                    r.detail = if !p.termination_reason.is_empty()
                        && p.termination_reason != "NO_TOOL_CALL"
                    {
                        p.termination_reason.clone()
                    } else {
                        "turn finished".to_string()
                    };
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(r))
    }
}

impl TranscriptProvider for AgyBackend {
    /// Reads the conversation transcript whose path hooks report as
    /// transcriptPath.
    fn transcript_reader(&self) -> Box<dyn agent_transcript::Reader> {
        Box::new(agent_transcript::Agy)
    }
}

fn is_agy_error_termination(reason: &str) -> bool {
    matches!(
        reason,
        "ERROR" | "MAX_INVOCATIONS" | "MAX_FORCED_INVOCATIONS" | "MAX_TOKEN_BUDGET_EXCEEDED"
    )
}

/// Renders an agy tool call as "tool: <what it's about>".
fn agy_tool_summary(tool: &str, args: &[u8]) -> String {
    if tool.is_empty() {
        return "tool".to_string();
    }
    if tool == "ask_question" {
        return agy_question_summary(tool, args);
    }
    tool_summary(tool, args)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AskQuestionCall {
    questions: Vec<AskQuestion>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AskQuestion {
    question: String,
    options: Vec<String>,
}

/// Renders an ask_question call as "ask_question: <first question> (<its
/// options>)", and how many more questions it asks.
fn agy_question_summary(tool: &str, args: &[u8]) -> String {
    let call: AskQuestionCall = match unmarshal_struct(args, "struct") {
        Ok(c) => c,
        Err(_) => return tool.to_string(),
    };
    let Some(q) = call.questions.first() else {
        return tool.to_string();
    };
    if q.question.trim().is_empty() {
        return tool.to_string();
    }
    let mut s = format!("{tool}: {}", q.question.trim());
    if !q.options.is_empty() {
        s.push_str(&format!(" ({})", q.options.join(", ")));
    }
    match call.questions.len() - 1 {
        0 => {}
        1 => s.push_str("; 1 more question"),
        n => s.push_str(&format!("; {n} more questions")),
    }
    s
}

/// These present a multiple-choice question dialog to the user rather than
/// a yes/no permission prompt; pressing "1" would pick an answer.
const AGY_DIALOG_TOOLS: &[&str] = &["ask_question"];

/// Match the numbered "1. Yes" and "N. No" lines of agy's permission dialog.
/// (Go's `\s`, `\d` and `\b` are ASCII.)
static AGY_DIALOG_YES_OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[\t\n\f\r >❯]*1\.[\t\n\f\r ]+Yes(?-u:\b)").expect("regex"));
static AGY_DIALOG_NO_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[\t\n\f\r >❯]*[0-9]+\.[\t\n\f\r ]+No(?-u:\b)").expect("regex")
});

/// Matches what ask_question's dialog shows: its "Question 1/N:" heading,
/// or its "↑/↓ Navigate · enter Select · esc Skip" hint (agy 1.4.7).
static AGY_QUESTION_DIALOG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^[\t\n\f\r ]*Question [0-9]+/[0-9]+:|Navigate(?-u:\b).*(?-u:\b)esc Skip(?-u:\b)",
    )
    .expect("regex")
});

impl PermissionResponder for AgyBackend {
    /// Returns the key that answers agy's numbered permission dialog: "1"
    /// approves once, "2" approves always in the conversation, Escape
    /// cancels/denies.
    fn permission_key(&self, resp: PermissionResponse, tool: &str) -> anyhow::Result<String> {
        permission_key(resp, tool, AGY_DIALOG_TOOLS)
    }

    /// Reports whether agy's permission dialog is on screen: numbered "1.
    /// Yes" and "N. No" options above the "esc to cancel" statusline footer,
    /// and not a question dialog whose options happen to be Yes and No. An
    /// idle prompt shows "? for shortcuts" instead.
    fn permission_prompt_visible(&self, screen: &str) -> bool {
        let tail = screen_tail(screen);
        if tail.is_empty() || !tail[0].to_lowercase().contains("esc to cancel") {
            return false;
        }
        let joined = tail.join("\n");
        AGY_DIALOG_YES_OPTION.is_match(&joined)
            && AGY_DIALOG_NO_OPTION.is_match(&joined)
            && !AGY_QUESTION_DIALOG.is_match(&joined)
    }
}

impl QuestionPrompter for AgyBackend {
    /// Reports whether ask_question's dialog is on screen: its heading or
    /// hint above the "esc to cancel" footer.
    fn question_prompt_visible(&self, screen: &str) -> bool {
        let tail = screen_tail(screen);
        if tail.is_empty() || !tail[0].to_lowercase().contains("esc to cancel") {
            return false;
        }
        AGY_QUESTION_DIALOG.is_match(&tail.join("\n"))
    }
}

/// The model agy says it runs, from the right of its footer (the screen's
/// last line): "Gemini 4 Argon · high", or with its mode first, "auto ·
/// Gemini 4 Carbon · high". None if the footer shows no "model · effort".
/// agy runs its default model when --model names one it doesn't know, with
/// only a warning in its log, so this is how spawn tells what it got.
pub fn agy_footer_model(screen: &str) -> Option<String> {
    let footer = screen.lines().rev().find(|l| !l.trim().is_empty())?;
    let footer = footer.trim_end();
    let right = match footer.rfind("  ") {
        Some(i) => footer[i..].trim(),
        None => footer.trim(),
    };
    if !right.contains(" · ") || right.starts_with('>') {
        return None;
    }
    Some(right.to_string())
}

/// How far a footer's text must start from the left for its left part to
/// count as blank: agy right-aligns the model name there.
const AGY_FOOTER_BLANK_COLS: usize = 16;

/// The line agy prints above its prompt after Esc or Ctrl+C.
const AGY_INTERRUPTED: &str = "Interrupted · What should Antigravity CLI do instead?";

/// How many ruled regions agy draws below its input box, at most: the panel
/// listing its running background tasks ("● [15:22:44] sleep 300 running",
/// agy 1.5.3), which also adds "· 1 task(s) · /tasks" to its footer.
const AGY_PANELS: usize = 1;

impl TurnEndReader for AgyBackend {
    /// Reports whether agy is back at its prompt: the input box ("> ") at
    /// the bottom of the screen, with only its footer below it (and the
    /// background tasks' panel, if any, between them), and the footer an
    /// idle one ("? for shortcuts", or nothing on the left while text is
    /// typed). A turn shows "esc to cancel" there instead, or "Press up to
    /// edit queued messages", and a dialog replaces the box. Esc and Ctrl+C
    /// fire no hook, so this is how gomuxai sees that the turn ended: how is
    /// "interrupted" when agy's line saying so is right above the box.
    fn at_prompt(&self, screen: &str) -> Option<String> {
        let mut lines: Vec<&str> = screen.trim_end_matches('\n').split('\n').collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let (start, _) = input_box(&lines, ">", AGY_PANELS)?;
        if lines.len() < 2 || !is_rule(&strip_ansi(lines[lines.len() - 2])) {
            return None;
        }
        let footer = lines[lines.len() - 1];
        let text = footer.trim_start_matches(' ');
        if !text.starts_with("? for shortcuts") && footer.len() - text.len() < AGY_FOOTER_BLANK_COLS
        {
            return None;
        }
        for i in (0..start.saturating_sub(1)).rev() {
            if lines[i].trim().is_empty() {
                continue;
            }
            if lines[i].contains(AGY_INTERRUPTED) {
                return Some("interrupted".to_string());
            }
            break;
        }
        Some(String::new())
    }
}

impl InputReader for AgyBackend {
    /// Reads agy's input box: the lines between two rules of "─" at the
    /// bottom of the screen, the first starting with the ">" prompt, above
    /// the background tasks' panel if one is drawn. Text drawn dim, if any,
    /// is left out as for Claude.
    fn input_text(&self, screen: &str) -> Option<String> {
        input_box_text(screen, ">", AGY_PANELS)
    }
}

/// Bounds each injected userMessage: agy cuts a longer one to about 192 KB,
/// keeping the head, and tells no one.
const AGY_MAX_STEP_BYTES: usize = 64 << 10;

/// Room left in each part for its label.
const AGY_PART_LABEL_ROOM: usize = 128;

#[derive(Serialize)]
struct AgyInjectStep {
    #[serde(rename = "userMessage")]
    user_message: String,
}

/// PostInvocation's output. agy parses it strictly: an unknown field or a
/// wrong type drops all of it, silently.
#[derive(Serialize)]
struct AgyPostInvocationOutput {
    #[serde(rename = "injectSteps")]
    inject_steps: Vec<AgyInjectStep>,
    #[serde(rename = "terminationBehavior")]
    termination_behavior: String,
}

/// PreToolUse's deny output for interrupt-urgency messages. Strict two-field
/// schema matching agy's protojson contract.
#[derive(Serialize)]
struct AgyPreToolDenyOutput<'a> {
    decision: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct AgyStopContinueOutput<'a> {
    decision: &'a str,
    reason: &'a str,
}

/// agy's PreToolUse decision output: `{"decision": "allow"|"deny", "reason":
/// ...}`. A deny blocks the call even under always-proceed, and the model is
/// told the reason (exploration/agy-hooks-findings.md, 2.3).
impl ConfinedHook for AgyBackend {
    fn confined_allow(&self, reason: &str) -> Vec<u8> {
        marshal(&AgyPreToolDenyOutput {
            decision: "allow",
            reason,
        })
    }
    fn confined_deny(&self, reason: &str) -> Vec<u8> {
        marshal(&AgyPreToolDenyOutput {
            decision: "deny",
            reason,
        })
    }
}

impl HookDeliverer for AgyBackend {
    /// agy's PostInvocation, which fires after every model call and its
    /// tools, including the turn's last one. With injectSteps and
    /// force_continue, the model answers the messages in the same turn, and
    /// each shows in the pane as a prompt of its own. Stop's "continue"
    /// works too, but shows nothing, and Esc fires no hook at all.
    fn can_deliver(&self, r: &HookReport) -> bool {
        r.event == "PostInvocation"
    }

    /// After its current model call and tools, mid-turn or last.
    fn delivery_point(&self) -> String {
        "its current step ends".to_string()
    }

    /// Returns the messages as PostInvocation's injectSteps, one
    /// userMessage each (a long one split into labeled parts, so agy never
    /// cuts it), with force_continue so the model answers them now rather
    /// than at the next prompt. Never force_continue without a step: the
    /// model would invent a reply. Empty if there is nothing to deliver.
    fn delivery_output(&self, messages: &[String]) -> Vec<u8> {
        let steps: Vec<AgyInjectStep> = messages
            .iter()
            .flat_map(|m| agy_message_parts(m))
            .map(|part| AgyInjectStep { user_message: part })
            .collect();
        if steps.is_empty() {
            return Vec::new();
        }
        marshal(&AgyPostInvocationOutput {
            inject_steps: steps,
            termination_behavior: "force_continue".to_string(),
        })
    }

    /// Coalesces the entire held batch into ONE `userMessage` (still split
    /// cleanly at 64 KB via `agy_message_parts` so agy never truncates it),
    /// with `force_continue` on `PostInvocation`, or `decision: "continue"` on
    /// `Stop` for `--later` messages.
    fn delivery_output_for(&self, event: &str, batch: &[crate::CoalescedMessage]) -> Vec<u8> {
        let text = crate::format_coalesced_batch(batch, false);
        if text.is_empty() {
            return Vec::new();
        }
        if event == "Stop" {
            return marshal(&AgyStopContinueOutput {
                decision: "continue",
                reason: &text,
            });
        }
        let steps: Vec<AgyInjectStep> = agy_message_parts(&text)
            .into_iter()
            .map(|part| AgyInjectStep { user_message: part })
            .collect();
        marshal(&AgyPostInvocationOutput {
            inject_steps: steps,
            termination_behavior: "force_continue".to_string(),
        })
    }

    /// On PreToolUse with an interrupt-urgency message waiting, denies the
    /// tool call cleanly so agy falls straight into PostInvocation and
    /// receives the coalesced urgent message with `force_continue`.
    fn interrupt_output(&self, r: &HookReport, reason: &str) -> Option<Vec<u8>> {
        if r.event != "PreToolUse" {
            return None;
        }
        Some(marshal(&AgyPreToolDenyOutput {
            decision: "deny",
            reason,
        }))
    }
}

/// The string value of `key` in an agy tool call's args. agy encodes each
/// value as JSON text inside a string (`{"CommandLine": "\"git log\""}`), so
/// it is decoded twice; a plain string is taken as it is. "" when missing.
pub fn agy_arg(args: &[u8], key: &str) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(args) else {
        return String::new();
    };
    match v.get(key) {
        Some(serde_json::Value::String(s)) => match serde_json::from_str::<String>(s) {
            Ok(inner) => inner,
            Err(_) => s.clone(),
        },
        _ => String::new(),
    }
}

/// Splits a message longer than AGY_MAX_STEP_BYTES into parts that fit, each
/// labeled with where it stands.
fn agy_message_parts(m: &str) -> Vec<String> {
    if m.len() <= AGY_MAX_STEP_BYTES {
        return vec![m.to_string()];
    }
    let mut parts = split_text(m, AGY_MAX_STEP_BYTES - AGY_PART_LABEL_ROOM);
    let n = parts.len();
    for (i, part) in parts.iter_mut().enumerate() {
        if i > 0 {
            *part = format!("[part {}/{n} of the message above]\n{part}", i + 1);
        }
        if i < n - 1 {
            part.push_str(&format!(
                "\n[part {}/{n}: the message goes on in the next prompt]",
                i + 1
            ));
        }
    }
    parts
}

/// Splits `s` into pieces of at most `max` bytes, at a line break in a
/// piece's second half if there is one, and never inside a UTF-8 sequence.
fn split_text(mut s: &str, max: usize) -> Vec<String> {
    let mut parts = Vec::new();
    while s.len() > max {
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(nl) = s[..cut].rfind('\n')
            && nl >= cut / 2
        {
            cut = nl + 1;
        }
        parts.push(s[..cut].to_string());
        s = &s[cut..];
    }
    parts.push(s.to_string());
    parts
}

impl Backend for AgyBackend {
    fn name(&self) -> &str {
        AgyBackend::name(self)
    }
    fn launch_command(&self) -> String {
        AgyBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        AgyBackend::init_message(self, task_file)
    }
    fn idle_pattern(&self) -> &str {
        AgyBackend::idle_pattern(self)
    }
    fn processing_pattern(&self) -> &str {
        AgyBackend::processing_pattern(self)
    }
}

impl Provider for AgyBackend {
    fn name(&self) -> &str {
        AgyBackend::name(self)
    }
    fn launch_command(&self) -> String {
        AgyBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        AgyBackend::init_message(self, task_file)
    }
    fn exit_command(&self) -> String {
        AgyBackend::exit_command(self)
    }

    fn as_hook_provider(&self) -> Option<&dyn HookProvider> {
        Some(self)
    }
    fn as_spawn_hooker(&self) -> Option<&dyn SpawnHooker> {
        Some(self)
    }
    fn as_hook_deliverer(&self) -> Option<&dyn HookDeliverer> {
        Some(self)
    }
    fn as_confined_hook(&self) -> Option<&dyn ConfinedHook> {
        Some(self)
    }
    fn as_input_reader(&self) -> Option<&dyn InputReader> {
        Some(self)
    }
    fn as_turn_end_reader(&self) -> Option<&dyn TurnEndReader> {
        Some(self)
    }
    fn as_question_prompter(&self) -> Option<&dyn QuestionPrompter> {
        Some(self)
    }
    fn as_instructions_provider(&self) -> Option<&dyn InstructionsProvider> {
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
    fn as_prompt_flag_provider(&self) -> Option<&dyn PromptFlagProvider> {
        Some(self)
    }
    fn as_permission_responder(&self) -> Option<&dyn PermissionResponder> {
        Some(self)
    }
    fn as_transcript_provider(&self) -> Option<&dyn TranscriptProvider> {
        Some(self)
    }
}

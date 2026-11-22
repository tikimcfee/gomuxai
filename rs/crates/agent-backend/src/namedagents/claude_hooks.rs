//! Go `namedagents/claude_hooks.go`. Payload shapes and firing order are
//! documented in exploration/claude-hooks-findings.md.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use agent_base::types::{END_QUIT, PromptChange};
use agent_base::util::{read_proc_tree, shell_quote};
use agent_claudereg as claudereg;
use anyhow::Context;
use regex::Regex;
use serde::Deserialize;
use serde_json::value::RawValue;

use super::claude::ClaudeBackend;
use super::claude_settings::{
    CLAUDE_HOOK_EVENTS, HookState, claude_hooks_status, claude_settings_path, live_others,
};
use super::hooks::{
    call_key, first_non_empty, input_box_text, marshal, permission_key, screen_tail, tool_input,
    tool_summary, truncate,
};
use super::trust::TrustDialog;
use crate::gojson::{raw_value, unmarshal_struct};
use crate::provider::*;

impl GlobalHooker for ClaudeBackend {
    /// Pins a new session's id; a resumed one keeps its own (Claude refuses
    /// --session-id with --resume unless it forks). The hooks come from
    /// Claude Code's user settings (agent hooks install), so nothing is
    /// added for them.
    fn launch_command_with_session(&self, session_id: &str) -> String {
        let mut cmd = self.launch_command();
        if self.config.resume.is_empty() {
            cmd.push_str(" --session-id ");
            cmd.push_str(&shell_quote(session_id));
        }
        cmd
    }

    /// Checks Claude Code's user settings for `binary`'s hook on every
    /// event.
    fn hooks_installed(&self, binary: &str) -> anyhow::Result<()> {
        let path = claude_settings_path()?;
        let r = claude_hooks_status(&path, binary)?;
        let has_current = r.events.values().any(|&s| s == HookState::Current);
        let missing: Vec<&str> = CLAUDE_HOOK_EVENTS
            .iter()
            .copied()
            .filter(|e| {
                let st = r.events.get(*e).copied().unwrap_or_default();
                // Hosts upgrading from 13-event settings before re-running
                // `agent hooks install` still have Stop installed, so let spawn
                // degrade gracefully to Stop rather than failing.
                if *e == "PostToolBatch" && st == HookState::Missing && has_current {
                    return false;
                }
                st != HookState::Current
            })
            .collect();
        let live = live_others(&r.others);
        if !missing.is_empty() && !live.is_empty() {
            anyhow::bail!(
                "Claude Code's hook ({path}) runs {}, not this binary, so this binary's store would never hear from a Claude agent it spawned: spawn with that binary, or point the hook here with agent hooks install --force (which stops that binary's agents reporting)",
                live.join(", ")
            );
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "Claude Code's settings ({path}) don't run this binary's hook on {}, so a Claude agent couldn't report its status, take held messages, or run pre-approved agent commands: agent hooks install registers it",
                missing.join(", ")
            );
        }
        Ok(())
    }
}

impl InstructionsProvider for ClaudeBackend {
    /// Appends the briefing to Claude's system prompt. Unlike a CLAUDE.md,
    /// nothing is written into the agent's project. It goes inline
    /// (--append-system-prompt), not as --append-system-prompt-file: Claude
    /// Code's /restart carries the inline flag into the restarted session,
    /// but refuses to restart a session launched with a prompt file (see
    /// CLAUDE_RESTART_BLOCKING_FLAGS).
    fn with_instructions(&self, launch_cmd: &str, path: &str) -> anyhow::Result<String> {
        let data = agent_base::gostd::os::read_file(path)
            .context("reading the instructions for claude's system prompt")?;
        Ok(format!(
            "{launch_cmd} --append-system-prompt {}",
            shell_quote(&String::from_utf8_lossy(&data))
        ))
    }
}

/// Holds the hook payload fields we use. Every payload carries
/// hook_event_name and session_id; the rest depend on the event.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ClaudeHookPayload {
    #[serde(rename = "hook_event_name")]
    event: String,
    session_id: String,
    transcript_path: String,
    prompt: String,
    cwd: String,
    source: String,
    tool_name: String,
    #[serde(deserialize_with = "raw_value")]
    tool_input: Option<Box<RawValue>>,
    notification_type: String,
    last_assistant_message: String,
    reason: String,
    error: String,
    /// `agent_id` and `agent_type` name the subagent a tool event came
    /// from; subagents' tool hooks fire in the main session, without them.
    agent_id: String,
    agent_type: String,
    /// (Stop) Lists work the session started that outlives the turn.
    /// Observed entries: {"id", "type": "shell" | "subagent", "status":
    /// "running", "description", "command" | "agent_type"}; finished tasks
    /// drop out of the list.
    background_tasks: Vec<ClaudeBackgroundTask>,
}

impl ClaudeHookPayload {
    fn tool_input(&self) -> &[u8] {
        self.tool_input
            .as_ref()
            .map(|r| r.get().as_bytes())
            .unwrap_or_default()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ClaudeBackgroundTask {
    status: String,
}

/// Counts the tasks still at work. When one finishes, Claude starts a new
/// turn on its own (a UserPromptSubmit whose prompt is a
/// <task-notification>), so the session isn't idle yet. session_crons are
/// left out on purpose: a cron fires later as a fresh turn, and between fires
/// the agent really is idle.
fn running_background_tasks(tasks: &[ClaudeBackgroundTask]) -> usize {
    tasks
        .iter()
        .filter(|t| t.status == "running" || t.status == "pending")
        .count()
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BashInput {
    command: String,
}

impl HookProvider for ClaudeBackend {
    /// Maps a Claude Code hook payload to a status report.
    fn parse_hook_event(&self, event: &str, payload: &[u8]) -> anyhow::Result<Option<HookReport>> {
        let mut p: ClaudeHookPayload = unmarshal_struct(payload, "namedagents.claudeHookPayload")
            .context("decoding hook payload")?;
        if p.event.is_empty() {
            p.event = event.to_string();
        }
        let input = p.tool_input().to_vec();

        let mut r = HookReport {
            event: p.event.clone(),
            session_id: p.session_id.clone(),
            tool: p.tool_name.clone(),
            transcript: p.transcript_path.clone(),
            agent: p.agent_id.clone(),
            ..Default::default()
        };
        match p.event.as_str() {
            "SessionStart" => {
                r.status = Status::Idle;
                r.detail = format!("session {}", p.source);
                // startup, resume, clear: a different conversation. compact keeps it.
                r.clear_result = p.source != "compact";
                r.prompt = PromptChange::AllEnded;
            }
            "UserPromptSubmit" => {
                r.status = Status::Processing;
                r.detail = "prompt received".to_string();
                r.prompt_text = p.prompt.clone();
                r.clear_result = true;
                r.prompt = PromptChange::MainEnded;
            }
            "PreToolUse" => {
                r.status = Status::Processing;
                r.detail = tool_summary(&p.tool_name, &input);
                if p.tool_name == "Bash" {
                    // As Go, a payload whose input doesn't decode has no command.
                    let cmd = unmarshal_struct::<BashInput>(&input, "struct").unwrap_or_default();
                    r.command = cmd.command;
                    r.cwd = p.cwd.clone();
                }
            }
            "PostToolUse" => {
                r.status = Status::Processing;
                r.detail = tool_summary(&p.tool_name, &input);
                r.prompt = PromptChange::Resolved;
                r.call_key = call_key(&p.tool_name, &input);
            }
            "PostToolUseFailure" => {
                r.status = Status::Processing;
                r.detail = format!("failed: {}", tool_summary(&p.tool_name, &input));
                r.prompt = PromptChange::Resolved;
                r.call_key = call_key(&p.tool_name, &input);
            }
            "PostToolBatch" => {
                // Fires once after every tool call in a batch resolves, right
                // before the next model call: delivers held messages mid-turn
                // without recording anything when nothing is held.
                r.status = Status::Processing;
                r.delivery_only = true;
            }
            "PermissionRequest" => {
                r.status = Status::Waiting;
                r.detail = format!("permission: {}", tool_summary(&p.tool_name, &input));
                if !p.agent_type.is_empty() {
                    // As Claude's dialog says: "Bash command · from the general-purpose agent".
                    r.detail
                        .push_str(&format!(" (from the {} agent)", p.agent_type));
                } else if !p.agent_id.is_empty() {
                    r.detail.push_str(" (from a subagent)");
                }
                r.tool_input = tool_input(&input);
                r.prompt = PromptChange::Opened;
                r.call_key = call_key(&p.tool_name, &input);
            }
            "PreCompact" => {
                r.status = Status::Processing;
                r.detail = "compacting".to_string();
            }
            "Notification" => {
                // permission_prompt arrives ~6s after PermissionRequest and can land
                // after the prompt is already answered, so only idle_prompt counts.
                if p.notification_type != "idle_prompt" {
                    return Ok(None);
                }
                r.status = Status::Idle;
                r.detail = "waiting for input".to_string();
            }
            "Stop" => {
                let n = running_background_tasks(&p.background_tasks);
                if n > 0 {
                    // The reply is interim ("started it, will report back"); the
                    // real one comes from the turn the task's completion starts.
                    r.status = Status::Processing;
                    r.detail = format!("turn finished, {n} background task(s) running");
                } else {
                    // Nothing left running, so nothing can be asking. (While background
                    // subagents run, their prompts outlive the turn.)
                    r.status = Status::Idle;
                    r.detail = "turn finished".to_string();
                    r.result = p.last_assistant_message.clone();
                    r.prompt = PromptChange::AllEnded;
                }
            }
            "StopFailure" => {
                r.status = Status::Error;
                r.detail = truncate(first_non_empty(&[&p.error, "turn failed"]));
                r.prompt = PromptChange::AllEnded;
            }
            "SessionEnd" => {
                // /clear ends the session but not the CLI; SessionStart follows.
                if p.reason == "clear" {
                    return Ok(None);
                }
                r.status = Status::Ended;
                r.end_reason = END_QUIT;
                r.detail = p.reason.clone();
                r.prompt = PromptChange::AllEnded;
            }
            _ => return Ok(None),
        }
        Ok(Some(r))
    }
}

/// `{"hookSpecificOutput": {...}}`, keys sorted as Go writes a map.
fn hook_specific_output(fields: &[(&str, &str)]) -> Vec<u8> {
    let inner: BTreeMap<&str, &str> = fields.iter().copied().collect();
    let mut outer = BTreeMap::new();
    outer.insert("hookSpecificOutput", inner);
    marshal(&outer)
}

impl HookDeliverer for ClaudeBackend {
    /// Claude's PostToolBatch (mid-turn, main session only — never let a
    /// subagent steal the main agent's held messages) and Stop (turn-end
    /// backstop, covering 0-tool turns and messages arriving during final
    /// generation).
    fn can_deliver(&self, r: &HookReport) -> bool {
        r.event == "Stop" || (r.event == "PostToolBatch" && r.agent.is_empty())
    }

    fn delivery_point(&self) -> String {
        "its next tool batch or turn ends".to_string()
    }

    /// Returns the messages as Stop's additionalContext, under a line saying
    /// what they are. Claude goes on with it and shows it as "Stop hook
    /// feedback: ...". A {"decision":"block"} works too, but shows as "Stop
    /// hook error" with an error footer.
    fn delivery_output(&self, messages: &[String]) -> Vec<u8> {
        let head = if messages.len() > 1 {
            format!(
                "{} messages arrived while you were working, oldest first:",
                messages.len()
            )
        } else {
            "A message arrived while you were working:".to_string()
        };
        let context = format!("{head}\n\n{}", messages.join("\n\n"));
        hook_specific_output(&[("hookEventName", "Stop"), ("additionalContext", &context)])
    }

    /// Coalesced delivery output for `PostToolBatch` (mid-turn, with a short
    /// user-visible `systemMessage` banner) or `Stop` (end of turn).
    fn delivery_output_for(&self, event: &str, batch: &[crate::CoalescedMessage]) -> Vec<u8> {
        let context = crate::format_coalesced_batch(batch, true);
        if context.is_empty() {
            return Vec::new();
        }
        let hook_event = if event == "PostToolBatch" {
            "PostToolBatch"
        } else {
            "Stop"
        };
        if hook_event == "PostToolBatch" {
            let mut inner = BTreeMap::new();
            inner.insert("additionalContext", context);
            inner.insert("hookEventName", hook_event.to_string());
            let mut outer = BTreeMap::new();
            outer.insert(
                "hookSpecificOutput",
                serde_json::to_value(inner).unwrap_or_default(),
            );
            outer.insert(
                "systemMessage",
                serde_json::Value::String(crate::mid_turn_banner(batch)),
            );
            return marshal(&outer);
        }
        hook_specific_output(&[
            ("hookEventName", hook_event),
            ("additionalContext", &context),
        ])
    }

    /// On main-session PreToolUse with an interrupt-urgency message waiting,
    /// denies the tool call AND sets `continue: false` + `stopReason` so
    /// Claude halts the turn cleanly rather than retrying the blocked tool.
    fn interrupt_output(&self, r: &HookReport, reason: &str) -> Option<Vec<u8>> {
        if r.event != "PreToolUse" || !r.agent.is_empty() {
            return None;
        }
        let mut inner = BTreeMap::new();
        inner.insert("hookEventName", "PreToolUse");
        inner.insert("permissionDecision", "deny");
        inner.insert("permissionDecisionReason", reason);
        let mut outer = BTreeMap::new();
        outer.insert("continue", serde_json::Value::Bool(false));
        outer.insert(
            "hookSpecificOutput",
            serde_json::to_value(inner).unwrap_or_default(),
        );
        outer.insert("stopReason", serde_json::Value::String(reason.to_string()));
        Some(marshal(&outer))
    }
}

/// Claude Code (2.1.319, measured) takes text that arrives at once as a
/// paste, bracketed or not, when it is longer than 800 characters (UTF-16
/// code units, as JavaScript counts) or has 3 or more line breaks: its input
/// box shows "[Pasted text #N +M lines]", and the prompt the model gets
/// wraps the text in <pasted_content> tags, which it is told to take as
/// material rather than as the request. CLAUDE_PASTE_CHARS leaves a margin
/// below the 800.
const CLAUDE_PASTE_CHARS: usize = 640;
const CLAUDE_PASTE_BREAKS: usize = 3;

impl PromptHandover for ClaudeBackend {
    /// See CLAUDE_PASTE_CHARS.
    fn typed_as_paste(&self, text: &str) -> bool {
        let text = text.replace("\r\n", "\n");
        if text.matches('\n').count() + text.matches('\r').count() >= CLAUDE_PASTE_BREAKS {
            return true;
        }
        text.encode_utf16().count() > CLAUDE_PASTE_CHARS
    }

    /// UserPromptSubmit runs before the model sees the prompt, for a prompt
    /// typed at the input box and for one typed mid-turn (Claude takes that
    /// in at its next tool boundary), and its additionalContext reaches the
    /// model with the prompt.
    fn submitted_prompt(&self, r: &HookReport) -> Option<String> {
        (r.event == "UserPromptSubmit" && r.agent.is_empty()).then(|| r.prompt_text.clone())
    }

    /// Hands text over as UserPromptSubmit's additionalContext, which the
    /// model gets as "UserPromptSubmit hook additional context: ...": whole
    /// up to 10,000 characters; beyond that, Claude Code saves it to a file
    /// and gives the model a 2 KB preview and the file's path to read.
    fn handover_output(&self, text: &str) -> Vec<u8> {
        hook_specific_output(&[
            ("hookEventName", "UserPromptSubmit"),
            ("additionalContext", text),
        ])
    }
}

impl HookAllower for ClaudeBackend {
    /// Approves PreToolUse's tool call: Claude runs it without a permission
    /// prompt, and shows `reason` to the person, not the model.
    fn allow_output(&self, reason: &str) -> Vec<u8> {
        hook_specific_output(&[
            ("hookEventName", "PreToolUse"),
            ("permissionDecision", "allow"),
            ("permissionDecisionReason", reason),
        ])
    }
}

impl TranscriptProvider for ClaudeBackend {
    /// Reads the session JSONL whose path hooks report as transcript_path.
    fn transcript_reader(&self) -> Box<dyn agent_transcript::Reader> {
        Box::new(agent_transcript::Claude)
    }
}

impl NativeStatusProvider for ClaudeBackend {
    /// Reads the session's status from Claude Code's session registry
    /// (agent-claudereg): found is false when no running process holds the
    /// session.
    fn native_status(&self, session_id: &str) -> (Status, bool, Option<anyhow::Error>) {
        let snap = match claudereg::read_system() {
            Ok(s) => s,
            Err(e) => return (Status::Unknown, false, Some(e)),
        };
        let sessions = snap.sessions();
        let Some(e) = sessions.get(session_id) else {
            // Not listed means not running only if every file could be read.
            return (Status::Unknown, false, snap.err());
        };
        let (st, detail) = registry_status(&e.entry);
        if !e.unknown.is_empty() {
            // Listed, running, in a state gomuxai can't name: say which.
            return (st, true, Some(anyhow::anyhow!(detail)));
        }
        (st, true, None)
    }
}

/// Returns the status of every Claude Code session running on this host, by
/// session id, from the session registry.
pub fn claude_sessions() -> anyhow::Result<HashMap<String, Status>> {
    let snap = claudereg::read_system()?;
    // An unread file could be any session: none can be called gone.
    snap.check()?;
    Ok(snap
        .sessions()
        .into_iter()
        .map(|(id, e)| (id, registry_status(&e.entry).0))
        .collect())
}

/// Maps a registry entry's status to an agent status, with a line saying
/// what it means.
pub fn registry_status(e: &claudereg::Entry) -> (Status, String) {
    if !e.unknown.is_empty() {
        return (Status::Unknown, e.unknown.clone());
    }
    match e.status.as_str() {
        claudereg::STATUS_BUSY => return (Status::Processing, "a turn is running".to_string()),
        claudereg::STATUS_IDLE => return (Status::Idle, "at its prompt".to_string()),
        claudereg::STATUS_SHELL => {
            // The turn is over, but a shell it started still runs in the
            // background, and its end starts the next turn (a task
            // notification): not done, as a Stop with background tasks isn't.
            // Claude Code's own `claude agents` calls it busy too.
            return (
                Status::Processing,
                "turn over; a background shell it started is still running, and its end starts the next turn"
                    .to_string(),
            );
        }
        claudereg::STATUS_WAITING => {
            if !e.waiting_for.is_empty() {
                return (Status::Waiting, format!("waiting for {}", e.waiting_for));
            }
            return (Status::Waiting, "waiting for a person".to_string());
        }
        _ => {}
    }
    // No status recorded yet (a CLI still starting).
    (
        Status::Unknown,
        "the CLI has not recorded a status yet".to_string(),
    )
}

/// The process tree, as agent-claudereg's `Snapshot::under_pane` reads it.
struct ProcTree(agent_base::util::ProcTree);

impl claudereg::Tree for ProcTree {
    fn depth(&self, pid: i32, root: i32) -> Option<usize> {
        self.0.depth(pid, root)
    }
}

impl PaneSessionProvider for ClaudeBackend {
    /// Finds the Claude session a pane runs: of the running processes the
    /// registry proves (pid and start time), the one nearest the pane's
    /// process in the process tree. A pane whose CLI was replaced (an exit,
    /// then `claude` typed at the shell) answers with the one running now; a
    /// `claude -p` started from inside the session is deeper, so it is never
    /// taken for it.
    fn pane_session(&self, pane_pid: i32) -> anyhow::Result<Option<PaneSession>> {
        let snap = claudereg::read_system()?;
        let tree = ProcTree(read_proc_tree()?);
        let Some(mut e) = snap.under_pane(&tree, pane_pid)? else {
            // Nothing found means nothing runs only if every file could be read.
            snap.check()?;
            return Ok(None);
        };
        // Claude Code may have moved the pane CLI's conversation into a
        // background session: the pane shows it, and that session is the one
        // going on, wherever its process runs (Snapshot::continuation).
        if let Some(moved) = snap.continuation(&e, claudereg::find_transcript) {
            if moved.verdict == claudereg::Verdict::Stale {
                return Ok(Some(PaneSession {
                    id: moved.session_id.clone(),
                    pid: e.pid,
                    status: Status::Unknown,
                    detail: format!(
                        "its conversation moved into background session {}, which no process runs now; the pane's CLI starts it again on its next input",
                        moved.session_id
                    ),
                }));
            }
            e = moved;
        }
        let (status, detail) = registry_status(&e.entry);
        Ok(Some(PaneSession {
            id: e.session_id.clone(),
            pid: e.pid,
            status,
            detail,
        }))
    }
}

/// These raise a PermissionRequest whose dialog is a question to the user
/// rather than a yes/no prompt; "1" would pick an answer.
const CLAUDE_DIALOG_TOOLS: &[&str] = &["AskUserQuestion", "ExitPlanMode"];

/// Matches the "No" line of a permission dialog. Its number varies with the
/// dialog (3 for Write, 4 for Bash in v2.1.310), and it behaves exactly like
/// Escape, so it is only used for detection. (Go's `\s`, `\d` and `\b` are
/// ASCII.)
static CLAUDE_DIALOG_NO_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[\t\n\f\r ❯]*[0-9]+\.[\t\n\f\r ]+No(?-u:\b)").expect("regex")
});

impl PermissionResponder for ClaudeBackend {
    /// Returns the key that answers Claude's permission dialog: "1" is
    /// "Yes", "2" is the "don't ask again" variant, Escape cancels the turn.
    fn permission_key(&self, resp: PermissionResponse, tool: &str) -> anyhow::Result<String> {
        permission_key(resp, tool, CLAUDE_DIALOG_TOOLS)
    }

    /// Reports whether Claude's permission dialog is on screen: a numbered
    /// "No" option above the "Esc to cancel" footer. A running turn shows
    /// "esc to interrupt" instead, and the idle prompt neither.
    fn permission_prompt_visible(&self, screen: &str) -> bool {
        let tail = screen_tail(screen);
        if tail.is_empty() || !tail[0].contains("Esc to cancel") {
            return false;
        }
        CLAUDE_DIALOG_NO_OPTION.is_match(&tail.join("\n"))
    }
}

impl InputReader for ClaudeBackend {
    /// Reads Claude's input box: the lines between the last two rules of "─"
    /// at the bottom of the screen, the first starting with the "❯" prompt.
    /// Placeholder text ("Try ...") and suggestions are drawn dim, so dim
    /// text is left out; what remains was typed.
    fn input_text(&self, screen: &str) -> Option<String> {
        input_box_text(screen, "❯", 0)
    }
}

impl SubmitChecker for ClaudeBackend {
    /// Claude takes input that arrives in one read with more than a
    /// keystroke in it for a paste, where "\r" is a newline. A message typed
    /// while its event loop is busy (a long render, a paste it is still
    /// collecting) arrives in one read with the Enter typed after it.
    fn enter_can_be_a_newline(&self) {}
}

impl CommandAllower for ClaudeBackend {
    /// Pre-approves Bash commands by prefix for this session only, via
    /// --allowedTools rules like Bash(agent status:*).
    fn allow_commands(&self, launch_cmd: &str, prefixes: &[String]) -> String {
        if prefixes.is_empty() {
            return launch_cmd.to_string();
        }
        let rules: Vec<String> = prefixes
            .iter()
            .map(|p| shell_quote(&format!("Bash({p}:*)")))
            .collect();
        format!("{launch_cmd} --allowedTools {}", rules.join(" "))
    }

    /// One --allowedTools with the Bash prefixes and `allow`, and a
    /// --disallowedTools with `deny`.
    fn allow_tools(
        &self,
        launch_cmd: &str,
        prefixes: &[String],
        allow: &[String],
        deny: &[String],
    ) -> String {
        let mut cmd = launch_cmd.to_string();
        let rules: Vec<String> = prefixes
            .iter()
            .map(|p| format!("Bash({p}:*)"))
            .chain(allow.iter().cloned())
            .map(|r| shell_quote(&r))
            .collect();
        if !rules.is_empty() {
            cmd = format!("{cmd} --allowedTools {}", rules.join(" "));
        }
        if !deny.is_empty() {
            let rules: Vec<String> = deny.iter().map(|r| shell_quote(r)).collect();
            cmd = format!("{cmd} --disallowedTools {}", rules.join(" "));
        }
        cmd
    }
}

/// Claude's folder trust dialog. Its highlighted option is "No, exit", so a
/// bare Enter would quit.
const CLAUDE_TRUST: TrustDialog = TrustDialog {
    question: "Quick safety check",
    marker: "❯",
    yes_first: false,
    footer: "Enter to confirm",
    below: 1,
};

impl TrustPrompter for ClaudeBackend {
    /// Reads Claude's folder trust dialog off the screen.
    fn trust_prompt(&self, screen: &str) -> Option<String> {
        CLAUDE_TRUST.read(screen).map(|(folder, _)| folder)
    }

    /// Accepts Claude's folder trust dialog, moving to "Yes" first if "No,
    /// exit" is highlighted.
    fn trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        CLAUDE_TRUST.keys(screen, true)
    }

    /// Picks "No, exit" at Claude's folder trust dialog.
    fn decline_trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        CLAUDE_TRUST.keys(screen, false)
    }
}

//! Go `internal/service/trust.go`.
//!
//! A CLI started in a folder it hasn't trusted (Claude Code, agy) first asks
//! whether to trust it. Until it is answered the CLI runs no hooks and
//! Claude's session registry doesn't list the session, so only the screen
//! shows the prompt: status reads it there as waiting, with TRUST_TOOL as the
//! tool, and approve and deny answer it.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use agent_backend::{END_QUIT, PermissionResponse, Provider};
use agent_base::gostd::strconv;
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, Time};

use super::agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_PROCESSING, AGENT_STATUS_WAITING, AgentService, AgentStatus,
    SpawnOptions,
};
use super::ctx::Ctx;
use super::since;
use super::status::{STATUS_SOURCE_SCRAPE, StatusInfo, StatusReport};

/// TrustTool is the tool status names for a workspace trust prompt, and what
/// approve and deny's --expect take to name it.
pub const TRUST_TOOL: &str = "trust";

/// trustDetailPrefix starts the detail of a trust prompt, before its folder.
const TRUST_DETAIL_PREFIX: &str = "trust: ";

// Events approve and deny record for a trust prompt they answered.
pub const EVENT_TRUSTED: &str = "trusted";
pub const EVENT_TRUST_DECLINED: &str = "trust-declined";

/// trustStartGrace is how long after approve trusts a folder the CLI may
/// take to show up in its own session list (Claude's registry) before its
/// absence means it died.
const TRUST_START_GRACE: Duration = Duration::from_secs(30);

/// Reports whether a status (and its detail, as status and the bus carry
/// them) is a workspace trust prompt, and the folder it asks about.
pub fn trust_prompt_folder(status: &AgentStatus, detail: &str) -> Option<String> {
    if *status != AGENT_STATUS_WAITING {
        return None;
    }
    detail.strip_prefix(TRUST_DETAIL_PREFIX).map(str::to_string)
}

impl StatusInfo {
    /// Reports whether info is an agent waiting at its CLI's workspace trust
    /// prompt.
    pub fn is_trust_prompt(&self) -> bool {
        trust_prompt_folder(&self.status, &self.detail).is_some() && self.tool == TRUST_TOOL
    }
}

/// trustStarting reports whether reported is approve's report of trusting
/// the folder, recent enough that the CLI may still be starting.
pub(crate) fn trust_starting(reported: Option<&StatusInfo>) -> bool {
    reported.is_some_and(|r| r.event == EVENT_TRUSTED && since(&r.updated_at) < TRUST_START_GRACE)
}

/// Reports whether a trust prompt's folder (as the CLI shows it: a path, or
/// one under "~") is dir, under any of its names: the two are compared with
/// every symlink resolved, so /var/tmp/x and /private/var/tmp/x (macOS's
/// /var is a link) are one folder. A CLI asks about the folder it was started
/// in as its own process sees it, with symlinks resolved, which needn't be
/// the path spawn was given; and it may ask about each name in turn.
pub fn same_folder(shown: &str, dir: &str) -> bool {
    let resolve = |p: &str| {
        let p = p.trim();
        let p = match p
            .strip_prefix("~/")
            .or(if p == "~" { Some("") } else { None })
        {
            Some(rest) => match std::env::var("HOME") {
                Ok(home) if !home.is_empty() => {
                    agent_base::gostd::filepath::join(&[home.as_str(), rest])
                }
                _ => p.to_string(),
            },
            None => p.to_string(),
        };
        agent_base::gostd::filepath::eval_symlinks(&p)
            .unwrap_or_else(|_| agent_base::gostd::filepath::clean(&p))
    };
    !shown.trim().is_empty() && !dir.trim().is_empty() && resolve(shown) == resolve(dir)
}

/// How messages name the CLI behind a backend: Claude for claude, the
/// backend's name otherwise.
pub fn trust_backend_name(backend_name: &str) -> String {
    if backend_name == "claude" {
        return "Claude".to_string();
    }
    backend_name.to_string()
}

impl AgentService {
    /// trustPrompter returns agent's backend, if it is a TrustPrompter.
    fn trust_prompter(&self, agent: &Agent) -> Option<Box<dyn Provider>> {
        let p = self.provider(agent).ok()?;
        p.as_trust_prompter()?;
        Some(p)
    }

    /// trustPromptOnScreen reads the agent's screen for its CLI's trust prompt,
    /// returning the folder it asks about (the agent's own if the screen
    /// doesn't show it).
    pub(crate) fn trust_prompt_on_screen(&self, agent: &Agent) -> Option<String> {
        let p = self.trust_prompter(agent)?;
        let tp = p.as_trust_prompter()?;
        let screen = self.tmux.capture(&agent.session_name, 0).ok()?;
        let mut folder = tp.trust_prompt(&screen)?;
        if folder.is_empty() {
            folder = agent.cwd.clone();
        }
        if folder.is_empty() {
            folder = "its folder".to_string();
        }
        Some(folder)
    }

    /// trustPromptStatus is the status of an agent whose CLI shows its trust
    /// prompt. It is looked for only while nothing has reported (the prompt
    /// comes before any hook) or the last report is approve or deny answering
    /// it, in case the keys didn't take.
    pub(crate) fn trust_prompt_status(
        &self,
        agent: &Agent,
        reported: Option<&StatusInfo>,
    ) -> Option<StatusInfo> {
        if let Some(r) = reported
            && r.event != EVENT_TRUSTED
            && r.event != EVENT_TRUST_DECLINED
        {
            return None;
        }
        let folder = self.trust_prompt_on_screen(agent)?;
        Some(StatusInfo {
            status: AGENT_STATUS_WAITING,
            source: STATUS_SOURCE_SCRAPE.into(),
            tool: TRUST_TOOL.into(),
            detail: format!("{TRUST_DETAIL_PREFIX}{folder}"),
            ..Default::default()
        })
    }

    /// respondTrust answers the workspace trust prompt info describes: approve
    /// trusts the folder, deny picks "No, exit" (the CLI exits). It checks the
    /// prompt is still on screen right before the keypress, as
    /// respond_permission does for a permission prompt, and reads which option
    /// is highlighted from that same screen.
    pub(crate) fn respond_trust(
        &self,
        agent: &Agent,
        info: StatusInfo,
        resp: PermissionResponse,
    ) -> Result<StatusInfo> {
        let folder = trust_prompt_folder(&info.status, &info.detail).unwrap_or_default();
        if resp == PermissionResponse::ApproveAlways {
            bail!(
                "agent {} is at its workspace trust prompt for {folder}, which has no \"always\" answer: agent approve {} trusts the folder for good",
                agent.name,
                agent.name
            );
        }
        let Some(p) = self.trust_prompter(agent) else {
            bail!(
                "agent {} runs backend {}, which can't answer a trust prompt: use agent keys {}@{} to press the keys yourself",
                agent.name,
                strconv::quote(&agent.backend),
                agent.name,
                self.cfg.host
            );
        };
        let tp = p.as_trust_prompter().expect("a TrustPrompter");
        let screen = self
            .tmux
            .capture(&agent.session_name, 0)
            .context("capturing screen")?;
        if tp.trust_prompt(&screen).is_none() {
            bail!(
                "agent {} has no workspace trust prompt on screen; it may have just been answered (agent status {} -v shows what it's doing)",
                agent.name,
                agent.name
            );
        }
        let (keys, report) = if resp == PermissionResponse::Deny {
            (
                tp.decline_trust_keys(&screen),
                StatusReport {
                    status: AGENT_STATUS_ENDED,
                    end_reason: END_QUIT,
                    event: EVENT_TRUST_DECLINED.into(),
                    detail: format!("declined to trust {folder}, so its CLI exited"),
                    ..Default::default()
                },
            )
        } else {
            (
                tp.trust_keys(&screen),
                StatusReport {
                    status: AGENT_STATUS_PROCESSING,
                    event: EVENT_TRUSTED.into(),
                    detail: format!("trusted {folder}; its CLI is starting"),
                    ..Default::default()
                },
            )
        };
        let Some(keys) = keys else {
            bail!(
                "agent {}'s trust prompt shows neither option highlighted, so the keys to answer it are unknown: agent capture {} shows it; answer it in its session ({})",
                agent.name,
                agent.name,
                agent.session_name
            );
        };
        for key in &keys {
            self.tmux.send_special_key(&agent.session_name, key)?;
        }
        // The prompt fired no hook, and the CLI won't report until it has
        // started (or, declined, at all): record what was answered.
        self.report(&agent.name, report)?;
        if resp != PermissionResponse::Deny && self.held_count(&agent.name) > 0 {
            // Spawn held its task while the prompt was up: type it once the
            // CLI is at its prompt.
            let _ = self.start_deliverer(&agent.name);
        }
        Ok(info)
    }

    /// holdFirstMessage holds a just-spawned agent's first message if its CLI
    /// shows its trust prompt or another dialog (see dialog.rs), where typing it
    /// would be lost (or its Enter answer the dialog). Approving the trust
    /// prompt delivers it; past another dialog, the deliverer types it once the
    /// agent is at its prompt. It reports whether the message was held.
    pub(crate) fn hold_first_message(
        &self,
        ctx: &Ctx,
        opts: &SpawnOptions,
        agent: &Agent,
        message: &str,
    ) -> Result<bool> {
        let Ok(provider) = self.provider(agent) else {
            return Ok(false);
        };
        let trusts = provider.as_trust_prompter().is_some();
        let dialogs = provider.as_dialog_reader().is_some();
        if !trusts && !dialogs {
            return Ok(false);
        }
        if let Some(sc) = self.submit_checker(agent) {
            // The CLI may still be drawing: wait for its input box, or a
            // dialog in its place, as typing a message does.
            let (_, why) = self.input_before(agent, sc.ir());
            if !why.is_empty() {
                // A dialog ends the wait with an error, which is the point:
                // whether it is the trust prompt is read below.
                let _ = self.await_input_box(ctx, agent, sc.ir(), true);
                if let Some(e) = ctx.err() {
                    return Err(e.into());
                }
            }
        }
        let trust = self.trust_prompt_on_screen(agent).is_some();
        let dialog = self.dialog_on_screen(agent).is_some();
        if !trust && !dialog {
            return Ok(false);
        }
        let from = if opts.spawned_by.is_empty() {
            "user"
        } else {
            opts.spawned_by.as_str()
        };
        let mut msg = Message {
            sender_id: from.to_string(),
            receiver_id: agent.name.clone(),
            content: message.to_string(),
            status: MESSAGE_STATUS_PENDING,
            created_at: Time::now(),
            ..Default::default()
        };
        self.storage
            .create_message(&mut msg)
            .context("holding the first message")?;
        if dialog {
            let _ = self.start_deliverer(&agent.name); // best effort: a send or agent deliver types it too
        }
        Ok(true)
    }
}

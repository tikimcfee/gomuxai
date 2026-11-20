//! Go `internal/service/dialog.go`.
//!
//! A CLI can show a dialog that no hook reports and its session status
//! doesn't tell from idle (backend.DialogReader): Claude's offers, notices
//! and setup wizards, which come up at startup or when a turn ends. Keys go
//! to the dialog while it is up, so a message typed then is lost, and its
//! Enter picks the dialog's highlighted option. So the screen is read for
//! one whenever the agent would otherwise be idle: status reports it as
//! waiting, with DIALOG_TOOL as the tool; send holds messages (or refuses,
//! for an agent that doesn't hold them) until it is gone; approve and deny
//! answer it.

use anyhow::{Context, Result, anyhow, bail};

use agent_backend::{Dialog, DialogReader, EndReason, PermissionResponse, Provider};
use agent_base::gostd::strconv;
use agent_store::Agent;

use super::agent_service::{
    AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AGENT_STATUS_UNKNOWN, AGENT_STATUS_WAITING,
    AgentService, AgentStatus, one_line,
};
use super::status::{STATUS_SOURCE_SCRAPE, StatusInfo, unloaded_backend};

/// DialogTool is the tool status names for such a dialog, and what approve
/// and deny's --expect take to name it.
pub const DIALOG_TOOL: &str = "dialog";

/// dialogDetailPrefix starts the detail of a dialog, before its title.
const DIALOG_DETAIL_PREFIX: &str = "dialog: ";

/// dialogHighlightedSep separates a dialog's title from its highlighted
/// option in its detail.
const DIALOG_HIGHLIGHTED_SEP: &str = "; Enter picks: ";

/// Reports whether a status (and its detail, as status and the bus carry
/// them) is a dialog, and its title.
pub fn dialog_title(status: &AgentStatus, detail: &str) -> Option<String> {
    if *status != AGENT_STATUS_WAITING {
        return None;
    }
    let rest = detail.strip_prefix(DIALOG_DETAIL_PREFIX)?;
    Some(
        rest.split_once(DIALOG_HIGHLIGHTED_SEP)
            .map_or(rest, |(t, _)| t)
            .to_string(),
    )
}

impl StatusInfo {
    /// Reports whether info is an agent waiting at a dialog its CLI shows
    /// (not a permission, question or trust prompt).
    pub fn is_dialog(&self) -> bool {
        dialog_title(&self.status, &self.detail).is_some() && self.tool == DIALOG_TOOL
    }
}

/// dialogFromDetail reads back the dialog a status detail describes.
pub(crate) fn dialog_from_detail(detail: &str) -> Dialog {
    let rest = detail.strip_prefix(DIALOG_DETAIL_PREFIX).unwrap_or(detail);
    let (title, high) = rest
        .split_once(DIALOG_HIGHLIGHTED_SEP)
        .unwrap_or((rest, ""));
    Dialog {
        title: title.to_string(),
        highlighted: high.to_string(),
    }
}

/// dialogDetail is the status detail for d.
fn dialog_detail(d: &Dialog) -> String {
    let mut detail = format!("{DIALOG_DETAIL_PREFIX}{}", d.title);
    if !d.highlighted.is_empty() {
        detail += DIALOG_HIGHLIGHTED_SEP;
        detail += &d.highlighted;
    }
    detail
}

/// dialogIn reads a dialog off screen with dr, unless provider recognizes
/// it as a permission, question or trust prompt.
fn dialog_in(provider: &dyn Provider, dr: &dyn DialogReader, screen: &str) -> Option<Dialog> {
    if provider
        .as_permission_responder()
        .is_some_and(|pr| pr.permission_prompt_visible(screen))
    {
        return None;
    }
    if provider
        .as_question_prompter()
        .is_some_and(|qp| qp.question_prompt_visible(screen))
    {
        return None;
    }
    if let Some(tp) = provider.as_trust_prompter()
        && tp.trust_prompt(screen).is_some()
    {
        return None;
    }
    dr.dialog(screen)
}

/// dialogSendErr is the error for typing into an agent at a dialog.
pub(crate) fn dialog_send_err(agent: &Agent, d: &Dialog) -> anyhow::Error {
    anyhow!(
        "agent {} shows a dialog ({}), which takes what is typed, and the message's Enter would pick {}: {}",
        agent.name,
        one_line(&d.title),
        highlighted_or(d, "its highlighted option"),
        dialog_hint(agent)
    )
}

/// highlightedOr names d's highlighted option, quoted, or says other.
fn highlighted_or(d: &Dialog, other: &str) -> String {
    if d.highlighted.is_empty() {
        return other.to_string();
    }
    strconv::quote(&d.highlighted)
}

/// dialogHint says how a person gets an agent past a dialog.
pub(crate) fn dialog_hint(agent: &Agent) -> String {
    format!(
        "agent deny {} closes it (Esc), agent approve {} picks the highlighted option (Enter), or agent capture {} shows it and agent key {} <key> answers it otherwise; held messages are typed in once it's gone",
        agent.name, agent.name, agent.name, agent.session_name
    )
}

impl AgentService {
    /// dialogOnScreen reads the agent's screen for a dialog its backend reports
    /// as one (backend.DialogReader). Permission, question and trust prompts
    /// have their own handling, so they don't count.
    pub(crate) fn dialog_on_screen(&self, agent: &Agent) -> Option<Dialog> {
        let provider = self.provider(agent).ok()?;
        let dr = provider.as_dialog_reader()?;
        let screen = self.tmux.capture(&agent.session_name, 0).ok()?;
        dialog_in(provider.as_ref(), dr, &screen)
    }

    /// dialogStatus is the status of an agent whose resolved status, info, says
    /// it takes input (idle, error, unknown) or waits with nothing to say on
    /// what, while its screen shows a dialog: waiting, at that dialog. None
    /// if the screen shows none.
    pub(crate) fn dialog_status(&self, agent: &Agent, info: &StatusInfo) -> Option<StatusInfo> {
        if info.status == AGENT_STATUS_IDLE
            || info.status == AGENT_STATUS_ERROR
            || info.status == AGENT_STATUS_UNKNOWN
        {
        } else if info.status == AGENT_STATUS_WAITING {
            if !info.tool.is_empty() || !info.pending.is_empty() {
                return None; // a prompt it already knows
            }
        } else {
            return None;
        }
        let d = self.dialog_on_screen(agent)?;
        let mut waiting = info.clone();
        waiting.status = AGENT_STATUS_WAITING;
        waiting.end_reason = EndReason::default();
        waiting.source = STATUS_SOURCE_SCRAPE.into();
        waiting.tool = DIALOG_TOOL.into();
        waiting.tool_input = String::new();
        waiting.detail = dialog_detail(&d);
        waiting.pending = Vec::new();
        waiting.prompt = None;
        Some(waiting)
    }

    /// respondDialog answers the dialog info describes: approve presses the
    /// key that picks its highlighted option, deny the one that closes it. As
    /// for a permission prompt, the dialog must still be on screen right before
    /// the keypress. Held messages are typed once the agent is back at its
    /// prompt.
    pub(crate) fn respond_dialog(
        &self,
        agent: &Agent,
        info: StatusInfo,
        resp: PermissionResponse,
    ) -> Result<StatusInfo> {
        let provider = self.provider(agent).map_err(|_| unloaded_backend(agent))?;
        let Some(dr) = provider.as_dialog_reader() else {
            bail!(
                "agent {} runs backend {}, which can't answer a dialog: use agent keys {}@{} to press the keys yourself",
                agent.name,
                strconv::quote(&agent.backend),
                agent.name,
                self.cfg.host
            );
        };
        let key = dr
            .dialog_key(resp)
            .map_err(|e| anyhow!("agent {}: {e:#}", agent.name))?;
        let screen = self
            .tmux
            .capture(&agent.session_name, 0)
            .context("capturing screen")?;
        let Some(d) = dialog_in(provider.as_ref(), dr, &screen) else {
            bail!(
                "agent {} has no dialog on screen; it may have just been answered (agent status {} -v shows what it's doing)",
                agent.name,
                agent.name
            );
        };
        let title = dialog_title(&info.status, &info.detail).unwrap_or_default();
        if d.title != title {
            bail!(
                "agent {} shows another dialog ({}) than the one its status described ({}): check it with agent status {} -v before answering it",
                agent.name,
                one_line(&d.title),
                one_line(&title),
                agent.name
            );
        }
        self.tmux.send_special_key(&agent.session_name, &key)?;
        if self.held_count(&agent.name) > 0 {
            let _ = self.start_deliverer(&agent.name); // one already waiting keeps its lock
        }
        Ok(info)
    }
}

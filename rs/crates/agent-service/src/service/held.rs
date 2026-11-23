//! Go `internal/service/held.go`.
//!
//! Held messages: a message sent to a busy agent whose backend can take text
//! from a hook (backend.HookDeliverer) is stored as a pending row in the
//! messages table instead of being typed into its input box mid-turn. Who
//! delivers it:
//!
//!   - the hook at the next tool-batch/step boundary or turn end (Claude's
//!     PostToolBatch and Stop, agy's PostInvocation): it claims held messages
//!     and prints them coalesced as its output, and the agent goes on with
//!     them (report_delivering);
//!   - send itself, if that hook ran between send's status check and the
//!     store (hold);
//!   - the deliverer, `agent deliver --wait`, which send starts for turns
//!     that end with no hook at all (Esc, a deny, a failed turn, or Claude's
//!     PreToolUse interrupt stop): it types the held messages once the agent
//!     has sat at its prompt for a while, or immediately for an interrupt
//!     (deliver_held);
//!   - the next send that finds the agent at its prompt (or `send --now` when
//!     the input line is empty), which types the held messages ahead of its own;
//!   - `agent watch`, if one runs, coalescing all pending messages per idle tick.
//!
//! Each claims the rows atomically before delivering, so a message is
//! delivered once.

use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use agent_backend::{CoalescedMessage, HookDeliverer, format_coalesced_batch};
use agent_base::gostd::filepath;
use agent_store::{
    Agent, ClaimFilter, MESSAGE_STATUS_DELIVERED, MESSAGE_STATUS_FAILED, MESSAGE_STATUS_PENDING,
    MESSAGE_STATUS_SUPERSEDED, Message, StateUpdate, Time, URGENCY_INTERRUPT, URGENCY_LATER,
    URGENCY_NOW, URGENCY_SOON,
};

use super::agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AGENT_STATUS_PROCESSING,
    AGENT_STATUS_UNKNOWN, AGENT_STATUS_WAITING, AgentService, AgentStatus, exited_err,
};
use super::ctx::{Ctx, CtxExt};
use super::handover::HandoverError;
use super::status::{
    STATE_KEY_DETAIL, STATE_KEY_HOOKED_SESSION, STATE_KEY_NOW_AT, STATE_KEY_SESSION_ID,
    STATE_KEY_SPAWN_ID, StatusInfo, StatusReport,
};
use super::submit::UnsubmittedError;

/// EventDelivered is the event recorded when a hook hands an agent its held
/// messages: it goes on with them, so the turn isn't over.
pub const EVENT_DELIVERED: &str = "delivered";

/// SendOptions changes how send delivers.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    /// Now types the message even while the agent is busy (Claude takes it
    /// at its next tool boundary; agy with `queuedMessages: "send-immediately"`
    /// takes it at its next step), sweeping and coalescing held messages first
    /// when the input box is empty.
    pub now: bool,
    /// From is the sender recorded with a held message: an agent's name
    /// (or `name@host` across hosts), "user", or "remote".
    pub from: String,
    /// Delivery urgency: "" / "soon" (default), "later", "now", or "interrupt".
    pub urgency: String,
    /// Replace any earlier pending held messages from the same sender
    /// (scoped to `topic` when non-empty) with this message.
    pub replace: bool,
    /// Explicitly supersede pending message ID (non-zero). If it was
    /// already delivered, this message is still sent and `already_delivered_id`
    /// is set on SendResult.
    pub supersede: i64,
    /// Optional coalescing topic/key (e.g. "pr-42"). Automatically supersedes
    /// earlier pending messages from the same sender with the same topic.
    pub topic: String,
}

impl SendOptions {
    /// Canonical urgency level (`"later"`, `"soon"`, `"now"`, or `"interrupt"`).
    pub fn effective_urgency(&self) -> &'static str {
        if self.urgency == URGENCY_INTERRUPT {
            URGENCY_INTERRUPT
        } else if self.now || self.urgency == URGENCY_NOW {
            URGENCY_NOW
        } else if self.urgency == URGENCY_LATER {
            URGENCY_LATER
        } else {
            URGENCY_SOON
        }
    }
}

/// SendResult says what send did with a message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SendResult {
    /// what send found the agent doing (unknown if it couldn't tell)
    pub status: AgentStatus,
    /// the status detail, e.g. the prompt a waiting agent is at
    pub detail: String,
    /// Held: stored, to be delivered when the agent's step/turn ends.
    /// held_count is how many active messages are held for it now.
    pub held: bool,
    pub held_count: usize,
    /// when the hook that delivers it runs, e.g. "its next tool batch or turn ends"
    pub held_until: String,
    /// InputBlocked, for a message held although the agent was at its
    /// prompt, says why: its input line isn't empty (see InputBlockedError).
    pub input_blocked: String,
    /// Typed is how many active messages were typed in: this one, after any
    /// held ones it overtook (oldest first, as one prompt).
    pub typed: usize,
    /// Unchecked, for messages typed into a CLI whose Enter can turn into a
    /// newline (Claude), says why send couldn't check they were submitted,
    /// e.g. its input box isn't on screen (see submit.rs).
    pub unchecked: String,
    /// Stored message id in SQLite.
    pub message_id: i64,
    /// Pending message IDs marked superseded by this send.
    pub superseded_ids: Vec<i64>,
    /// Non-zero when `--supersede` or `--replace` missed a message that was
    /// already delivered.
    pub already_delivered_id: i64,
    /// Delivery channel that delivered `already_delivered_id`, if known.
    pub already_delivered_via: String,
    /// True when the send armed or triggered an interrupt.
    pub interrupted: bool,
}

/// Receipt for a previously sent message (`agent status <name> --receipt <id>`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MessageReceipt {
    pub id: i64,
    pub sender_id: String,
    pub receiver_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub urgency: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub topic: String,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub superseded_by: i64,
    pub created_at: Time,
    #[serde(skip_serializing_if = "agent_store::gotime::is_zero")]
    pub delivered_at: Time,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub delivery_via: String,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub delivery_batch: i64,
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

/// turnOver reports whether the last report says the agent's turn is over
/// and its prompt takes input: idle, error, or a Stop that left background
/// work running (native status says busy until that work ends).
pub(crate) fn turn_over(reported: Option<&StatusInfo>) -> bool {
    let Some(reported) = reported else {
        return false;
    };
    if reported.status == AGENT_STATUS_IDLE || reported.status == AGENT_STATUS_ERROR {
        return true;
    }
    if reported.status == AGENT_STATUS_PROCESSING {
        return reported.event == "Stop";
    }
    false
}

/// heldUntilDialog is when a message held for an agent at a dialog (see
/// dialog.rs) is delivered.
const HELD_UNTIL_DIALOG: &str = "its dialog is gone and it has been at its prompt for 10s";

/// maxHeldCount bounds how many held messages are counted.
pub(crate) const MAX_HELD_COUNT: i64 = 1000;

/// Fast poll interval used by `deliver_held` when an interrupt message is waiting.
const INTERRUPT_DELIVER_POLL: Duration = Duration::from_millis(200);

/// InputBlockedError is type_held's error when the agent's input line isn't
/// known to be empty: typed messages would mix into what a person is typing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "agent {agent}'s input line isn't empty ({why}), so messages typed now would mix into what a person is typing there: held messages are typed once it's empty (agent send --now types anyway)"
)]
pub struct InputBlockedError {
    pub agent: String,
    /// e.g. "it has text in it"
    pub why: String,
}

/// DeliverHeld's error when no messages are held (Go's ErrNothingHeld):
/// check with `downcast_ref::<ErrNothingHeld>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no messages held")]
pub struct ErrNothingHeld;

/// DeliverOptions tunes deliver_held.
#[derive(Debug, Clone, Default)]
pub struct DeliverOptions {
    /// Wait keeps checking, every interval, while messages are held;
    /// otherwise deliver_held checks once.
    pub wait: bool,
    pub interval: Duration,
    /// Grace is how long the agent must be seen at its prompt before held
    /// messages are typed: a person who just pressed Esc gets that long to
    /// type something themselves.
    pub grace: Duration,
}

/// DeliverResult says what deliver_now did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeliverResult {
    /// messages typed in
    pub typed: usize,
    /// messages still held
    pub held: usize,
    /// Status is what the agent was doing, when messages are still held.
    pub status: AgentStatus,
}

/// plural formats a count with a noun, adding "s" unless n is 1.
pub(crate) fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        return format!("1 {noun}");
    }
    format!("{n} {noun}s")
}

/// Maps claimed store messages into backend coalescing inputs.
pub(crate) fn to_coalesced(msgs: &[Message], now: &Time) -> Vec<CoalescedMessage> {
    msgs.iter()
        .map(|m| {
            let age_secs = (now.0.timestamp() - m.created_at.0.timestamp()).max(0);
            CoalescedMessage {
                id: m.id,
                sender: m.sender_id.clone(),
                content: m.content.clone(),
                age_secs,
                urgency: m.urgency.clone(),
                superseded_by: m.superseded_by,
                topic: m.topic.clone(),
            }
        })
        .collect()
}

fn rfc3339_now() -> String {
    Time::now().format_rfc3339()
}

impl AgentService {
    /// holdsMessages reports whether messages to agent can be held for a hook:
    /// its backend takes text from a hook, and spawn wired its hooks to this
    /// binary (only spawn sets _spawn_id).
    pub(crate) fn holds_messages(&self, agent: &Agent) -> bool {
        let Ok(p) = self.provider(agent) else {
            return false;
        };
        if p.as_hook_deliverer().is_none() {
            return false;
        }
        // Hooks wired at spawn (agy; Claude agents spawned before the global
        // hook) report from the start.
        if !self.state_value(&agent.name, STATE_KEY_SPAWN_ID).is_empty() {
            return true;
        }
        // A CLI revive took over obeys the same rules (review-3, 2.1): held
        // while busy or at a dialog, never typed ahead of what is held. With
        // no hook of its own to hand them over, the deliverer types them once
        // it is at its prompt (typed_by_deliverer).
        if !self
            .state_value(&agent.name, super::revive::STATE_KEY_REVIVED_CLI)
            .is_empty()
        {
            return true;
        }
        // The global hook reaches the CLI only if it started with the hook
        // installed: seen, when the hook has reported the current session.
        let hooked = self.state_value(&agent.name, STATE_KEY_HOOKED_SESSION);
        !hooked.is_empty() && hooked == self.state_value(&agent.name, STATE_KEY_SESSION_ID)
    }

    /// HoldsMessages reports whether a message to name that finds it busy is
    /// held rather than typed (see send).
    pub fn holds_messages_named(&self, name: &str) -> bool {
        self.get_agent(name).is_ok_and(|a| self.holds_messages(&a))
    }

    /// DeliveryPoint says when the hook that hands name its held messages runs,
    /// e.g. "its next tool batch or turn ends" (see backend.HookDeliverer).
    pub fn delivery_point(&self, name: &str) -> String {
        match self.get_agent(name) {
            Ok(agent) => self.delivery_point_of(&agent),
            Err(_) => "its turn ends".to_string(),
        }
    }

    pub(crate) fn delivery_point_of(&self, agent: &Agent) -> String {
        self.delivery_point_for_urgency(agent, URGENCY_SOON)
    }

    pub(crate) fn delivery_point_for_urgency(&self, agent: &Agent, urgency: &str) -> String {
        if self.typed_by_deliverer(agent) {
            return super::revive::HELD_UNTIL_PROMPT.to_string();
        }
        if urgency == URGENCY_INTERRUPT {
            return "its next tool call or step (interrupt)".to_string();
        }
        if urgency == URGENCY_LATER {
            return "its turn ends".to_string();
        }
        if let Ok(p) = self.provider(agent)
            && let Some(hd) = p.as_hook_deliverer()
        {
            return hd.delivery_point();
        }
        "its turn ends".to_string()
    }

    /// Inserts a pending message in SQLite, atomically applying `--replace` /
    /// `--supersede` / `--key`.
    pub(crate) fn insert_pending_message(
        &self,
        receiver_id: &str,
        message: &str,
        opts: &SendOptions,
        urgency: &str,
    ) -> Result<(i64, Vec<i64>, Option<Message>)> {
        let from = if opts.from.is_empty() {
            "user"
        } else {
            opts.from.as_str()
        };
        let mut msg = Message {
            sender_id: from.to_string(),
            receiver_id: receiver_id.to_string(),
            content: message.to_string(),
            status: MESSAGE_STATUS_PENDING,
            created_at: Time::now(),
            urgency: urgency.to_string(),
            topic: opts.topic.clone(),
            ..Default::default()
        };
        let (superseded_ids, already_delivered) = self
            .storage
            .create_message_replacing(&mut msg, opts.replace, opts.supersede)
            .context("storing message")?;
        Ok((msg.id, superseded_ids, already_delivered))
    }

    /// Records a message that was typed directly onto a non-hook backend so
    /// receipts and `--supersede` / `--replace` still work consistently.
    pub(crate) fn record_direct_delivery(
        &self,
        receiver_id: &str,
        message: &str,
        opts: &SendOptions,
        via: &str,
    ) -> Result<(i64, Vec<i64>, Option<Message>)> {
        let (id, sup, prev) =
            self.insert_pending_message(receiver_id, message, opts, opts.effective_urgency())?;
        let _ = self.storage.claim_single_message_batch(id, via)?;
        Ok((id, sup, prev))
    }

    /// `send --now`: inserts into SQLite first (recording a real `message_id`
    /// and applying `--replace` / `--supersede`), claims and coalesces all
    /// pending rows ahead of `--now` when the input box is empty (so held
    /// messages never arrive out-of-order after `--now`), or claims only this
    /// message when a human has draft text in the input box so held messages
    /// keep waiting for the line to clear.
    pub(crate) fn send_now(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        message: &str,
        opts: &SendOptions,
    ) -> Result<SendResult> {
        let status = self.check_can_send(agent)?;
        let mut res = SendResult {
            status,
            ..Default::default()
        };
        let (msg_id, superseded_ids, already_delivered) =
            self.insert_pending_message(&agent.name, message, opts, URGENCY_NOW)?;
        res.message_id = msg_id;
        res.superseded_ids = superseded_ids;
        if let Some(prev) = already_delivered {
            res.already_delivered_id = prev.id;
            res.already_delivered_via = prev.delivery_via;
        }

        let sweep_all = self.holds_messages(agent) && self.input_blocked_of(agent).is_empty();
        let claimed = if sweep_all {
            self.storage
                .claim_pending_batch_matching(&agent.name, ClaimFilter::All, "now", None)?
        } else {
            self.storage.claim_single_message_batch(msg_id, "now")?
        };
        let active_count = claimed
            .iter()
            .filter(|m| m.status != MESSAGE_STATUS_SUPERSEDED)
            .count();
        let text = if claimed.is_empty() {
            message.to_string()
        } else {
            let now = Time::now();
            format_coalesced_batch(&to_coalesced(&claimed, &now), false)
        };
        let _ = self.set_state(
            &agent.name,
            STATE_KEY_NOW_AT,
            &Time::now().format_rfc3339_nano(),
        );
        match self.send_message(ctx, &agent.name, &text, true) {
            Ok(unchecked) => {
                res.typed = active_count.max(1);
                res.unchecked = unchecked;
                Ok(res)
            }
            Err(err) => {
                if err.downcast_ref::<UnsubmittedError>().is_none()
                    && err.downcast_ref::<HandoverError>().is_none()
                {
                    let _ = self.storage.delete_state_key(&agent.name, STATE_KEY_NOW_AT);
                    let _ = self
                        .storage
                        .abort_failed_send(msg_id, &res.superseded_ids, &claimed);
                }
                Err(err)
            }
        }
    }

    /// sendHeld is send for an agent that holds messages: it holds the message
    /// while the agent is mid-turn or at a prompt, and otherwise records and
    /// types it, coalesced after any messages still held.
    pub(crate) fn send_held(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        message: &str,
        opts: &SendOptions,
    ) -> Result<SendResult> {
        let urgency = opts.effective_urgency();
        let mut res = SendResult {
            status: AGENT_STATUS_UNKNOWN,
            interrupted: urgency == URGENCY_INTERRUPT,
            ..Default::default()
        };
        let mut dialog = false;
        if let Ok(info) = self.status_info(&agent.name) {
            dialog = info.is_dialog();
            res.status = info.status;
            res.detail = info.detail;
        }
        if res.status == AGENT_STATUS_ENDED {
            return Err(exited_err(agent));
        }
        if dialog {
            // No turn runs, so no hook will deliver it: the deliverer types
            // it once the dialog is gone (never touch the dialog).
            let mut res = self.hold(ctx, agent, res, message, opts)?;
            if res.held {
                res.held_until = HELD_UNTIL_DIALOG.to_string();
            }
            return Ok(res);
        }
        if res.status == AGENT_STATUS_PROCESSING || res.status == AGENT_STATUS_WAITING {
            // Waiting too: a held message never goes near the dialog, and once
            // the prompt is answered, the turn's next boundary delivers it.
            return self.hold(ctx, agent, res, message, opts);
        }

        // At its prompt (idle, error, or unknown): verify no dialog is on
        // screen before recording in SQLite so refused sends leave no pending row.
        self.check_screen(agent)?;
        let (msg_id, superseded_ids, already_delivered) =
            self.insert_pending_message(&agent.name, message, opts, urgency)?;
        res.message_id = msg_id;
        res.superseded_ids = superseded_ids;
        if let Some(prev) = already_delivered {
            res.already_delivered_id = prev.id;
            res.already_delivered_via = prev.delivery_via;
        }

        let (n, unchecked, err) = self.type_held(ctx, agent, "");
        if let Err(err) = err {
            if let Some(blocked) = err.downcast_ref::<InputBlockedError>() {
                // A person may be typing: `msg_id` is already pending in SQLite
                // behind any earlier held messages, for the deliverer to type
                // once the input line is empty.
                self.mark_sent(&agent.name);
                res.held = true;
                res.input_blocked = blocked.why.clone();
                res.held_until = self.delivery_point_for_urgency(agent, urgency);
                res.held_count = self.held_count(&agent.name).max(1);
                let _ = self.start_deliverer(&agent.name);
                return Ok(res);
            }
            if err.downcast_ref::<UnsubmittedError>().is_none()
                && err.downcast_ref::<HandoverError>().is_none()
            {
                let _ = self
                    .storage
                    .abort_failed_send(msg_id, &res.superseded_ids, &[]);
            }
            return Err(err);
        }
        res.typed = n;
        res.unchecked = unchecked;
        Ok(res)
    }

    /// hold stores message for delivery at the next hook boundary (or end of
    /// turn), unless the last report says the turn is already over.
    fn hold(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        mut res: SendResult,
        message: &str,
        opts: &SendOptions,
    ) -> Result<SendResult> {
        let urgency = opts.effective_urgency();
        let (msg_id, superseded_ids, already_delivered) =
            self.insert_pending_message(&agent.name, message, opts, urgency)?;
        res.message_id = msg_id;
        res.superseded_ids = superseded_ids;
        if let Some(prev) = already_delivered {
            res.already_delivered_id = prev.id;
            res.already_delivered_via = prev.delivery_via;
        }
        // For wait --next: the turn that takes this message starts later.
        self.mark_sent(&agent.name);

        // The turn may be over although status said busy: its Stop may have
        // run since the status check (or native status lags it), finding
        // nothing held, or left background work running. That hook claims and
        // reports in one transaction, so if it missed this message, its report
        // is visible now: type the message.
        if let Ok(reported) = self.reported_status(&agent.name)
            && turn_over(reported.as_ref())
        {
            match self.type_held(ctx, agent, "") {
                (n, unchecked, Ok(())) if n > 0 => {
                    res.typed = n;
                    res.unchecked = unchecked;
                    return Ok(res);
                }
                (_, _, Err(err)) if err.downcast_ref::<UnsubmittedError>().is_some() => {
                    return Err(err);
                }
                _ => {}
            }
        }

        res.held = true;
        res.held_until = self.delivery_point_for_urgency(agent, urgency);
        res.held_count = 1;
        if let Ok(held) = self
            .storage
            .get_pending_messages(&agent.name, MAX_HELD_COUNT)
            && !held.is_empty()
        {
            res.held_count = held.len();
        }
        let _ = self.start_deliverer(&agent.name); // best effort: the turn's hook delivers either way
        Ok(res)
    }

    /// HeldCount returns how many active messages are held for name.
    pub fn held_count(&self, name: &str) -> usize {
        self.storage
            .get_pending_messages(name, MAX_HELD_COUNT)
            .map(|h| h.len())
            .unwrap_or(0)
    }

    /// Returns whether `name` has at least one pending message with `urgency = 'later'`.
    pub(crate) fn has_pending_later(&self, name: &str) -> bool {
        self.storage
            .get_pending_messages(name, MAX_HELD_COUNT)
            .is_ok_and(|msgs| msgs.iter().any(|m| m.urgency == URGENCY_LATER))
    }

    /// Checks that `from` is allowed to interrupt `target`: a person ("",
    /// "user"), or an agent in `target`'s host-aware `spawned_by` ancestor chain.
    pub(crate) fn check_can_interrupt(&self, target: &Agent, from: &str) -> Result<()> {
        if from.is_empty() || from == "user" {
            return Ok(());
        }
        let my_host = self.cfg.host.as_str();
        let (caller_name, caller_host) = split_agent_host(from, my_host);
        if caller_name == target.name && caller_host.eq_ignore_ascii_case(my_host) {
            anyhow::bail!("agent {from} cannot interrupt itself");
        }
        let mut cur = target.spawned_by.clone();
        for _ in 0..64 {
            if cur.is_empty() {
                break;
            }
            let (parent_name, parent_host) = split_agent_host(&cur, my_host);
            if parent_name == caller_name && parent_host.eq_ignore_ascii_case(caller_host) {
                return Ok(());
            }
            // Only walk further in this host's store when the parent lives on
            // this host: a local agent sharing a bare name with a remote parent
            // (`lead` here vs `lead@other-host`) is a different agent.
            if !parent_host.eq_ignore_ascii_case(my_host) {
                break;
            }
            let Ok(Some(parent)) = self.storage.get_agent(parent_name) else {
                break;
            };
            cur = parent.spawned_by;
        }
        anyhow::bail!(
            "agent {from} cannot interrupt {}: only a person or an ancestor in {}'s spawned_by chain may interrupt",
            target.name,
            target.name
        )
    }

    /// Looks up a stored message's delivery/superseding receipt for `receiver`
    /// ("" matches any receiver).
    pub fn message_receipt(&self, receiver: &str, id: i64) -> Result<MessageReceipt> {
        let Some(msg) = self.storage.get_message(id)? else {
            anyhow::bail!("no message #{id} on record");
        };
        if !receiver.is_empty() && msg.receiver_id != receiver {
            anyhow::bail!(
                "message #{id} was addressed to {}, not {receiver}",
                msg.receiver_id
            );
        }
        let mut status = msg.status.to_string();
        if msg.status == MESSAGE_STATUS_PENDING
            && let Ok(info) = self.status_info(&msg.receiver_id)
            && info.status == AGENT_STATUS_ENDED
        {
            status = MESSAGE_STATUS_FAILED.to_string();
        }
        Ok(MessageReceipt {
            id: msg.id,
            sender_id: msg.sender_id,
            receiver_id: msg.receiver_id,
            status,
            urgency: msg.urgency,
            topic: msg.topic,
            superseded_by: msg.superseded_by,
            created_at: msg.created_at,
            delivered_at: msg.delivered_at,
            delivery_via: msg.delivery_via,
            delivery_batch: msg.delivery_batch,
        })
    }

    /// inputBlocked returns why typing into the agent would land in a person's
    /// input, or behind a message Enter failed to submit: "" if its input line
    /// is empty, or the backend can't read it.
    pub(crate) fn input_blocked_of(&self, agent: &Agent) -> String {
        let Ok(p) = self.provider(agent) else {
            return String::new();
        };
        let Some(ir) = p.as_input_reader() else {
            return String::new();
        };
        let Ok(screen) = self.tmux.capture_styled(&agent.session_name, 0) else {
            return String::new(); // send_unchecked reports a missing session
        };
        match ir.input_text(&screen) {
            // Something covers it: a dialog or notice no status reads (agy's
            // feedback survey, say, friction #44), which keys typed now would
            // answer.
            None => format!(
                "it isn't on screen: something covers it, which agent capture {} shows",
                agent.name
            ),
            Some(text) if self.unsubmitted_text(agent, &text) => format!(
                "a message typed into it wasn't submitted: agent key {} Enter submits it",
                agent.session_name
            ),
            Some(text) if !text.is_empty() => "it has text in it".to_string(),
            Some(_) => String::new(),
        }
    }

    /// InputBlocked returns why held messages can't be typed into name now
    /// because of its input line, or "" if they can (see InputBlockedError).
    pub fn input_blocked(&self, name: &str) -> String {
        match self.get_agent(name) {
            Ok(agent) => self.input_blocked_of(&agent),
            Err(_) => String::new(),
        }
    }

    /// typeHeld types the agent's held messages, oldest first, then extra if
    /// set (e.g. `revive`'s re-brief), as one coalesced prompt. It claims them
    /// first, so the agent's hook (or another sender) can't deliver them too,
    /// and requeues them if typing fails.
    pub(crate) fn type_held(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        extra: &str,
    ) -> (usize, String, Result<()>) {
        if let Err(e) = self.check_screen(agent) {
            return (0, String::new(), Err(e));
        }
        if let Ok(held) = self.storage.get_pending_messages(&agent.name, 1)
            && held.is_empty()
            && extra.is_empty()
        {
            return (0, String::new(), Ok(()));
        }
        let why = self.input_blocked_of(agent);
        if !why.is_empty() {
            return (
                0,
                String::new(),
                Err(InputBlockedError {
                    agent: agent.name.clone(),
                    why,
                }
                .into()),
            );
        }
        let mut extra_row: Option<Message> = None;
        if !extra.is_empty() {
            let mut m = Message {
                sender_id: "user".to_string(),
                receiver_id: agent.name.clone(),
                content: extra.to_string(),
                status: MESSAGE_STATUS_PENDING,
                created_at: Time::now(),
                urgency: URGENCY_SOON.to_string(),
                ..Default::default()
            };
            if let Err(e) = self.storage.create_message(&mut m) {
                return (0, String::new(), Err(e));
            }
            extra_row = Some(m);
        }
        let claimed = match self.storage.claim_pending_batch(&agent.name, "typed", None) {
            Ok(c) => c,
            Err(e) => return (0, String::new(), Err(e)),
        };
        let active_count = claimed
            .iter()
            .filter(|m| m.status != MESSAGE_STATUS_SUPERSEDED)
            .count();
        if active_count == 0 {
            return (0, String::new(), Ok(()));
        }
        let now = Time::now();
        let batch = to_coalesced(&claimed, &now);
        let text = format_coalesced_batch(&batch, false);
        match self.send_message(ctx, &agent.name, &text, true) {
            Ok(unchecked) => (active_count, unchecked, Ok(())),
            Err(err) => {
                if err.downcast_ref::<UnsubmittedError>().is_some() {
                    return (active_count, String::new(), Err(err)); // typed: they sit in its input box
                }
                if err.downcast_ref::<HandoverError>().is_some() {
                    // Typed as a short line the agent took, and kept in the failed
                    // message: holding them again would have the deliverer type
                    // that line over and over.
                    return (active_count, String::new(), Err(err));
                }
                let to_requeue: Vec<Message> = match &extra_row {
                    Some(ex) => {
                        let _ = self
                            .storage
                            .update_message_status(ex.id, MESSAGE_STATUS_FAILED);
                        claimed.into_iter().filter(|m| m.id != ex.id).collect()
                    }
                    None => claimed,
                };
                let _ = self.storage.requeue_batch(&to_requeue);
                (0, String::new(), Err(err))
            }
        }
    }

    pub(crate) fn report_delivering(
        &self,
        name: &str,
        r: &StatusReport,
        hd: &dyn HookDeliverer,
        delivery_only: bool,
    ) -> Result<Option<Vec<u8>>> {
        let filter = if r.event == "PostToolBatch"
            || r.event == "PostInvocation"
            || r.event == "PreInvocation"
        {
            ClaimFilter::NonLater
        } else {
            ClaimFilter::All
        };
        self.report_delivering_filtered(name, r, hd, delivery_only, filter)
    }

    pub(crate) fn report_delivering_filtered(
        &self,
        name: &str,
        r: &StatusReport,
        hd: &dyn HookDeliverer,
        delivery_only: bool,
        filter: ClaimFilter,
    ) -> Result<Option<Vec<u8>>> {
        let now = Time::now();
        let ended = self.report_update(name, r, &now)?;
        let mut delivering = r.clone();
        delivering.status = AGENT_STATUS_PROCESSING;
        delivering.event = EVENT_DELIVERED.to_string();
        delivering.result = String::new();
        delivering.clear_result = true;
        let going = self.report_update(name, &delivering, &now)?;

        let mut detail = String::new();
        let mut update = |claimed: &[Message]| -> StateUpdate {
            let active = claimed
                .iter()
                .filter(|m| m.status != MESSAGE_STATUS_SUPERSEDED)
                .count();
            let superseded = claimed.len() - active;
            if active == 0 {
                if delivery_only {
                    return StateUpdate::default();
                }
                return ended.clone();
            }
            detail = if superseded > 0 {
                format!(
                    "{} delivered ({superseded} superseded)",
                    plural(active, "held message")
                )
            } else {
                format!("{} delivered", plural(active, "held message"))
            };
            let mut going = going.clone();
            for e in &mut going.entries {
                if e.key == STATE_KEY_DETAIL {
                    e.value = detail.clone();
                }
            }
            if let Some(ev) = &mut going.event {
                ev.detail = detail.clone();
            }
            going
        };
        let claimed =
            self.storage
                .claim_pending_batch_matching(name, filter, &r.event, Some(&mut update))?;
        if claimed.is_empty() {
            if !delivery_only {
                self.publish_status(name, r, &now);
            }
            return Ok(None);
        }
        let _ = MESSAGE_STATUS_DELIVERED;
        delivering.detail = detail;
        delivering.delivered_ids = claimed
            .iter()
            .filter(|m| m.status != MESSAGE_STATUS_SUPERSEDED)
            .map(|m| m.id)
            .collect();
        delivering.superseded_ids = claimed
            .iter()
            .filter(|m| m.status == MESSAGE_STATUS_SUPERSEDED)
            .map(|m| m.id)
            .collect();
        self.publish_status(name, &delivering, &now);
        let batch = to_coalesced(&claimed, &now);
        let out = hd.delivery_output_for(&r.event, &batch);
        if out.is_empty() {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// DeliverHeld types the messages held for name once the agent is at its
    /// prompt with them still held, which happens when its turn ended without a
    /// hook that delivers them: Esc, a deny, a failed turn, or Claude's
    /// PreToolUse interrupt stop.
    pub fn deliver_held(
        &self,
        ctx: &Ctx,
        name: &str,
        opts: &DeliverOptions,
    ) -> (usize, Result<()>) {
        let mut typed = 0;
        let mut at_prompt_since: Option<Instant> = None;
        let mut first = true;
        loop {
            let agent = match self.get_agent(name) {
                Ok(a) => a,
                Err(e) => return (typed, Err(e)),
            };
            let held = match self.storage.get_pending_messages(name, MAX_HELD_COUNT) {
                Ok(h) => h,
                Err(e) => return (typed, Err(e)),
            };
            if held.is_empty() {
                if first {
                    return (0, Err(ErrNothingHeld.into()));
                }
                return (typed, Ok(()));
            }
            first = false;
            let urgent = held.iter().any(|m| m.urgency == URGENCY_INTERRUPT);
            let info = match self.status_info(name) {
                Ok(i) => i,
                Err(e) => return (typed, Err(e)),
            };
            let reported = self.reported_status(name).ok().flatten();
            if info.status == AGENT_STATUS_ENDED {
                return (
                    typed,
                    Err(anyhow!(
                        "agent {name}'s session has ended with {} held for it, which won't be delivered: agent logs {name} shows how it ended",
                        plural(held.len(), "message")
                    )),
                );
            } else if info.status == AGENT_STATUS_IDLE
                || info.status == AGENT_STATUS_ERROR
                || (info.status == AGENT_STATUS_PROCESSING && turn_over(reported.as_ref()))
            {
                if opts.wait && !self.input_blocked_of(&agent).is_empty() {
                    // A person is typing: the grace starts once the line is
                    // empty, so a pause in their typing isn't typed over.
                    at_prompt_since = None;
                } else {
                    let since = *at_prompt_since.get_or_insert_with(Instant::now);
                    let grace = if urgent { Duration::ZERO } else { opts.grace };
                    if !opts.wait || since.elapsed() >= grace {
                        let (n, unchecked, err) = self.type_held(ctx, &agent, "");
                        let err = err.err();
                        typed += n;
                        if !unchecked.is_empty() {
                            eprintln!(
                                "{}: typed {name}'s held messages without checking they were submitted: {unchecked}",
                                rfc3339_now()
                            );
                        }
                        match err {
                            Some(err) if !opts.wait => return (typed, Err(err)),
                            Some(err) if err.downcast_ref::<InputBlockedError>().is_some() => {
                                at_prompt_since = None; // typed into since the check above
                            }
                            Some(err) => {
                                eprintln!(
                                    "{}: typing {name}'s held messages: {err:#}",
                                    rfc3339_now()
                                );
                            }
                            None => {}
                        }
                    }
                }
            } else {
                at_prompt_since = None;
            }
            if !opts.wait {
                return (typed, Ok(()));
            }
            let poll = if urgent && opts.interval > INTERRUPT_DELIVER_POLL {
                INTERRUPT_DELIVER_POLL
            } else {
                opts.interval
            };
            if let Err(e) = ctx.sleep(poll) {
                return (typed, Err(e.into()));
            }
        }
    }

    /// DeliverNow types the messages held for name if the agent is at its prompt
    /// with an empty input line (agent deliver, here or from another host): one
    /// check of deliver_held, and how many are still held, and why.
    pub fn deliver_now(&self, ctx: &Ctx, name: &str) -> Result<DeliverResult> {
        let (n, err) = self.deliver_held(ctx, name, &DeliverOptions::default());
        err?;
        let mut res = DeliverResult {
            typed: n,
            ..Default::default()
        };
        res.held = self.held_count(name);
        if res.held > 0 {
            res.status = AGENT_STATUS_UNKNOWN;
            if let Ok(info) = self.status_info(name) {
                res.status = info.status;
            }
        }
        Ok(res)
    }

    /// SetDelivererStarter replaces how a deliverer is started (tests).
    pub fn set_deliverer_starter(
        &mut self,
        start: impl Fn(&str) -> Result<()> + Send + Sync + 'static,
    ) {
        self.deliverer = Some(Box::new(start));
    }

    /// startDeliverer starts `agent deliver --wait <name>` detached from this
    /// process (its own session, so it outlives a Bash tool call), logging to the
    /// agent's directory. One that finds another holding the agent's lock exits
    /// at once.
    pub(crate) fn start_deliverer(&self, name: &str) -> Result<()> {
        if let Some(d) = &self.deliverer {
            return d(name);
        }
        if self.cfg.agent_binary.is_empty() {
            anyhow::bail!("no agent binary to start a deliverer with");
        }
        // An adopted agent has no directory of its own until something needs
        // one: without it the log couldn't open and its held messages would
        // never be typed in.
        let agent_dir = self.cfg.agent_dir(name);
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&agent_dir)
                .map_err(|e| {
                    anyhow!(agent_base::gostd::os::PathError::new(
                        "mkdir", &agent_dir, e
                    ))
                })
                .context("opening deliverer log")?;
        }
        let log_path = filepath::join(&[agent_dir.as_str(), "deliver.log"]);
        let log_file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .open(&log_path)
                .map_err(|e| anyhow!(agent_base::gostd::os::PathError::new("open", &log_path, e)))
                .context("opening deliverer log")?
        };
        let err_file = log_file.try_clone().context("opening deliverer log")?;
        let mut cmd = std::process::Command::new(&self.cfg.agent_binary);
        cmd.args(["deliver", "--wait", "-q", name])
            .stdin(std::process::Stdio::null())
            .stdout(log_file)
            .stderr(err_file);
        // SAFETY: setsid is async-signal-safe; it puts the child in its own
        // session, as Go's SysProcAttr{Setsid: true}.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("starting deliverer")?;
        // Released, as Go's Process.Release: nothing waits for it, and a
        // thread reaps it so it leaves no zombie while this process lives.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        Ok(())
    }

    /// DelivererLockPath is the file a running deliverer for name holds locked.
    pub fn deliverer_lock_path(&self, name: &str) -> String {
        filepath::join(&[self.cfg.agent_dir(name).as_str(), "deliver.lock"])
    }
}

fn split_agent_host<'a>(s: &'a str, default_host: &'a str) -> (&'a str, &'a str) {
    match s.split_once('@') {
        Some((name, host)) if !host.is_empty() => (name, host),
        Some((name, _)) => (name, default_host),
        None => (s, default_host),
    }
}

//! Go `internal/service/held.go`.
//!
//! Held messages: a message sent to a busy agent whose backend can take text
//! from a hook (backend.HookDeliverer) is stored as a pending row in the
//! messages table instead of being typed into its input box mid-turn. Who
//! delivers it:
//!
//!   - the hook that ends the turn (Claude's Stop): it claims every held
//!     message and prints them as its output, and the agent goes on with
//!     them (report_delivering);
//!   - send itself, if that hook ran between send's status check and the
//!     store (hold);
//!   - the deliverer, `agent deliver --wait`, which send starts for turns
//!     that end with no hook at all (Esc, a deny, a failed turn): it types
//!     the held messages once the agent has sat at its prompt for a while
//!     (deliver_held);
//!   - the next send that finds the agent at its prompt, which types the
//!     held messages ahead of its own;
//!   - `agent watch`, if one runs, as for any queued message.
//!
//! Each claims the rows atomically before delivering, so a message is
//! delivered once.

use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use agent_backend::HookDeliverer;
use agent_base::gostd::filepath;
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, StateUpdate, Time};

use super::agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AGENT_STATUS_PROCESSING,
    AGENT_STATUS_UNKNOWN, AGENT_STATUS_WAITING, AgentService, AgentStatus, exited_err,
};
use super::ctx::{Ctx, CtxExt};
use super::handover::HandoverError;
use super::status::{
    STATE_KEY_DETAIL, STATE_KEY_HOOKED_SESSION, STATE_KEY_SESSION_ID, STATE_KEY_SPAWN_ID,
    StatusInfo, StatusReport,
};
use super::submit::UnsubmittedError;

/// EventDelivered is the event recorded when a hook hands an agent its held
/// messages: it goes on with them, so the turn isn't over.
pub const EVENT_DELIVERED: &str = "delivered";

/// SendOptions changes how send delivers.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    /// Now types the message even while the agent is busy (Claude takes it
    /// at its next tool boundary), instead of holding it for the turn's end.
    pub now: bool,
    /// From is the sender recorded with a held message: an agent's name,
    /// "user", or "remote" for a send from another host.
    pub from: String,
}

/// SendResult says what send did with a message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SendResult {
    /// what send found the agent doing (unknown if it couldn't tell)
    pub status: AgentStatus,
    /// the status detail, e.g. the prompt a waiting agent is at
    pub detail: String,
    /// Held: stored, to be delivered when the agent's turn ends (or, per
    /// held_until, its next step). held_count is how many messages are held
    /// for it now, this one included.
    pub held: bool,
    pub held_count: usize,
    /// when the hook that delivers it runs, e.g. "its turn ends"
    pub held_until: String,
    /// InputBlocked, for a message held although the agent was at its
    /// prompt, says why: its input line isn't empty (see InputBlockedError).
    pub input_blocked: String,
    /// Typed is how many messages were typed in: this one, after any held
    /// ones it overtook (oldest first, as one prompt).
    pub typed: usize,
    /// Unchecked, for messages typed into a CLI whose Enter can turn into a
    /// newline (Claude), says why send couldn't check they were submitted,
    /// e.g. its input box isn't on screen (see submit.rs).
    pub unchecked: String,
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

/// contents returns the messages' texts, each with its "[from ...]" tag.
fn contents(msgs: &[Message]) -> Vec<String> {
    msgs.iter().map(|m| m.content.clone()).collect()
}

fn rfc3339_now() -> String {
    Time::now().format_rfc3339()
}

impl AgentService {
    /// holdsMessages reports whether messages to agent can be held for the end
    /// of its turn: its backend takes text from a hook, and spawn wired its
    /// hooks to this binary (only spawn sets _spawn_id).
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
    /// held for the end of its turn rather than typed (see send).
    pub fn holds_messages_named(&self, name: &str) -> bool {
        self.get_agent(name).is_ok_and(|a| self.holds_messages(&a))
    }

    /// DeliveryPoint says when the hook that hands name its held messages runs,
    /// e.g. "its turn ends" (see backend.HookDeliverer).
    pub fn delivery_point(&self, name: &str) -> String {
        match self.get_agent(name) {
            Ok(agent) => self.delivery_point_of(&agent),
            Err(_) => "its turn ends".to_string(),
        }
    }

    pub(crate) fn delivery_point_of(&self, agent: &Agent) -> String {
        if self.typed_by_deliverer(agent) {
            return super::revive::HELD_UNTIL_PROMPT.to_string();
        }
        if let Ok(p) = self.provider(agent)
            && let Some(hd) = p.as_hook_deliverer()
        {
            return hd.delivery_point();
        }
        "its turn ends".to_string()
    }

    /// sendHeld is send for an agent that holds messages: it holds the message
    /// while the agent is mid-turn or at a prompt, and otherwise types it, after
    /// any messages still held.
    pub(crate) fn send_held(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        message: &str,
        from: &str,
    ) -> Result<SendResult> {
        let mut res = SendResult {
            status: AGENT_STATUS_UNKNOWN,
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
            // it once the dialog is gone.
            let mut res = self.hold(ctx, agent, res, message, from)?;
            if res.held {
                res.held_until = HELD_UNTIL_DIALOG.to_string();
            }
            return Ok(res);
        }
        if res.status == AGENT_STATUS_PROCESSING || res.status == AGENT_STATUS_WAITING {
            // Waiting too: a held message never goes near the dialog, and once
            // the prompt is answered, the turn's end delivers it.
            return self.hold(ctx, agent, res, message, from);
        }
        let (n, unchecked, err) = self.type_held(ctx, agent, message);
        if let Err(err) = err {
            if let Some(blocked) = err.downcast_ref::<InputBlockedError>() {
                let why = blocked.why.clone();
                // A person may be typing: hold this one (behind any held), for
                // the deliverer to type once the input line is empty.
                let mut res = self.hold(ctx, agent, res, message, from)?;
                if res.held {
                    res.input_blocked = why;
                }
                return Ok(res);
            }
            return Err(err);
        }
        res.typed = n;
        res.unchecked = unchecked;
        Ok(res)
    }

    /// hold stores message for delivery at the end of the agent's turn, unless
    /// the last report says the turn is already over.
    fn hold(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        mut res: SendResult,
        message: &str,
        from: &str,
    ) -> Result<SendResult> {
        let from = if from.is_empty() { "user" } else { from };
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
            .context("holding message")?;
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
        res.held_until = self.delivery_point_of(agent);
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

    /// HeldCount returns how many messages are held for name.
    pub fn held_count(&self, name: &str) -> usize {
        self.storage
            .get_pending_messages(name, MAX_HELD_COUNT)
            .map(|h| h.len())
            .unwrap_or(0)
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
    /// set, as one prompt. It claims them first, so the agent's hook (or
    /// another sender) can't deliver them too, and holds them again if typing
    /// fails. It refuses (InputBlockedError) unless the agent's input line is
    /// empty or unreadable. It returns how many messages it typed, and why it
    /// couldn't check they were submitted (see send_unchecked).
    ///
    /// As in the Go, the count comes with an UnsubmittedError or a
    /// HandoverError too: those messages were typed.
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
        let claimed = match self.storage.claim_pending_messages(&agent.name, None) {
            Ok(c) => c,
            Err(e) => return (0, String::new(), Err(e)),
        };
        let mut texts = contents(&claimed);
        if !extra.is_empty() {
            texts.push(extra.to_string());
        }
        if texts.is_empty() {
            return (0, String::new(), Ok(()));
        }
        match self.send_message(ctx, &agent.name, &texts.join("\n\n"), true) {
            Ok(unchecked) => (texts.len(), unchecked, Ok(())),
            Err(err) => {
                if err.downcast_ref::<UnsubmittedError>().is_some() {
                    return (texts.len(), String::new(), Err(err)); // typed: they sit in its input box
                }
                if err.downcast_ref::<HandoverError>().is_some() {
                    // Typed as a short line the agent took, and kept in the failed
                    // message: holding them again would have the deliverer type
                    // that line over and over.
                    return (texts.len(), String::new(), Err(err));
                }
                for m in &claimed {
                    let _ = self
                        .storage
                        .update_message_status(m.id, MESSAGE_STATUS_PENDING);
                }
                (0, String::new(), Err(err))
            }
        }
    }

    /// reportDelivering records r, the report of a hook that can hand the agent
    /// its held messages, and claims them in the same transaction (see hold for
    /// why). With messages claimed, the agent goes on with them: the report says
    /// processing (event "delivered") and starts a turn, for wait --next, and
    /// r's result, the reply to a step that wasn't the last, isn't stored. With
    /// none claimed, r is recorded unless delivery_only (the hook says nothing
    /// else; see backend.HookReport). It returns the hook output that hands the
    /// messages over, None if none.
    pub(crate) fn report_delivering(
        &self,
        name: &str,
        r: &StatusReport,
        hd: &dyn HookDeliverer,
        delivery_only: bool,
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
            if claimed.is_empty() {
                if delivery_only {
                    return StateUpdate::default();
                }
                return ended.clone();
            }
            detail = format!("{} delivered", plural(claimed.len(), "held message"));
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
        let claimed = self
            .storage
            .claim_pending_messages(name, Some(&mut update))?;
        if claimed.is_empty() {
            if !delivery_only {
                self.publish_status(name, r, &now);
            }
            return Ok(None);
        }
        delivering.detail = detail;
        self.publish_status(name, &delivering, &now);
        Ok(Some(hd.delivery_output(&contents(&claimed))))
    }

    /// DeliverHeld types the messages held for name once the agent is at its
    /// prompt with them still held, which happens when its turn ended without a
    /// hook that delivers them: Esc, a deny, a failed turn. It returns how many
    /// it typed, ErrNothingHeld if none were held to begin with, and an error if
    /// the agent has exited or is gone. Without wait, it checks once (ignoring
    /// grace) and returns with any messages still held while the agent is busy.
    ///
    /// As in the Go, the count comes with the error too: messages typed before
    /// a failure were typed.
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
                    if !opts.wait || since.elapsed() >= opts.grace {
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
            if let Err(e) = ctx.sleep(opts.interval) {
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

//! Go `internal/service/handover.go`.
//!
//! Handing a long message over by hook. Claude takes a long or multi-line
//! message typed into it as a paste, and gives the model its text inside
//! <pasted_content> tags, as material to consider rather than as the
//! request (backend.PromptHandover says when). Instructions sent to an agent
//! would arrive downgraded. So when such a message goes to an agent whose
//! hook reaches its CLI (holds_messages), type_message:
//!
//!  1. stores the message (status handover), and types one short line in
//!     its place: the message's first line, cut short, and a handover token
//!     naming the stored message ("[gomuxai message #42, 10.4 KB: ...]");
//!  2. checks that line was submitted, as for any typed message;
//!  3. waits for the hook the submitted prompt runs (Claude's
//!     UserPromptSubmit), which finds the token, claims the message, and
//!     prints its whole text as the prompt's additional context.
//!
//! The model then gets the line as the prompt and the whole message beside
//! it as hook context, which it takes as coming from the user rather than
//! as pasted material. The turn starts and ends as for any typed message,
//! so wait --next and _result cover the reply.
//!
//! If the hook doesn't take the message within HANDOVER_WAITS submit_waits of
//! the line's submission, the message is failed (a late hook then hands over nothing)
//! and type_message says the agent got only the line. If the line wasn't
//! submitted (UnsubmittedError) or couldn't be checked, the message waits
//! for whenever it is: the hook hands it over then.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::Regex;

use agent_backend::{HookReport, Provider};
use agent_base::config::DEFAULT_SUBMIT_WAIT;
use agent_base::gostd::time::format_duration;
use agent_store::{Agent, MESSAGE_STATUS_HANDOVER, Message, Time};

use super::agent_service::AgentService;
use super::ctx::{Ctx, CtxExt};
use super::held::plural;
use super::submit::UnsubmittedError;

/// handoverWaits is how many submit_waits typing waits, after the short line
/// is submitted, for the hook to take the whole message: 9s by default. The
/// hook runs before the model is called, so this is the hook's own start-up
/// time.
const HANDOVER_WAITS: u32 = 3;

/// handoverPoll is how often it checks.
const HANDOVER_POLL: Duration = Duration::from_millis(50);

/// maxHandoverHead bounds the part of the message's first line the short
/// line carries, in runes: far below any paste threshold.
const MAX_HANDOVER_HEAD: usize = 160;

/// handoverToken finds the handover token in a submitted prompt.
static HANDOVER_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[gomuxai message #([0-9]+), ").unwrap());

/// HandoverError says the hook didn't take a handed-over message's text:
/// the agent got only the short line typed in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoverError {
    pub agent: String,
    pub id: i64,
    pub wait: Duration,
}

impl std::fmt::Display for HandoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "typed a short line into {} in place of the message (its first line, at most {} characters, and the token gomuxai message #{}), but its UserPromptSubmit hook didn't take the whole message within {}, so it got only that line: the hook isn't reaching its session (agent status {} -v; agent logs {} shows what it got)",
            self.agent,
            MAX_HANDOVER_HEAD,
            self.id,
            format_duration(self.wait),
            self.agent,
            self.agent
        )
    }
}

impl std::error::Error for HandoverError {}

/// handoverLine is the line typed in place of message id: the start of its
/// first line, and the token that names it.
pub(crate) fn handover_line(id: i64, text: &str) -> String {
    let trimmed = text.trim_matches(is_go_space);
    let (head, rest, mut more) = match trimmed.split_once('\n') {
        Some((h, r)) => (h, r, true),
        None => (trimmed, "", false),
    };
    let mut head = head
        .strip_suffix('\r')
        .unwrap_or(head)
        .trim_matches(is_go_space)
        .to_string();
    if head.chars().count() > MAX_HANDOVER_HEAD {
        head = head.chars().take(MAX_HANDOVER_HEAD).collect();
        more = true;
    }
    if more || !rest.is_empty() {
        head += " …";
    }
    let lines = text.trim_end_matches('\n').matches('\n').count() + 1;
    let size = format!("{}, {}", byte_size(text.len()), plural(lines, "line"));
    format!("{head} [gomuxai message #{id}, {size}: the whole message follows as hook context]")
}

/// Go's strings.TrimSpace: unicode.IsSpace.
fn is_go_space(c: char) -> bool {
    c.is_whitespace()
}

/// byteSize formats n bytes for people: "512 B", "10.4 KB".
pub(crate) fn byte_size(n: usize) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    format!("{:.1} KB", n as f64 / 1000.0)
}

impl AgentService {
    /// handoverWait is that wait, or 9s with checking off.
    fn handover_wait(&self) -> Duration {
        if self.cfg.submit_wait.is_zero() {
            return DEFAULT_SUBMIT_WAIT * HANDOVER_WAITS;
        }
        self.cfg.submit_wait * HANDOVER_WAITS
    }

    /// promptHandover returns the agent's backend, which is a PromptHandover,
    /// when its hook reaches the CLI it runs, so a handed-over message would
    /// be taken.
    pub(crate) fn prompt_handover(&self, agent: &Agent) -> Option<Box<dyn Provider>> {
        let p = self.provider(agent).ok()?;
        p.as_prompt_handover()?;
        if !self.holds_messages(agent) {
            return None;
        }
        Some(p)
    }

    /// typeHandover types text into the agent as a short line, for its hook to
    /// hand over in whole (see above). It returns what type_text returns, or a
    /// HandoverError.
    pub(crate) fn type_handover(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        text: &str,
        dialogs: bool,
    ) -> Result<String> {
        let mut msg = Message {
            sender_id: "send".into(),
            receiver_id: agent.name.clone(),
            content: text.to_string(),
            status: MESSAGE_STATUS_HANDOVER,
            created_at: Time::now(),
            ..Default::default()
        };
        self.storage
            .create_message(&mut msg)
            .context("storing the message for its hook")?;
        match self.type_text(ctx, agent, &handover_line(msg.id, text), dialogs) {
            // the line sits in its input box: whoever submits it gets the message
            Err(err) if err.downcast_ref::<UnsubmittedError>().is_some() => return Err(err),
            Err(err) => {
                let _ = self.storage.fail_handover(msg.id);
                return Err(err);
            }
            // maybe not submitted yet: the hook hands it over when it is
            Ok(why) if !why.is_empty() => return Ok(why),
            Ok(_) => {}
        }
        let wait = self.handover_wait();
        let deadline = Instant::now() + wait;
        loop {
            if let Ok(status) = self.storage.get_message_status(msg.id)
                && status != MESSAGE_STATUS_HANDOVER
            {
                return Ok(String::new());
            }
            if Instant::now() > deadline {
                break;
            }
            if ctx.sleep(HANDOVER_POLL).is_err() {
                break;
            }
        }
        match self.storage.fail_handover(msg.id) {
            Ok(true) => {}
            Ok(false) => return Ok(String::new()), // taken at the last moment
            Err(e) => return Err(e),
        }
        Err(HandoverError {
            agent: agent.name.clone(),
            id: msg.id,
            wait,
        }
        .into())
    }

    /// handoverOutput is the hook output for a prompt that names a message
    /// awaiting handover to name: its whole text, claimed so it goes out once.
    /// None if the prompt names none, or the hook isn't the one that hands over.
    pub(crate) fn handover_output(
        &self,
        name: &str,
        provider: &dyn Provider,
        hr: &HookReport,
    ) -> Option<Vec<u8>> {
        let ph = provider.as_prompt_handover()?;
        let prompt = ph.submitted_prompt(hr)?;
        let m = HANDOVER_TOKEN.captures(&prompt)?;
        let id: i64 = m[1].parse().ok()?;
        let msg = self.storage.claim_handover(name, id).ok()??;
        // A short header: Claude hands over up to 10,000 characters inline.
        Some(ph.handover_output(&format!("gomuxai message #{id}, whole:\n\n{}", msg.content)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_line_shapes() {
        assert_eq!(
            handover_line(42, "do this\nand that\n"),
            "do this … [gomuxai message #42, 17 B, 2 lines: the whole message follows as hook context]"
        );
        assert_eq!(
            handover_line(7, "one line"),
            "one line [gomuxai message #7, 8 B, 1 line: the whole message follows as hook context]"
        );
        let long = "x".repeat(2000);
        let line = handover_line(1, &long);
        assert!(
            line.starts_with(&format!(
                "{} … [gomuxai message #1, 2.0 KB, 1 line:",
                "x".repeat(160)
            )),
            "{line}"
        );
        assert_eq!(byte_size(999), "999 B");
        assert_eq!(byte_size(10_449), "10.4 KB");
    }
}

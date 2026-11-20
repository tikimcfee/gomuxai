//! Go `internal/service/submit.go`.
//!
//! Typing a message into a CLI and pressing Enter are two writes to its
//! terminal, and Claude handles whatever has arrived in one read as one
//! input. While its event loop is busy (a long render, a paste it is still
//! collecting), the text and the Enter arrive together; Claude reads them as
//! a paste, where the Enter is a newline, and the message sits in its input
//! box until someone presses Enter. The first Enter after that is often
//! eaten too. So for a backend that says its CLI does this
//! (backend.SubmitChecker), typing a message:
//!
//!  1. types the text, then waits for the input box to show something new
//!     that has stopped changing (the text, or a paste's placeholder), so
//!     the Enter arrives on its own;
//!  2. presses Enter and waits for the box to empty;
//!  3. presses Enter again while the box still shows what was typed, up to
//!     SUBMIT_ENTERS times in all;
//!  4. failing that, records what the box shows (STATE_KEY_UNSUBMITTED) and
//!     returns an UnsubmittedError, so the sender, status, and the next send
//!     say so.
//!
//! If the box never shows the text, something else took the keys: a dialog
//! (see dialog.rs) that came up since the sender checked the screen. Then no
//! Enter is pressed, which would pick the dialog's highlighted option, and
//! it returns a DialogTookError. If the screen shows no dialog it presses
//! Enter as above, but says the message wasn't seen (STATE_KEY_UNCHECKED)
//! rather than that it was sent.
//!
//! It presses no more Enters once the box is off screen (a dialog may have
//! taken its place) or shows anything else (a person typing). Each wait is
//! bounded by the config's submit_wait; 0 turns all this off.
//!
//! A CLI that hasn't started a turn since it was launched may still be
//! starting: right after spawn (or resume) returns, Claude may not have drawn
//! its input box yet, and what is typed then sits in the box once it does,
//! its Enter lost (friction #38). So when the box isn't on screen and no turn
//! has started, it first waits, up to STARTUP_WAITS submit_waits, for the box
//! to come up, then goes on as above. A dialog that comes up instead (the
//! trust prompt) ends the wait with check_screen's error, unless the sender
//! meant to type into whatever is there (send_unchecked).
//!
//! If it can't read the box before typing (it isn't on screen, or the screen
//! can't be captured), it types and presses Enter without checking, and says
//! so: the sender's result names why, and STATE_KEY_UNCHECKED records it, so
//! status shows it until a turn starts.

use std::time::{Duration, Instant};

use anyhow::Result;

use agent_backend::{Dialog, InputReader, Provider};
use agent_base::gostd::time::format_duration;
use agent_store::{Agent, Time};

use super::agent_service::{AgentService, one_line};
use super::ctx::{Ctx, CtxExt};
use super::dialog::dialog_hint;
use super::status::STATE_KEY_TURN_AT;

/// submitEnters is how many times Enter is pressed for one message, at most.
const SUBMIT_ENTERS: usize = 3;

/// submitPoll is how often the input box is read while typing a message.
const SUBMIT_POLL: Duration = Duration::from_millis(50);

/// startupWaits is how many submit_waits typing waits for the input box of
/// a CLI that may still be starting (see above): 30s by default.
const STARTUP_WAITS: u32 = 10;

/// startupPoll is how often the screen is read while waiting for that box.
pub(crate) const STARTUP_POLL: Duration = Duration::from_millis(200);

/// submitQuiet is how long the input box must stay unchanged after typing
/// before the text counts as taken in: a paste's placeholder or an image
/// path's [Image #N] can take a moment to replace the text.
const SUBMIT_QUIET: Duration = Duration::from_millis(150);

/// StateKeyUnsubmitted is what a message's input box showed when Enter
/// failed to submit it: status compares it with what the box shows now.
pub const STATE_KEY_UNSUBMITTED: &str = "_unsubmitted";

/// StateKeyUnchecked is why a message was typed without checking it was
/// submitted (see type_message); its time is when.
pub const STATE_KEY_UNCHECKED: &str = "_unchecked";

/// UnsubmittedError is the error for a message typed into the agent's input
/// box that is still there after SUBMIT_ENTERS Enters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the message was typed into {agent}'s input box, but it is still there after {enters} Enters: don't send it again; agent capture {agent} shows it, and agent key {session} Enter submits it"
)]
pub struct UnsubmittedError {
    pub agent: String,
    pub session: String,
    pub enters: usize,
}

/// DialogTookError is the error for a message typed into an agent whose
/// input box never showed it because a dialog came up and took the keys:
/// Enter wasn't pressed, so the dialog is unanswered.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogTookError {
    pub agent: Agent,
    pub dialog: Dialog,
}

impl std::fmt::Display for DialogTookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the message was typed into {}, but a dialog came up ({}) and took it, so Enter wasn't pressed: {}",
            self.agent.name,
            one_line(&self.dialog.title),
            dialog_hint(&self.agent)
        )
    }
}

impl std::error::Error for DialogTookError {}

/// The agent's backend, when it is a SubmitChecker and checking is on: the
/// provider owns the trait object, so this holds it.
pub(crate) struct Checker(Box<dyn Provider>);

impl Checker {
    pub(crate) fn ir(&self) -> &dyn InputReader {
        // Only built when as_submit_checker is Some.
        self.0.as_submit_checker().expect("a SubmitChecker")
    }
}

impl AgentService {
    /// submitChecker returns the agent's backend as a SubmitChecker, if it is
    /// one and checking is on.
    pub(crate) fn submit_checker(&self, agent: &Agent) -> Option<Checker> {
        if self.cfg.submit_wait.is_zero() {
            return None;
        }
        let p = self.provider(agent).ok()?;
        p.as_submit_checker()?;
        Some(Checker(p))
    }

    /// readInput reads the agent's input box off its screen.
    fn read_input(&self, agent: &Agent, ir: &dyn InputReader) -> Option<String> {
        let screen = self.tmux.capture_styled(&agent.session_name, 0).ok()?;
        ir.input_text(&screen)
    }

    /// typeMessage types text into the agent and presses Enter (type_text). Text
    /// the CLI would take as a paste goes as one short line, with the whole text
    /// handed over by the agent's hook, when its hook reaches the CLI
    /// (type_handover, see handover.rs).
    pub(crate) fn type_message(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        text: &str,
        dialogs: bool,
    ) -> Result<String> {
        if let Some(p) = self.prompt_handover(agent)
            && p.as_prompt_handover()
                .is_some_and(|ph| ph.typed_as_paste(text))
        {
            return self.type_handover(ctx, agent, text, dialogs);
        }
        self.type_text(ctx, agent, text, dialogs)
    }

    /// typeText types text into the agent and presses Enter, making sure
    /// the Enter submitted it where the backend needs that (see above). It
    /// returns why it couldn't check, or "" if it did or the backend needs no
    /// checking.
    /// With dialogs, a dialog that comes up while it waits for a starting CLI's
    /// input box is an error, and nothing is typed.
    pub(crate) fn type_text(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        text: &str,
        dialogs: bool,
    ) -> Result<String> {
        let Some(sc) = self.submit_checker(agent) else {
            self.paste_line(&agent.session_name, text)?;
            return Ok(String::new());
        };
        let ir = sc.ir();
        let (mut before, mut why) = self.input_before(agent, ir);
        if !why.is_empty() && self.state_value(&agent.name, STATE_KEY_TURN_AT).is_empty() {
            (before, why) = self.await_input_box(ctx, agent, ir, dialogs)?;
        }
        if !why.is_empty() {
            self.paste_line(&agent.session_name, text)?;
            let _ = self.set_state(&agent.name, STATE_KEY_UNCHECKED, &why);
            return Ok(why);
        }
        self.tmux.paste_text(&agent.session_name, text)?;
        let (mut typed, mut seen) = self.settled_input(ctx, agent, ir, &before)?;
        if !seen {
            // The box didn't take the text: a dialog that came up since the
            // check before typing may have, and Enter would answer it.
            if let Some(d) = self.dialog_on_screen(agent) {
                return Err(DialogTookError {
                    agent: agent.clone(),
                    dialog: d,
                }
                .into());
            }
        }
        let mut enters = 1;
        loop {
            self.tmux.send_special_key(&agent.session_name, "Enter")?;
            let sits;
            (typed, seen, sits) = self.await_submit(ctx, agent, ir, &before, typed, seen)?;
            if !sits {
                let _ = self
                    .storage
                    .delete_state_key(&agent.name, STATE_KEY_UNSUBMITTED);
                if !seen {
                    // Something else took the text, or the box is drawn in a
                    // way agent doesn't read: say so rather than "Sent".
                    let why = "its input box never showed the message (a dialog, or a screen agent doesn't recognize, may have taken it)".to_string();
                    let _ = self.set_state(&agent.name, STATE_KEY_UNCHECKED, &why);
                    return Ok(why);
                }
                let _ = self
                    .storage
                    .delete_state_key(&agent.name, STATE_KEY_UNCHECKED);
                return Ok(String::new());
            }
            if enters == SUBMIT_ENTERS {
                let _ = self.set_state(&agent.name, STATE_KEY_UNSUBMITTED, &typed);
                return Err(UnsubmittedError {
                    agent: agent.name.clone(),
                    session: agent.session_name.clone(),
                    enters,
                }
                .into());
            }
            enters += 1;
        }
    }

    /// inputBefore reads the agent's input box before a message is typed. It
    /// returns what the box shows, or why it can't be read.
    pub(crate) fn input_before(&self, agent: &Agent, ir: &dyn InputReader) -> (String, String) {
        let screen = match self.tmux.capture_styled(&agent.session_name, 0) {
            Ok(s) => s,
            Err(e) => {
                return (
                    String::new(),
                    format!("its screen couldn't be captured ({e:#})"),
                );
            }
        };
        match ir.input_text(&screen) {
            Some(text) => (text, String::new()),
            None => (
                String::new(),
                "its input box isn't on screen (a dialog, or a screen agent doesn't recognize)"
                    .to_string(),
            ),
        }
    }

    /// awaitInputBox waits, up to STARTUP_WAITS submit_waits, for the input box
    /// of an agent whose CLI may still be starting to come on screen. It returns
    /// what the box shows once it does, or, if it doesn't, why it can't be read
    /// (as input_before). With dialogs, a dialog on screen ends the wait with
    /// check_screen's error.
    pub(crate) fn await_input_box(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        ir: &dyn InputReader,
        dialogs: bool,
    ) -> Result<(String, String)> {
        let bound = self.cfg.submit_wait * STARTUP_WAITS;
        let deadline = Instant::now() + bound;
        loop {
            if dialogs {
                self.check_screen(agent)?;
            }
            if Instant::now() >= deadline {
                break;
            }
            ctx.sleep(STARTUP_POLL)?;
            let (before, why) = self.input_before(agent, ir);
            if why.is_empty() {
                return Ok((before, String::new()));
            }
        }
        let (_, mut why) = self.input_before(agent, ir);
        if !why.is_empty() {
            why += &format!(
                ", after waiting {} for it: no turn has started, so the CLI may still have been starting",
                format_duration(bound)
            );
        }
        Ok((String::new(), why))
    }

    /// settledInput waits, up to submit_wait, for the input box to show
    /// something other than before that then stays the same for SUBMIT_QUIET.
    /// It returns that (seen), or seen false if the box didn't change in time.
    fn settled_input(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        ir: &dyn InputReader,
        before: &str,
    ) -> Result<(String, bool)> {
        let deadline = Instant::now() + self.cfg.submit_wait;
        let mut last = String::new();
        let mut since: Option<Instant> = None;
        while Instant::now() < deadline {
            ctx.sleep(SUBMIT_POLL)?;
            match self.read_input(agent, ir) {
                None => since = None,
                Some(text) if text == before => since = None,
                Some(text) => match since {
                    Some(s) if text == last => {
                        if s.elapsed() >= SUBMIT_QUIET {
                            return Ok((text, true));
                        }
                    }
                    _ => {
                        last = text;
                        since = Some(Instant::now());
                    }
                },
            }
        }
        Ok((String::new(), false))
    }

    /// awaitSubmit waits, up to submit_wait, for the Enter just pressed to
    /// submit what was typed. typed is what the box showed before the Enter, if
    /// seen; a box that settles on the message only now (its text and the Enter
    /// arrived together) restarts the wait. It returns sits true if the box
    /// still shows typed, so another Enter is due; sits false means submitted,
    /// or that it can't tell (the box is off screen or shows something else),
    /// so no Enter is due. A box that never showed the message is one it can't
    /// tell about either.
    fn await_submit(
        &self,
        ctx: &Ctx,
        agent: &Agent,
        ir: &dyn InputReader,
        before: &str,
        mut typed: String,
        mut seen: bool,
    ) -> Result<(String, bool, bool)> {
        let mut deadline = Instant::now() + self.cfg.submit_wait;
        let mut last = String::new();
        let mut since: Option<Instant> = None;
        while Instant::now() < deadline {
            ctx.sleep(SUBMIT_POLL)?;
            let Some(text) = self.read_input(agent, ir) else {
                return Ok((typed, seen, false));
            };
            if seen && text == typed {
                // Still there; the Enter may not have been read yet.
            } else if seen {
                return Ok((typed, seen, false)); // emptied, or someone typed
            } else if text == before {
                since = None; // nothing read yet
            } else if text.is_empty() {
                return Ok((typed, seen, false)); // read and submitted at once
            } else if since.is_none() || text != last {
                last = text;
                since = Some(Instant::now());
            } else if since.is_some_and(|s| s.elapsed() >= SUBMIT_QUIET) {
                typed = text;
                seen = true;
                deadline = Instant::now() + self.cfg.submit_wait;
            }
        }
        Ok((typed, seen, seen))
    }

    /// unsubmitted reports whether text, what the agent's input box shows, is
    /// a message Enter failed to submit (see type_message).
    pub(crate) fn unsubmitted_text(&self, agent: &Agent, text: &str) -> bool {
        !text.is_empty() && self.state_value(&agent.name, STATE_KEY_UNSUBMITTED) == text
    }

    /// Unsubmitted reports whether a message Enter failed to submit still sits
    /// in name's input box: when it was typed, and the session to press Enter in.
    pub fn unsubmitted(&self, name: &str) -> Option<(Time, String)> {
        let entry = self.storage.get_state(name, STATE_KEY_UNSUBMITTED).ok()??;
        if entry.value.is_empty() {
            return None;
        }
        let agent = self.get_agent(name).ok()?;
        let p = self.provider(&agent).ok()?;
        let ir = p.as_input_reader()?;
        let text = self.read_input(&agent, ir)?;
        (text == entry.value).then_some((entry.updated_at, agent.session_name))
    }

    /// Unchecked reports a message typed into name without checking it was
    /// submitted, if no turn has started since: why, and when it was typed.
    pub fn unchecked(&self, name: &str) -> Option<(String, Time)> {
        let entry = self.storage.get_state(name, STATE_KEY_UNCHECKED).ok()??;
        if entry.value.is_empty() {
            return None;
        }
        if let Ok(Some(reported)) = self.reported_status(name)
            && reported.turn_at.after(&entry.updated_at)
        {
            return None;
        }
        Some((entry.value, entry.updated_at))
    }
}

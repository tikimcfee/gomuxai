//! Go `internal/await`: how tests wait for state: poll a condition until it
//! holds, and on timeout fail saying what was awaited and what was last seen.
//!
//! A test that sleeps a fixed time and then asserts guesses at timing, and
//! under a loaded run the guess is wrong. Waiting on the condition itself
//! costs nothing when the state arrives early and only spends the timeout
//! when something is really broken.
//!
//! Go's functions take a `testing.TB` to fail; Rust tests fail by panicking,
//! so `until` and `within` panic with Go's message.

use std::time::{Duration, Instant};

use crate::gostd::time::format_duration;

/// How long `until` waits. It is generous on purpose: a met condition
/// returns at once, so the timeout is only spent on a failure, and a loaded
/// machine can take seconds to start a shell.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// How often a condition is checked.
pub const INTERVAL: Duration = Duration::from_millis(20);

/// Checks `cond` until it reports done, for up to `TIMEOUT`. `cond` returns
/// the state it saw (a screen, a status) and whether that is what the test
/// waits for. On timeout it panics (failing the test) with `what` and the
/// last state seen. It returns the state that satisfied cond.
#[track_caller]
pub fn until(what: &str, cond: impl FnMut() -> (String, bool)) -> String {
    within(TIMEOUT, what, cond)
}

/// `until` with its own timeout, for a wait that is part of what the test
/// asserts (the state must arrive within d).
#[track_caller]
pub fn within(d: Duration, what: &str, cond: impl FnMut() -> (String, bool)) -> String {
    let (state, ok) = poll(d, cond);
    if !ok {
        panic!("{}", timeout_message(d, what, &state));
    }
    state
}

/// The failure `within` reports.
pub fn timeout_message(d: Duration, what: &str, state: &str) -> String {
    format!(
        "timed out after {} waiting for {what}; last saw:\n{state}",
        format_duration(d)
    )
}

/// Checks `cond` until it reports done or `d` passes, and returns the last
/// state and whether cond was met. It never fails the test, for a caller
/// that has its own way to report, or that may run off the test thread.
pub fn poll(d: Duration, mut cond: impl FnMut() -> (String, bool)) -> (String, bool) {
    let deadline = Instant::now() + d;
    loop {
        let (state, done) = cond();
        if done {
            return (state, true);
        }
        if Instant::now() > deadline {
            return (state, false);
        }
        std::thread::sleep(INTERVAL); // the polling interval, not a guess at timing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    #[test]
    fn poll_returns_once_met() {
        let n = AtomicI32::new(0);
        let (state, ok) = poll(Duration::from_secs(1), || {
            let v = n.fetch_add(1, Ordering::SeqCst) + 1;
            (v.to_string(), v == 3)
        });
        assert!(ok && state == "3", "Poll = {state:?}, {ok}; want 3, true");
    }

    #[test]
    fn poll_times_out_with_the_last_state() {
        let n = AtomicI32::new(0);
        let (state, ok) = poll(Duration::from_millis(50), || {
            ((n.fetch_add(1, Ordering::SeqCst) + 1).to_string(), false)
        });
        let last = n.load(Ordering::SeqCst).to_string();
        assert!(
            !ok && state == last,
            "Poll = {state:?}, {ok}; want the last state ({last}), false"
        );
    }

    #[test]
    fn within_fails_saying_what_and_the_last_state() {
        let result = std::panic::catch_unwind(|| {
            within(Duration::from_millis(30), "the shell's prompt", || {
                ("screen: $ ".to_string(), false)
            })
        });
        let err = result.expect_err("within should fail");
        let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(
            msg.contains("waiting for the shell's prompt") && msg.contains("screen: $ "),
            "failure = {msg:?}; want what was awaited and the last state"
        );
        assert!(msg.starts_with("timed out after 30ms "), "{msg:?}");
    }
}

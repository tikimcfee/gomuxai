//! Go tests/watcher: the watcher delivers, holds and fails queued messages by
//! the agent's status. The mux is a fake (no termd), and each status comes
//! from the last report (no session id, so no native lookup).

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_backend::Registry;
use agent_base::await_;
use agent_base::config::Config;
use agent_service::watcher::{self, Level, Logger};
use agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AGENT_STATUS_PROCESSING,
    AGENT_STATUS_UNKNOWN, AGENT_STATUS_WAITING, AgentService, AgentStatus, Context, StatusReport,
};
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, Store, Time};
use agent_termd::{Ops, Session};

/// fakeTmux records what the watcher types; every session exists. With
/// fail_send set, typing fails and is only counted.
#[derive(Default)]
struct FakeTmux {
    inner: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    sent: Vec<String>,
    attempts: usize,
    fail_send: bool,
}

impl FakeTmux {
    fn counts(&self) -> (usize, usize) {
        let s = self.inner.lock().unwrap();
        (s.sent.len(), s.attempts)
    }
}

impl Ops for FakeTmux {
    fn create_session(&self, _: &str, _: &HashMap<String, String>) -> anyhow::Result<()> {
        Ok(())
    }
    fn create_session_in(
        &self,
        _: &str,
        _: &str,
        _: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill_session(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn respawn_pane(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn session_exists(&self, _: &str) -> anyhow::Result<bool> {
        Ok(true)
    }
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(Vec::new())
    }
    fn send_special_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn capture(&self, _: &str, _: i32) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn capture_styled(&self, _: &str, _: i32) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn pane_pids(&self) -> anyhow::Result<HashMap<i32, String>> {
        Ok(HashMap::new())
    }
    /// PasteText is how messages are typed (see termd's Client::paste_text).
    fn paste_text(&self, session: &str, text: &str) -> anyhow::Result<()> {
        self.send_keys(session, text)
    }
    fn send_keys(&self, _: &str, text: &str) -> anyhow::Result<()> {
        let mut s = self.inner.lock().unwrap();
        s.attempts += 1;
        if s.fail_send {
            anyhow::bail!("send failed");
        }
        s.sent.push(text.to_string());
        Ok(())
    }
}

/// An agent with one queued message, whose status comes from the last report.
struct DeliveryEnv {
    store: Arc<Store>,
    tmux: Arc<FakeTmux>,
    svc: Arc<AgentService>,
    _dir: tempfile::TempDir,
}

fn setup_delivery(status: AgentStatus) -> DeliveryEnv {
    let dir = tempfile::Builder::new()
        .prefix("watcher-test-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
    let ft = Arc::new(FakeTmux::default());
    let cfg = Config {
        session_prefix: "test-".to_string(),
        ..Default::default()
    };
    let svc = Arc::new(AgentService::new(
        cfg,
        Arc::clone(&store),
        ft.clone(),
        Arc::new(Registry::new()),
    ));

    store
        .create_agent(&Agent {
            name: "worker".to_string(),
            session_name: "test-worker".to_string(),
            backend: "claude".to_string(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    svc.report(
        "worker",
        StatusReport {
            status,
            event: "test".to_string(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut msg = Message {
        sender_id: "coordinator".to_string(),
        receiver_id: "worker".to_string(),
        content: "next task".to_string(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        ..Default::default()
    };
    store.create_message(&mut msg).unwrap();
    DeliveryEnv {
        store,
        tmux: ft,
        svc,
        _dir: dir,
    }
}

/// Collects a watcher's log lines, which say what it decided on each tick.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::fmt::Display for LogBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0.lock().unwrap()))
    }
}

impl DeliveryEnv {
    /// Runs a fast watcher, logging every decision to the returned buffer,
    /// until the watcher is stopped (or dropped).
    fn start_watcher(&self) -> (LogBuffer, watcher::Watcher) {
        let logs = LogBuffer::default();
        let mut w = watcher::new_watcher(
            Arc::clone(&self.svc),
            Arc::clone(&self.store),
            Duration::from_millis(5),
            [watcher::with_logger(Logger::text(
                Box::new(logs.clone()),
                Level::Debug,
            ))],
        );
        w.start(&Context::background());
        (logs, w)
    }

    /// Whether the message is still pending.
    fn pending(&self) -> bool {
        !self
            .store
            .get_pending_messages("worker", 1)
            .unwrap()
            .is_empty()
    }

    /// Runs a fast watcher until the message leaves pending, then stops it.
    fn run_watcher(&self) {
        let (logs, mut w) = self.start_watcher();
        await_::until("the message to leave pending", || {
            (logs.to_string(), !self.pending())
        });
        w.stop();
    }
}

/// Waits until the watcher has logged msg n times.
fn wait_logged(logs: &LogBuffer, msg: &str, n: usize) {
    await_::until(&format!("the watcher to log {msg:?} {n} times"), || {
        let out = logs.to_string();
        let ok = out.matches(&format!("msg=\"{msg}\"")).count() >= n;
        (out, ok)
    });
}

// A failed turn (Claude's StopFailure) leaves the CLI at its prompt, and the
// status stays error until the next prompt: deliver rather than wait forever.
#[test]
fn watcher_delivers_to_idle_or_errored_agent() {
    for st in [AGENT_STATUS_IDLE, AGENT_STATUS_ERROR] {
        let env = setup_delivery(st.clone());
        env.run_watcher();
        let (sent, _) = env.tmux.counts();
        assert_eq!(sent, 1, "{st}: typed {sent} message(s), want 1");
    }
}

// Repro: an agent's CLI exits (or crashes) with messages queued. The watcher
// skipped every status but idle and unknown, so they sat pending forever.
#[test]
fn watcher_fails_messages_to_exited_agent() {
    let env = setup_delivery(AGENT_STATUS_ENDED);
    env.run_watcher();
    let (sent, _) = env.tmux.counts();
    assert_eq!(
        sent, 0,
        "typed {sent} message(s) into an exited agent's shell"
    );
}

#[test]
fn watcher_holds_messages_while_busy() {
    for st in [AGENT_STATUS_PROCESSING, AGENT_STATUS_WAITING] {
        let env = setup_delivery(st.clone());
        // The watcher says when it holds a message: once it has, twice, the
        // message must still be waiting.
        let (logs, mut w) = env.start_watcher();
        wait_logged(&logs, "agent busy, skipping delivery", 2);
        w.stop();
        assert!(
            env.pending(),
            "message left pending while the agent was {st}"
        );
    }
}

// An agent whose status stays unknown fails its message after a few checks
// instead of holding it forever.
#[test]
fn watcher_fails_message_after_repeated_unknown_status() {
    let env = setup_delivery(AGENT_STATUS_UNKNOWN);
    env.run_watcher();
    let (_, attempts) = env.tmux.counts();
    assert_eq!(
        attempts, 0,
        "tried to type {attempts} message(s) into an agent of unknown status"
    );
}

// A failed send leaves the message pending for a retry, and the retry waits
// out the backoff (1s after the first failure) rather than the next tick.
#[test]
fn watcher_retries_failed_send_after_backoff() {
    let env = setup_delivery(AGENT_STATUS_IDLE);
    env.tmux.inner.lock().unwrap().fail_send = true;

    // The watcher claims the message (marks it delivered) before typing and
    // puts it back when typing fails, so look once it has passed it over for
    // the backoff on later ticks, not as soon as the message isn't pending.
    let (logs, mut w) = env.start_watcher();
    wait_logged(&logs, "delivery failed, will retry", 1);
    wait_logged(&logs, "skipping delivery: backoff active", 2);
    w.stop();
    assert!(
        env.pending(),
        "message failed after one failed send, want pending for a retry"
    );
    let (_, attempts) = env.tmux.counts();
    assert_eq!(
        attempts, 1,
        "tried to send {attempts} time(s) within the backoff, want 1"
    );
}

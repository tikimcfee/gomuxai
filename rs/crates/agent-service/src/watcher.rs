//! Go `internal/watcher`: automatic message delivery when agents become idle.
//!
//! The watcher delivers what `agent queue` stored for agents whose CLI has no
//! hook to hand messages over (Claude and agy agents hold messages instead;
//! see held.rs). Its decisions go to a [`Logger`], which writes the lines Go's
//! log/slog does.

use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_base::gostd::{strconv, time as gotime};
#[cfg(test)]
use agent_store::MESSAGE_STATUS_PENDING;
use agent_store::{Agent, MESSAGE_STATUS_FAILED, Message, Store};

use crate::service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AGENT_STATUS_UNKNOWN, AgentService,
    Context,
};

/// MAX_RETRIES is how many failed sends a message gets before it fails.
pub const MAX_RETRIES: u32 = 3;
/// BASE_BACKOFF is the wait after the first failed send; it doubles with each
/// further failure.
pub const BASE_BACKOFF: Duration = Duration::from_secs(1);
/// UNKNOWN_THRESHOLD is how many checks in a row may find the agent's status
/// unknown before its message fails.
pub const UNKNOWN_THRESHOLD: u32 = 5;

/// How often a running watcher looks at its context between ticks: Go's
/// select wakes at once on ctx.Done(); this is as close as a poll gets.
const CTX_POLL: Duration = Duration::from_millis(50);

/// messageState tracks retry and unknown status counts for a message.
#[derive(Debug, Default, Clone)]
struct MessageState {
    retry_count: u32,
    unknown_count: u32,
    last_attempt: Option<Instant>,
}

/// A log level, as slog's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// How a [`Logger`] lays out a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// slog.Default() before any SetDefault: the log package's
    /// "2006/01/02 15:04:05 INFO msg k=v".
    Default,
    /// slog.NewTextHandler: "time=... level=INFO msg=\"...\" k=v".
    Text,
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A structured logger writing Go log/slog's lines (Go: *slog.Logger, as the
/// watcher uses it: With, Debug, Info, Warn, Error).
#[derive(Clone)]
pub struct Logger {
    out: SharedWriter,
    level: Level,
    format: LogFormat,
    attrs: Vec<(String, String)>,
}

impl Logger {
    /// A logger writing lines of this format at this level and above.
    pub fn new(out: Box<dyn Write + Send>, level: Level, format: LogFormat) -> Logger {
        Logger {
            out: Arc::new(Mutex::new(out)),
            level,
            format,
            attrs: Vec::new(),
        }
    }

    /// Go's slog.Default(): Info and above, on stderr, in the log package's
    /// layout.
    pub fn default_logger() -> Logger {
        Logger::new(Box::new(std::io::stderr()), Level::Info, LogFormat::Default)
    }

    /// slog.New(slog.NewTextHandler(out, &HandlerOptions{Level: level})).
    pub fn text(out: Box<dyn Write + Send>, level: Level) -> Logger {
        Logger::new(out, level, LogFormat::Text)
    }

    /// A logger that writes nothing (Go: a handler on io.Discard).
    pub fn discard() -> Logger {
        Logger::new(Box::new(std::io::sink()), Level::Error, LogFormat::Text)
    }

    /// Go's Logger.With: this logger, adding the attribute to every line.
    pub fn with(&self, key: &str, value: impl Display) -> Logger {
        let mut l = self.clone();
        l.attrs.push((key.to_string(), value.to_string()));
        l
    }

    pub fn debug(&self, msg: &str, attrs: &[(&str, &dyn Display)]) {
        self.log(Level::Debug, msg, attrs);
    }

    pub fn info(&self, msg: &str, attrs: &[(&str, &dyn Display)]) {
        self.log(Level::Info, msg, attrs);
    }

    pub fn warn(&self, msg: &str, attrs: &[(&str, &dyn Display)]) {
        self.log(Level::Warn, msg, attrs);
    }

    pub fn error(&self, msg: &str, attrs: &[(&str, &dyn Display)]) {
        self.log(Level::Error, msg, attrs);
    }

    fn log(&self, level: Level, msg: &str, attrs: &[(&str, &dyn Display)]) {
        if level < self.level {
            return;
        }
        let now = chrono::Local::now();
        let mut line = match self.format {
            LogFormat::Default => {
                format!("{} {} {msg}", now.format("%Y/%m/%d %H:%M:%S"), level.word())
            }
            LogFormat::Text => format!(
                "time={} level={} msg={}",
                now.format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
                level.word(),
                slog_value(msg)
            ),
        };
        let pairs = self.attrs.iter().map(|(k, v)| (k.clone(), v.clone()));
        let more = attrs.iter().map(|(k, v)| (k.to_string(), v.to_string()));
        for (k, v) in pairs.chain(more) {
            line.push(' ');
            line.push_str(&slog_value(&k));
            line.push('=');
            line.push_str(&slog_value(&v));
        }
        line.push('\n');
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = out.write_all(line.as_bytes());
    }
}

/// A value as slog's text handler writes it: quoted when it is empty or holds
/// a space, '=', '"', or anything unprintable (slog's needsQuoting).
fn slog_value(s: &str) -> String {
    let needs = s.is_empty()
        || s.chars().any(|c| {
            c == ' ' || c == '=' || c == '"' || c.is_whitespace() || !strconv::is_print(c)
        });
    if needs {
        strconv::quote(s)
    } else {
        s.to_string()
    }
}

/// A duration as Go prints one ("2s", "1.5s").
struct GoDuration(Duration);

impl Display for GoDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&gotime::format_duration(self.0))
    }
}

/// An error as Go's %v prints it: the chain on one line.
struct GoError<'a>(&'a anyhow::Error);

impl Display for GoError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

/// What the watcher's thread shares with it.
struct Inner {
    service: Arc<AgentService>,
    storage: Arc<Store>,
    interval: Duration,
    logger: Logger,
    states: Mutex<HashMap<i64, MessageState>>,
}

/// Watcher monitors agents and delivers queued messages when idle.
pub struct Watcher {
    inner: Arc<Inner>,
    stop: Option<mpsc::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

/// WatcherOption configures a Watcher.
pub enum WatcherOption {
    /// WithLogger sets a custom logger (default: slog.Default()'s).
    WithLogger(Logger),
}

/// Go's WithLogger.
pub fn with_logger(logger: Logger) -> WatcherOption {
    WatcherOption::WithLogger(logger)
}

/// NewWatcher creates a new watcher.
pub fn new_watcher(
    svc: Arc<AgentService>,
    store: Arc<Store>,
    interval: Duration,
    opts: impl IntoIterator<Item = WatcherOption>,
) -> Watcher {
    let mut logger = Logger::default_logger();
    for opt in opts {
        match opt {
            WatcherOption::WithLogger(l) => logger = l,
        }
    }
    Watcher {
        inner: Arc::new(Inner {
            service: svc,
            storage: store,
            interval,
            logger,
            states: Mutex::new(HashMap::new()),
        }),
        stop: None,
        handle: None,
    }
}

impl Watcher {
    /// Start begins watching agents, until stop or until ctx is done.
    pub fn start(&mut self, ctx: &Context) {
        let (tx, rx) = mpsc::channel();
        self.stop = Some(tx);
        let inner = Arc::clone(&self.inner);
        let ctx = ctx.clone();
        self.handle = Some(std::thread::spawn(move || inner.run(&ctx, &rx)));
    }

    /// Stop stops the watcher and waits for it to finish.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// One agent's check: deliver, hold, or fail its oldest queued message.
    pub fn process_agent(&self, ctx: &Context, agent: &Agent) {
        self.inner.process_agent(ctx, agent);
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    fn run(&self, ctx: &Context, stop: &mpsc::Receiver<()>) {
        self.logger.info(
            "watcher started",
            &[("interval", &GoDuration(self.interval))],
        );
        let mut next = Instant::now() + self.interval;
        loop {
            if ctx.is_done() {
                self.logger.info("watcher stopping: context cancelled", &[]);
                return;
            }
            let now = Instant::now();
            if now >= next {
                self.check_and_deliver(ctx);
                // A ticker drops ticks a slow check missed.
                let after = Instant::now();
                while next <= after {
                    next += self.interval.max(Duration::from_nanos(1));
                }
                continue;
            }
            match stop.recv_timeout((next - now).min(CTX_POLL)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    self.logger
                        .info("watcher stopping: stop signal received", &[]);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn check_and_deliver(&self, ctx: &Context) {
        // List all agents
        let agents = match self.service.list() {
            Ok(a) => a,
            Err(e) => {
                self.logger
                    .error("failed to list agents", &[("error", &GoError(&e))]);
                return;
            }
        };
        for agent in &agents {
            self.process_agent(ctx, agent);
        }
    }

    fn process_agent(&self, ctx: &Context, agent: &Agent) {
        let logger = self.logger.with("agent", &agent.name);

        // Check for pending messages
        let messages = match self.storage.get_pending_messages(&agent.name, 1) {
            Ok(m) => m,
            Err(e) => {
                logger.error("failed to get pending messages", &[("error", &GoError(&e))]);
                return;
            }
        };
        let Some(msg) = messages.into_iter().next() else {
            return;
        };
        let msg_logger = logger.with("message_id", msg.id);

        // Check backoff before attempting delivery
        if !self.can_attempt_delivery(msg.id) {
            msg_logger.debug("skipping delivery: backoff active", &[]);
            return;
        }

        // Check if agent is idle
        let status = match self.service.status(&agent.name) {
            Ok(s) => s,
            Err(e) => {
                msg_logger.error("failed to check agent status", &[("error", &GoError(&e))]);
                return;
            }
        };
        let msg_logger = msg_logger.with("status", status.as_str());

        // UNKNOWN_THRESHOLD counts unknowns in a row: any known status ends a run.
        if status != AGENT_STATUS_UNKNOWN {
            self.reset_unknown_count(msg.id);
        }

        if status == AGENT_STATUS_IDLE || status == AGENT_STATUS_ERROR {
            // error means the last turn failed, not that the agent is gone: for
            // Claude it comes from StopFailure (API error, rate limit), after
            // which the CLI is back at its prompt and takes input like idle. It
            // is also sticky: no hook fires again until the next prompt
            // arrives, so holding messages for it would hold them forever.
            self.deliver_message(ctx, &agent.name, &msg, &msg_logger);
        } else if status == AGENT_STATUS_ENDED {
            // The CLI is gone; typing into the shell left behind would run the
            // message as a command. Nothing restarts it, so fail now.
            msg_logger.warn("agent's session ended, marking message as failed", &[]);
            self.fail_message(&msg, &msg_logger);
        } else if status == AGENT_STATUS_UNKNOWN {
            self.handle_unknown_status(&msg, &msg_logger);
        } else {
            // processing, or waiting on a human: deliver once it's idle again
            msg_logger.debug("agent busy, skipping delivery", &[]);
        }
    }

    fn fail_message(&self, msg: &Message, logger: &Logger) {
        if let Err(e) = self
            .storage
            .update_message_status(msg.id, MESSAGE_STATUS_FAILED)
        {
            logger.error(
                "failed to update message status",
                &[("error", &GoError(&e))],
            );
            return;
        }
        self.clear_state(msg.id);
    }

    fn deliver_message(&self, ctx: &Context, agent_name: &str, msg: &Message, logger: &Logger) {
        // Claim every pending message (and any unreported superseded rows)
        // atomically so they arrive coalesced in one prompt instead of one
        // tick at a time.
        let claimed = match self
            .storage
            .claim_pending_batch(agent_name, "watcher", None)
        {
            Ok(c) => c,
            Err(e) => {
                logger.error("failed to claim message", &[("error", &GoError(&e))]);
                return;
            }
        };
        if claimed.is_empty() {
            self.clear_state(msg.id);
            logger.debug("message already delivered", &[]);
            return;
        }
        let now = agent_store::Time::now();
        let batch = crate::to_coalesced(&claimed, &now);
        let text = agent_backend::format_coalesced_batch(&batch, false);
        let res = self
            .service
            .check_can_send(&match self.service.get_agent(agent_name) {
                Ok(a) => a,
                Err(e) => {
                    logger.error("failed to look up agent", &[("error", &GoError(&e))]);
                    let _ = self.storage.requeue_batch(&claimed);
                    self.handle_delivery_failure(msg, logger);
                    return;
                }
            })
            .and_then(|_| self.service.send_message(ctx, agent_name, &text, true));
        if let Err(e) = res {
            logger.error("failed to send message", &[("error", &GoError(&e))]);
            if let Err(e) = self.storage.requeue_batch(&claimed) {
                logger.error(
                    "failed to update message status",
                    &[("error", &GoError(&e))],
                );
            }
            self.handle_delivery_failure(msg, logger);
            return;
        }

        // Clean up state for all claimed messages
        for m in &claimed {
            self.clear_state(m.id);
        }
        logger.info("message delivered successfully", &[]);
    }

    fn handle_delivery_failure(&self, msg: &Message, logger: &Logger) {
        let retry_count = {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(msg.id).or_default();
            state.retry_count += 1;
            state.last_attempt = Some(Instant::now());
            state.retry_count
        };

        if retry_count >= MAX_RETRIES {
            logger.warn(
                "max retries exceeded, marking message as failed",
                &[("retry_count", &retry_count), ("max_retries", &MAX_RETRIES)],
            );
            self.fail_message(msg, logger);
            return;
        }

        let backoff = calculate_backoff(retry_count);
        logger.info(
            "delivery failed, will retry",
            &[
                ("retry_count", &retry_count),
                ("next_backoff", &GoDuration(backoff)),
            ],
        );
    }

    fn handle_unknown_status(&self, msg: &Message, logger: &Logger) {
        let unknown_count = {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(msg.id).or_default();
            state.unknown_count += 1;
            state.unknown_count
        };

        if unknown_count >= UNKNOWN_THRESHOLD {
            logger.warn(
                "unknown status threshold exceeded, marking message as failed",
                &[
                    ("unknown_count", &unknown_count),
                    ("threshold", &UNKNOWN_THRESHOLD),
                ],
            );
            self.fail_message(msg, logger);
            return;
        }

        logger.debug(
            "agent status unknown, will retry",
            &[
                ("unknown_count", &unknown_count),
                ("threshold", &UNKNOWN_THRESHOLD),
            ],
        );
    }

    fn can_attempt_delivery(&self, msg_id: i64) -> bool {
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = states.get(&msg_id) else {
            return true;
        };
        if state.retry_count == 0 {
            return true;
        }
        let backoff = calculate_backoff(state.retry_count);
        state.last_attempt.is_none_or(|t| t.elapsed() >= backoff)
    }

    fn reset_unknown_count(&self, msg_id: i64) {
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = states.get_mut(&msg_id) {
            state.unknown_count = 0;
        }
    }

    fn clear_state(&self, msg_id: i64) {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&msg_id);
    }
}

/// Exponential backoff: BASE_BACKOFF * 2^(retry_count-1).
fn calculate_backoff(retry_count: u32) -> Duration {
    BASE_BACKOFF * (1u32 << (retry_count.max(1) - 1))
}

#[cfg(test)]
mod watcher_internal_test {
    use std::collections::HashMap;

    use agent_backend::Registry;
    use agent_base::config::Config;
    use agent_store::Time;
    use agent_termd::Session;

    use super::*;
    use crate::service::{
        AGENT_STATUS_PROCESSING, AGENT_STATUS_WAITING, AgentStatus, StatusReport,
    };

    /// A mux where every session exists and nothing happens.
    struct NopMux;

    impl agent_termd::Ops for NopMux {
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
        fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn paste_text(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
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
    }

    // The unknown threshold counts unknowns in a row: busy checks between them
    // start the count over, so an agent that is mostly busy keeps its message.
    #[test]
    fn unknown_count_resets_on_known_status() {
        let dir = tempfile::Builder::new()
            .prefix("agent-watcher-test-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
        let cfg = Config {
            session_prefix: "test-".to_string(),
            ..Default::default()
        };
        let svc = Arc::new(AgentService::new(
            cfg,
            Arc::clone(&store),
            Arc::new(NopMux),
            Arc::new(Registry::new()),
        ));

        let agent = Agent {
            name: "worker".to_string(),
            session_name: "test-worker".to_string(),
            backend: "claude".to_string(),
            created_at: Time::now(),
            ..Default::default()
        };
        store.create_agent(&agent).unwrap();
        let mut msg = Message {
            sender_id: "coordinator".to_string(),
            receiver_id: "worker".to_string(),
            content: "next task".to_string(),
            status: MESSAGE_STATUS_PENDING,
            created_at: Time::now(),
            ..Default::default()
        };
        store.create_message(&mut msg).unwrap();

        let w = new_watcher(
            Arc::clone(&svc),
            Arc::clone(&store),
            Duration::from_secs(3600),
            [with_logger(Logger::discard())],
        );
        let ctx = Context::background();
        let check = |status: AgentStatus| -> bool {
            svc.report(
                "worker",
                StatusReport {
                    status,
                    event: "test".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
            w.process_agent(&ctx, &agent);
            store.get_pending_messages("worker", 1).unwrap().len() == 1
        };

        let busy = [AGENT_STATUS_PROCESSING, AGENT_STATUS_WAITING];
        for i in 0..2 * UNKNOWN_THRESHOLD as usize {
            assert!(
                check(AGENT_STATUS_UNKNOWN) && check(busy[i % 2].clone()),
                "message failed after {} unknowns, each followed by a busy status",
                i + 1
            );
        }
        for i in 1..UNKNOWN_THRESHOLD {
            assert!(
                check(AGENT_STATUS_UNKNOWN),
                "message failed after {i} unknowns in a row, want {UNKNOWN_THRESHOLD}"
            );
        }
        assert!(
            !check(AGENT_STATUS_UNKNOWN),
            "message still pending after {UNKNOWN_THRESHOLD} unknowns in a row, want failed"
        );
    }

    #[test]
    fn slog_values_quote_as_slog_does() {
        assert_eq!(slog_value("idle"), "idle");
        assert_eq!(
            slog_value("agent busy, skipping delivery"),
            "\"agent busy, skipping delivery\""
        );
        assert_eq!(slog_value(""), "\"\"");
        assert_eq!(slog_value("a=b"), "\"a=b\"");
        assert_eq!(GoDuration(Duration::from_secs(2)).to_string(), "2s");
    }
}

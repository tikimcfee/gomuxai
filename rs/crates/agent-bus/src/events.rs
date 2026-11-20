//! Status events and `wait` (`internal/natsbus/events.go`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use agent_base::gostd::strconv::quote;
use agent_base::gostd::time::format_duration;
use agent_base::util::sanitize_terminal;
use futures::StreamExt;

use crate::client::{Conn, LocalConnector, Msg, server_flush};
use crate::contract::{
    ErrNoBus, FocusEvent, StatusEvent, is_no_bus, is_valid_name, subject_focus, subject_status,
    validate_agent, validate_host,
};
use crate::gotime::Time;
use crate::runtime::{Context, block_on};

/// Bounds how long a short-lived process waits for its buffered events to reach
/// the bus before it exits.
pub const DEFAULT_FLUSH_TIMEOUT: Duration = Duration::from_millis(200);

const SUBSCRIBE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_RECHECK_DELAY: Duration = Duration::from_millis(1500);

/// Publishes `ev` on `subject_status(ev.host, ev.agent)`. Like any core NATS
/// publish it only buffers: flush before the process exits.
pub fn publish_status(nc: &Conn, ev: &StatusEvent) -> anyhow::Result<()> {
    validate_host(&ev.host)?;
    validate_agent(&ev.agent)?;
    let data = serde_json::to_vec(ev).map_err(|e| anyhow::anyhow!("encoding status event: {e}"))?;
    nc.publish(&subject_status(&ev.host, &ev.agent), data)
}

/// Publishes `ev` on `subject_focus(ev.host)`. `ev.agent` may be `""` when
/// clearing focus (`agent focus --clear`), otherwise it must be a valid agent
/// name. Like any core NATS publish it only buffers: flush before exiting.
pub fn publish_focus(nc: &Conn, ev: &FocusEvent) -> anyhow::Result<()> {
    validate_host(&ev.host)?;
    if !ev.agent.is_empty() {
        validate_agent(&ev.agent)?;
    }
    let data = serde_json::to_vec(ev).map_err(|e| anyhow::anyhow!("encoding focus event: {e}"))?;
    nc.publish(&subject_focus(&ev.host), data)
}

#[derive(Clone)]
enum SavedConnectError {
    NoBus,
    Closed,
    Other(String),
}

impl SavedConnectError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::NoBus => ErrNoBus.into(),
            Self::Closed => anyhow::anyhow!("nats: connection closed"),
            Self::Other(msg) => anyhow::anyhow!("{msg}"),
        }
    }
}

struct PublisherState {
    tried: bool,
    nc: Option<Conn>,
    err: Option<SavedConnectError>,
    last_kind: &'static str,
}

/// Publishes status and focus events over a connection it opens on first use,
/// so a process that never publishes never touches the bus.
pub struct Publisher {
    connect: LocalConnector,
    state: Mutex<PublisherState>,
}

impl Publisher {
    pub fn new(connect: impl Fn() -> anyhow::Result<Conn> + Send + Sync + 'static) -> Self {
        Self {
            connect: Box::new(connect),
            state: Mutex::new(PublisherState {
                tried: false,
                nc: None,
                err: None,
                last_kind: "events",
            }),
        }
    }

    pub fn publish_status(&self, ev: &StatusEvent) -> anyhow::Result<()> {
        let nc = self.conn_for("status events")?;
        publish_status(&nc, ev)
    }

    pub fn publish_focus(&self, ev: &FocusEvent) -> anyhow::Result<()> {
        let nc = self.conn_for("focus events")?;
        publish_focus(&nc, ev)
    }

    fn conn_for(&self, kind: &'static str) -> anyhow::Result<Conn> {
        let mut st = self.state.lock().unwrap();
        st.last_kind = kind;
        if !st.tried {
            st.tried = true;
            match (self.connect)() {
                Ok(c) => st.nc = Some(c),
                Err(e) => {
                    st.err = Some(if is_no_bus(&e) {
                        SavedConnectError::NoBus
                    } else {
                        SavedConnectError::Other(format!("{e:#}"))
                    });
                }
            }
        }
        if let Some(ref nc) = st.nc {
            return Ok(nc.clone());
        }
        Err(st
            .err
            .clone()
            .unwrap_or(SavedConnectError::NoBus)
            .into_anyhow())
    }

    /// Waits up to `timeout` until the bus has received everything published so
    /// far. No-op if nothing was published.
    pub fn flush(&self, timeout: Duration) -> anyhow::Result<()> {
        let (nc, kind) = {
            let st = self.state.lock().unwrap();
            (st.nc.clone(), st.last_kind)
        };
        let Some(nc) = nc else {
            return Ok(());
        };
        nc.flush_timeout(timeout)
            .map_err(|e| anyhow::anyhow!("flushing {kind}: {e}"))
    }

    /// Flushes (bounded by [`DEFAULT_FLUSH_TIMEOUT`]) and closes the connection.
    /// Later publishes fail.
    pub fn close(&self) -> anyhow::Result<()> {
        let flush_res = self.flush(DEFAULT_FLUSH_TIMEOUT);
        let mut st = self.state.lock().unwrap();
        if let Some(nc) = st.nc.take() {
            nc.close();
        }
        st.tried = true;
        st.err = Some(SavedConnectError::Closed);
        flush_res
    }
}

/// Parses a message published on a status subject, rejecting any body whose
/// host or agent does not match the subject.
pub fn decode_status_event(msg: &Msg) -> anyhow::Result<StatusEvent> {
    let ev: StatusEvent = serde_json::from_slice(&msg.data)
        .map_err(|e| anyhow::anyhow!("decoding status event on {}: {e}", msg.subject))?;
    if !is_valid_name(&ev.host)
        || !is_valid_name(&ev.agent)
        || msg.subject != subject_status(&ev.host, &ev.agent)
    {
        anyhow::bail!(
            "status event on {} claims to be from {}@{}",
            msg.subject,
            quote(&ev.agent),
            quote(&ev.host)
        );
    }
    Ok(ev)
}

/// Parses a message published on a focus subject (`gomux.<host>.focus`),
/// rejecting any body whose host does not match the subject or whose non-empty
/// agent name is invalid, and sanitizing remote text fields.
pub fn decode_focus_event(msg: &Msg) -> anyhow::Result<FocusEvent> {
    let mut ev: FocusEvent = serde_json::from_slice(&msg.data)
        .map_err(|e| anyhow::anyhow!("decoding focus event on {}: {e}", msg.subject))?;
    if !is_valid_name(&ev.host) || msg.subject != subject_focus(&ev.host) {
        anyhow::bail!(
            "focus event on {} claims to be from host {}",
            msg.subject,
            quote(&ev.host)
        );
    }
    if !ev.agent.is_empty() && !is_valid_name(&ev.agent) {
        anyhow::bail!(
            "focus event on {} has invalid agent name {}",
            msg.subject,
            quote(&ev.agent)
        );
    }
    ev.session = sanitize_terminal(&ev.session);
    ev.by = sanitize_terminal(&ev.by);
    Ok(ev)
}

/// Blocking subscription yielding decoded, subject-verified [`FocusEvent`]s.
pub struct FocusSubscription {
    sub: Option<async_nats::Subscriber>,
}

impl FocusSubscription {
    /// Blocks until the next valid [`FocusEvent`] arrives, `ctx` ends, or the
    /// subscription closes. Malformed or forged messages are ignored.
    pub fn next(&mut self, ctx: &Context) -> anyhow::Result<FocusEvent> {
        let Some(sub) = self.sub.as_mut() else {
            anyhow::bail!("nats: subscription closed");
        };
        loop {
            let msg = block_on(async {
                tokio::select! {
                    biased;
                    cerr = ctx.done() => Err(anyhow::Error::new(cerr)),
                    maybe_msg = sub.next() => match maybe_msg {
                        Some(m) => Ok(Msg::from_async(m, None)),
                        None => Err(anyhow::anyhow!("nats: subscription closed")),
                    },
                }
            })?;
            if let Ok(ev) = decode_focus_event(&msg) {
                return Ok(ev);
            }
        }
    }

    /// Waits up to `timeout` for the next valid [`FocusEvent`], returning
    /// `Ok(None)` if `timeout` elapses before one arrives.
    pub fn next_timeout(&mut self, timeout: Duration) -> anyhow::Result<Option<FocusEvent>> {
        let (ctx, _cancel) = Context::with_timeout(&Context::background(), timeout);
        match self.next(&ctx) {
            Ok(ev) => Ok(Some(ev)),
            Err(e) if crate::runtime::is_deadline_exceeded(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Unsubscribes from focus events.
    pub fn unsubscribe(&mut self) -> anyhow::Result<()> {
        if let Some(mut sub) = self.sub.take() {
            block_on(async move { sub.unsubscribe().await }).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(())
    }
}

impl Drop for FocusSubscription {
    fn drop(&mut self) {
        let _ = self.unsubscribe();
    }
}

/// Subscribes to focus events on `host` (`Some("hub")` -> `gomux.hub.focus`,
/// `None` / `Some("")` / `Some("*")` -> `gomux.*.focus`) and flushes so events
/// published afterwards are guaranteed to arrive.
pub fn subscribe_focus(nc: &Conn, host: Option<&str>) -> anyhow::Result<FocusSubscription> {
    let subj = match host {
        None | Some("") | Some("*") => subject_focus("*"),
        Some(h) => {
            validate_host(h)?;
            subject_focus(h)
        }
    };
    let client = nc.inner().clone();
    let sub = block_on(async move {
        let sub = client
            .subscribe(subj)
            .await
            .map_err(|e| anyhow::anyhow!("subscribing to focus events: {e}"))?;
        server_flush(&client, SUBSCRIBE_FLUSH_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("subscribing to focus events: {e}"))?;
        Ok::<_, anyhow::Error>(sub)
    })?;
    Ok(FocusSubscription { sub: Some(sub) })
}

/// One real status check returned by [`WaitOptions::current`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observation {
    pub status: String,
    pub detail: String,
    pub reported_status: String,
    pub reported_at: Time,
    pub sent_at: Time,
    pub turn_at: Time,
}

/// Authoritative ground-truth check callback for [`WaitOptions`].
pub type CheckFn<'a> = Box<dyn FnMut(&Context) -> anyhow::Result<Observation> + 'a>;

/// Configures [`wait`] and [`wait_observation`].
pub struct WaitOptions<'a> {
    pub host: String,
    pub agent: String,
    pub for_statuses: Vec<String>,
    pub fail_statuses: Vec<String>,
    pub next: bool,
    pub after: Time,
    pub current: Option<CheckFn<'a>>,
    pub interval: Duration,
}

impl WaitOptions<'_> {
    fn arrived_since(&self, obs: &Observation) -> bool {
        if !self.for_statuses.iter().any(|s| s == &obs.reported_status) {
            return false;
        }
        if !self.after.is_zero() {
            return obs.reported_at.after(&self.after);
        }
        !obs.sent_at.is_zero()
            && obs.turn_at.after(&obs.sent_at)
            && obs.reported_at.after(&obs.turn_at)
    }
}

/// Blocks until the agent's status is one of `opts.for_statuses` and returns it,
/// or `(last_status, error)` on failure (matching Go's `(string, error)` return).
/// `nc` may be `None` (no bus), in which case `wait` only polls `opts.current`.
pub fn wait(
    ctx: &Context,
    nc: Option<&Conn>,
    opts: WaitOptions<'_>,
) -> Result<String, (String, anyhow::Error)> {
    match wait_observation(ctx, nc, opts) {
        Ok(obs) => Ok(obs.status),
        Err((obs, err)) => Err((obs.status, err)),
    }
}

/// Like [`wait`], returning the full [`Observation`] that ended the wait (or
/// `(last_observation, error)` on failure, matching Go's `(Observation, error)`).
#[allow(clippy::result_large_err)]
pub fn wait_observation(
    ctx: &Context,
    nc: Option<&Conn>,
    mut opts: WaitOptions<'_>,
) -> Result<Observation, (Observation, anyhow::Error)> {
    if opts.for_statuses.is_empty() {
        return Err((
            Observation::default(),
            anyhow::anyhow!("no statuses to wait for"),
        ));
    }
    if opts.current.is_none() {
        return Err((
            Observation::default(),
            anyhow::anyhow!("no current-status check"),
        ));
    }
    if opts.interval.is_zero() {
        return Err((
            Observation::default(),
            anyhow::anyhow!(
                "check interval must be positive, got {}",
                format_duration(opts.interval)
            ),
        ));
    }

    let mut subscriber: Option<async_nats::Subscriber> = if let Some(conn) = nc {
        let subj = subject_status(&opts.host, &opts.agent);
        let client = conn.inner().clone();
        let res: anyhow::Result<async_nats::Subscriber> = block_on(async {
            let sub = client
                .subscribe(subj)
                .await
                .map_err(|e| anyhow::anyhow!("subscribing to status events: {e}"))?;
            server_flush(&client, SUBSCRIBE_FLUSH_TIMEOUT)
                .await
                .map_err(|e| anyhow::anyhow!("subscribing to status events: {e}"))?;
            Ok(sub)
        });
        match res {
            Ok(s) => Some(s),
            Err(e) => return Err((Observation::default(), e)),
        }
    } else {
        None
    };

    // Unsubscribe cleanly on all exit paths.
    struct SubCleanup<'s>(&'s mut Option<async_nats::Subscriber>);
    impl Drop for SubCleanup<'_> {
        fn drop(&mut self) {
            if let Some(mut sub) = self.0.take() {
                let _ = block_on(async move { sub.unsubscribe().await });
            }
        }
    }
    let cleanup = SubCleanup(&mut subscriber);

    let mut last = Observation::default();
    let mut armed = !opts.next;
    let mut first: Option<Observation> = None;

    let check = |opts: &mut WaitOptions<'_>,
                 last: &mut Observation,
                 armed: &mut bool,
                 first: &mut Option<Observation>|
     -> (bool, Option<anyhow::Error>) {
        let current_fn = opts.current.as_mut().unwrap();
        let obs = match current_fn(ctx) {
            Ok(o) => o,
            Err(e) => {
                if let Some(cerr) = ctx.err() {
                    return (
                        true,
                        Some(
                            anyhow::Error::new(cerr).context(format!("waiting for {}", opts.agent)),
                        ),
                    );
                }
                return (true, Some(e));
            }
        };
        *last = obs.clone();
        match first {
            None => *first = Some(obs.clone()),
            Some(f) => {
                if !*armed
                    && obs.reported_at.after(&f.reported_at)
                    && opts.for_statuses.iter().any(|s| s == &obs.reported_status)
                {
                    *armed = true;
                }
            }
        }
        if !*armed && opts.arrived_since(&obs) {
            *armed = true;
        }
        if opts.for_statuses.iter().any(|s| s == &obs.status) {
            return (*armed, None);
        }
        if opts.fail_statuses.iter().any(|s| s == &obs.status) {
            return (
                true,
                Some(anyhow::anyhow!(
                    "agent {}: status is {}, so it won't become {}",
                    opts.agent,
                    obs.status,
                    opts.for_statuses.join(" or ")
                )),
            );
        }
        *armed = true;
        (false, None)
    };

    let (done, err) = check(&mut opts, &mut last, &mut armed, &mut first);
    if done {
        drop(cleanup);
        return match err {
            None => Ok(last),
            Some(e) => Err((last, e)),
        };
    }

    let interval = opts.interval;
    let mut next_tick = Instant::now() + interval;
    let mut recheck_at: Option<Instant> = None;

    enum Wake {
        Ctx(crate::runtime::ContextError),
        Msg(Box<Msg>),
        Recheck,
        Tick,
    }

    loop {
        let wake = block_on(async {
            let sub = cleanup.0.as_mut();
            let tick_sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(next_tick));
            let recheck_fut = async {
                match recheck_at {
                    Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                    None => std::future::pending().await,
                }
            };
            let msg_fut = async {
                match sub {
                    Some(s) => match s.next().await {
                        Some(m) => Msg::from_async(m, None),
                        None => std::future::pending().await,
                    },
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                cerr = ctx.done() => Wake::Ctx(cerr),
                m = msg_fut => Wake::Msg(Box::new(m)),
                () = recheck_fut => Wake::Recheck,
                () = tick_sleep => Wake::Tick,
            }
        });

        match wake {
            Wake::Ctx(cerr) => {
                let err = anyhow::Error::new(cerr).context(format!("waiting for {}", opts.agent));
                drop(cleanup);
                return Err((last, err));
            }
            Wake::Msg(msg) => {
                let Ok(ev) = decode_status_event(&msg) else {
                    continue;
                };
                if opts.for_statuses.iter().any(|s| s == &ev.status)
                    || opts.fail_statuses.iter().any(|s| s == &ev.status)
                {
                    let (done, err) = check(&mut opts, &mut last, &mut armed, &mut first);
                    if done {
                        drop(cleanup);
                        return match err {
                            None => Ok(last),
                            Some(e) => Err((last, e)),
                        };
                    }
                    recheck_at = Some(Instant::now() + EVENT_RECHECK_DELAY);
                    continue;
                }
                if !armed {
                    let (done, err) = check(&mut opts, &mut last, &mut armed, &mut first);
                    if done {
                        drop(cleanup);
                        return match err {
                            None => Ok(last),
                            Some(e) => Err((last, e)),
                        };
                    }
                }
            }
            Wake::Recheck => {
                recheck_at = None;
                let (done, err) = check(&mut opts, &mut last, &mut armed, &mut first);
                if done {
                    drop(cleanup);
                    return match err {
                        None => Ok(last),
                        Some(e) => Err((last, e)),
                    };
                }
            }
            Wake::Tick => {
                let now = Instant::now();
                while next_tick <= now {
                    next_tick += interval;
                }
                let (done, err) = check(&mut opts, &mut last, &mut armed, &mut first);
                if done {
                    drop(cleanup);
                    return match err {
                        None => Ok(last),
                        Some(e) => Err((last, e)),
                    };
                }
            }
        }
    }
}

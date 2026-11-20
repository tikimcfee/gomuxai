//! `agent wait` (Go: internal/cli/wait.go).
use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use agent_base::gostd::time as gotime;
use agent_base::util::sanitize_terminal;
use agent_bus::{
    AgentRef, CheckFn, Conn, ContextError, Observation, RpcClient, Time as BusTime, WaitOptions,
    is_deadline_exceeded, is_no_bus, parse_agent_ref,
};
use agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_WAITING, AgentNotFoundError, Context,
    one_line, parse_agent_status,
};

use super::command::{ArgRule, Def, Doc};
use super::exit::ExitError;
use super::flags;
use super::output::{plural, usage_error};
use super::remote::{canonical_host, is_remote_host};
use super::root::{Ctx, command};
use super::watch::{INTERRUPT_POLL, Interrupts};

// `agent wait`

pub const DOC: Doc = Doc {
    use_line: "wait [name[@host]...] --for <status[,status...]>",
    short: "Block until agents reach a status (exit 2 on timeout)",
    long: r#"Wait blocks until an agent's status is one of the given statuses, prints
it, and exits 0. Statuses: idle, processing, waiting, error, ended, unknown
("agent statuses" says what each means).

With --timeout, wait gives up after that long and exits 2. It exits 1 on
errors, including the agent exiting while you wait for another status.

Several agents: wait watches them all at once and prints "name: status" for
each as it gets there, so one that finishes early isn't missed. It returns
when every one has (or, with --any, at the first). An agent that exits or
can't be checked is reported on stderr and the others are still waited on;
wait then exits 1. A waiting or error line says what the agent is asking or
why, e.g. "helper: waiting (permission: Bash: rm -rf build)".

No names: every agent on this host but yourself, with --any or --follow.
"agent wait --any --for waiting" blocks until any of them needs a person and
says which one and what it's asking.

--follow keeps going: it prints a line each time one of the agents arrives
at a status (each arrival once; an agent must leave and come back to be
printed again), until Ctrl-C or --timeout. With no names it also picks up
agents spawned while it runs. For a person in a spare terminal, or a script
that reads lines.

With an agent bus running, wait reacts to status events as soon as they are
reported, and re-checks the status every 5s as a backstop for changes no hook
reports (e.g. an interrupted permission prompt going back to idle). Without a
bus it checks every 2s. --interval overrides either.

A status reached after wait starts counts even if the agent has moved on by
the time wait returns.

An agent on another host is addressed as name@host (needs a running bus);
its host's own status check confirms events, as for a local agent.

Right after "agent send", the agent still looks idle from its previous turn
until it picks the message up, so "--for idle" would return at once. Use
--next: it waits for the turn the last send started. It ignores the given
statuses until the agent has been seen in another one (e.g. processing), or
until that turn has reported one of them, so a turn that ended before wait
started still counts. Waiting again with --next and no new send returns that
same turn's status at once; to see each new arrival, use --follow.

An agent blocked on a permission prompt stays "waiting" until someone
answers, so a coordinator usually waits with --for idle,waiting and checks
which it got. Without "waiting" in --for, wait says on stderr when it sees
the agent waiting, and keeps waiting.

For a Claude agent, its reply is then in "agent state get <name> _result"."#,
    example: r#"  agent send helper "review auth.go" && agent wait helper --for idle,waiting --next --timeout 30m
  agent wait lead-api lead-db --for idle,waiting --next --timeout 30m
  agent wait lead-api lead-db --any --for idle,waiting --next
  agent wait --any --for waiting          # until any agent here needs a person
  agent wait --follow --for waiting       # each prompt, as it comes, until Ctrl-C
  agent wait helper@hub --for idle,waiting --timeout 10m"#,
    args: ArgRule::Any,
    required: &["for"],
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent wait`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "any", help = "With several agents, return when the first one gets there")]
    pub any: bool,
    #[arg(long = "follow", help = "Print every arrival until Ctrl-C or --timeout")]
    pub follow: bool,
    #[arg(
        long = "for",
        value_name = "string",
        default_value = "",
        help = "Status(es) to wait for, comma-separated (required)",
    )]
    pub for_: String,
    #[arg(
        long = "interval",
        value_name = "duration",
        default_value = "0s",
        value_parser = flags::parse_duration,
        help = "Status re-check interval (default: 5s with a bus, 2s without)",
    )]
    pub interval: Duration,
    #[arg(
        long = "next",
        help = "Wait for the turn the last send started, not a status the agent is already in",
    )]
    pub next: bool,
    #[arg(
        long = "timeout",
        value_name = "duration",
        default_value = "0s",
        value_parser = flags::parse_duration,
        help = "Give up after this long and exit 2 (default: wait forever)",
    )]
    pub timeout: Duration,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

// Default status re-check intervals for wait. With a bus, events carry the
// transitions and the check is only a backstop; without one, it is the only
// source. A check reads the backend CLI's session registry and may read the
// screen, so keep it slow.
const WAIT_BACKSTOP_INTERVAL: Duration = Duration::from_secs(5);
const WAIT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often wait --follow with no names looks for agents spawned since it
/// started, in milliseconds (an atomic for tests).
static WAIT_RELIST_INTERVAL_MS: AtomicU64 = AtomicU64::new(10_000);

/// Tells a timeout apart from an error (exit 1) in scripts.
pub const WAIT_TIMEOUT_EXIT_CODE: i32 = 2;

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let statuses = parse_wait_statuses(&args.for_)?;
    let refs = parse_wait_refs(ctx, &args.args)?;
    let every_agent = refs.is_empty();
    if every_agent && !args.any && !args.follow {
        return Err(usage_error(
            &command("wait"),
            anyhow::anyhow!(
                "name the agents to wait on, or add --any or --follow to watch every agent on this host"
            ),
        ));
    }

    let mut remote: Option<&AgentRef> = None; // the first agent on another host, if any
    for r in &refs {
        if is_remote_host(ctx, &r.host) {
            if remote.is_none() {
                remote = Some(r);
            }
            continue;
        }
        // A typo would otherwise wait out the whole timeout on the others.
        ctx.service()?.agent(&r.name)?;
    }

    let nc = match ctx.bus_connect() {
        Ok(nc) => Some(nc),
        Err(e) => {
            if let Some(r) = remote {
                return Err(e.context(format!("waiting on {r} needs a running agent bus")));
            }
            if !is_no_bus(&e) {
                let _ = writeln!(
                    ctx.err(),
                    "agent wait: can't use the bus ({e:#}); polling instead"
                );
            }
            None
        }
    };

    let mut interval = args.interval;
    if interval.is_zero() {
        interval = if nc.is_some() {
            WAIT_BACKSTOP_INTERVAL
        } else {
            WAIT_POLL_INTERVAL
        };
    }
    let mut fail = Vec::new();
    // An ended agent never reaches anything else.
    if !statuses.iter().any(|s| s == AGENT_STATUS_ENDED.as_str()) {
        fail.push(AGENT_STATUS_ENDED.to_string());
    }
    let w = Waiter {
        ctx,
        nc: nc.as_ref(),
        statuses,
        fail,
        interval,
        args,
        mu: Mutex::new(()),
    };

    let (wctx, cancel) = if args.timeout.is_zero() {
        Context::with_cancel(&Context::background())
    } else {
        Context::with_timeout(&Context::background(), args.timeout)
    };
    let res = if args.follow {
        w.follow(&wctx, refs)
    } else if refs.len() == 1 {
        w.one(&wctx, &refs[0])
    } else {
        let refs = if every_agent {
            let found = other_agents(ctx)?;
            if found.is_empty() {
                anyhow::bail!(
                    "no other agents on host {} to wait on: agent list shows them",
                    ctx.cfg().host
                );
            }
            found
        } else {
            refs
        };
        w.several(&wctx, &refs, every_agent)
    };
    cancel.cancel();
    if let Some(nc) = &nc {
        nc.close();
    }
    res
}

/// Parses wait's agent names, dropping repeats.
fn parse_wait_refs(ctx: &Ctx, args: &[String]) -> anyhow::Result<Vec<AgentRef>> {
    let mut refs: Vec<AgentRef> = Vec::new();
    for arg in args {
        let mut r = parse_agent_ref(arg)?;
        r.host = canonical_host(ctx, &r.host);
        if !refs.contains(&r) {
            refs.push(r);
        }
    }
    Ok(refs)
}

/// Every agent on this host except the caller.
fn other_agents(ctx: &Ctx) -> anyhow::Result<Vec<AgentRef>> {
    let agents = ctx.service()?.list()?;
    let me = ctx.non_interactive_caller_agent();
    Ok(agents
        .into_iter()
        .filter(|a| a.name != me)
        .map(|a| AgentRef::local(a.name))
        .collect())
}

/// What every agent's wait shares.
struct Waiter<'a> {
    ctx: &'a Ctx,
    /// None without a bus
    nc: Option<&'a Conn>,
    statuses: Vec<String>,
    fail: Vec<String>,
    interval: Duration,
    args: &'a Args,
    /// serializes output and hints from several agents' waits
    mu: Mutex<()>,
}

/// How one agent's wait ended: what it last saw, and why it stopped if it
/// didn't get there.
type WaitResult = Result<Observation, (Observation, anyhow::Error)>;

impl Waiter<'_> {
    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits for one agent; see agent_bus::WaitOptions for next and after.
    #[allow(clippy::result_large_err)] // agent_bus::wait_observation's shape
    fn wait(&self, wctx: &Context, r: &AgentRef, next: bool, after: BusTime) -> WaitResult {
        let host = if is_remote_host(self.ctx, &r.host) {
            r.host.clone()
        } else {
            self.ctx.cfg().host.clone()
        };
        agent_bus::wait_observation(
            wctx,
            self.nc,
            WaitOptions {
                host,
                agent: r.name.clone(),
                for_statuses: self.statuses.clone(),
                fail_statuses: self.fail.clone(),
                next,
                after,
                current: Some(self.current(r)),
                interval: self.interval,
            },
        )
    }

    /// The status check for r: the service locally, the remote host's own
    /// status check otherwise.
    fn current<'s>(&'s self, r: &'s AgentRef) -> CheckFn<'s> {
        let remote = is_remote_host(self.ctx, &r.host);
        let client = match (remote, self.nc) {
            (true, Some(nc)) => Some(RpcClient::new(nc.clone(), Duration::ZERO)),
            _ => None,
        };
        let check = move |c: &Context| -> anyhow::Result<Observation> {
            if let Some(client) = &client {
                let resp = client.status(c, r)?;
                return Ok(Observation {
                    status: resp.status,
                    detail: resp.detail,
                    reported_status: resp.reported_status,
                    reported_at: resp.reported_at,
                    sent_at: resp.sent_at,
                    turn_at: resp.turn_at,
                });
            }
            let info = self.ctx.service()?.status_info(&r.name)?;
            Ok(Observation {
                status: info.status.to_string(),
                detail: info.detail,
                reported_status: info.reported_status.to_string(),
                reported_at: BusTime(info.reported_at.0),
                sent_at: BusTime(info.sent_at.0),
                turn_at: BusTime(info.turn_at.0),
            })
        };
        if self.statuses.iter().any(|s| s == AGENT_STATUS_WAITING.as_str()) {
            return Box::new(check);
        }
        // Blocked on a person, the agent won't reach the statuses until
        // someone answers: say so once, from the status check that saw it.
        Box::new(move |c: &Context| {
            let obs = check(c);
            if let Ok(o) = &obs
                && o.status == AGENT_STATUS_WAITING.as_str()
            {
                let _g = self.lock();
                self.ctx.hint(format!(
                    "{r} is waiting on a person (a permission prompt, a question, a workspace trust prompt, or a dialog of its CLI's own), so it won't be {} until someone answers: agent status {r} -v shows what it's asking (--for {},waiting returns on prompts)",
                    self.statuses.join(" or "),
                    self.statuses.join(",")
                ));
            }
            obs
        })
    }

    /// Waits for a single named agent. Its output and errors are wait's
    /// original ones, which scripts rely on.
    fn one(&self, wctx: &Context, r: &AgentRef) -> anyhow::Result<()> {
        let obs = match self.wait(wctx, r, self.args.next, BusTime::default()) {
            Ok(obs) => obs,
            Err((obs, e)) if is_deadline_exceeded(&e) => {
                return Err(ExitError::new(
                    WAIT_TIMEOUT_EXIT_CODE,
                    anyhow::anyhow!(
                        "timed out after {} waiting for {r} to be {} (last status: {})",
                        gotime::format_duration(self.args.timeout),
                        self.statuses.join(" or "),
                        or_unknown(&obs.status)
                    ),
                )
                .into());
            }
            Err((_, e)) => return Err(e),
        };
        self.ctx.write_out(&format!("{r}: {}\n", obs.status))?;
        if obs.status == AGENT_STATUS_WAITING.as_str() && !obs.detail.is_empty() {
            self.ctx.hint(format!(
                "{r} is asking: {}; agent status {r} -v shows all of it, then agent approve {r} or agent deny {r}",
                one_line_detail(&obs.detail)
            ));
        }
        Ok(())
    }

    /// Waits for refs at once, until all of them (or, with --any, the first)
    /// get there. every_agent says refs weren't named: an agent whose session
    /// has ended is then skipped, not reported.
    fn several(&self, wctx: &Context, refs: &[AgentRef], every_agent: bool) -> anyhow::Result<()> {
        let (sctx, cancel) = Context::with_cancel(wctx);
        let (tx, rx) = mpsc::channel::<(AgentRef, WaitResult)>();
        std::thread::scope(|s| {
            for r in refs {
                let tx = tx.clone();
                let sctx = sctx.clone();
                s.spawn(move || {
                    let res = self.wait(&sctx, r, self.args.next, BusTime::default());
                    let _ = tx.send((r.clone(), res));
                });
            }
            drop(tx);
            let res = self.collect(&rx, refs, every_agent);
            // The waits still running end with the context (Go: defer cancel()).
            cancel.cancel();
            res
        })
    }

    /// several's reading of the waits' outcomes, as they come.
    fn collect(
        &self,
        rx: &mpsc::Receiver<(AgentRef, WaitResult)>,
        refs: &[AgentRef],
        every_agent: bool,
    ) -> anyhow::Result<()> {
        let (mut failed, mut timed_out) = (Vec::new(), Vec::new());
        let mut reached = 0;
        for _ in refs {
            let Ok((r, res)) = rx.recv() else { break };
            match res {
                Ok(obs) => {
                    reached += 1;
                    let line = arrival_line(&r, &obs);
                    {
                        let _g = self.lock();
                        self.ctx.write_out(&format!("{line}\n"))?;
                    }
                    if self.args.any {
                        return Ok(());
                    }
                }
                Err((obs, e)) if is_deadline_exceeded(&e) => {
                    timed_out.push(format!("{r} {}", or_unknown(&obs.status)));
                }
                // Not one you asked about, and it will never need anything.
                Err((obs, _)) if every_agent && obs.status == AGENT_STATUS_ENDED.as_str() => {}
                Err((obs, e)) => {
                    {
                        let _g = self.lock();
                        let _ = writeln!(self.ctx.err(), "agent wait: {e:#}");
                    }
                    failed.push(format!("{r} ({})", or_unknown(&obs.status)));
                }
            }
        }

        let want = self.statuses.join(" or ");
        if !failed.is_empty() {
            let mut left = failed;
            if !timed_out.is_empty() {
                left.push(format!("and timed out on {}", timed_out.join(", ")));
            }
            anyhow::bail!(
                "{reached} of {} reached {want}; not {}",
                plural(refs.len(), "agent"),
                left.join(", ")
            );
        }
        if !timed_out.is_empty() {
            let which = if self.args.any { "for any of" } else { "for" };
            return Err(ExitError::new(
                WAIT_TIMEOUT_EXIT_CODE,
                anyhow::anyhow!(
                    "timed out after {} waiting {which} {} to be {want} (last status: {})",
                    gotime::format_duration(self.args.timeout),
                    plural(timed_out.len(), "agent"),
                    timed_out.join(", ")
                ),
            )
            .into());
        }
        if reached == 0 {
            anyhow::bail!(
                "none of the {} can become {want}: their sessions have ended",
                plural(refs.len(), "agent")
            );
        }
        Ok(())
    }

    /// Prints every arrival of refs (every agent on this host, if none) until
    /// interrupted or wctx ends.
    fn follow(&self, wctx: &Context, refs: Vec<AgentRef>) -> anyhow::Result<()> {
        let (fctx, cancel) = Context::with_cancel(wctx);
        let interrupts = Interrupts::new();
        let every_agent = refs.is_empty();
        let (ended_tx, ended_rx) = mpsc::channel::<AgentRef>();
        let watching: Mutex<HashSet<AgentRef>> = Mutex::new(HashSet::new());

        // The scope waits for every watch, so Ctrl-C must end fctx before
        // leaving it. (The Go deferred wg.Wait() ahead of cancel(), and so
        // hung on Ctrl-C without --timeout.)
        let res = std::thread::scope(|s| -> anyhow::Result<()> {
            // watch follows one agent until its wait fails or fctx ends.
            let watch = |r: AgentRef| {
                let fctx = fctx.clone();
                let ended_tx = ended_tx.clone();
                move || {
                    let (mut next, mut after) = (self.args.next, BusTime::default());
                    loop {
                        let res = self.wait(&fctx, &r, next, after);
                        if fctx.is_done() {
                            return;
                        }
                        {
                            let _g = self.lock();
                            match &res {
                                Ok(obs) => {
                                    let _ = self.ctx.write_out(&format!("{}\n", arrival_line(&r, obs)));
                                }
                                Err((obs, _))
                                    if every_agent && obs.status == AGENT_STATUS_ENDED.as_str() => {}
                                Err((_, e))
                                    if every_agent && e.downcast_ref::<AgentNotFoundError>().is_some() => {}
                                Err((_, e)) => {
                                    let _ = writeln!(
                                        self.ctx.err(),
                                        "agent wait: {e:#}; no longer watching {r}"
                                    );
                                }
                            }
                        }
                        let obs = match res {
                            Ok(obs) => obs,
                            Err(_) => {
                                let _ = ended_tx.send(r);
                                return;
                            }
                        };
                        // Each arrival once: the next one must be newer.
                        next = true;
                        after = obs.reported_at;
                        if after.is_zero() {
                            after = BusTime::now();
                        }
                    }
                }
            };
            let start = |refs: &[AgentRef]| {
                let mut w = watching.lock().unwrap_or_else(|e| e.into_inner());
                for r in refs {
                    if w.insert(r.clone()) {
                        s.spawn(watch(r.clone()));
                    }
                }
            };

            let mut refs = refs;
            let relist_every = Duration::from_millis(WAIT_RELIST_INTERVAL_MS.load(Ordering::Relaxed));
            let mut next_relist = None; // stays None for named agents
            if every_agent {
                refs = other_agents(self.ctx)?;
                next_relist = Some(Instant::now() + relist_every);
            }
            start(&refs);
            let who = if every_agent {
                format!(
                    "every agent on host {} ({} now)",
                    self.ctx.cfg().host,
                    plural(refs.len(), "agent")
                )
            } else {
                refs.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            };
            self.ctx.notice(format!(
                "Waiting for {who} to be {}, each time. Ctrl-C stops.",
                self.statuses.join(" or ")
            ));

            let mut live = refs.len();
            loop {
                if interrupts.fired() {
                    cancel.cancel();
                    return Ok(());
                }
                if let Some(err) = wctx.err() {
                    // The parent ended: the watches end with it.
                    if err == ContextError::DeadlineExceeded {
                        return Err(ExitError::new(
                            WAIT_TIMEOUT_EXIT_CODE,
                            anyhow::anyhow!(
                                "stopped following after {} (--timeout)",
                                gotime::format_duration(self.args.timeout)
                            ),
                        )
                        .into());
                    }
                    return Err(err.into());
                }
                if let Some(at) = next_relist
                    && Instant::now() >= at
                {
                    next_relist = Some(at + relist_every);
                    // The next relist may work; the agents already watched
                    // still are.
                    if let Ok(found) = other_agents(self.ctx) {
                        {
                            let w = watching.lock().unwrap_or_else(|e| e.into_inner());
                            live += found.iter().filter(|r| !w.contains(*r)).count();
                        }
                        start(&found);
                    }
                }
                match ended_rx.recv_timeout(INTERRUPT_POLL) {
                    Ok(r) => {
                        // A respawn under the same name is picked up by the
                        // next relist.
                        watching
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&r);
                        live -= 1;
                        if live == 0 && !every_agent {
                            anyhow::bail!("no agents left to follow");
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {}
                }
            }
        });
        cancel.cancel();
        res
    }
}

/// One agent's arrival for several agents' waits: "name: status", plus what
/// it's asking when waiting, or why when error.
fn arrival_line(r: &AgentRef, obs: &Observation) -> String {
    let mut line = format!("{r}: {}", obs.status);
    if (obs.status == AGENT_STATUS_WAITING.as_str() || obs.status == AGENT_STATUS_ERROR.as_str())
        && !obs.detail.is_empty()
    {
        line += &format!(" ({})", one_line_detail(&obs.detail));
    }
    line
}

/// Shortens a status detail to one line that can't write escape sequences to
/// the terminal.
fn one_line_detail(detail: &str) -> String {
    one_line(&sanitize_terminal(detail))
}

fn or_unknown(status: &str) -> &str {
    if status.is_empty() { "unknown" } else { status }
}

/// Validates a comma-separated --for list.
fn parse_wait_statuses(s: &str) -> anyhow::Result<Vec<String>> {
    let mut statuses = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        statuses.push(parse_agent_status(part)?.to_string());
    }
    if statuses.is_empty() {
        anyhow::bail!("--for needs at least one status");
    }
    Ok(statuses)
}

#[cfg(test)]
mod wait_test {
    use std::sync::Arc;

    use agent_service::{
        AGENT_STATUS_IDLE, AGENT_STATUS_PROCESSING, AgentStatus, STATE_KEY_SENT_AT, StatusReport,
    };
    use agent_store::{Agent, StateEntry, Time};

    use super::*;
    use crate::cli::exit::exit_code;
    use crate::cli::root::SharedBuf;
    use crate::cli::testenv::TestEnv;

    fn words(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// wait's flags as a test sets them: interval 20ms, the rest off.
    fn args(for_: &str, names: &[&str]) -> Args {
        Args {
            any: false,
            follow: false,
            for_: for_.to_string(),
            interval: Duration::from_millis(20),
            next: false,
            timeout: Duration::ZERO,
            args: words(names),
        }
    }

    /// setup_cli (worker waiting on "permission: Bash: ls"), without a bus,
    /// plus a second agent, builder, in status builder (Go: setupWait).
    fn setup_wait(env: TestEnv, builder: AgentStatus) -> TestEnv {
        env.no_bus();
        add_agent(&env.ctx, "builder", builder, "");
        env
    }

    fn add_agent(ctx: &Ctx, name: &str, status: AgentStatus, detail: &str) {
        let session = format!("rpc-cli-{name}");
        ctx.termd()
            .unwrap()
            .create_session(&session, &Default::default())
            .unwrap();
        ctx.store()
            .unwrap()
            .create_agent(&Agent {
                name: name.to_string(),
                session_name: session,
                backend: "claude".to_string(),
                created_at: Time::now(),
                ..Default::default()
            })
            .unwrap();
        report(ctx.service().unwrap(), name, status, detail);
    }

    fn report(svc: &AgentService, name: &str, status: AgentStatus, detail: &str) {
        svc.report(
            name,
            StatusReport {
                status,
                detail: detail.to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    use agent_service::AgentService;

    /// Records what `agent send` leaves behind: a message sent, then the turn
    /// it started. --next then counts the turn's end whenever it lands, even
    /// before wait's first check.
    fn start_turn(env: &TestEnv, name: &str) {
        let now = Time::now();
        env.store()
            .set_state(&StateEntry {
                agent_name: name.to_string(),
                key: STATE_KEY_SENT_AT.to_string(),
                value: now.format_rfc3339_nano(),
                updated_at: now,
            })
            .unwrap();
        env.service()
            .report(
                name,
                StatusReport {
                    status: AGENT_STATUS_PROCESSING,
                    clear_result: true,
                    ..Default::default()
                },
            )
            .unwrap();
    }

    /// Runs f on another thread once out holds line: wait has then handled
    /// everything before it, so what f changes comes after (Go: whenPrinted).
    fn when_printed(
        out: &SharedBuf,
        line: &str,
        f: impl FnOnce() + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let (out, line) = (out.clone(), line.to_string());
        std::thread::spawn(move || {
            let (_, printed) = agent_base::await_::poll(agent_base::await_::TIMEOUT, || {
                let s = out.contents();
                let ok = s.lines().any(|l| l == line);
                (s, ok)
            });
            if printed {
                f();
            }
        })
    }

    fn err_text(e: &anyhow::Error) -> String {
        format!("{e:#}")
    }

    #[test]
    fn wait_several_prints_each_as_it_arrives() {
        crate::require_termd!();
        let env = setup_wait(TestEnv::setup_cli(), AGENT_STATUS_PROCESSING);
        start_turn(&env, "worker");
        start_turn(&env, "builder");
        report(env.service(), "builder", AGENT_STATUS_IDLE, "");
        let svc = Arc::clone(env.service());
        let bg = when_printed(&env.out, "builder: idle", move || {
            report(&svc, "worker", AGENT_STATUS_WAITING, "permission: Bash: rm -rf build");
        });
        let mut a = args("idle,waiting", &["worker", "builder", "worker"]);
        a.next = true;
        a.timeout = Duration::from_secs(60);
        let res = run(&env.ctx, &a);
        bg.join().unwrap();
        res.unwrap();
        assert_eq!(
            env.take_out(),
            "builder: idle\nworker: waiting (permission: Bash: rm -rf build)\n"
        );
    }

    #[test]
    fn wait_any_returns_at_the_first() {
        crate::require_termd!();
        let env = setup_wait(TestEnv::setup_cli(), AGENT_STATUS_PROCESSING);
        let mut a = args("idle,waiting", &["builder", "worker"]);
        a.any = true;
        run(&env.ctx, &a).unwrap();
        assert_eq!(env.take_out(), "worker: waiting (permission: Bash: ls)\n");
    }

    // With no names, --any watches every agent on the host except the caller.
    #[test]
    fn wait_any_no_names_watches_every_other_agent() {
        crate::require_termd!();
        let mut env = setup_wait(TestEnv::setup_cli_private(), AGENT_STATUS_PROCESSING);
        let err = run(&env.ctx, &args("waiting", &[])).unwrap_err();
        assert!(err_text(&err).contains("--any or --follow"), "no names without --any: err = {err:#}");

        let mut a = args("waiting", &[]);
        a.any = true;
        run(&env.ctx, &a).unwrap();
        assert_eq!(env.take_out(), "worker: waiting (permission: Bash: ls)\n");

        env.set_env("AGENT_NAME", "worker");
        a.timeout = Duration::from_millis(100);
        let err = run(&env.ctx, &a).unwrap_err();
        assert_eq!(env.take_out(), "");
        assert_eq!(
            exit_code(&err),
            WAIT_TIMEOUT_EXIT_CODE,
            "wait as worker: {err:#}; want a timeout (builder never waits)"
        );
    }

    #[test]
    fn wait_several_unknown_name_fails_at_once() {
        crate::require_termd!();
        let env = setup_wait(TestEnv::setup_cli(), AGENT_STATUS_PROCESSING);
        let start = Instant::now();
        let err = run(&env.ctx, &args("idle", &["builder", "biulder"])).unwrap_err();
        assert!(
            err_text(&err).contains("biulder") && exit_code(&err) == 1,
            "err = {err:#} (exit {}); want a not-found error naming biulder",
            exit_code(&err)
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "took {:?}; a missing name should fail before waiting",
            start.elapsed()
        );
    }

    // An agent that exits is reported, the others are still waited on, and the
    // wait exits 1.
    #[test]
    fn wait_several_one_exits() {
        crate::require_termd!();
        let env = setup_wait(TestEnv::setup_cli(), AGENT_STATUS_ENDED);
        let err = run(&env.ctx, &args("idle,waiting", &["builder", "worker"])).unwrap_err();
        assert_eq!(env.take_out(), "worker: waiting (permission: Bash: ls)\n");
        assert_eq!(
            err_text(&err),
            "1 of 2 agents reached idle or waiting; not builder (ended)"
        );
        assert_eq!(exit_code(&err), 1);
        let hints = env.take_err();
        assert!(
            hints.contains("agent wait: agent builder: status is ended"),
            "stderr = {hints:?}; want the exit reported"
        );
    }

    #[test]
    fn wait_several_timeout() {
        crate::require_termd!();
        let env = setup_wait(TestEnv::setup_cli(), AGENT_STATUS_PROCESSING);
        // The timeout is what's tested; it only has to outlast wait's first
        // checks on a loaded machine.
        report(env.service(), "worker", AGENT_STATUS_IDLE, "");
        let mut a = args("idle", &["worker", "builder"]);
        a.timeout = Duration::from_millis(500);
        let err = run(&env.ctx, &a).unwrap_err();
        assert_eq!(env.take_out(), "worker: idle\n");
        assert_eq!(
            err_text(&err),
            "timed out after 500ms waiting for 1 agent to be idle (last status: builder processing)"
        );
        assert_eq!(exit_code(&err), WAIT_TIMEOUT_EXIT_CODE);
    }

    // --follow prints each prompt once, including one that came and went
    // between checks, until the timeout.
    #[test]
    fn wait_follow_prints_each_arrival_once() {
        crate::require_termd!();
        let _serial = crate::cli::watch::INTERRUPT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup_wait(TestEnv::setup_cli(), AGENT_STATUS_IDLE);
        let svc = Arc::clone(env.service());
        let bg1 = when_printed(&env.out, "worker: waiting (permission: Bash: ls)", move || {
            report(&svc, "worker", AGENT_STATUS_PROCESSING, "approved");
            report(&svc, "worker", AGENT_STATUS_WAITING, "permission: Bash: pwd");
        });
        let svc = Arc::clone(env.service());
        let bg2 = when_printed(&env.out, "worker: waiting (permission: Bash: pwd)", move || {
            // The next one comes and goes faster than a check.
            report(&svc, "worker", AGENT_STATUS_PROCESSING, "approved");
            report(&svc, "worker", AGENT_STATUS_WAITING, "permission: Bash: whoami");
        });
        // --follow runs until the timeout: long enough for the three prompts
        // on a loaded machine.
        let mut a = args("waiting", &["worker"]);
        a.follow = true;
        a.timeout = Duration::from_secs(1);
        let res = run(&env.ctx, &a);
        bg1.join().unwrap();
        bg2.join().unwrap();
        let out = env.take_out();
        let want = "worker: waiting (permission: Bash: ls)\nworker: waiting (permission: Bash: pwd)\n";
        assert!(
            out.starts_with(want) && out.contains("whoami") && out.matches('\n').count() == 3,
            "stdout = {out:?}; want the three prompts once each"
        );
        let err = res.unwrap_err();
        assert_eq!(exit_code(&err), WAIT_TIMEOUT_EXIT_CODE, "err = {err:#}; want the timeout's exit 2");
    }

    // With no names, --follow picks up agents spawned while it runs.
    #[test]
    fn wait_follow_no_names_picks_up_new_agents() {
        crate::require_termd!();
        let _serial = crate::cli::watch::INTERRUPT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let env = setup_wait(TestEnv::setup_cli(), AGENT_STATUS_IDLE);
        WAIT_RELIST_INTERVAL_MS.store(30, Ordering::Relaxed);
        let share = env.ctx.share().unwrap();
        let bg = when_printed(&env.out, "worker: waiting (permission: Bash: ls)", move || {
            let ctx = share.quiet_ctx();
            add_agent(&ctx, "late", AGENT_STATUS_WAITING, "permission: Edit: a.go");
        });
        // --follow runs until the timeout: long enough for a relist on a
        // loaded machine.
        let mut a = args("waiting", &[]);
        a.follow = true;
        a.timeout = Duration::from_secs(1);
        let _ = run(&env.ctx, &a);
        WAIT_RELIST_INTERVAL_MS.store(10_000, Ordering::Relaxed);
        bg.join().unwrap();
        assert_eq!(
            env.take_out(),
            "worker: waiting (permission: Bash: ls)\nlate: waiting (permission: Edit: a.go)\n"
        );
    }

    // The end of Go's TestHostNamesMatchIgnoringCase: a trusted peer's name
    // in two spellings is one agent to wait on, in the peer's own spelling.
    #[test]
    fn parse_wait_refs_folds_host_case() {
        let env = TestEnv::new();
        crate::cli::remote_test::trust_peers(&env, &["Dev-Box-M2"]);
        let refs =
            parse_wait_refs(&env.ctx, &words(&["worker@dev-box-m2", "worker@Dev-Box-M2"])).unwrap();
        assert_eq!(refs, vec![AgentRef { name: "worker".into(), host: "Dev-Box-M2".into() }]);
        // This host's own name, in any case, is its own spelling (as the Go
        // canonicalHost gives it: not "", so it stays a ref of its own).
        let refs = parse_wait_refs(&env.ctx, &words(&["worker@LOCAL", "worker@local"])).unwrap();
        assert_eq!(refs, vec![AgentRef { name: "worker".into(), host: "local".into() }]);
    }
}

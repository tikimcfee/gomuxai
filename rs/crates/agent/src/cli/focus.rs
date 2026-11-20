//! `agent focus` (no Go file: new in the Rust build; docs/FLEET.md, "Focus and
//! follow"; the work is agent_fleet::focus).
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_base::gostd::strconv::quote;
use agent_bus::{Context, FleetRequest, FocusEvent, Time};
use agent_fleet::focus;

use super::command::{ArgRule, Def, Doc};
use super::logs::go_json;
use super::remote::{call_remote, canonical_host, is_remote_host, parse_target};
use super::root::Ctx;
use super::watch::{INTERRUPT_POLL, Interrupts};

// `agent focus`

pub const DOC: Doc = Doc {
    use_line: "focus [name]",
    short: "Show, set or follow which agent has focus on a host",
    long: r#"Focus is one agent per host that has a person's attention: a display (a
wall, a status bar) shows it, and agent fleet carries it. It is kept in
this host's store. Setting or clearing it also publishes the change on the
bus, for whoever follows it.

  agent focus               # which agent has focus here
  agent focus helper        # give it to an agent of this host
  agent focus --clear       # nothing has focus
  agent focus --host hub    # another host's, asked through its bus

Only this host's focus is set or cleared here: run agent focus on the
other host for its own. Cleaning up the focused agent clears focus.

--follow prints the current focus, then a line for each change, until
Ctrl-C. The changes come over the bus; with no bus running here, it reads
this host's store again every second instead. What it follows is said once
on stderr (-q drops it), so stdout holds only focus lines (JSON lines with
--json)."#,
    example: r#"  agent focus
  agent focus helper
  agent focus --clear
  agent focus --host hub
  agent focus --follow --host hub --json"#,
    args: ArgRule::Max(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent focus`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "clear", help = "Clear this host's focus: nothing has focus")]
    pub clear: bool,
    #[arg(long = "follow", help = "Print the focus, then each change, until Ctrl-C")]
    pub follow: bool,
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Host whose focus to show or follow (default: this host)",
    )]
    pub host: String,
    #[arg(long = "json", help = "Print as JSON (with --follow, a JSON line each)")]
    pub json: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// How often --follow without a bus reads the store again.
const STORE_POLL: Duration = Duration::from_secs(1);

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let this = ctx.cfg().host.clone();
    let host = if args.host.is_empty() {
        this.clone()
    } else {
        agent_bus::validate_host(&args.host)?;
        canonical_host(ctx, &args.host)
    };
    let remote = is_remote_host(ctx, &host);
    let setting = args.clear || !args.args.is_empty();
    if args.clear && !args.args.is_empty() {
        anyhow::bail!("give an agent to focus, or --clear, not both");
    }
    if setting && args.follow {
        anyhow::bail!("--follow only shows focus: set it first, then follow");
    }
    if setting && remote {
        anyhow::bail!(
            "focus is set only on its own host: run agent focus{} on {host}",
            if args.clear { " --clear" } else { "" }
        );
    }

    if args.follow {
        return follow(ctx, &host, remote, args.json);
    }
    if args.clear {
        let ev = focus::clear(ctx.store()?, None, &this, &by(ctx))?;
        publish(ctx, &ev);
        return print_event(ctx, &ev, args.json);
    }
    if let Some(arg) = args.args.first() {
        let (r, other) = parse_target(ctx, arg)?;
        if other {
            anyhow::bail!(
                "focus is set only on its own host: run agent focus {} on {}",
                r.name,
                r.host
            );
        }
        // focus::set's own "not found" names close names, as the service's
        // lookup does: focus needs the store and the bus only, not termd.
        let ev = focus::set(ctx.store()?, None, &this, &r.name, &by(ctx))?;
        publish(ctx, &ev);
        return print_event(ctx, &ev, args.json);
    }

    let ev = if remote {
        remote_focus(ctx, &host)?
    } else {
        focus::get(ctx.store()?)?.unwrap_or_default()
    };
    print_event(ctx, &with_host(ev, &host), args.json)
}

/// Who sets focus: the agent running this command, or "user" for a person.
fn by(ctx: &Ctx) -> String {
    let me = ctx.non_interactive_caller_agent();
    if me.is_empty() { "user".to_string() } else { me }
}

/// Publishes a change on the bus for followers. The change is made (the
/// store is the truth, and a follower reads it again when it reconnects), so
/// a publish that didn't reach the bus is a hint, not an error: the publish
/// only buffers, and the flush is what says the bus took it.
fn publish(ctx: &Ctx, ev: &FocusEvent) {
    let p = ctx.bus_publisher();
    match p.publish_focus(ev).and_then(|()| p.flush(Duration::from_secs(1))) {
        Ok(()) => {}
        Err(e) if agent_bus::is_no_bus(&e) => ctx.hint(
            "no agent bus runs here, so no follower was told (agent fleet and agent focus read it all the same)",
        ),
        Err(e) => ctx.hint(format!("the change is stored, but wasn't published on the bus: {e:#}")),
    }
}

/// Another host's focus, from its fleet op. A host that doesn't answer it
/// but answers list runs an agent without focus, or a bus that doesn't allow
/// fleet; one that answers neither is down or unknown, which the error says.
fn remote_focus(ctx: &Ctx, host: &str) -> anyhow::Result<FocusEvent> {
    let answers_list = std::cell::Cell::new(false);
    let res = call_remote(ctx, host, |c| {
        let bg = Context::background();
        c.fleet(&bg, host, FleetRequest { report: false }).inspect_err(|e| {
            if agent_bus::is_unanswered(e) {
                answers_list.set(c.list_host(&bg, host).is_ok());
            }
        })
    });
    match res {
        Ok(fh) => Ok(fh.focus.unwrap_or_default()),
        Err(e) => {
            if answers_list.get() {
                ctx.hint(format!(
                    "{host} serves focus from an agent built with it, and only if its bus allows fleet (agent bus status there shows what it allows; agent list --host {host} lists its agents)"
                ));
            }
            Err(e)
        }
    }
}

fn with_host(mut ev: FocusEvent, host: &str) -> FocusEvent {
    if ev.host.is_empty() {
        ev.host = host.to_string();
    }
    ev
}

fn print_event(ctx: &Ctx, ev: &FocusEvent, json: bool) -> anyhow::Result<()> {
    if json {
        return ctx.print_json(ev);
    }
    ctx.write_out(&format!("{}\n", focus_line(ev)))
}

/// One line for a focus value: "hub: helper (session agent-helper), set by
/// lead at 2026-11-20 15:04:05", or "hub: nothing has focus".
pub(crate) fn focus_line(ev: &FocusEvent) -> String {
    let mut line = if ev.agent.is_empty() {
        format!("{}: nothing has focus", ev.host)
    } else if ev.session.is_empty() {
        format!("{}: {}", ev.host, ev.agent)
    } else {
        format!("{}: {} (session {})", ev.host, ev.agent, ev.session)
    };
    let verb = if ev.agent.is_empty() { "cleared" } else { "set" };
    let at = if ev.at.is_zero() {
        String::new()
    } else {
        format!(
            " at {}",
            ev.at.0.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S")
        )
    };
    match (ev.by.is_empty(), at.is_empty()) {
        (true, true) => {}
        (false, _) => line += &format!(", {verb} by {}{at}", printable(&ev.by)),
        (true, false) => line += &format!(", {verb}{at}"),
    }
    line
}

/// Text from another host, quoted if it isn't plain printable text.
fn printable(s: &str) -> String {
    if s.chars().all(|c| !c.is_control()) {
        s.to_string()
    } else {
        quote(s)
    }
}

/// --follow: the current value, then each change, until Ctrl-C.
fn follow(ctx: &Ctx, host: &str, remote: bool, json: bool) -> anyhow::Result<()> {
    let interrupts = Interrupts::new();
    let nc = match ctx.bus_connect() {
        Ok(nc) => Some(nc),
        Err(e) if agent_bus::is_no_bus(&e) => {
            if remote {
                return Err(e.context(format!(
                    "{} is not this host ({}); reaching other hosts needs a running agent bus",
                    quote(host),
                    quote(&ctx.cfg().host)
                )));
            }
            None
        }
        Err(e) => return Err(e.context("connecting to the agent bus")),
    };
    let how = if nc.is_some() { "" } else { " (no agent bus here: reading the store every second)" };
    ctx.notice(format!("Following focus on host {host}{how}. Ctrl-C stops."));

    let mut write_err: Option<anyhow::Error> = None;
    let mut emit = |ev: &FocusEvent| -> bool {
        let ev = with_host(ev.clone(), host);
        let line = if json {
            match go_json(&ev) {
                Ok(l) => l,
                Err(e) => {
                    write_err = Some(e);
                    return false;
                }
            }
        } else {
            focus_line(&ev)
        };
        let mut out = ctx.out();
        if let Err(e) = writeln!(out, "{line}").and_then(|_| out.flush()) {
            write_err = Some(e.into());
            return false;
        }
        !interrupts.fired()
    };

    let Some(nc) = nc else {
        // No bus: this host's store, read again every second. The store
        // keeps the last change, a clear included, so each one shows as the
        // bus would carry it (who cleared it, and when); the first line is
        // the current value, as over the bus. A read that fails (the store
        // busy, say) is said once and tried again, not the end of following.
        let store = ctx.store()?;
        let mut last: Option<(String, Time, String)> = None;
        let mut read_failed = false;
        while !interrupts.fired() {
            match focus::last_change(store) {
                Ok(change) => {
                    read_failed = false;
                    let key = change
                        .as_ref()
                        .map(|ev| (ev.agent.clone(), ev.at, ev.by.clone()))
                        .unwrap_or_default();
                    if last.as_ref() != Some(&key) {
                        let ev = match (&last, change) {
                            (Some(_), Some(ev)) => ev,
                            (_, change) => change.filter(|ev| !ev.agent.is_empty()).unwrap_or_default(),
                        };
                        last = Some(key);
                        if !emit(&ev) {
                            break;
                        }
                    }
                }
                Err(e) => {
                    if !read_failed {
                        ctx.notice(format!("reading focus from the store: {e:#}; trying again"));
                    }
                    read_failed = true;
                }
            }
            let until = std::time::Instant::now() + STORE_POLL;
            while !interrupts.fired() && std::time::Instant::now() < until {
                std::thread::sleep(INTERRUPT_POLL);
            }
        }
        return write_err.map_or(Ok(()), Err);
    };

    let (fctx, cancel) = Context::with_cancel(&Context::background());
    let done = AtomicBool::new(false);
    let store = if remote { None } else { Some(ctx.store()?.as_ref()) };
    let res = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                if interrupts.fired() {
                    cancel.cancel();
                    return;
                }
                std::thread::sleep(INTERRUPT_POLL);
            }
        });
        let res = focus::follow(&fctx, store, Some(&nc), Some(host), &mut emit);
        done.store(true, Ordering::Relaxed);
        res
    });
    // As remote_focus: a host that answers list but not fleet.
    let answers_list = match &res {
        Err(e) if remote && !interrupts.fired() && agent_bus::is_unanswered(e) => {
            agent_bus::RpcClient::new(nc.clone(), Duration::ZERO)
                .list_host(&Context::background(), host)
                .is_ok()
        }
        _ => false,
    };
    nc.close();
    if let Some(e) = write_err {
        return Err(e);
    }
    match res {
        Err(_) if interrupts.fired() => Ok(()),
        Err(e) => {
            if answers_list {
                ctx.hint(format!(
                    "{host} serves focus from an agent built with it, and only if its bus allows fleet (agent bus status there shows what it allows)"
                ));
            }
            Err(e)
        }
        Ok(()) => Ok(()),
    }
}

#[cfg(test)]
#[path = "focus_test.rs"]
mod focus_test;

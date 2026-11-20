//! `agent events` (Go: internal/cli/events.go).
use std::io::Write;

use agent_base::gostd::strconv::{is_print, quote};
use agent_bus::{
    StatusEvent, decode_status_event, parse_agent_ref, subject_status, validate_host,
};
use agent_service::one_line;
use anyhow::Context as _;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;
use super::watch::{INTERRUPT_POLL, Interrupts};

// `agent events`

pub const DOC: Doc = Doc {
    use_line: "events [name[@host]]",
    short: "Stream agent status events from the bus (runs until Ctrl-C)",
    long: r#"Events prints each status event published on the agent bus, one line per
event, until interrupted. Every recorded status report (hooks, "agent report",
approve/deny) publishes one. Needs a running bus.

Events are what was reported, not the resolved status: a change only the
backend's native status shows (e.g. an interrupted permission prompt) has no
event. "agent status" stays the authority.

What it is watching is said once on stderr (-q drops it), so stdout holds
only events."#,
    example: r#"  agent events                 # every agent on this host
  agent events helper
  agent events helper@hub      # an agent on another host
  agent events --all-hosts --json"#,
    args: ArgRule::Max(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent events`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "all-hosts", help = "Show events from every host")]
    pub all_hosts: bool,
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Host whose events to show (default: this host)",
    )]
    pub host: String,
    #[arg(long = "json", help = "Print each event as JSON")]
    pub json: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (mut host, mut agent) = (ctx.cfg().host.clone(), "*".to_string());
    if let Some(arg) = args.args.first() {
        let r = parse_agent_ref(arg)?;
        agent = r.name;
        if !r.host.is_empty() {
            if !args.host.is_empty() || args.all_hosts {
                anyhow::bail!("give the host once: {arg}, or --host/--all-hosts");
            }
            host = r.host;
        }
    }
    if !args.host.is_empty() && args.all_hosts {
        anyhow::bail!("use --host or --all-hosts, not both");
    } else if args.all_hosts {
        host = "*".to_string();
    } else if !args.host.is_empty() {
        validate_host(&args.host)?;
        host = args.host.clone();
    }

    let nc = ctx.bus_connect()?;
    let mut sub = nc
        .subscribe(&subject_status(&host, &agent))
        .context("subscribing to status events")?;
    let who = if agent == "*" {
        "every agent".to_string()
    } else {
        format!("agent {agent}")
    };
    let place = if host == "*" {
        "on every host".to_string()
    } else {
        format!("on host {host}")
    };
    ctx.notice(format!("Watching status events of {who} {place}. Ctrl-C stops."));

    let interrupts = Interrupts::new();
    let mut open = true;
    while !interrupts.fired() {
        if !open {
            // Go's channel subscription goes quiet when the connection
            // closes, and the command waits for Ctrl-C all the same.
            std::thread::sleep(INTERRUPT_POLL);
            continue;
        }
        let msg = match sub.next_msg(INTERRUPT_POLL) {
            Ok(msg) => msg,
            Err(e) if format!("{e:#}") == "nats: timeout" => continue,
            Err(_) => {
                open = false;
                continue;
            }
        };
        // Events can come from other hosts: decode (which rejects forged
        // host/agent fields) and re-encode rather than echo raw bytes into the
        // terminal.
        let ev = match decode_status_event(&msg) {
            Ok(ev) => ev,
            Err(e) => {
                let _ = writeln!(ctx.err(), "agent events: {}", quote(&format!("{e:#}")));
                continue;
            }
        };
        let line = if args.json {
            go_json(&ev)?
        } else {
            format_status_event(&ev)
        };
        let mut out = ctx.out();
        writeln!(out, "{line}")?;
        out.flush()?;
    }
    let _ = sub.unsubscribe();
    nc.close();
    Ok(())
}

/// v as Go's json.Marshal writes it: compact, with <, > and & escaped (and
/// U+2028, U+2029), as Go's encoder does by default.
fn go_json<T: Serialize>(v: &T) -> anyhow::Result<String> {
    Ok(agent_base::gostd::json::to_string(v, true)?)
}

/// Renders ev as one line, e.g. "15:04:05.000 helper@hub waiting
/// event=PermissionRequest tool=Bash detail=...". Fields other than the
/// (validated) host and agent are quoted if they hold anything but printable
/// text, so an event can't write escape sequences to the terminal.
fn format_status_event(ev: &StatusEvent) -> String {
    let at = ev.at.0.with_timezone(&chrono::Local);
    let mut line = format!(
        "{} {}@{} {}",
        at.format("%H:%M:%S%.3f"),
        ev.agent,
        ev.host,
        printable(&ev.status)
    );
    if !ev.event.is_empty() {
        line += &format!(" event={}", printable(&ev.event));
    }
    if !ev.tool.is_empty() {
        line += &format!(" tool={}", printable(&ev.tool));
    }
    if !ev.detail.is_empty() {
        // One line per event: --json has the detail whole.
        line += &format!(" detail={}", quote(&one_line(&ev.detail)));
    }
    line
}

/// s, or s quoted if it has non-printable characters or spaces (Go: printable).
pub fn printable(s: &str) -> String {
    if s.chars().any(|c| !is_print(c) || c.is_whitespace()) {
        return quote(s);
    }
    s.to_string()
}

#[cfg(test)]
mod events_test {
    use super::*;

    // Events can come from other hosts, so `agent events` must not write their
    // fields to the terminal raw: an escape sequence could retitle the window,
    // set the clipboard (OSC 52), or redraw what the user sees.
    #[test]
    fn format_status_event_escapes_control_characters() {
        let ev = StatusEvent {
            host: "peer".to_string(),
            agent: "worker".to_string(),
            at: agent_bus::Time::now(),
            status: "idle\x1b[2J".to_string(),
            event: "Stop\x1b]52;c;ZWNobyBwd25lZA==\x07".to_string(),
            tool: "Bash\r\nfake line".to_string(),
            detail: "\x1b[31m".to_string(),
            ..Default::default()
        };
        let line = format_status_event(&ev);
        assert!(
            !line.contains(['\x1b', '\x07', '\r', '\n']),
            "control characters reached the terminal: {line:?}"
        );

        let plain = format_status_event(&StatusEvent {
            host: "peer".to_string(),
            agent: "worker".to_string(),
            status: "waiting".to_string(),
            event: "PermissionRequest".to_string(),
            tool: "Bash".to_string(),
            ..Default::default()
        });
        assert!(
            plain.ends_with("worker@peer waiting event=PermissionRequest tool=Bash"),
            "plain event changed: {plain:?}"
        );
    }

    #[test]
    fn json_escapes_as_go_marshal() {
        let ev = StatusEvent {
            host: "h".to_string(),
            agent: "a".to_string(),
            status: "idle".to_string(),
            detail: "a<b>&c".to_string(),
            ..Default::default()
        };
        let s = go_json(&ev).unwrap();
        assert!(s.contains("\"detail\":\"a\\u003cb\\u003e\\u0026c\""), "{s}");
    }

    use std::time::{Duration, Instant};

    use agent_service::{AGENT_STATUS_WAITING, StatusReport};
    use agent_store::{Agent, Time};

    use crate::cli::root::SharedBuf;
    use crate::cli::testenv::{TestBus, TestEnv};
    use crate::cli::watch::{INTERRUPT_TESTS, interrupt_until};

    // The host is given once, and the flags are checked before the bus is
    // dialed.
    #[test]
    fn events_checks_its_host_args_before_the_bus() {
        let mut env = TestEnv::new();
        env.forbid_bus();
        for (words, want) in [
            (
                &["events", "worker@h1", "--host", "h2"][..],
                "give the host once: worker@h1, or --host/--all-hosts",
            ),
            (
                &["events", "worker@h1", "--all-hosts"],
                "give the host once: worker@h1, or --host/--all-hosts",
            ),
            (
                &["events", "--host", "h1", "--all-hosts"],
                "use --host or --all-hosts, not both",
            ),
        ] {
            let (out, res) = env.run(words);
            assert_eq!(format!("{:#}", res.unwrap_err()), want, "agent {words:?}");
            assert_eq!(out, "");
        }
        let (_, res) = env.run(&["events", "--host", "bad host"]);
        assert!(res.is_err(), "events --host 'bad host' took the host");
        assert!(!env.dialed_forbidden(), "events dialed the bus over bad args");
    }

    #[test]
    fn events_needs_a_bus() {
        let mut env = TestEnv::new();
        env.no_bus();
        let (out, res) = env.run(&["events"]);
        let err = res.unwrap_err();
        assert!(agent_bus::is_no_bus(&err), "events without a bus: {err:#}");
        assert_eq!(out, "");
        assert_eq!(env.take_err(), "", "a notice before the bus was there");
    }

    /// Runs events with words (after "events") on another thread until its
    /// notice is out.
    fn start_events(
        env: &TestEnv,
        words: &[&str],
    ) -> (SharedBuf, SharedBuf, std::thread::JoinHandle<anyhow::Result<()>>) {
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let share = env.ctx.share().unwrap();
        let (o, e) = (out.clone(), err.clone());
        let mut line = vec!["events".to_string()];
        line.extend(words.iter().map(|s| s.to_string()));
        let h = std::thread::spawn(move || {
            let mut ctx = share.ctx(o.writer(), e.writer());
            let words: Vec<&str> = line.iter().map(String::as_str).collect();
            crate::cli::root::run_words(&mut ctx, &words)
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        while !err.contents().contains("Ctrl-C stops.") {
            assert!(!h.is_finished(), "events returned early: {}", err.contents());
            assert!(Instant::now() < deadline, "no notice: {:?}", err.contents());
            std::thread::sleep(Duration::from_millis(10));
        }
        (out, err, h)
    }

    /// Calls act until cond holds: the subscription is only sure to be on
    /// the server once an event has come through it.
    fn until(what: &str, mut act: impl FnMut(), cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "never got {what}");
            act();
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Every recorded report is an event: --json prints it as Go marshals
    // it, and the plain form one quoted line; an event whose body claims
    // another agent than its subject is refused on stderr.
    #[test]
    fn events_streams_reported_statuses() {
        crate::require_termd!();
        let _serial = INTERRUPT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let env = TestEnv::with_termd();
        let bus = crate::require_bus!(TestBus::start());
        let (dir, host) = (bus.dir.path().to_path_buf(), bus.host.clone());
        env.ctx
            .set_bus_connect(move || agent_bus::connect_local(&dir, &host));
        env.create_live_agent(&Agent {
            name: "worker".into(),
            session_name: "rpc-cli-worker".into(),
            backend: "claude".into(),
            created_at: Time::now(),
            ..Default::default()
        });
        let report = || {
            env.service()
                .report(
                    "worker",
                    StatusReport {
                        status: AGENT_STATUS_WAITING,
                        event: "PermissionRequest".into(),
                        tool: "Bash".into(),
                        detail: "permission: Bash: a<b".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
        };

        let (out, err, h) = start_events(&env, &["--json"]);
        assert_eq!(
            err.contents(),
            "Watching status events of every agent on host local. Ctrl-C stops.\n"
        );
        until("an event", report, || !out.contents().is_empty());
        let forged = serde_json::json!({
            "host": "local", "agent": "other", "status": "idle", "at": "2026-11-20T10:00:00Z",
        });
        let subject = agent_bus::subject_status("local", "worker");
        until(
            "the forged event's refusal",
            || {
                bus.conn.publish(&subject, forged.to_string()).unwrap();
                let _ = bus.conn.flush();
            },
            || err.contents().contains("agent events: "),
        );
        interrupt_until(|| h.is_finished());
        h.join().unwrap().expect("events' result");
        let out = out.contents();
        let first = out.lines().next().unwrap();
        // Go's json.Marshal writes < as a backslash-u003c escape.
        let escaped = format!(r#""detail":"permission: Bash: a{}u003cb""#, '\\');
        assert!(first.contains(&escaped), "{first}");
        let ev: serde_json::Value = serde_json::from_str(first).unwrap();
        for (k, v) in [
            ("host", "local"),
            ("agent", "worker"),
            ("status", "waiting"),
            ("event", "PermissionRequest"),
            ("tool", "Bash"),
        ] {
            assert_eq!(ev[k], v, "{k} in {first}");
        }
        assert!(
            !out.contains("\"other\""),
            "the forged event was printed: {out}"
        );
        assert!(
            err.contents().contains(
                r#"agent events: "status event on gomux.local.agent.worker.status claims to be from \"other\"@\"local\"""#
            ),
            "{}",
            err.contents()
        );

        // One agent's events, as lines.
        let (out, err, h) = start_events(&env, &["worker"]);
        assert_eq!(
            err.contents(),
            "Watching status events of agent worker on host local. Ctrl-C stops.\n"
        );
        until("an event", report, || !out.contents().is_empty());
        interrupt_until(|| h.is_finished());
        h.join().unwrap().expect("events' result");
        let out = out.contents();
        let first = out.lines().next().unwrap();
        assert!(
            first.ends_with(
                r#" worker@local waiting event=PermissionRequest tool=Bash detail="permission: Bash: a<b""#
            ),
            "{first}"
        );
    }
}

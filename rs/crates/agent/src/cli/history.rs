//! `agent history` (Go: internal/cli/history.go), and the command log every run
//! writes to.

use std::fmt::Write as _;
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime};

use agent_base::buildinfo;
use agent_base::gostd::os;
use agent_base::util::shell_join;
use agent_store::{CommandFilter, CommandRecord, Store, Time, gojson};
use chrono::{DateTime, Local};

use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;
use super::{exit, flags};

// `agent history`

pub const DOC: Doc = Doc {
    use_line: "history",
    short: "List recent agent commands run on this host",
    long: r##"History lists the agent commands run on this host, newest last, one per
line: time, who ran it (an agent's name, or "-" for a person), exit code, the
command line, and for a failure, its error after "#".

Every run is recorded, help included, except hook reports and the bus daemon.
The newest 20000 are kept."##,
    example: r#"  agent history
  agent history --agent lead --failed
  agent history --agent - -n 50      # a person's commands
  agent history --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent history`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "agent",
        value_name = "string",
        default_value = "",
        help = "Only this agent's commands (\"-\" for a person's)",
    )]
    pub agent: String,
    #[arg(long = "failed", help = "Only commands that failed")]
    pub failed: bool,
    #[arg(long = "json", help = "Print the commands as a JSON array")]
    pub json: bool,
    #[arg(
        long = "lines",
        short = 'n',
        value_name = "int",
        default_value = "20",
        value_parser = flags::parse_int,
        help = "Number of commands to list",
    )]
    pub lines: i64,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    if args.lines < 1 {
        anyhow::bail!("--lines must be at least 1");
    }
    let mut f = CommandFilter {
        failed: args.failed,
        limit: args.lines,
        ..Default::default()
    };
    if ctx.changed("agent") {
        f.by_agent = true;
        f.agent = if args.agent == "-" {
            String::new()
        } else {
            args.agent.clone()
        };
    }
    let mut recs = ctx.store()?.list_commands(&f)?;
    recs.reverse();
    if super::logs::in_dream(ctx) {
        // A command's words can hold a secret: a dream's agent reads them
        // redacted, as it reads transcripts.
        for r in &mut recs {
            r.args.iter_mut().for_each(|a| *a = super::logs::redact_line(a));
            r.error = super::logs::redact_line(&r.error);
        }
    }

    if args.json {
        // json.NewEncoder(out).Encode: one line, with Go's HTML escaping.
        return ctx.write_out(&format!("{}\n", gojson::marshal(&recs)?));
    }
    if recs.is_empty() {
        if f.by_agent || f.failed {
            return ctx.write_out(
                "No recorded commands match (agent history without --agent or --failed lists them all).\n",
            );
        }
        return ctx.write_out(&format!(
            "No commands recorded on host {} yet.\n",
            ctx.cfg().host
        ));
    }
    ctx.write_out(&format_history(&recs))
}

// History lines are cut to keep a long message from flooding the screen.
const HISTORY_COMMAND_WIDTH: usize = 100;
const HISTORY_ERROR_WIDTH: usize = 80;

/// Renders recs one per line, e.g.
/// "10-24 14:03:11 lead   1 wait helper --for idel  # invalid status".
pub fn format_history(recs: &[CommandRecord]) -> String {
    let width = recs.iter().map(|r| r.agent.len()).fold(1, usize::max);
    let mut b = String::new();
    for r in recs {
        let who = if r.agent.is_empty() { "-" } else { &r.agent };
        let _ = write!(
            b,
            "{} {who:<width$} {:>3} {}",
            r.started_at.local().0.format("%m-%d %H:%M:%S"),
            r.exit_code,
            clip(&shell_join(&r.args), HISTORY_COMMAND_WIDTH)
        );
        if !r.error.is_empty() {
            let _ = write!(b, "  # {}", clip(&r.error, HISTORY_ERROR_WIDTH));
        }
        b.push('\n');
    }
    b
}

// The command log.

/// Caps on what the command log stores: messages can be long.
const COMMAND_ARG_CAP: usize = 300;
const COMMAND_ERROR_CAP: usize = 300;

/// Bounds how long recording can hold up the process exit when the database
/// is busy.
const RECORD_WAIT: Duration = Duration::from_millis(500);

/// Writes a run's row in the command log: every run, help and failed parses
/// included, except those `recorded` leaves out (Go: recordCommand, called by
/// Execute, or by exec_into before the process is replaced). It is best
/// effort: if the database can't be opened or written, the run goes
/// unrecorded and nothing is said. Help, and commands that never asked for the
/// store, didn't open it: it opens it itself.
pub fn record_command(ctx: &Ctx, args: &[String], start: SystemTime, run_err: Option<&anyhow::Error>) {
    if !recorded(&ctx.path, ctx.bool_flag("hook")) {
        return;
    }
    let duration = start.elapsed().unwrap_or_default();
    let Some(cfg) = ctx.record_cfg() else { return };
    let store = match ctx.opened_store() {
        Some(s) => Arc::clone(s),
        None => match Store::open(&cfg.database_path) {
            Ok(s) => Arc::new(s),
            Err(_) => return,
        },
    };
    let mut rec = new_command_record(ctx, &cfg.host, args, start, duration, run_err);
    // The write waits at most RECORD_WAIT (Go: a context with that timeout);
    // a write still waiting on a busy database then ends with the process.
    let (done, wait) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = store.log_command(&mut rec);
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(RECORD_WAIT);
}

fn new_command_record(
    ctx: &Ctx,
    host: &str,
    args: &[String],
    start: SystemTime,
    duration: Duration,
    run_err: Option<&anyhow::Error>,
) -> CommandRecord {
    let mut rec = CommandRecord {
        started_at: Time(DateTime::<Local>::from(start).fixed_offset()),
        duration_ms: duration.as_millis() as i64,
        host: host.to_string(),
        agent: ctx.non_interactive_caller_agent(),
        cwd: os::getwd().unwrap_or_default(),
        args: args.iter().map(|a| cap_bytes(a, COMMAND_ARG_CAP)).collect(),
        exit_code: run_err.map_or(0, |e| exit::exit_code(e) as i64),
        build: buildinfo::id(),
        ..Default::default()
    };
    if let Some(e) = run_err {
        let text = format!("{e:#}");
        let first = text.split('\n').next().unwrap_or_default();
        rec.error = cap_bytes(first, COMMAND_ERROR_CAP);
    }
    rec
}

/// Cuts s to at most n bytes, at a character boundary, and marks how much it
/// cut.
pub fn cap_bytes(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…[+{} bytes]", &s[..cut], s.len() - cut)
}

/// Cuts s to at most n characters, ending in "…" if it cut.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n - 1).collect();
    out.push('…');
    out
}

/// Whether runs of a command go in the command log. Hook reports run on every
/// tool call of every agent, and bus run is a daemon: neither is worth a row.
/// `report_hook` is report's --hook.
pub fn recorded(path: &str, report_hook: bool) -> bool {
    match path {
        "bus run" | "hook" => false,
        // every hook event of every session would crowd out the commands people ran
        "report" => !report_hook,
        _ => true,
    }
}

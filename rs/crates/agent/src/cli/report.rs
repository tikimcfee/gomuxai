//! `agent report` (Go: internal/cli/report.go).
use std::io::Write;

use agent_service::{StatusReport, parse_agent_status, parse_end_reason};
use anyhow::Context as _;

use super::command::{ArgRule, Def, Doc};
use super::output::usage_error;
use super::root::{Ctx, command};

// `agent report`

pub const DOC: Doc = Doc {
    use_line: "report <status> [detail]",
    short: "Record an agent's status by hand (claude/agy agents report automatically)",
    long: r#"Report records an agent's status, so others don't have to infer it from
terminal output. Statuses: idle, processing, waiting, error, ended (with an
--end-reason, quit unless given), unknown. "agent statuses" says what each
status and end reason means.

Agents spawned with a hook-capable backend report automatically: a Claude
agent through the hook Claude Code runs for every session ("agent hook",
registered once by "agent hooks install"), an agy agent through the hooks
spawn wires to "agent report --hook".

With --hook, the backend's hook payload is read from stdin. Hook mode exits 0
even if the report fails, so a reporting problem can't disturb the agent;
errors go to stderr. (It still exits 1 if the tool can't start, e.g. its
database can't be opened.) It writes to stdout only to hand the agent the
messages held for it (see "agent send"), from the hook that can: Claude's
Stop, agy's PostInvocation (which reports nothing else).

The agent name defaults to AGENT_NAME."#,
    example: r#"  agent report idle
  agent report processing "running test suite"
  agent report --hook --as helper < payload.json
  agent report --hook --as helper --event PreInvocation < payload.json"#,
    args: ArgRule::Max(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent report`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "as",
        value_name = "string",
        default_value = "",
        help = "Agent name (defaults to AGENT_NAME)",
    )]
    pub as_: String,
    #[arg(
        long = "end-reason",
        value_name = "string",
        default_value = "",
        help = "With ended: how the session ended (quit, gone, replaced, finished, parent, closed; default quit)",
    )]
    pub end_reason: String,
    #[arg(
        long = "event",
        value_name = "string",
        default_value = "",
        help = "Hook event name (for backends whose payload omits it)",
    )]
    pub event: String,
    #[arg(long = "hook", help = "Read a backend hook payload from stdin")]
    pub hook: bool,
    #[arg(
        long = "spawn-id",
        value_name = "string",
        default_value = "",
        help = "Drop the report unless the agent is still this spawn (set by spawn)",
    )]
    pub spawn_id: String,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    // Go's setup opens the store before any command runs: hook mode exits 1
    // when the tool can't start (its database can't be opened), as its help
    // says, and 0 for every other failure.
    ctx.service()?;
    let mut name = args.as_.clone();
    if name.is_empty() {
        name = ctx.caller_agent();
    }
    let mut err = None;
    if name.is_empty() {
        err = Some(anyhow::anyhow!(
            "which agent? AGENT_NAME isn't set, so name it with --as <name>"
        ));
    }

    if args.hook {
        if err.is_none() {
            err = report_from_hook(ctx, &name, args).err();
        }
        if let Some(e) = err {
            let _ = writeln!(ctx.err(), "agent report: {e:#}");
        }
        return Ok(());
    }

    if let Some(e) = err {
        return Err(e);
    }
    let Some(word) = args.args.first() else {
        return Err(usage_error(
            &command("report"),
            anyhow::anyhow!(
                "a status is required (idle, processing, waiting, error, ended, or unknown) unless --hook"
            ),
        ));
    };
    let status = parse_agent_status(word)?;
    let reason = parse_end_reason(&status, &args.end_reason)?;
    let svc = ctx.service()?;
    svc.agent(&name)
        .map_err(|e| e.context("status is recorded on an agent record"))?;
    let r = StatusReport {
        status: status.clone(),
        end_reason: reason.clone(),
        event: "report".to_string(),
        spawn_id: args.spawn_id.clone(),
        detail: args.args.get(1).cloned().unwrap_or_default(),
        ..Default::default()
    };
    svc.report(&name, r.clone())?;
    let mut line = format!("Reported {name} {status}");
    if !reason.is_empty() {
        line += &format!(" {}", reason.as_str());
    }
    if !r.detail.is_empty() {
        line += &format!(" ({})", r.detail);
    }
    ctx.write_out(&(line + ".\n"))
}

fn report_from_hook(ctx: &Ctx, name: &str, args: &Args) -> anyhow::Result<()> {
    let payload = ctx.read_stdin().context("reading hook payload")?;
    let (out, mut res) = ctx
        .service()?
        .handle_hook(name, &args.spawn_id, &args.event, &payload);
    if let Some(out) = out {
        // Held messages, claimed: print them even if recording failed after.
        let mut stdout = ctx.out();
        if let Err(werr) = stdout.write_all(&out).and_then(|()| stdout.flush()) {
            let werr = anyhow::Error::new(werr).context("handing over held messages");
            res = Err(match res {
                Ok(()) => werr,
                Err(e) => anyhow::anyhow!("{e:#}\n{werr:#}"),
            });
        }
    }
    res
}

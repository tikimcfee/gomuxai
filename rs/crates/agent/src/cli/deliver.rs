//! `agent deliver` (Go: internal/cli/deliver.go).
use std::os::unix::fs::DirBuilderExt;
use std::time::Duration;

use agent_base::gostd::os::PathError;
use agent_service::{AgentStatus, Context, DeliverOptions, DeliverResult, ErrNothingHeld};
use anyhow::Context as _;
use agent_store::Time;

use super::command::{ArgRule, Def, Doc};
use super::output::plural;
use super::remote::{call_remote, parse_target};
use super::root::Ctx;
use super::watch::try_lock;

// `agent deliver`

pub const DOC: Doc = Doc {
    use_line: "deliver <name>",
    short: "Type an agent's held messages in once it's at its prompt (send starts this)",
    long: r#"Deliver types the messages held for an agent (see "agent send") into its
terminal, oldest first, as one prompt, if the agent is at its prompt.

Held messages normally reach the agent from a hook: Claude's Stop when its
turn ends, agy's PostInvocation when its current step ends. A turn that
ends without that hook (Esc, Ctrl+C, a deny, a failed turn) leaves
them held, so send starts "agent deliver --wait <name>" in the background
when it holds a message. That checks every 2s and types the held messages
once the agent has sat at its prompt, with nothing typed in its input line,
for 10s (time for a person who pressed Esc to type first; it never types
into their half-written input). It exits once nothing is held, or when the
agent's session has ended or the agent is gone. One runs per agent at a time; it logs to
.gomuxai/agents/<name>/deliver.log.

Without --wait, deliver checks once: it types the held messages now if the
agent is at its prompt with an empty input line, and otherwise says why not. To type into a busy
agent, use "agent send <name> --now".

An agent on another host (name@host) goes over the bus, as send does: that
host checks once and types them, if its bus serves the respond op (the
default; it also answers approve and deny). --wait runs only on the
agent's host."#,
    example: r#"  agent deliver helper
  agent deliver helper@hub
  agent deliver --wait helper"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent deliver`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "wait", help = "Keep checking until nothing is held (what send starts)")]
    pub wait: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// How often a waiting deliverer checks, and how long the agent must sit at its
/// prompt before it types.
const DELIVER_INTERVAL: Duration = Duration::from_secs(2);
const DELIVER_GRACE: Duration = Duration::from_secs(10);

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (r, remote) = parse_target(ctx, &args.args[0])?;
    if remote {
        if args.wait {
            anyhow::bail!(
                "deliver --wait runs on the agent's host, where send starts it when it holds a message: agent deliver {r} (without --wait) types {}'s held messages now, over the bus",
                r.name
            );
        }
        let resp = call_remote(ctx, &r.host, |c| c.deliver(&Context::background(), &r))
            .map_err(|e| {
                if format!("{e:#}").contains("invalid permission response") {
                    anyhow::anyhow!(
                        "{}'s agent binary predates deliver over the bus ({e:#}): run agent deliver {} on that host",
                        r.host,
                        r.name
                    )
                } else {
                    e
                }
            })?;
        return print_delivered(
            ctx,
            &r.to_string(),
            &DeliverResult {
                typed: usize::try_from(resp.typed).unwrap_or(0),
                held: usize::try_from(resp.held).unwrap_or(0),
                status: AgentStatus::from(resp.status),
            },
        );
    }
    let name = &r.name;
    let svc = ctx.service()?;
    let bg = Context::background();
    if !args.wait {
        let res = match svc.deliver_now(&bg, name) {
            Ok(res) => res,
            Err(e) if e.downcast_ref::<ErrNothingHeld>().is_some() => DeliverResult::default(),
            Err(e) => return Err(e),
        };
        return print_delivered(ctx, name, &res);
    }

    // An adopted agent has no directory of its own until something writes
    // there: make it, or the lock in it can't be opened. (The Go fails here:
    // "opening deliverer lock: open .../deliver.lock: no such file or
    // directory".) Only for an agent on record: a name that isn't one fails
    // as before, leaving nothing behind.
    if ctx.store()?.get_agent(name)?.is_some() {
        let dir = ctx.cfg().agent_dir(name);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| anyhow::Error::new(PathError::new("mkdir", &dir, e)))
            .context("opening deliverer lock")?;
    }
    loop {
        let path = svc.deliverer_lock_path(name);
        let Some(lock) = try_lock(&path, "deliverer lock")? else {
            return Ok(()); // another deliverer has it (send starts one per held message)
        };
        let (n, res) = svc.deliver_held(
            &bg,
            name,
            &DeliverOptions {
                wait: true,
                interval: DELIVER_INTERVAL,
                grace: DELIVER_GRACE,
            },
        );
        drop(lock);
        if n > 0 {
            ctx.write_out(&format!(
                "{}: typed {} held for {name}.\n",
                log_stamp(),
                plural(n, "message")
            ))?;
        }
        if let Err(e) = res
            && e.downcast_ref::<ErrNothingHeld>().is_none()
        {
            return Err(e);
        }
        // A deliverer started while this one held the lock exited at once:
        // look again, now that the lock is free, for what it was started for.
        if svc.held_count(name) == 0 {
            return Ok(());
        }
    }
}

/// Says what agent deliver did for name.
fn print_delivered(ctx: &Ctx, name: &str, res: &DeliverResult) -> anyhow::Result<()> {
    let line = if res.typed > 0 {
        format!("Typed {} held for {name}.\n", plural(res.typed, "message"))
    } else if res.held == 0 {
        format!("Nothing is held for {name}.\n")
    } else {
        format!(
            "Still holding {} for {name} (it is {}): the end of its turn delivers them.\n",
            plural(res.held, "message"),
            res.status
        )
    };
    ctx.write_out(&line)
}

/// The time, for deliver's log lines (Go: time.RFC3339).
fn log_stamp() -> String {
    Time::now().format_rfc3339()
}

#[cfg(test)]
#[path = "deliver_test.rs"]
mod deliver_test;

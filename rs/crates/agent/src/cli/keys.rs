//! `agent keys` (Go: internal/cli/keys.go).
use super::command::{ArgRule, Def, Doc};
use super::remote::{call_remote, parse_target};
use super::root::Ctx;
use super::session::check_session;

// `agent keys`

pub const DOC: Doc = Doc {
    use_line: "keys <session | name@host> <text>",
    short: "Type text into a session (then Enter)",
    long: r#"Keys types text into a g3d-termd session, then presses Enter (--no-enter
skips it). The text is typed literally: key names like "Escape" are typed as
words; use "agent key" to press special keys.

This is a low-level command for driving arbitrary sessions, and takes a
session name (agent "helper"'s session is "agent-helper"). It does no dialog
checks: to message an agent, use "agent send".

name@host addresses a managed agent instead of a session, on any host
(including this one). Buses don't serve keys to other hosts unless
configured to."#,
    example: r#"  agent keys my-session "ls -la"
  agent keys my-session "echo hello" --no-enter
  agent keys builder@hub "y" --no-enter"#,
    args: ArgRule::Min(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent keys`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "no-enter", help = "Don't send Enter after text")]
    pub no_enter: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let session = &args.args[0];
    let text = args.args[1..].join(" ");

    if session.contains('@') {
        return agent_keys(ctx, args, session, &text);
    }
    check_session(ctx, session)?;

    let termd = ctx.termd()?;
    termd.send_keys(session, &text)?;
    if !args.no_enter {
        termd.send_special_key(session, "Enter")?;
    }
    ctx.write_out(&format!(
        "Typed into session {session}{}.\n",
        enter_note(args)
    ))
}

/// Says whether keys pressed Enter after the text.
fn enter_note(args: &Args) -> &'static str {
    if args.no_enter {
        " (no Enter)"
    } else {
        " and pressed Enter"
    }
}

/// Sends text (and Enter, unless --no-enter) to the agent named by target
/// ("name@host"), on this host or through the bus.
fn agent_keys(ctx: &Ctx, args: &Args, target: &str, text: &str) -> anyhow::Result<()> {
    let (r, remote) = parse_target(ctx, target)?;
    let key = if args.no_enter { "" } else { "Enter" };
    if remote {
        call_remote(ctx, &r.host, |c| {
            c.keys(&agent_bus::Context::background(), &r, text, key)
        })?;
    } else {
        ctx.service()?.keys(&r.name, text, key)?;
    }
    ctx.write_out(&format!(
        "Typed into agent {target}{}.\n",
        enter_note(args)
    ))
}

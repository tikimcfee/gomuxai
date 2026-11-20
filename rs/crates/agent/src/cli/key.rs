//! `agent key` (Go: internal/cli/key.go).
use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;
use super::session::check_session;

// `agent key`

pub const DOC: Doc = Doc {
    use_line: "key <session> <key>",
    short: "Press a special key (Enter, Escape, c-c, ...) in a session",
    long: r#"Key presses a special key or key combination in a g3d-termd session, by session
name (agent "helper"'s session is "agent-helper").

Supported keys include:
  Enter, Escape, Tab, Space
  c-c (Ctrl+C), c-d (Ctrl+D), c-z (Ctrl+Z), etc.
  Up, Down, Left, Right
  Home, End, PgUp, PgDn
  F1-F24

Unrecognized multi-character names are typed literally."#,
    example: r#"  agent key my-session Enter
  agent key agent-helper c-c
  agent key my-session Escape"#,
    args: ArgRule::Exact(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent key`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (session, key) = (&args.args[0], &args.args[1]);
    check_session(ctx, session)?;
    ctx.termd()?.send_special_key(session, key)?;
    ctx.write_out(&format!("Pressed {key} in session {session}.\n"))
}

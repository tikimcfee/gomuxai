//! `agent adopt` (Go: internal/cli/adopt.go).
use agent_backend::ErrNotFound;
use agent_service::{AdoptOptions, AdoptResult};

use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;

// `agent adopt`

pub const DOC: Doc = Doc {
    use_line: "adopt <name>",
    short: "Create a bus entry for an existing g3d-termd session",
    long: r#"Adopt gives an existing g3d-termd session a seat on the agent bus under <name>,
so other agents and hosts can reach it with agent send, status, capture, and
wait without restarting or interrupting what is running in the session.

Run "agent adopt <name>" inside a g3d terminal in the target namespace
(default "agents", or --termd-ns <ns>; "default" and "wall" are refused), or
pass --session <session> to adopt another terminal in that namespace.
If Claude Code is running in the session (or started there later), its session
and status are tracked automatically; otherwise the session uses the "shell"
backend (status is "unknown", and messages are typed immediately).

Cleaning up an adopted session ("agent cleanup <name>") unregisters it from the
bus while leaving the pre-existing session running.

gomuxai never launches anything in an adopted session: agent resume refuses an
adopted agent. If its CLI exits, start it again there yourself (claude -c
continues its last conversation); adopt keeps tracking it."#,
    example: r#"  agent adopt helper
  agent adopt helper --session dev
  agent adopt helper --session dev --backend agy"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent adopt`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "session",
        value_name = "string",
        default_value = "",
        help = "Existing g3d-termd session id to adopt (defaults to $G3D_TERMD_ID)",
    )]
    pub session: String,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let name = &args.args[0];

    // Only pass an explicit --backend (or --backend-config) override; ignore any
    // default backend from an app config unless the user asked for one, so
    // adopt auto-detects Claude or defaults to shell.
    let mut explicit_backend = ctx.globals.backend.clone();
    if explicit_backend.is_empty() && !ctx.globals.backend_config.is_empty() {
        explicit_backend = ctx.cfg().backend.clone();
    }

    let res = ctx.service()?.adopt(AdoptOptions {
        name: name.clone(),
        session_name: args.session.clone(),
        current_pane: std::env::var("G3D_TERMD_ID").unwrap_or_default(),
        backend: explicit_backend,
        spawned_by: ctx.non_interactive_caller_agent(),
    });
    let res = match res {
        Err(e) if e.chain().any(|c| c.downcast_ref::<ErrNotFound>().is_some()) => {
            return Err(ctx.with_backend_config_err(e));
        }
        r => r?,
    };
    ctx.write_out(&adopt_summary(ctx, &res))
}

/// Go: adoptSummary.
fn adopt_summary(ctx: &Ctx, res: &AdoptResult) -> String {
    let a = &res.agent;
    let mut b = format!("Adopted agent {} (session {})\n", a.name, a.session_name);
    b += &format!("  Backend: {}\n", a.backend);
    if !a.cwd.is_empty() {
        b += &format!("  Cwd:     {}\n", a.cwd);
    }
    if !res.session_id.is_empty() {
        b += &format!("  Session: {}\n", res.session_id);
    }
    if !ctx.quiet() {
        b += &format!(
            "\nNext: agent status {} -v  |  agent send {} \"...\"  |  agent cleanup {} (leaves {} running)\n",
            a.name, a.name, a.name, a.session_name
        );
    }
    b
}

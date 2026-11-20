//! `agent version` (Go: internal/cli/version.go). `agent --version` prints the
//! same line (root.rs).

use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;

// `agent version`

pub const DOC: Doc = Doc {
    use_line: "version",
    short: "Print which build of agent this is",
    long: r#"Version prints one line: the module version ("devel" unless built from a
tag), the VCS revision ("-dirty" if built with uncommitted changes), the
commit time, the Go version, and the platform. Builds without VCS info (e.g.
go test, or outside the checkout) say "(no VCS info)". "agent --version"
prints the same line."#,
    example: r#"  agent version
  agent --version"#,
    args: ArgRule::None,
    // Needs no database: it must work even when nothing else does.
    skip_setup: true,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent version`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, _args: &Args) -> anyhow::Result<()> {
    ctx.write_out(&format!("{}\n", version_line()))
}

/// "agent <build>": what version and --version print.
pub fn version_line() -> String {
    format!("agent {}", agent_base::buildinfo::current())
}

//! `agent statuses` (Go: internal/cli/statuses.go).

use agent_base::types;

use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;

// `agent statuses`

pub const DOC: Doc = Doc {
    use_line: "statuses",
    short: "Say what each status and end reason means",
    long: r#"Statuses prints every status an agent or session can have, and every end
reason a status of ended carries, with what each means. agent status,
agent list, agent wait, agent sessions and agent export all use these words;
--json prints the end reason as "end_reason"."#,
    example: "  agent statuses",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent statuses`.
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
    let mut out = String::from("Statuses:\n");
    for m in types::STATUSES {
        out.push_str(&format!("  {:<11} {}\n", m.name, m.means));
    }
    out.push_str("\nEnd reasons (beside ended):\n");
    for m in types::END_REASONS {
        out.push_str(&format!("  {:<11} {}\n", m.name, m.means));
    }
    ctx.write_out(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_lists_every_status_and_end_reason() {
        let (ctx, out, _) = Ctx::for_test();
        run(&ctx, &Args { args: Vec::new() }).unwrap();
        let out = out.contents();
        assert!(out.starts_with("Statuses:\n  idle        at its prompt"), "{out}");
        assert!(out.contains("\n\nEnd reasons (beside ended):\n  quit        "), "{out}");
        assert_eq!(out.lines().count(), 3 + types::STATUSES.len() + types::END_REASONS.len());
    }
}

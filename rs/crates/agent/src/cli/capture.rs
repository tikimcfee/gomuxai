//! `agent capture` (Go: internal/cli/capture.go).
use agent_service::Context;

use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::remote::{call_remote, parse_target};
use super::root::Ctx;

// `agent capture`

pub const DOC: Doc = Doc {
    use_line: "capture <name>[@host]",
    short: "Print an agent's screen and recent scrollback",
    long: r#"Capture prints what an agent's terminal shows now: the screen plus up to
--lines lines of scrollback above it. It takes an agent name, not a session
name. For history older than the scrollback, or after cleanup, use
"agent logs".

By default it prints plain text. --raw prints the screen with its colors and
styling as SGR escape codes: what tells a CLI's dim placeholder text from
typed text. Print it to a terminal to see the styling, or pipe it to cat -v
to read the codes.

An agent on another host is addressed as name@host (needs a running bus).
Its text is sanitized, since that host may be hostile: plain text keeps no
control characters, and --raw keeps only the SGR codes (colors, bold, dim),
never titles, clipboard writes, cursor moves, or mode changes."#,
    example: r#"  agent capture researcher
  agent capture builder --lines 100
  agent capture builder --raw
  agent capture builder@hub"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent capture`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "lines",
        value_name = "int",
        default_value = "50",
        value_parser = flags::parse_int,
        help = "Lines of scrollback above the screen to include",
    )]
    pub lines: i64,
    #[arg(long = "raw", help = "Keep colors and styling, as SGR escape codes")]
    pub raw: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (r, remote) = parse_target(ctx, &args.args[0])?;

    let output = if remote {
        call_remote(ctx, &r.host, |c| {
            c.capture(&Context::background(), &r, args.lines, args.raw)
        })?
    } else {
        ctx.service()?.capture(&r.name, args.lines as i32, args.raw)?
    };

    ctx.write_out(&output)
}

#[cfg(test)]
mod capture_test {
    use std::time::{Duration, Instant};

    use crate::cli::testenv::TestEnv;

    // capture prints the agent's screen as text, and with --raw keeps its
    // styling as SGR codes.
    #[test]
    fn capture_prints_the_screen_plain_and_raw() {
        crate::require_termd!();
        let mut env = TestEnv::setup_cli();
        let t = env.termd_ops();
        t.send_keys(
            "rpc-cli-worker",
            r"printf 'plain-%d \033[2mdim-text\033[0m\n' $((40+2))",
        )
        .unwrap();
        t.send_special_key("rpc-cli-worker", "Enter").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !t
            .capture("rpc-cli-worker", 0)
            .unwrap_or_default()
            .contains("plain-42 dim-text")
        {
            assert!(Instant::now() < deadline, "the output never showed");
            std::thread::sleep(Duration::from_millis(20));
        }

        let (out, res) = env.run(&["capture", "worker"]);
        res.unwrap();
        assert!(
            out.lines().any(|l| l.trim_end() == "plain-42 dim-text"),
            "capture: {out:?}"
        );
        assert!(!out.contains('\x1b'), "plain capture kept escapes: {out:?}");

        let (raw, res) = env.run(&["capture", "worker", "--raw", "--lines", "10"]);
        res.unwrap();
        let line = raw
            .lines()
            .find(|l| l.starts_with("plain-42"))
            .unwrap_or_else(|| panic!("capture --raw: {raw:?}"));
        let dim = line.find("dim-text").unwrap();
        let before = &line[..dim];
        assert!(
            before.contains("\x1b[") && before.contains('2'),
            "capture --raw lost the dim SGR before dim-text: {line:?}"
        );

        let (_, res) = env.run(&["capture", "ghost"]);
        assert!(res.is_err(), "capture of no agent");
    }
}

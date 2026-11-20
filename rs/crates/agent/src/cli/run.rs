//! `agent run`: heavy jobs (no Go file: new in the Rust build; design in
//! docs/FLEET.md, "`agent run`: heavy jobs"; the work is agent_fleet::run).

use std::fmt::Write as _;
use std::time::Duration;

use agent_base::gostd::{os, time};
use agent_base::util::shell_join;
use agent_fleet::run as job;
use agent_store::Job;
use clap::parser::ValueSource;
use clap::{ArgMatches, FromArgMatches as _};

use super::command::{ArgRule, Cmd, Def, Doc};
use super::exit::ExitError;
use super::flags;
use super::root::{Ctx, command, positionals};

// `agent run`

pub const DOC: Doc = Doc {
    use_line: "run <file|name> [args...]",
    short: "Run a script as a job in its own memory-limited scope, and keep its record",
    long: r#"Run runs a script on disk as a job and waits for it, its output in this
terminal, then exits with the script's exit code. It takes a file, not a
command line, so every job leaves a record: the script's path, sha256 and
content as it was read, who started it (an agent's name, or "user"), the
host, the directory, its arguments, when it started and ended, and its exit
code. Everything after the script goes to it as its arguments, flags
included.

What runs is the job's own copy of what was read,
<data dir>/jobs/<id>/script, in the directory run was started in, so the
record is what ran even if the file changes meanwhile. The script sees that
copy as $0; AGENT_RUN_SCRIPT holds the original's path, and AGENT_JOB_ID
the job's id. A script with a #! line runs with the interpreter it names,
an executable binary as itself, anything else with sh.

On Linux the job runs in its own systemd user scope
(agent-job-<store>-<id>.scope, <store> naming this data dir) with
MemoryHigh and MemoryMax (by default 75% and 85% of RAM) and MemorySwapMax
0, so when memory runs out the job is what gets killed, not the terminal
that started it. Sizes are as systemd takes them: bytes, a K, M, G or T
suffix (24G), a percentage of RAM (80%), or infinity. The limits shown and
recorded are the ones systemd reports for the scope, checked before the
script starts. A limit given with --memory-high, --memory-max or
--memory-swap-max that can't be applied (no scope, or systemd or the
kernel not holding it) is an error, and nothing runs, unless
--no-limit-ok; the default limits that can't be applied are said, and
where no scope can be created the job runs without limits, its record
saying so. On macOS it always runs that way: only the record is kept. A
job runs once. It gets TMPDIR=/var/tmp and CARGO_BUILD_JOBS=12 unless they
are set (even to empty). Jobs run in parallel: there is no lock.

Ctrl-C reaches the job, which decides what to do with it. SIGTERM or SIGHUP
to run stops the job (its scope, or its process) and records the end.

Before each run it also asks systemd to kill g3d-termd's units last
(ManagedOOMPreference=avoid), and says once if it can't; --no-protect skips
that.

--promote copies a script to <data dir>/scripts/<name> (--as names it, else
the file's name does) and records its sha256, so "agent run <name>" runs it
from any directory. A different script than the one recorded under that
name replaces it only with --force, and a promoted script whose file has
changed since is refused until it is promoted again.

--list shows the running jobs (--all: finished ones too), and --show <id>
one job's record, with the script as it ran. A job whose runner was killed
shows as ended at a time not known, its exit code unknown."#,
    example: r#"  agent run build.sh
  agent run build.sh --release -j 4     # everything after the script is its own
  agent run --memory-max 24G --memory-high 20G build.sh
  agent run --memory-max 24G --no-limit-ok build.sh   # run even where 24G can't be applied
  agent run --list --all
  agent run --show 17
  agent run --promote build.sh --as nightly
  agent run nightly"#,
    args: ArgRule::Custom(run_args),
    exclusive: &[&["list", "show", "promote"]],
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent run`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "all",
        help = "With --list: finished jobs too"
    )]
    pub all: bool,
    #[arg(
        id = "as",
        long = "as",
        value_name = "string",
        default_value = "",
        help = "With --promote: the name to run it by (default: the file's name)"
    )]
    pub as_name: String,
    #[arg(
        long = "force",
        help = "With --promote: replace a different script promoted under that name"
    )]
    pub force: bool,
    #[arg(long = "json", help = "With --list or --show: print as JSON")]
    pub json: bool,
    #[arg(long = "list", help = "List the running jobs")]
    pub list: bool,
    #[arg(
        long = "memory-high",
        value_name = "string",
        default_value = "",
        help = "MemoryHigh for the job's scope: throttled above it (default: 75% of RAM)"
    )]
    pub memory_high: String,
    #[arg(
        long = "memory-max",
        value_name = "string",
        default_value = "",
        help = "MemoryMax for the job's scope: killed above it (default: 85% of RAM)"
    )]
    pub memory_max: String,
    #[arg(
        long = "memory-swap-max",
        value_name = "string",
        default_value = "",
        help = "MemorySwapMax for the job's scope (default: 0, no swap)"
    )]
    pub memory_swap_max: String,
    #[arg(
        long = "no-limit-ok",
        help = "Run the job even where a limit given can't be applied (saying so)"
    )]
    pub no_limit_ok: bool,
    #[arg(
        long = "no-protect",
        help = "Don't ask systemd to kill g3d-termd's units last"
    )]
    pub no_protect: bool,
    #[arg(
        long = "promote",
        help = "Copy the script to the shared scripts dir, to run by name"
    )]
    pub promote: bool,
    #[arg(
        long = "show",
        value_name = "int",
        value_parser = flags::parse_int,
        help = "Print one job's record, with the script as it ran"
    )]
    pub show: Option<i64>,
    /// The script, then its arguments: everything after the script is the
    /// script's, flags included.
    #[arg(hide = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// run's argument rule: a script to run (and its arguments); --list, --show
/// <id> and their flags, and nothing else; or --promote and one file.
fn run_args(_cmd: &Cmd, m: &ArgMatches, args: &[String]) -> anyhow::Result<()> {
    let mut f = Args::from_arg_matches(m)?;
    let given = |id: &str| m.value_source(id) == Some(ValueSource::CommandLine);
    if f.promote {
        f = promote_args(f)?;
    }
    if f.all && !f.list {
        anyhow::bail!("--all goes with --list: agent run --list --all");
    }
    if f.json && !f.list && f.show.is_none() {
        anyhow::bail!("--json goes with --list or --show: agent run --list --json");
    }
    if (given("as") || f.force) && !f.promote {
        anyhow::bail!("--as and --force go with --promote: agent run --promote <file> --as <name>");
    }
    // (id, flag): the flags that go with running a script.
    let run_flags = [
        ("memory_high", "memory-high"),
        ("memory_max", "memory-max"),
        ("memory_swap_max", "memory-swap-max"),
        ("no_limit_ok", "no-limit-ok"),
        ("no_protect", "no-protect"),
    ];
    if (f.list || f.show.is_some() || f.promote)
        && let Some((_, flag)) = run_flags.iter().find(|(id, _)| given(id))
    {
        anyhow::bail!("--{flag} goes with running a script: agent run --{flag} ... <file>");
    }
    if f.list && !args.is_empty() {
        anyhow::bail!("--list takes no script: agent run --list [--all]");
    }
    if f.show.is_some() && !args.is_empty() {
        anyhow::bail!("--show takes a job's id, not a script: agent run --show <id>");
    }
    if f.promote && f.args.len() != 1 {
        anyhow::bail!("--promote takes one file: agent run --promote <file> [--as <name>]");
    }
    if !f.list && f.show.is_none() && !f.promote && args.is_empty() {
        anyhow::bail!("requires at least 1 arg(s), only received 0");
    }
    Ok(())
}

/// With --promote there is no job to give the words after the file to, so
/// they are run's own flags (agent run --promote build.sh --as nightly): the
/// flags parsed again from them, the file the only argument left. Only --as
/// and --force go there; any other flag of run's is refused, not dropped.
fn promote_args(mut f: Args) -> anyhow::Result<Args> {
    if f.args.len() <= 1 {
        return Ok(f);
    }
    // As parse_flags, without the usage it adds: the argument rule adds its own.
    let rest = &f.args[1..];
    let parser = command("run").def().clap_command();
    let m = parser
        .clone()
        .try_get_matches_from(rest)
        .map_err(|e| anyhow::anyhow!(flags::pflag_message(&e, &parser, rest)))?;
    for id in m.ids() {
        let id = id.as_str();
        // Only flags: ids that aren't an argument (the derive's group) are
        // left out.
        let Some(flag) = parser
            .get_arguments()
            .find(|a| a.get_id().as_str() == id)
            .and_then(|a| a.get_long())
        else {
            continue;
        };
        if matches!(id, "as" | "force") || m.value_source(id) != Some(ValueSource::CommandLine) {
            continue;
        }
        anyhow::bail!(
            "--{flag} doesn't go with --promote: agent run --promote <file> [--as <name>] [--force]"
        );
    }
    let more = Args::from_arg_matches(&m)?;
    let mut rest = vec![f.args[0].clone()];
    rest.extend(positionals(&m));
    if m.value_source("as") == Some(ValueSource::CommandLine) {
        f.as_name = more.as_name;
    }
    f.force |= more.force;
    f.args = rest;
    Ok(f)
}

/// Who starts a job or promotes a script: the agent running this command, or
/// "user" for a person.
fn by(ctx: &Ctx) -> String {
    let name = ctx.caller_agent();
    if name.is_empty() {
        "user".to_string()
    } else {
        name
    }
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    if args.list {
        return run_list(ctx, args);
    }
    if let Some(id) = args.show {
        return run_show(ctx, args, id);
    }
    if args.promote {
        return run_promote(ctx, &promote_args(args.clone())?);
    }
    run_job(ctx, args)
}

/// Runs the script to its end, and exits with its code.
fn run_job(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let cfg = ctx.cfg();
    let cwd = os::getwd()?;
    let mut opts = job::StartOptions::new(
        &args.args[0],
        &args.args[1..],
        &by(ctx),
        &cfg.host,
        &cwd,
        &ctx.data_dir(),
    );
    let size = |s: &str| (!s.is_empty()).then(|| s.to_string());
    opts.memory_high = size(&args.memory_high);
    opts.memory_max = size(&args.memory_max);
    opts.memory_swap_max = size(&args.memory_swap_max);
    opts.no_limit_ok = args.no_limit_ok;
    opts.protect_termd = !args.no_protect;
    opts.quiet = ctx.quiet();
    let finished = job::start(ctx.store()?, &opts)?;
    let j = &finished.job;
    let why = if j.not_run {
        // The note says why; it starts "not run: ".
        format!("job {} ({}) {}", j.id, j.script, j.note)
    } else if let Some(sig) = finished.stopped_by {
        format!(
            "job {} ({}) stopped: agent run got {}; it exited {}",
            j.id,
            j.script,
            job::signal_name(sig),
            finished.exit_code
        )
    } else if finished.exit_code != 0 {
        format!("job {} ({}) exited {}", j.id, j.script, finished.exit_code)
    } else {
        return Ok(());
    };
    let code = match finished.stopped_by {
        Some(sig) => 128 + sig,
        None if finished.exit_code == 0 => 1,
        None => finished.exit_code,
    };
    Err(ExitError::new(
        code,
        anyhow::anyhow!("{why} (agent run --show {} has its record)", j.id),
    )
    .into())
}

fn run_list(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let jobs = job::list(ctx.store()?, args.all)?;
    if args.json {
        return ctx.print_json(&jobs);
    }
    if jobs.is_empty() {
        if args.all {
            return ctx.write_out("No jobs yet (agent run <file> runs one).\n");
        }
        return ctx.write_out(
            "No jobs running (agent run --list --all shows finished ones too).\n",
        );
    }
    let by_width = jobs.iter().map(|j| j.by.len()).fold(2, usize::max);
    let mut out = format!(
        "{:<6} {:<14} {:<10} {:<4} {:<by_width$} {}\n",
        "ID", "STARTED", "TOOK", "EXIT", "BY", "SCRIPT"
    );
    for j in &jobs {
        let _ = writeln!(
            out,
            "{:<6} {:<14} {:<10} {:<4} {:<by_width$} {}",
            j.id,
            j.started_at.local().0.format("%m-%d %H:%M:%S"),
            took(j),
            exit_text(j),
            j.by,
            command_line(j)
        );
    }
    ctx.write_out(&out)
}

fn run_show(ctx: &Ctx, args: &Args, id: i64) -> anyhow::Result<()> {
    let Some(j) = job::show(ctx.store()?, id)? else {
        anyhow::bail!("no job {id} (agent run --list --all lists them)");
    };
    if args.json {
        return ctx.print_json(&j);
    }
    let mut out = format!("Job {}: {}\n", j.id, command_line(&j));
    let status = if j.running() {
        if j.pid > 0 {
            format!("running (pid {})", j.pid)
        } else {
            "running".to_string()
        }
    } else if j.not_run {
        match j.exit_code {
            Some(code) => format!("not run (exit code {code})"),
            None => "not run".to_string(),
        }
    } else if let Some(found) = &j.found_gone_at {
        format!(
            "ended, when and with what exit code unknown (its runner was killed; found no longer running at {})",
            found.local().0.format("%Y-%m-%d %H:%M:%S")
        )
    } else {
        match j.exit_code {
            Some(code) => format!("exited {code} after {}", took(&j)),
            None => format!(
                "ended after {}, exit code unknown (its runner was killed)",
                took(&j)
            ),
        }
    };
    let _ = writeln!(out, "Status:   {status}");
    let _ = writeln!(
        out,
        "Started:  {} by {} on {}, in {}",
        j.started_at.local().0.format("%Y-%m-%d %H:%M:%S"),
        j.by,
        j.host,
        j.cwd
    );
    if j.ran.is_empty() {
        let _ = writeln!(out, "Ran:      {}", j.path);
    } else {
        let _ = writeln!(out, "Ran:      {} (a copy of {})", j.ran, j.path);
    }
    let _ = writeln!(out, "Sha256:   {}", j.sha256);
    if j.unit.is_empty() {
        let _ = writeln!(out, "Scope:    none (no memory limits)");
    } else {
        let _ = writeln!(
            out,
            "Scope:    {} ({})",
            j.unit,
            job::recorded_limits(&j)
        );
    }
    if !j.note.is_empty() {
        let _ = writeln!(out, "Note:     {}", j.note);
    }
    out.push_str("Script:\n");
    out.push_str(&j.content);
    if !j.content.is_empty() && !j.content.ends_with('\n') {
        out.push('\n');
    }
    ctx.write_out(&out)
}

fn run_promote(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let cwd = os::getwd()?;
    let name = (!args.as_name.is_empty()).then_some(args.as_name.as_str());
    let p = job::promote(
        ctx.store()?,
        &ctx.data_dir(),
        &cwd,
        &args.args[0],
        name,
        &by(ctx),
        args.force,
    )?;
    let (name, sha) = (&p.script.name, short_sha(&p.script.sha256));
    let line = match p.outcome {
        job::PromoteOutcome::New => format!(
            "Promoted {} as {name} (sha256 {sha}): agent run {name} runs it.\n",
            p.script.source
        ),
        job::PromoteOutcome::Unchanged => format!(
            "{name} is already this script (sha256 {sha}): agent run {name} runs it.\n"
        ),
        job::PromoteOutcome::Restored => format!(
            "{name} is this script (sha256 {sha}), but its file had changed since it was promoted: wrote it back. agent run {name} runs it.\n"
        ),
        job::PromoteOutcome::Replaced => format!(
            "Replaced the script promoted as {name} with {} (sha256 {sha}).\n",
            p.script.source
        ),
    };
    ctx.write_out(&line)
}

/// The script and its arguments, as a shell would take them.
fn command_line(j: &Job) -> String {
    let mut words = vec![j.script.clone()];
    words.extend(j.args.iter().cloned());
    shell_join(&words)
}

/// How long a job ran, in whole seconds; "running" while it runs, "not run"
/// for one whose script never started, "?" when its end isn't known.
fn took(j: &Job) -> String {
    if j.running() {
        return "running".to_string();
    }
    if j.not_run {
        return "not run".to_string();
    }
    let Some(ended) = j.ended_at else {
        return "?".to_string();
    };
    let secs = (ended - j.started_at).num_seconds().max(0) as u64;
    time::format_duration(Duration::from_secs(secs))
}

/// A job's exit code: "-" while it runs (or for one not run without one),
/// "?" when its runner was killed.
fn exit_text(j: &Job) -> String {
    match (j.running(), j.exit_code) {
        (true, _) => "-".to_string(),
        (false, Some(code)) => code.to_string(),
        (false, None) if j.not_run => "-".to_string(),
        (false, None) => "?".to_string(),
    }
}

/// A sha256's first 12 hex digits, for messages (as agent_fleet's own).
fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

//! `agent dream schedule`: a systemd user timer that runs `agent dream run`
//! (docs/DREAMING.md, "Schedule"; the units are rendered by
//! agent_dream::schedule). New in the Rust build; wired under `agent dream`
//! by its group's def().

use std::path::{Path, PathBuf};

use agent_base::config::agent_binary_path;
use agent_dream::schedule::{self, Recurrence};
use anyhow::{Context as _, bail};

use super::command::{ArgRule, Def, Doc};
use super::dream::read_scope;
use super::root::{Ctx, getenv};

pub const DOC: Doc = Doc {
    use_line: "schedule <daily HH:MM | every Nh | off | status>",
    short: "Run dreams on a timer (a systemd user timer)",
    long: r#"Schedule runs agent dream run on a timer: a systemd user timer and service,
gomuxai-dream.timer and gomuxai-dream.service in ~/.config/systemd/user.

  daily HH:MM   once a day at that local time
  every Nh      every N hours (1h to 24h), on the hour
  off           stop the timer and remove both units
  status        whether it's on, how often, and when it runs next (the
                default)

Turning it on writes both units, reloads systemd and enables the timer. The
service runs this agent binary, from its directory, with the PATH schedule
was run with (so a dream finds claude and git). It refuses while nothing is
in the dream scope (agent dream scope add). A dream missed while the
machine was off runs once when it's back. Schedule again to change the
time.

--dry-run prints the units and the systemctl commands, and changes nothing.
Linux only for now."#,
    example: r#"  agent dream schedule daily 04:00 --dry-run
  agent dream schedule daily 04:00
  agent dream schedule every 6h
  agent dream schedule status
  agent dream schedule off"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ScheduleArgs {
    #[arg(
        long = "dry-run",
        help = "Print the units and commands, and change nothing"
    )]
    pub dry_run: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, ScheduleArgs, run_schedule)
}

/// Where systemd looks for a user's units.
pub(super) fn unit_dir() -> anyhow::Result<PathBuf> {
    let config = getenv("XDG_CONFIG_HOME");
    let base = if config.starts_with('/') {
        PathBuf::from(config)
    } else {
        let home = getenv("HOME");
        if home.is_empty() {
            bail!("HOME isn't set, so there's no ~/.config/systemd/user to write to");
        }
        Path::new(&home).join(".config")
    };
    Ok(base.join("systemd").join("user"))
}

fn timer_name() -> String {
    format!("{}.timer", schedule::UNIT)
}

fn service_name() -> String {
    format!("{}.service", schedule::UNIT)
}

/// What turning the schedule on or off writes, removes and runs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub write: Vec<(PathBuf, String)>,
    pub remove: Vec<PathBuf>,
    pub commands: Vec<Vec<String>>,
}

pub(super) fn systemctl(args: &[&str]) -> Vec<String> {
    std::iter::once("systemctl")
        .chain(std::iter::once("--user"))
        .chain(args.iter().copied())
        .map(String::from)
        .collect()
}

/// A value going into a unit file: systemd would split it at whitespace,
/// expand % specifiers, and read quotes and backslashes.
pub(super) fn unit_value(what: &str, v: &str) -> anyhow::Result<()> {
    if v.is_empty() {
        bail!("{what} is empty");
    }
    if let Some(c) = v
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || matches!(c, '%' | '"' | '\'' | '\\'))
    {
        bail!("{what} {v:?} has {c:?} in it, which a systemd unit would misread");
    }
    Ok(())
}

/// The plan to turn the schedule on: both units, a reload, and the timer
/// enabled and started (restarted, if it was on, so a new time applies).
pub fn plan_on(
    unit_dir: &Path,
    binary: &str,
    path_env: &str,
    r: &Recurrence,
) -> anyhow::Result<Plan> {
    unit_value("the agent binary's path", binary)?;
    unit_value("PATH", path_env)?;
    let workdir = Path::new(binary)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    unit_value("the agent binary's directory", &workdir)?;
    Ok(Plan {
        write: vec![
            (
                unit_dir.join(service_name()),
                schedule::service_unit(binary, path_env, &workdir),
            ),
            (unit_dir.join(timer_name()), schedule::timer_unit(r)),
        ],
        remove: Vec::new(),
        commands: vec![
            systemctl(&["daemon-reload"]),
            systemctl(&["enable", "--now", &timer_name()]),
            systemctl(&["restart", &timer_name()]),
        ],
    })
}

/// The plan to turn it off: the timer stopped and disabled, both units
/// removed, and a reload.
pub fn plan_off(unit_dir: &Path) -> Plan {
    Plan {
        write: Vec::new(),
        remove: vec![unit_dir.join(timer_name()), unit_dir.join(service_name())],
        commands: vec![
            systemctl(&["disable", "--now", &timer_name()]),
            systemctl(&["daemon-reload"]),
        ],
    }
}

/// The plan as --dry-run prints it.
pub fn format_plan(p: &Plan) -> String {
    let mut out = String::new();
    for (path, content) in &p.write {
        out.push_str(&format!("Would write {}:\n", path.display()));
        for l in content.lines() {
            out.push_str(&format!("  {l}\n"));
        }
    }
    // The timer is disabled before its files go.
    let (first, rest) = if p.remove.is_empty() {
        (&p.commands[..], &[][..])
    } else {
        p.commands.split_at(1)
    };
    for c in first {
        out.push_str(&format!("Would run: {}\n", c.join(" ")));
    }
    for path in &p.remove {
        out.push_str(&format!("Would remove {}\n", path.display()));
    }
    for c in rest {
        out.push_str(&format!("Would run: {}\n", c.join(" ")));
    }
    out
}

fn run_command(c: &[String]) -> anyhow::Result<String> {
    let out = std::process::Command::new(&c[0])
        .args(&c[1..])
        .output()
        .with_context(|| format!("running {}", c.join(" ")))?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            c.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn carry_out(p: &Plan, tolerate_failure: bool) -> anyhow::Result<()> {
    for (path, content) in &p.write {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    }
    let (first, rest) = if p.remove.is_empty() {
        (&p.commands[..], &[][..])
    } else {
        p.commands.split_at(1)
    };
    for c in first {
        if let Err(e) = run_command(c)
            && !tolerate_failure
        {
            return Err(e);
        }
    }
    for path in &p.remove {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    for c in rest {
        run_command(c)?;
    }
    Ok(())
}

/// The schedule a timer unit carries, in words (its Description), if any.
pub fn described(timer_unit: &str) -> Option<String> {
    timer_unit
        .lines()
        .find_map(|l| l.strip_prefix("Description=gomuxai dream, "))
        .map(|s| s.trim().to_string())
}

pub fn run_schedule(ctx: &Ctx, args: &ScheduleArgs) -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        bail!(
            "agent dream schedule isn't supported on macOS yet; run agent dream run from launchd or cron yourself"
        );
    }
    let words: Vec<&str> = args.args.iter().map(String::as_str).collect();
    let dir = unit_dir()?;
    match words.as_slice() {
        [] | ["status"] => status(ctx, &dir),
        ["off"] => {
            let p = plan_off(&dir);
            if args.dry_run {
                return ctx.write_out(&format_plan(&p));
            }
            if !dir.join(timer_name()).exists() && !dir.join(service_name()).exists() {
                return ctx.write_out("Dreams aren't scheduled; nothing to turn off.\n");
            }
            // A timer systemd no longer knows can't be disabled: the files
            // still go.
            carry_out(&p, true)?;
            ctx.write_out(
                "Dreams are no longer scheduled: the timer is off and its units are removed.\n",
            )
        }
        _ => {
            let r = Recurrence::parse(&words)
                .context("agent dream schedule takes daily HH:MM, every Nh, off or status")?;
            if read_scope(ctx)?.is_empty() {
                bail!(
                    "nothing is in the dream scope, so a scheduled dream would review nothing; add a directory first (agent dream scope add <dir>)"
                );
            }
            let binary = agent_binary_path();
            if binary.is_empty() {
                bail!("can't tell where this agent binary is");
            }
            let p = plan_on(&dir, &binary, &getenv("PATH"), &r)?;
            if args.dry_run {
                return ctx.write_out(&format_plan(&p));
            }
            carry_out(&p, false)?;
            ctx.write_out(&format!(
                "Dreams are scheduled {}: {} runs {} dream run.\n",
                r.describe(),
                timer_name(),
                binary
            ))
        }
    }
}

fn status(ctx: &Ctx, dir: &Path) -> anyhow::Result<()> {
    let timer = dir.join(timer_name());
    let Ok(unit) = std::fs::read_to_string(&timer) else {
        return ctx.write_out(
            "Dreams aren't scheduled. agent dream schedule daily HH:MM (or every Nh) turns it on.\n",
        );
    };
    let when = described(&unit).unwrap_or_else(|| "on an unknown schedule".to_string());
    let active = run_command(&systemctl(&["is-active", &timer_name()]))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "inactive".to_string());
    let mut out = format!(
        "Dreams are scheduled {when} ({}, {active}).\n",
        timer.display()
    );
    match run_command(&systemctl(&[
        "list-timers",
        "--all",
        "--no-pager",
        &timer_name(),
    ])) {
        Ok(listing) => {
            for l in listing.lines().filter(|l| !l.trim().is_empty()) {
                out.push_str(&format!("  {l}\n"));
            }
        }
        Err(e) => out.push_str(&format!("The next run is unknown: {e:#}\n")),
    }
    ctx.write_out(&out)
}

#[cfg(test)]
#[path = "dream_schedule_test.rs"]
mod dream_schedule_test;

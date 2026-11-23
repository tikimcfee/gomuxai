//! `agent bus service`: systemd user units that run this host's bus from
//! boot, and optionally resume agents once it is up. New in the Rust build
//! (no Go file); wired under `agent bus` by its group's def().
//!
//! The bus unit runs `agent bus run --saved`: the settings `agent bus start`
//! and `restart` save (bus/settings.json), so changing them never rewrites
//! the unit. While the unit runs the bus, `agent bus start`, `stop`,
//! `restart` and `rename` go through systemctl instead of the gomux-bus
//! session.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use super::bus::{BUS_START_TIMEOUT, bus_session, bus_set_up, connect_as, query_bus};
use super::bus_identity::{
    BusSettings, load_bus_settings, running_bus_settings, save_bus_settings,
};
use super::command::{ArgRule, Def, Doc};
use super::dream_schedule::{systemctl, unit_value};
use super::root::{Ctx, getenv};

pub const DOC: Doc = Doc {
    use_line: "service <install | remove | status>",
    short: "Run the bus from boot (systemd user units)",
    long: r#"Service makes this host's bus come back by itself after a reboot: a
systemd user unit, gomuxai-bus.service in ~/.config/systemd/user, that runs
the bus with its saved settings (what agent bus start and restart save:
--listen, --client-port, --remote-ops).

  install   write the unit and enable it. With no bus running it starts
            now. A bus running in session gomux-bus is left alone: stop it
            and start it again to hand it to the unit (install says how),
            or let the unit take over at the next boot.
  remove    stop the bus if the unit runs it, and remove the units
  status    what is installed, what it runs, and whether it is running
            (the default)

While the unit runs the bus, agent bus status, start, stop, restart and
rename work as before: they start and stop the unit instead of a session.
The bus's output is in the journal (journalctl --user -u gomuxai-bus).

install can also write gomuxai-resume.service, which runs once at boot,
after the bus, and resumes agents the reboot ended:

  --resume a,b,c   those agents, in that order (parents before helpers)
  --resume-all     agent resume --all: every agent the reboot ended
  --no-resume      remove the resume unit

With none of them, install leaves an installed resume unit as it is. The
units run this agent binary with the PATH install was run with. They start
at boot only if the user's systemd manager does: loginctl enable-linger.

--dry-run prints what install or remove would write and run, and changes
nothing. Linux only for now."#,
    example: r#"  agent bus service install --dry-run
  agent bus service install --resume lead,lead-build
  agent bus service status
  agent bus service remove"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ServiceArgs {
    #[arg(
        long = "resume",
        value_name = "strings",
        value_delimiter = ',',
        help = "install: also resume these agents at boot, in this order"
    )]
    pub resume: Vec<String>,
    #[arg(
        long = "resume-all",
        help = "install: also run agent resume --all at boot"
    )]
    pub resume_all: bool,
    #[arg(long = "no-resume", help = "install: remove the resume unit")]
    pub no_resume: bool,
    #[arg(
        long = "dry-run",
        help = "Print the units and commands, and change nothing"
    )]
    pub dry_run: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, ServiceArgs, run_service)
}

/// What a test's stand-in for running a command answers: whether it
/// succeeded, and its stdout (or, failed, its stderr).
type ToolFn = Box<dyn Fn(&[String]) -> (bool, String)>;

thread_local! {
    /// A test's unit directory, instead of the user's.
    static TEST_UNIT_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// A test's stand-in for systemctl, loginctl and journalctl.
    static TEST_TOOL: RefCell<Option<ToolFn>> = const { RefCell::new(None) };
}

/// Points this test thread at its own unit directory and stand-in tools.
#[cfg(test)]
pub fn set_test_units(dir: Option<PathBuf>, tool: Option<ToolFn>) {
    TEST_UNIT_DIR.with(|d| *d.borrow_mut() = dir);
    TEST_TOOL.with(|t| *t.borrow_mut() = tool);
}

/// Where systemd looks for the user's units.
fn unit_dir() -> anyhow::Result<PathBuf> {
    match TEST_UNIT_DIR.with(|d| d.borrow().clone()) {
        Some(dir) => Ok(dir),
        // A test without its own never reads the user's units.
        None if cfg!(test) => bail!("no unit directory in tests"),
        None => super::dream_schedule::unit_dir(),
    }
}

/// Runs a command: whether it succeeded, and its stdout (failed: its stderr
/// if it wrote any, else its stdout, which is where systemctl is-active
/// says "inactive").
fn tool(c: &[String]) -> (bool, String) {
    if let Some(res) = TEST_TOOL.with(|t| t.borrow().as_ref().map(|f| f(c))) {
        return res;
    }
    // A test without a stand-in never reaches the user's systemd.
    if cfg!(test) {
        return (false, "no systemd in tests".to_string());
    }
    match std::process::Command::new(&c[0]).args(&c[1..]).output() {
        Err(e) => (false, format!("running {}: {e}", c[0])),
        Ok(out) => {
            let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
            if out.status.success() {
                return (true, text(&out.stdout));
            }
            let err = text(&out.stderr);
            (
                false,
                if err.is_empty() {
                    text(&out.stdout)
                } else {
                    err
                },
            )
        }
    }
}

/// Runs a command that must succeed.
fn must(c: &[String]) -> anyhow::Result<String> {
    match tool(c) {
        (true, out) => Ok(out),
        (false, err) => bail!("{} failed: {err}", c.join(" ")),
    }
}

/// The first part of both units' names: "gomuxai", or GOMUXAI_UNIT_PREFIX
/// (letters, digits, "-" and "_"), for a second agent binary with its own
/// data dir on the machine, and for tests against a real systemd.
pub fn unit_prefix() -> String {
    let p = getenv("GOMUXAI_UNIT_PREFIX");
    let plain = !p.is_empty()
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if plain { p } else { "gomuxai".to_string() }
}

/// The unit that runs the bus.
pub fn bus_unit() -> String {
    format!("{}-bus.service", unit_prefix())
}

/// The unit that resumes agents at boot.
pub fn resume_unit() -> String {
    format!("{}-resume.service", unit_prefix())
}

/// What the resume unit resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// `agent resume --all`.
    All,
    /// These agents, in this order.
    Names(Vec<String>),
}

/// The bus unit. KillMode=process: stopping it signals only `agent bus run`,
/// which shuts agent-busd down itself (and agent-busd exits when its parent
/// is gone), so nothing else that ever lands in the unit's cgroup, a termd
/// daemon above all, is killed with it.
pub fn bus_unit_text(binary: &str, path_env: &str, workdir: &str) -> String {
    format!(
        "[Unit]\n\
         Description=gomuxai agent bus (agent bus run)\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={workdir}\n\
         Environment=PATH={path_env}\n\
         ExecStart={binary} bus run --saved\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// The resume unit: once per boot, after the bus unit. Each agent is its own
/// ExecStart with "-", so one that can't be resumed doesn't stop the rest.
/// RemainAfterExit and KillMode=process keep what a resume started (a termd
/// daemon, if none ran) alive after the unit is done.
pub fn resume_unit_text(
    binary: &str,
    path_env: &str,
    workdir: &str,
    bus_unit: &str,
    what: &Resume,
) -> String {
    let mut out = format!(
        "[Unit]\n\
         Description=gomuxai: resume agents after a boot (agent resume)\n\
         After={bus_unit}\n\
         Wants={bus_unit}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         KillMode=process\n\
         WorkingDirectory={workdir}\n\
         Environment=PATH={path_env}\n"
    );
    match what {
        Resume::All => out.push_str(&format!("ExecStart=-{binary} resume --all\n")),
        Resume::Names(names) => {
            for n in names {
                out.push_str(&format!("ExecStart=-{binary} resume {n}\n"));
            }
        }
    }
    out.push_str("\n[Install]\nWantedBy=default.target\n");
    out
}

/// The binary a unit's first ExecStart runs.
pub fn unit_binary(unit: &str) -> Option<String> {
    unit.lines().find_map(|l| {
        let cmd = l.strip_prefix("ExecStart=")?;
        cmd.trim_start_matches('-')
            .split_whitespace()
            .next()
            .map(str::to_string)
    })
}

/// What a resume unit resumes, read back from its ExecStart lines.
pub fn unit_resumes(unit: &str) -> Option<Resume> {
    let mut names = Vec::new();
    for l in unit.lines() {
        let Some(cmd) = l.strip_prefix("ExecStart=") else {
            continue;
        };
        let words: Vec<&str> = cmd.split_whitespace().collect();
        match words.as_slice() {
            [_, "resume", "--all"] => return Some(Resume::All),
            [_, "resume", name] => names.push(name.to_string()),
            _ => {}
        }
    }
    (!names.is_empty()).then_some(Resume::Names(names))
}

/// One thing install or remove does, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Write(PathBuf, String),
    Remove(PathBuf),
    /// A command; `true`: its failure is ignored (disabling a unit systemd
    /// no longer knows).
    Run(Vec<String>, bool),
}

/// The steps as --dry-run prints them.
pub fn format_steps(steps: &[Step]) -> String {
    let mut out = String::new();
    for s in steps {
        match s {
            Step::Write(path, content) => {
                out.push_str(&format!("Would write {}:\n", path.display()));
                for l in content.lines() {
                    out.push_str(&format!("  {l}\n"));
                }
            }
            Step::Remove(path) => out.push_str(&format!("Would remove {}\n", path.display())),
            Step::Run(c, _) => out.push_str(&format!("Would run: {}\n", c.join(" "))),
        }
    }
    out
}

fn carry_out(steps: &[Step]) -> anyhow::Result<()> {
    for s in steps {
        match s {
            Step::Write(path, content) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                }
                std::fs::write(path, content)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            Step::Remove(path) => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            },
            Step::Run(c, tolerate) => {
                if let Err(e) = must(c)
                    && !tolerate
                {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// The bus unit's name, if it is installed and runs this agent binary (and
/// so this data dir's bus). A unit for another binary isn't this one's.
pub fn own_unit(ctx: &Ctx) -> Option<String> {
    if cfg!(target_os = "macos") {
        return None;
    }
    let unit = bus_unit();
    let text = std::fs::read_to_string(unit_dir().ok()?.join(&unit)).ok()?;
    let binary = &ctx.cfg().agent_binary;
    (!binary.is_empty() && unit_binary(&text).as_deref() == Some(binary.as_str())).then_some(unit)
}

/// What systemd says of a unit: "active", "inactive", "failed",
/// "activating", ..., or "unknown" if it can't be asked.
fn unit_state(unit: &str) -> String {
    let (_, out) = tool(&systemctl(&["is-active", unit]));
    match out.lines().next().map(str::trim) {
        Some(w) if !w.is_empty() && !w.contains(' ') => w.to_string(),
        _ => "unknown".to_string(),
    }
}

/// The unit that runs this data dir's bus right now, if one does.
pub fn running_unit(ctx: &Ctx) -> Option<String> {
    let unit = own_unit(ctx)?;
    (unit_state(&unit) == "active").then_some(unit)
}

/// The last lines the unit wrote to the journal, for an error.
fn journal_tail(unit: &str) -> String {
    let c: Vec<String> = [
        "journalctl",
        "--user",
        "-u",
        unit,
        "-n",
        "20",
        "--no-pager",
        "-o",
        "cat",
    ]
    .map(String::from)
    .to_vec();
    match tool(&c) {
        (true, out) if !out.is_empty() => out,
        _ => format!("(nothing read; journalctl --user -u {unit} shows it)"),
    }
}

/// Waits for the bus the unit started to answer as host.
fn wait_up(ctx: &Ctx, unit: &str, host: &str) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if let Ok(nc) = connect_as(ctx, host) {
            nc.close();
            return Ok(());
        }
        let state = unit_state(unit);
        if start.elapsed() >= Duration::from_secs(2)
            && matches!(state.as_str(), "failed" | "inactive")
        {
            bail!(
                "the bus exited under {unit} without coming up ({state}); its last output:\n{}",
                journal_tail(unit)
            );
        }
        if start.elapsed() >= BUS_START_TIMEOUT {
            bail!(
                "the bus hasn't answered within {}s under {unit} ({state}): agent bus status shows whether it came up; its last output:\n{}",
                BUS_START_TIMEOUT.as_secs(),
                journal_tail(unit)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts the bus under its unit with s (saved first: the unit runs the
/// saved settings), and waits for it to answer as host.
pub fn start(ctx: &Ctx, unit: &str, host: &str, s: &BusSettings) -> anyhow::Result<()> {
    save_bus_settings(ctx, s)?;
    must(&systemctl(&["start", unit]))?;
    wait_up(ctx, unit, host)
}

/// Stops the bus under its unit; systemctl returns once it has exited.
pub fn stop(unit: &str) -> anyhow::Result<()> {
    must(&systemctl(&["stop", unit])).map(|_| ())
}

/// Restarts the bus under its unit with s, and waits for it to answer.
pub fn restart(ctx: &Ctx, unit: &str, host: &str, s: &BusSettings) -> anyhow::Result<()> {
    save_bus_settings(ctx, s)?;
    must(&systemctl(&["restart", unit]))?;
    wait_up(ctx, unit, host)
}

/// Whether the user's systemd manager starts at boot (linger): None if it
/// can't be told.
fn linger() -> Option<bool> {
    // SAFETY: getuid has no failure mode.
    let uid = unsafe { libc::getuid() }.to_string();
    let c: Vec<String> = [
        "loginctl",
        "show-user",
        &uid,
        "--property=Linger",
        "--value",
    ]
    .map(String::from)
    .to_vec();
    match tool(&c) {
        (true, out) => match out.trim() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

const LINGER_NOTE: &str = "Linger is off for this user, so the units start at login, not at boot: loginctl enable-linger\n";

/// What --resume, --resume-all and --no-resume ask for: None leaves the
/// resume unit as it is, Some(None) removes it.
fn resume_wanted(args: &ServiceArgs) -> anyhow::Result<Option<Option<Resume>>> {
    let given = [!args.resume.is_empty(), args.resume_all, args.no_resume];
    if given.iter().filter(|g| **g).count() > 1 {
        bail!("give one of --resume, --resume-all and --no-resume");
    }
    if args.no_resume {
        return Ok(Some(None));
    }
    if args.resume_all {
        return Ok(Some(Some(Resume::All)));
    }
    if args.resume.is_empty() {
        return Ok(None);
    }
    for n in &args.resume {
        unit_value("an agent name", n)?;
        if n.starts_with('-') || n.contains('@') {
            bail!("--resume takes this host's agents by name, not {n:?}");
        }
    }
    Ok(Some(Some(Resume::Names(args.resume.clone()))))
}

/// The settings the unit will run with: the running bus's, else the saved
/// ones, else the defaults. The second value says which.
fn unit_settings(ctx: &Ctx) -> anyhow::Result<(BusSettings, &'static str)> {
    if let Ok(Some(s)) = running_bus_settings(ctx) {
        return Ok((s, "as the bus runs now"));
    }
    if let Some(s) = load_bus_settings(ctx)? {
        return Ok((s, "the saved settings"));
    }
    Ok((
        BusSettings {
            listen: String::new(),
            client_port: 7420,
            remote_ops: agent_bus::DEFAULT_REMOTE_OPS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        },
        "the defaults; on the hub: agent bus restart --listen 0.0.0.0:7422",
    ))
}

/// A PATH with each directory once, in its first place (a shell inside
/// several nested sessions repeats them).
pub fn unique_path(path: &str) -> String {
    let mut seen = Vec::new();
    for d in path.split(':').filter(|d| !d.is_empty()) {
        if !seen.contains(&d) {
            seen.push(d);
        }
    }
    seen.join(":")
}

pub fn run_service(ctx: &Ctx, args: &ServiceArgs) -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        bail!(
            "agent bus service isn't supported on macOS yet (it writes systemd units); run agent bus start from a LaunchAgent yourself"
        );
    }
    let words: Vec<&str> = args.args.iter().map(String::as_str).collect();
    let dir = unit_dir()?;
    match words.as_slice() {
        [] | ["status"] => status(ctx, &dir),
        ["install"] => install(ctx, args, &dir),
        ["remove"] => remove(ctx, args, &dir),
        _ => bail!("agent bus service takes install, remove or status"),
    }
}

fn install(ctx: &Ctx, args: &ServiceArgs, dir: &Path) -> anyhow::Result<()> {
    let resume = resume_wanted(args)?;
    let cfg = ctx.cfg();
    if !bus_set_up(ctx) {
        bail!(
            "this host ({}) has no bus identity yet, so there is no bus to run: agent bus init (see agent bus --help)",
            cfg.host
        );
    }
    let binary = cfg.agent_binary.clone();
    unit_value("the agent binary's path", &binary)?;
    let path_env = unique_path(&getenv("PATH"));
    unit_value("PATH", &path_env)?;
    let workdir = Path::new(&binary)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    unit_value("the agent binary's directory", &workdir)?;

    let (bus, res) = (bus_unit(), resume_unit());
    let (bus_path, res_path) = (dir.join(&bus), dir.join(&res));
    let installed = std::fs::read_to_string(&bus_path).ok();
    if let Some(other) = installed.as_deref().and_then(unit_binary)
        && other != binary
    {
        bail!(
            "{} already runs another agent binary's bus ({other}): remove it with that binary's agent bus service remove, or set GOMUXAI_UNIT_PREFIX to name this one's units apart",
            bus_path.display()
        );
    }
    if let Some(Some(Resume::Names(names))) = &resume
        && let Ok(store) = ctx.store()
    {
        for n in names {
            if store.get_agent(n)?.is_none() {
                bail!(
                    "no agent named {n:?} on this host to resume at boot (agent list shows them)"
                );
            }
        }
    }

    let (settings, from) = unit_settings(ctx)?;
    let answering = query_bus(ctx, Duration::from_secs(2)).is_ok();
    let by_unit = running_unit(ctx).is_some();
    let bus_text = bus_unit_text(&binary, &path_env, &workdir);
    let changed = installed.as_deref() != Some(bus_text.as_str());

    let mut steps = vec![Step::Write(bus_path, bus_text)];
    match &resume {
        Some(Some(what)) => steps.push(Step::Write(
            res_path.clone(),
            resume_unit_text(&binary, &path_env, &workdir, &bus, what),
        )),
        Some(None) => {
            steps.push(Step::Run(systemctl(&["disable", "--now", &res]), true));
            steps.push(Step::Remove(res_path.clone()));
        }
        None => {}
    }
    steps.push(Step::Run(systemctl(&["daemon-reload"]), false));
    // Started now only when no bus answers: a running one is never stopped
    // by install.
    let enable: &[&str] = if answering {
        &["enable", &bus]
    } else {
        &["enable", "--now", &bus]
    };
    steps.push(Step::Run(systemctl(enable), false));
    if matches!(resume, Some(Some(_))) {
        // Enabled, not started: it resumes at the next boot, not now.
        steps.push(Step::Run(systemctl(&["enable", &res]), false));
    }

    if args.dry_run {
        let mut out = format_steps(&steps);
        out.push_str(&format!("The bus would run with {settings} ({from}).\n"));
        return ctx.write_out(&out);
    }
    save_bus_settings(ctx, &settings)?;
    carry_out(&steps)?;
    let mut out =
        format!("Installed {bus}: it runs {binary} bus run with {settings} ({from}) at boot.\n");
    if !answering {
        wait_up(ctx, &bus, &cfg.host)?;
        out.push_str("The bus is started under it now.\n");
    } else if by_unit {
        if changed {
            out.push_str("It runs the bus now; the unit changed, which applies when the bus restarts: agent bus restart\n");
        } else {
            out.push_str("It runs the bus now.\n");
        }
    } else {
        let session = bus_session();
        let in_session = ctx
            .termd()
            .ok()
            .and_then(|t| t.session_exists(&session).ok())
            .unwrap_or(false);
        if in_session {
            out.push_str(&format!(
                "The bus still runs in session {session}, and the unit takes over at the next boot. To hand it over now (peers relink in a few seconds): agent bus stop, then agent bus start\n"
            ));
        } else {
            out.push_str("A bus is running outside the unit, and the unit takes over at the next boot. To hand it over now: stop the bus where it runs, then agent bus start\n");
        }
    }
    match std::fs::read_to_string(&res_path)
        .ok()
        .as_deref()
        .and_then(unit_resumes)
    {
        Some(what) => out.push_str(&format!(
            "{res}: at boot, after the bus, it resumes {}.\n",
            describe_resume(&what)
        )),
        None if resume == Some(None) => out.push_str(&format!("Removed {res}.\n")),
        None => {}
    }
    if linger() == Some(false) {
        out.push_str(LINGER_NOTE);
    }
    ctx.write_out(&out)
}

fn describe_resume(what: &Resume) -> String {
    match what {
        Resume::All => "every agent the reboot ended (agent resume --all)".to_string(),
        Resume::Names(names) => names.join(", "),
    }
}

fn remove(ctx: &Ctx, args: &ServiceArgs, dir: &Path) -> anyhow::Result<()> {
    let (bus, res) = (bus_unit(), resume_unit());
    let (bus_path, res_path) = (dir.join(&bus), dir.join(&res));
    let steps = vec![
        Step::Run(systemctl(&["disable", "--now", &bus]), true),
        // --now: it stays "active (exited)" after its run; nothing runs in it.
        Step::Run(systemctl(&["disable", "--now", &res]), true),
        Step::Remove(bus_path.clone()),
        Step::Remove(res_path.clone()),
        Step::Run(systemctl(&["daemon-reload"]), false),
    ];
    if args.dry_run {
        return ctx.write_out(&format_steps(&steps));
    }
    let installed = std::fs::read_to_string(&bus_path).ok();
    if installed.is_none() && !res_path.exists() {
        return ctx.write_out("No bus service is installed; nothing to remove.\n");
    }
    let binary = &ctx.cfg().agent_binary;
    if let Some(other) = installed.as_deref().and_then(unit_binary)
        && &other != binary
    {
        bail!(
            "{} runs another agent binary's bus ({other}): remove it with that binary's agent bus service remove",
            bus_path.display()
        );
    }
    let ran = running_unit(ctx).is_some();
    carry_out(&steps)?;
    let mut out = "Removed the bus service: its units are disabled and deleted.\n".to_string();
    if ran {
        out.push_str(
            "The bus ran under it and is stopped: agent bus start runs it in a session.\n",
        );
    }
    ctx.write_out(&out)
}

fn status(ctx: &Ctx, dir: &Path) -> anyhow::Result<()> {
    let (bus, res) = (bus_unit(), resume_unit());
    let bus_path = dir.join(&bus);
    let Ok(text) = std::fs::read_to_string(&bus_path) else {
        return ctx.write_out(
            "No bus service is installed: the bus runs only when started (agent bus start). agent bus service install runs it from boot.\n",
        );
    };
    let binary = unit_binary(&text).unwrap_or_default();
    let enabled = tool(&systemctl(&["is-enabled", &bus])).1;
    let enabled = enabled
        .lines()
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_string();
    let mut out = format!(
        "{bus}: {}, {enabled} ({})\n  runs {binary} bus run --saved\n",
        unit_state(&bus),
        bus_path.display()
    );
    if binary != ctx.cfg().agent_binary {
        out.push_str(&format!(
            "  that is another agent binary's bus, not this one's ({})\n",
            ctx.cfg().agent_binary
        ));
    } else {
        match load_bus_settings(ctx)? {
            Some(s) => out.push_str(&format!("  saved settings: {s}\n")),
            None => out.push_str("  no saved settings: it runs with the defaults\n"),
        }
        if query_bus(ctx, Duration::from_secs(2)).is_ok() && running_unit(ctx).is_none() {
            out.push_str("  a bus is running outside the unit; to hand it over: stop it (agent bus stop), then agent bus start\n");
        }
    }
    match std::fs::read_to_string(dir.join(&res))
        .ok()
        .as_deref()
        .and_then(unit_resumes)
    {
        Some(what) => {
            let enabled = tool(&systemctl(&["is-enabled", &res])).1;
            out.push_str(&format!(
                "{res}: {}; at boot it resumes {}\n",
                enabled.lines().next().unwrap_or("unknown").trim(),
                describe_resume(&what)
            ));
        }
        None => out.push_str(&format!(
            "{res}: not installed (agent bus service install --resume NAMES)\n"
        )),
    }
    match linger() {
        Some(true) => out.push_str("Linger is on: the units start at boot.\n"),
        Some(false) => out.push_str(LINGER_NOTE),
        None => {}
    }
    ctx.write_out(&out)
}

#[cfg(test)]
#[path = "bus_service_test.rs"]
mod bus_service_test;

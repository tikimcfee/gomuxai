//! `agent run`: heavy jobs (docs/FLEET.md, "`agent run`: heavy jobs").
//!
//! A job is a script on disk, run to its end with its stdio inherited. It is
//! recorded in the store (`jobs`) before it starts: the script as read (path,
//! sha256, content), who started it where, then the scope and the limits
//! systemd applied, then its end and exit code. What runs is the job's own
//! copy of what was read (`<data dir>/jobs/<id>/script`), so the record is
//! what ran. On Linux it runs in its own transient systemd scope with
//! MemoryHigh and MemoryMax, so when memory runs out the job is what gets
//! killed, not the terminal that started it. A job runs once: in its scope,
//! or (when the scope can't be created, and only the default limits were
//! asked for) plainly, said before it starts. Jobs run in parallel; there is
//! no lock.
//!
//! The CLI calls:
//!
//! - [`start`] with [`StartOptions`]: runs the job and returns its exit code.
//! - [`list`] and [`show`]: the records, with jobs whose runner died settled
//!   from the system's state (a job's unit, or its process).
//! - [`promote`] and [`resolve`]: promoted scripts.
//! - [`protect_termd`]: also called by start when asked.

mod limits;
mod process;
mod script;
mod systemd;

use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Result, bail};

use agent_base::gostd::{filepath, os};
use agent_store::{Job, Store, Time};

pub use limits::{Limits, format_exact, format_size, parse_size, physical_memory, size_matches};
pub use process::{JobSignals, child_signals, exit_code, pid_alive, process_start, signal_name};
pub use script::{
    PromoteOutcome, Promoted, Resolved, check_promoted, command_line, job_command_line, job_dir,
    promote, resolve, scripts_dir, sha256_hex, validate_name, write_job_copy,
};
pub use systemd::{
    CmdOutput, ProtectReport, SCOPE_WRAPPER, ShownLimits, is_termd_unit, parse_shown_limits,
    protect_termd, protect_termd_with, scope_command, show_limits_command, stop_command,
};

/// The prefix of a job's scope: unit `agent-job-<store>-<id>.scope`.
pub const UNIT_PREFIX: &str = "agent-job-";

/// The host_state key under which start remembers the last protect_termd
/// problem it printed, so each problem is said once, not on every run.
pub const PROTECT_SAID_KEY: &str = "run.protect_termd";

/// The environment a job gets unless the caller set it (even to empty).
pub const DEFAULT_ENV: &[(&str, &str)] = &[("TMPDIR", "/var/tmp"), ("CARGO_BUILD_JOBS", "12")];

/// store_tag is a short stable id of the store at data_dir: the first 8 hex
/// digits of the sha256 of its absolute path (symlinks resolved, where it
/// exists). Every store on one account starts its job ids at 1, and they
/// share one systemd user manager, so a job's unit carries it.
pub fn store_tag(data_dir: &str) -> String {
    let path = std::fs::canonicalize(data_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| filepath::clean(data_dir));
    sha256_hex(path.as_bytes())[..8].to_string()
}

/// unit_name is the scope job id of the store at data_dir runs in:
/// `agent-job-<store_tag>-<id>.scope`.
pub fn unit_name(data_dir: &str, id: i64) -> String {
    format!("{UNIT_PREFIX}{}-{id}.scope", store_tag(data_dir))
}

/// StartOptions is one `agent run`.
#[derive(Clone, Debug)]
pub struct StartOptions {
    /// A script's path (relative to cwd), or a promoted script's name.
    pub script: String,
    pub args: Vec<String>,
    /// Limits in systemd's size syntax; None takes the default (MemoryHigh
    /// 75% and MemoryMax 85% of RAM; MemorySwapMax 0). One given (Some) is
    /// explicit: a job whose scope can't have it isn't run, unless
    /// no_limit_ok.
    pub memory_high: Option<String>,
    pub memory_max: Option<String>,
    pub memory_swap_max: Option<String>,
    /// Run the job even where an explicit limit can't be applied (saying so
    /// before it starts).
    pub no_limit_ok: bool,
    /// Who starts it: an agent's name, or "user".
    pub by: String,
    pub host: String,
    /// The directory the job runs in (absolute).
    pub cwd: String,
    /// The data dir (`.gomuxai`): scripts/ holds promoted scripts, jobs/ the
    /// jobs' copies; its path names the store in unit names.
    pub data_dir: String,
    /// The systemd-run to use (default "systemd-run"); "" runs plainly. Only
    /// Linux uses it.
    pub systemd_run: String,
    /// The systemctl that reads the scope's limits and stops it (default
    /// "systemctl").
    pub systemctl: String,
    /// Where cgroupfs is (default /sys/fs/cgroup): the scope's cgroup is
    /// looked at there to tell whether its limits are enforced.
    pub cgroup_root: String,
    /// Ask systemd to spare g3d-termd's units first (protect_termd).
    pub protect_termd: bool,
    /// Leave out the line start prints when the job starts. Problems are
    /// printed regardless.
    pub quiet: bool,
}

impl StartOptions {
    /// new is a run of script with args, with every default: limits, the
    /// systemd scope, protect_termd on.
    pub fn new(
        script: &str,
        args: &[String],
        by: &str,
        host: &str,
        cwd: &str,
        data_dir: &str,
    ) -> StartOptions {
        StartOptions {
            script: script.to_string(),
            args: args.to_vec(),
            memory_high: None,
            memory_max: None,
            memory_swap_max: None,
            no_limit_ok: false,
            by: by.to_string(),
            host: host.to_string(),
            cwd: cwd.to_string(),
            data_dir: data_dir.to_string(),
            systemd_run: "systemd-run".to_string(),
            systemctl: "systemctl".to_string(),
            cgroup_root: "/sys/fs/cgroup".to_string(),
            protect_termd: true,
            quiet: false,
        }
    }
}

/// Finished is a job that ended: its record as it ended, and the exit code
/// to exit with. job.not_run says its script never started (job.note says
/// why).
#[derive(Clone, Debug)]
pub struct Finished {
    pub job: Job,
    pub exit_code: i32,
    /// What protect_termd did, when start asked it.
    pub protect: Option<ProtectReport>,
    /// The signal (SIGTERM, SIGHUP) that made the runner stop the job.
    pub stopped_by: Option<i32>,
}

/// One limit as asked: its property, the flag that sets it, the size in
/// systemd's syntax and in bytes, and whether the caller gave it.
#[derive(Clone, Debug)]
struct Asked {
    property: &'static str,
    flag: &'static str,
    cgroup_file: &'static str,
    text: String,
    bytes: u64,
    explicit: bool,
}

fn asked(opts: &StartOptions, limits: &Limits, ram: u64) -> Result<Vec<Asked>> {
    let mut v = Vec::new();
    for (property, flag, cgroup_file, text, given) in [
        (
            "MemoryHigh",
            "--memory-high",
            "memory.high",
            &limits.memory_high,
            &opts.memory_high,
        ),
        (
            "MemoryMax",
            "--memory-max",
            "memory.max",
            &limits.memory_max,
            &opts.memory_max,
        ),
        (
            "MemorySwapMax",
            "--memory-swap-max",
            "memory.swap.max",
            &limits.memory_swap_max,
            &opts.memory_swap_max,
        ),
    ] {
        if text.is_empty() {
            continue;
        }
        v.push(Asked {
            property,
            flag,
            cgroup_file,
            text: text.clone(),
            bytes: parse_size(text, ram)?,
            explicit: given.is_some(),
        });
    }
    Ok(v)
}

/// explicit_flags names the limits the caller gave, as given.
fn explicit_flags(asked: &[Asked]) -> Option<String> {
    let f: Vec<String> = asked
        .iter()
        .filter(|a| a.explicit)
        .map(|a| format!("{} {}", a.flag, a.text))
        .collect();
    (!f.is_empty()).then(|| f.join(", "))
}

/// start records a job and runs it to its end, its stdio the caller's, and
/// returns its exit code (a signal death as 128 + signal). The error cases
/// are the ones where the job doesn't run: no such script, a bad limit, a
/// promoted script edited since promotion, an explicit limit its scope
/// can't have, the store. A job recorded before it was refused stays
/// recorded, as not run.
pub fn start(store: &Store, opts: &StartOptions) -> Result<Finished> {
    let resolved = resolve(&opts.script, &opts.cwd, &opts.data_dir)?;
    // Read once: what is checked, recorded, copied and run.
    let content = os::read_file(&resolved.path)?;
    let sha = sha256_hex(&content);
    if let Some(name) = &resolved.promoted {
        check_promoted(store, name, &sha)?;
    }
    let ram = physical_memory();
    let limits = Limits::resolve(
        opts.memory_high.as_deref(),
        opts.memory_max.as_deref(),
        opts.memory_swap_max.as_deref(),
        ram,
    )?;
    let asked = asked(opts, &limits, ram)?;
    let executable = script::is_executable(&resolved.path);

    let mut job = Job {
        script: resolved.given.clone(),
        path: resolved.path.clone(),
        sha256: sha,
        content: String::from_utf8_lossy(&content).into_owned(),
        by: opts.by.clone(),
        host: opts.host.clone(),
        cwd: opts.cwd.clone(),
        args: opts.args.clone(),
        started_at: Time::now(),
        ..Default::default()
    };
    store.create_job(&mut job)?;
    // From here a SIGTERM or SIGHUP is noted, not fatal: the job is stopped
    // (or never started) and its end recorded.
    let signals = JobSignals::new();
    let copy = match write_job_copy(&opts.data_dir, job.id, &content) {
        Ok(p) => p,
        Err(e) => {
            end_not_run(store, &mut job, None, format!("not run: {e:#}"))?;
            return Err(e);
        }
    };
    job.ran = copy.clone();
    let argv = job_command_line(&copy, &content, executable, &opts.args);
    let mut env = job_env(|k| std::env::var_os(k).is_some());
    env.push(("AGENT_JOB_ID".into(), job.id.to_string()));
    env.push(("AGENT_RUN_SCRIPT".into(), resolved.path.clone()));

    let use_systemd = cfg!(target_os = "linux") && !opts.systemd_run.is_empty();
    let protect = if opts.protect_termd && use_systemd {
        let r = protect_termd();
        say_protect_problem(store, &r);
        Some(r)
    } else {
        None
    };
    if let Some(sig) = signals.caught() {
        return stopped_before_start(store, job, sig, protect);
    }

    let outcome = if use_systemd {
        run_in_scope(
            store, opts, &mut job, &limits, &asked, &argv, &env, &signals,
        )?
    } else if cfg!(target_os = "linux") {
        Scoped::NoScope {
            why: "systemd-run not used".into(),
            failed: false,
        }
    } else {
        Scoped::NoScope {
            why: format!("{}: no systemd", std::env::consts::OS),
            failed: false,
        }
    };
    let (code, stopped_by) = match outcome {
        Scoped::Done { code, stopped_by } => (code, stopped_by),
        Scoped::NoScope { why, failed } => {
            if let Some(flags) = explicit_flags(&asked)
                && !opts.no_limit_ok
            {
                let msg = format!(
                    "{flags} can't be applied: {why}; nothing ran (--no-limit-ok runs it without limits)"
                );
                end_not_run(store, &mut job, None, format!("not run: {msg}"))?;
                bail!("job {}: {msg}", job.id);
            }
            if failed || !opts.quiet {
                eprintln!(
                    "agent run: {why}; job {} runs without memory limits",
                    job.id
                );
            }
            job.note = format!("{why}: ran plainly, no memory limits");
            if let Some(sig) = signals.caught() {
                return stopped_before_start(store, job, sig, protect);
            }
            run_plainly(store, &mut job, &argv, &env, &opts.cwd, &signals)?
        }
    };
    if job.not_run {
        // Ended (and recorded) before its script started.
        return Ok(Finished {
            job,
            exit_code: code,
            protect,
            stopped_by,
        });
    }

    if let Some(sig) = stopped_by {
        add_note(
            &mut job,
            &format!("stopped: agent run got {}", signal_name(sig)),
        );
    }
    job.ended_at = Some(Time::now());
    job.exit_code = Some(code);
    store.record_job_end(&job)?;
    Ok(Finished {
        job,
        exit_code: code,
        protect,
        stopped_by,
    })
}

/// add_note appends to a job's note.
fn add_note(job: &mut Job, note: &str) {
    if job.note.is_empty() {
        job.note = note.to_string();
    } else {
        job.note = format!("{}; {note}", job.note);
    }
}

/// end_not_run records that job ended without its script having started.
fn end_not_run(store: &Store, job: &mut Job, code: Option<i32>, note: String) -> Result<()> {
    job.not_run = true;
    job.note = note;
    job.exit_code = code;
    job.ended_at = Some(Time::now());
    store.record_job_end(job)
}

/// stopped_before_start ends a job a stop signal came for before its script
/// started.
fn stopped_before_start(
    store: &Store,
    mut job: Job,
    sig: i32,
    protect: Option<ProtectReport>,
) -> Result<Finished> {
    let code = 128 + sig;
    let note = format!(
        "not run: agent run got {} before the script started",
        signal_name(sig)
    );
    eprintln!("agent run: job {}: {note}", job.id);
    end_not_run(store, &mut job, Some(code), note)?;
    Ok(Finished {
        job,
        exit_code: code,
        protect,
        stopped_by: Some(sig),
    })
}

/// How run_in_scope went.
enum Scoped {
    /// No scope was created, and nothing ran: why, and whether systemd-run
    /// failed (rather than not being used).
    NoScope { why: String, failed: bool },
    /// The job ended (job.not_run says whether its script started).
    Done { code: i32, stopped_by: Option<i32> },
}

/// waitpid, retried on EINTR. flags: WNOHANG, WUNTRACED.
fn wait_pid(pid: libc::pid_t, flags: libc::c_int) -> std::io::Result<Option<libc::c_int>> {
    loop {
        let mut st = 0;
        // SAFETY: waitpid on our own child, into a local.
        let r = unsafe { libc::waitpid(pid, &mut st, flags) };
        if r == pid {
            return Ok(Some(st));
        }
        if r == 0 {
            return Ok(None);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn kill(pid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: signals a child of ours (not yet reaped, so the pid is its).
    unsafe { libc::kill(pid, sig) };
}

fn status_code(st: libc::c_int) -> i32 {
    exit_code(ExitStatus::from_raw(st))
}

/// proc_cmdline is pid's argv, as /proc tells it (empty when it can't).
fn proc_cmdline(pid: libc::pid_t) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// proc_cgroup is the cgroup (v2) pid is in, as /proc tells it.
fn proc_cgroup(pid: libc::pid_t) -> Option<String> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_string)
}

/// run_in_scope runs the job in its own systemd scope: systemd-run creates
/// the scope and starts SCOPE_WRAPPER in it, which stops itself before
/// anything of the job runs; with it stopped, the limits systemd applied are
/// read and checked; then it is continued into the job, or killed (an
/// explicit limit that didn't apply). Returns NoScope when systemd-run
/// exits without the wrapper having stopped: nothing ran.
#[allow(clippy::too_many_arguments)]
fn run_in_scope(
    store: &Store,
    opts: &StartOptions,
    job: &mut Job,
    limits: &Limits,
    asked: &[Asked],
    argv: &[String],
    env: &[(String, String)],
    signals: &JobSignals,
) -> Result<Scoped> {
    let unit = unit_name(&opts.data_dir, job.id);
    let version = systemd::systemd_run_version(&opts.systemd_run);
    let cmdline = scope_command(&opts.systemd_run, version, &unit, limits, argv);
    let mut cmd = Command::new(&cmdline[0]);
    cmd.args(&cmdline[1..])
        .current_dir(&opts.cwd)
        .envs(env.iter().map(|(k, v)| (k, v)));
    // SAFETY: child_signals is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            child_signals();
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let why = format!("{} can't be run ({})", opts.systemd_run, os::errno_text(&e));
            return Ok(Scoped::NoScope { why, failed: true });
        }
    };
    // Waited for with waitpid (stops too), not through child.
    let pid = child.id() as libc::pid_t;
    drop(child);
    job.pid = pid as i64;
    job.pid_start = process_start(job.pid).unwrap_or_default();
    store.update_job_launch(job)?;

    // Wait for the wrapper to stop in the scope. Only its own stop counts: a
    // stop of systemd-run (a Ctrl-Z as it starts) is left to the person's
    // `fg`. A continue this process didn't send (that `fg`, as the wrapper
    // waited) may have let the job start: from then on it is never taken
    // for a scope that wasn't created, whatever its end.
    let mut continued_elsewhere = false;
    loop {
        match wait_pid(pid, libc::WUNTRACED | libc::WCONTINUED | libc::WNOHANG)? {
            None => {
                if let Some(sig) = signals.caught() {
                    if continued_elsewhere {
                        break;
                    }
                    kill(pid, libc::SIGKILL);
                    let _ = wait_pid(pid, 0);
                    let f = stopped_before_start(store, std::mem::take(job), sig, None)?;
                    *job = f.job;
                    return Ok(Scoped::Done {
                        code: f.exit_code,
                        stopped_by: Some(sig),
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Some(st) if libc::WIFSTOPPED(st) => {
                if systemd::is_wrapper(&proc_cmdline(pid)) {
                    break;
                }
            }
            Some(st) if libc::WIFCONTINUED(st) => {
                let argv = proc_cmdline(pid);
                if systemd::is_wrapper(&argv) || argv.contains(&job.ran) {
                    eprintln!(
                        "agent run: job {}: something continued it in {unit} before its limits were checked",
                        job.id
                    );
                    continued_elsewhere = true;
                    break;
                }
            }
            Some(st) if libc::WIFSIGNALED(st) => {
                // Died in the scope (or on its way in) before the script
                // started: the limit, or a signal. Never run it again.
                let sig = libc::WTERMSIG(st);
                let max = asked
                    .iter()
                    .find(|a| a.property == "MemoryMax")
                    .map(|a| a.text.as_str())
                    .unwrap_or("");
                let hint = if sig == libc::SIGKILL {
                    format!(" (MemoryMax={max} may be too small for it to start)")
                } else {
                    String::new()
                };
                let note = format!(
                    "not run: killed by {} in {unit} before the script started{hint}",
                    signal_name(sig)
                );
                eprintln!("agent run: job {}: {note}", job.id);
                end_not_run(store, job, Some(128 + sig), note)?;
                return Ok(Scoped::Done {
                    code: 128 + sig,
                    stopped_by: None,
                });
            }
            Some(st) => {
                // systemd-run said why on stderr (no user manager, a unit by
                // that name, a property it refused).
                let why = format!(
                    "systemd-run couldn't start {unit} (it exited {})",
                    status_code(st)
                );
                return Ok(Scoped::NoScope { why, failed: true });
            }
        }
    }

    // The scope exists, and nothing of the job has run: what did systemd
    // apply?
    let check = check_limits(opts, &unit, pid, asked);
    let refused: Vec<&Problem> = check.problems.iter().filter(|p| p.explicit).collect();
    if !refused.is_empty() && !opts.no_limit_ok && !continued_elsewhere {
        kill(pid, libc::SIGKILL);
        let _ = wait_pid(pid, 0);
        let what: Vec<String> = refused
            .iter()
            .map(|p| format!("{} can't be applied: {}", p.flag, p.text))
            .collect();
        let msg = format!(
            "{}; nothing ran (--no-limit-ok runs it anyway)",
            what.join("; ")
        );
        end_not_run(store, job, None, format!("not run: {msg}"))?;
        bail!("job {}: {msg}", job.id);
    }
    for p in &check.problems {
        let anyway = match (p.explicit, continued_elsewhere) {
            (true, false) => " (--no-limit-ok: running it anyway)",
            (true, true) => " (it was already running)",
            (false, _) => "",
        };
        eprintln!("agent run: job {}: {}{anyway}", job.id, p.text);
        add_note(job, &p.text);
    }
    job.unit = unit.clone();
    job.memory_high = check.recorded[0].clone();
    job.memory_max = check.recorded[1].clone();
    job.memory_swap_max = check.recorded[2].clone();
    store.update_job_launch(job)?;
    if !continued_elsewhere && let Some(sig) = signals.caught() {
        kill(pid, libc::SIGKILL);
        let _ = wait_pid(pid, 0);
        let f = stopped_before_start(store, std::mem::take(job), sig, None)?;
        *job = f.job;
        return Ok(Scoped::Done {
            code: f.exit_code,
            stopped_by: Some(sig),
        });
    }
    if !opts.quiet {
        eprintln!(
            "agent run: job {} in {unit} ({})",
            job.id,
            recorded_limits(job)
        );
    }
    kill(pid, libc::SIGCONT);

    // The job runs. A SIGTERM or SIGHUP to the runner stops its scope.
    let (code, stopped_by) = wait_for_job(pid, signals, |sig| {
        eprintln!(
            "agent run: got {}: stopping job {} ({unit})",
            signal_name(sig),
            job.id
        );
        let stop = systemd::run_command(&stop_command(&opts.systemctl, &unit));
        if !stop.as_ref().is_ok_and(|o| o.success) {
            let why = match stop {
                Ok(o) => o
                    .stderr
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("systemctl failed")
                    .to_string(),
                Err(e) => format!("{e:#}"),
            };
            eprintln!(
                "agent run: couldn't stop {unit} ({why}); sending SIGTERM to the job's process"
            );
            kill(pid, libc::SIGTERM);
        }
    })?;
    Ok(Scoped::Done { code, stopped_by })
}

/// wait_for_job waits for the job's process pid to end and returns its exit
/// code. A thread waits (so the end is recorded the moment it comes); this
/// one looks for a stop signal meanwhile, and on the first calls stop with
/// it, then goes on waiting for the end it brings.
fn wait_for_job(
    pid: libc::pid_t,
    signals: &JobSignals,
    mut stop: impl FnMut(i32),
) -> Result<(i32, Option<i32>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait_pid(pid, 0));
    });
    let mut stopped_by = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(Some(st))) => return Ok((status_code(st), stopped_by)),
            Ok(Ok(None)) => bail!("waiting for the job (pid {pid}): no status"),
            Ok(Err(e)) => {
                return Err(
                    anyhow::Error::new(e).context(format!("waiting for the job (pid {pid})"))
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("waiting for the job (pid {pid}): the waiter went away")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if stopped_by.is_none()
                    && let Some(sig) = signals.caught()
                {
                    stopped_by = Some(sig);
                    stop(sig);
                }
            }
        }
    }
}

/// recorded_limits is a job's limits as `-p` properties, for a person.
pub fn recorded_limits(job: &Job) -> String {
    let p = Limits {
        memory_high: job.memory_high.clone(),
        memory_max: job.memory_max.clone(),
        memory_swap_max: job.memory_swap_max.clone(),
    }
    .properties();
    if p.is_empty() {
        "no limits".to_string()
    } else {
        p.join(" ")
    }
}

/// A limit that didn't apply as asked.
#[derive(Clone, Debug)]
struct Problem {
    flag: &'static str,
    explicit: bool,
    text: String,
}

/// What check_limits found: each limit's value to record (MemoryHigh,
/// MemoryMax, MemorySwapMax; "" for one not in force), and the problems.
struct LimitCheck {
    recorded: [String; 3],
    problems: Vec<Problem>,
}

/// check_limits reads what systemd applied to the scope (`systemctl show`)
/// and whether the kernel enforces it (the scope's cgroup has the limit's
/// file: it doesn't when the memory controller isn't delegated to the user's
/// manager), and holds it against what was asked.
fn check_limits(opts: &StartOptions, unit: &str, pid: libc::pid_t, asked: &[Asked]) -> LimitCheck {
    let (shown, why) = match systemd::run_command(&show_limits_command(&opts.systemctl, unit)) {
        Ok(o) if o.success => {
            let s = parse_shown_limits(&o.stdout);
            if s.load_state == "loaded" {
                (Some(s), String::new())
            } else {
                (
                    None,
                    format!(
                        "systemd says {unit} is {}",
                        if s.load_state.is_empty() {
                            "unknown"
                        } else {
                            &s.load_state
                        }
                    ),
                )
            }
        }
        Ok(o) => (
            None,
            format!(
                "systemctl show failed: {}",
                o.stderr.trim().lines().next().unwrap_or("")
            ),
        ),
        Err(e) => (None, format!("{e:#}")),
    };
    let cgroup = shown
        .as_ref()
        .map(|s| s.control_group.clone())
        .filter(|c| !c.is_empty())
        .or_else(|| proc_cgroup(pid));
    let cgroup_dir = cgroup
        .map(|cg| format!("{}{cg}", opts.cgroup_root))
        .filter(|d| std::path::Path::new(d).is_dir());

    let mut check = LimitCheck {
        recorded: Default::default(),
        problems: Vec::new(),
    };
    for a in asked {
        let i = match a.property {
            "MemoryHigh" => 0,
            "MemoryMax" => 1,
            _ => 2,
        };
        let reported = shown.as_ref().and_then(|s| match i {
            0 => s.memory_high,
            1 => s.memory_max,
            _ => s.memory_swap_max,
        });
        // The kernel's own value, where the file is there to read.
        let file = cgroup_dir
            .as_ref()
            .map(|d| filepath::join(&[d.as_str(), a.cgroup_file]));
        let in_kernel = file.as_ref().map(|f| {
            std::fs::read_to_string(f)
                .ok()
                .and_then(|v| systemd::parse_size_value(&v))
        });
        let value = reported.or(in_kernel.flatten());
        let problem = |text: String| Problem {
            flag: a.flag,
            explicit: a.explicit,
            text,
        };
        match (value, in_kernel) {
            (None, _) => check.problems.push(problem(format!("couldn't confirm {}={}: {why}", a.property, a.text))),
            (Some(_), Some(None)) => check.problems.push(problem(format!(
                "{}={} isn't enforced: its cgroup has no {} (the memory controller isn't delegated to this user's systemd, or swap isn't accounted)",
                a.property, a.text, a.cgroup_file
            ))),
            (Some(v), _) if !size_matches(a.bytes, v) => {
                check.recorded[i] = format_exact(v);
                check.problems.push(problem(format!(
                    "{} is {} in its scope, not the {} asked",
                    a.property,
                    format_exact(v),
                    a.text
                )));
            }
            (Some(v), _) => check.recorded[i] = format_exact(v),
        }
    }
    check
}

/// run_plainly runs the job as a child of this process, with no limits. A
/// SIGTERM or SIGHUP to the runner is passed on to the job's process (it
/// shares the caller's process group, so the terminal's Ctrl-C reaches it).
fn run_plainly(
    store: &Store,
    job: &mut Job,
    argv: &[String],
    env: &[(String, String)],
    cwd: &str,
    signals: &JobSignals,
) -> Result<(i32, Option<i32>)> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k, v)));
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // SAFETY: child_signals is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            child_signals();
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // As a shell says it, and with its codes: 126 found but not
            // runnable, 127 not found.
            eprintln!("agent run: {}: {}", argv[0], os::errno_text(&e));
            store.update_job_launch(job)?;
            return Ok((
                if e.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                },
                None,
            ));
        }
    };
    // Waited for with waitpid, not through child.
    let pid = child.id() as libc::pid_t;
    drop(child);
    job.pid = pid as i64;
    job.pid_start = process_start(job.pid).unwrap_or_default();
    store.update_job_launch(job)?;
    let id = job.id;
    wait_for_job(pid, signals, |sig| {
        eprintln!(
            "agent run: got {}: stopping job {id} (pid {pid})",
            signal_name(sig)
        );
        kill(pid, sig);
    })
}

/// job_env is what start adds to a job's environment: each DEFAULT_ENV
/// variable that is_set (the caller's environment) says isn't set at all. A
/// variable set to anything, even empty or not UTF-8, is the caller's.
pub fn job_env(is_set: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    DEFAULT_ENV
        .iter()
        .filter(|(k, _)| !is_set(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// say_protect_problem prints protect_termd's problem once: again only when
/// it changes, or after a run where there was none.
fn say_protect_problem(store: &Store, r: &ProtectReport) {
    let said = store.get_host_state(PROTECT_SAID_KEY).ok().flatten();
    match &r.problem {
        Some(p) if said.as_deref() != Some(p) => {
            eprintln!("agent run: couldn't ask systemd to spare g3d-termd's units: {p}");
            let _ = store.set_host_state(PROTECT_SAID_KEY, p);
        }
        Some(_) => {}
        None if said.is_some() => {
            let _ = store.delete_host_state(PROTECT_SAID_KEY);
        }
        None => {}
    }
}

/// is_running says whether a job the store records as running still is,
/// from the system's state: its scope's state from systemd, else (no unit,
/// or no user manager to ask) whether its process, by pid and start time,
/// is alive. A job recorded in the last 30s with no process yet counts as
/// starting.
pub fn is_running(job: &Job) -> bool {
    if !job.running() {
        return false;
    }
    if !job.unit.is_empty()
        && cfg!(target_os = "linux")
        && let Some(active) = systemd::unit_active(&job.unit)
    {
        return active;
    }
    if job.pid > 0 {
        return pid_alive(job.pid, &job.pid_start);
    }
    (Time::now() - job.started_at).num_seconds() < 30
}

/// GONE_NOTE is what settle notes on a job found no longer running.
pub const GONE_NOTE: &str = "its runner was killed: found no longer running, so when it ended and its exit code aren't known";

/// settle records, for a job recorded as running that isn't any more (its
/// runner was killed), that it was found so, now. Its end time stays
/// unknown, and its exit code.
fn settle(store: &Store, job: &mut Job) -> Result<()> {
    if job.running() && !is_running(job) {
        let now = Time::now();
        let mut found = job.clone();
        add_note(&mut found, GONE_NOTE);
        if store.found_job_gone(job.id, &now, &found.note)? {
            found.found_gone_at = Some(now);
            found.exit_code = None;
            *job = found;
        } else if let Some(j) = store.get_job(job.id)? {
            // Its runner recorded the end meanwhile.
            *job = j;
        }
    }
    Ok(())
}

/// list returns the running jobs (all: every job), oldest first, having
/// settled the ones whose runner died.
pub fn list(store: &Store, all: bool) -> Result<Vec<Job>> {
    let mut jobs = store.list_jobs(all)?;
    for j in jobs.iter_mut() {
        settle(store, j)?;
    }
    if !all {
        jobs.retain(|j| j.running());
    }
    Ok(jobs)
}

/// show returns one job's record, settled as list does.
pub fn show(store: &Store, id: i64) -> Result<Option<Job>> {
    let Some(mut job) = store.get_job(id)? else {
        return Ok(None);
    };
    settle(store, &mut job)?;
    Ok(Some(job))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_problem_is_remembered_until_it_changes_or_clears() {
        let dir = tempfile::Builder::new()
            .prefix("agent-fleet-said-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let store = Store::open(dir.path().join("s.db")).unwrap();
        let said = || store.get_host_state(PROTECT_SAID_KEY).unwrap();
        let report = |p: Option<&str>| ProtectReport {
            problem: p.map(str::to_string),
            ..Default::default()
        };
        say_protect_problem(&store, &report(Some("no user manager")));
        assert_eq!(said().as_deref(), Some("no user manager"));
        say_protect_problem(&store, &report(Some("no user manager")));
        assert_eq!(said().as_deref(), Some("no user manager"));
        say_protect_problem(&store, &report(Some("access denied")));
        assert_eq!(said().as_deref(), Some("access denied"));
        say_protect_problem(&store, &report(None));
        assert_eq!(said(), None);
    }

    /// review-3 1.2: a unit carries its store, so two stores' job 1 differ.
    #[test]
    fn unit_names_carry_the_store() {
        let a = tempfile::Builder::new()
            .prefix("agent-fleet-tag-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let b = tempfile::Builder::new()
            .prefix("agent-fleet-tag-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let (a, b) = (
            a.path().to_string_lossy().into_owned(),
            b.path().to_string_lossy().into_owned(),
        );
        let tag = store_tag(&a);
        assert_eq!(tag.len(), 8);
        assert!(
            tag.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            tag,
            sha256_hex(
                std::fs::canonicalize(&a)
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes()
            )[..8]
        );
        assert_eq!(unit_name(&a, 17), format!("agent-job-{tag}-17.scope"));
        assert_ne!(unit_name(&a, 1), unit_name(&b, 1));
        // The same store by another spelling of its path.
        assert_eq!(store_tag(&format!("{a}/.")), tag);
        // A data dir not created yet: its path as given, cleaned.
        assert_eq!(store_tag("/no/such/dir/"), sha256_hex(b"/no/such/dir")[..8]);
    }
}

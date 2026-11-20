//! The systemd side of a run: the scope a job runs in, asking whether it
//! still runs, and asking oomd to spare g3d-termd's units.

use std::process::Command;

use anyhow::{Context, Result};

use super::limits::Limits;

/// SCOPE_WRAPPER is what systemd-run starts in the scope: it stops itself,
/// and once continued becomes the job. systemd-run execs it only after the
/// scope exists, so the runner, waiting for it to stop (waitpid WUNTRACED),
/// learns the scope was created with nothing of the job run yet; it then
/// checks the limits systemd applied and continues it (SIGCONT) or kills it
/// (SIGKILL) before the script starts. There is nothing on disk to lose: a
/// systemd-run that exits without the wrapper having stopped created no
/// scope, or died in it (the limit, a signal) before the script started.
/// The `&&`s keep the job from starting unless the stop happened. Its own
/// pid comes from /proc (the scope is Linux's), not `$$`: systemd-run 254+
/// rewrites `$$` and `${...}` in a scope's command line unless told not to
/// (`--expand-environment=no`), and an older one can't be told.
pub const SCOPE_WRAPPER: &str =
    r#"read -r pid rest < /proc/self/stat && kill -STOP "$pid" && exec "$@"; exit 125"#;

/// The wrapper's $0, which tells it in /proc/<pid>/cmdline.
pub const WRAPPER_NAME: &str = "agent-job";

/// EXPAND_ENVIRONMENT_SINCE is the systemd version whose systemd-run expands
/// environment variables in a scope's command line (and takes
/// `--expand-environment=no`).
pub const EXPAND_ENVIRONMENT_SINCE: u32 = 254;

/// systemd_run_version is the version systemd_run says it is (`systemd 262
/// (...)`), None when it can't be run or says something else.
pub fn systemd_run_version(systemd_run: &str) -> Option<u32> {
    let out = Command::new(systemd_run)
        .arg("--version")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

pub(crate) fn parse_version(out: &str) -> Option<u32> {
    out.lines()
        .next()?
        .strip_prefix("systemd ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// scope_command is the argv that runs argv in its own transient scope,
/// `unit`, under limits, by way of SCOPE_WRAPPER. version is systemd-run's:
/// from EXPAND_ENVIRONMENT_SINCE on, it is told to pass the job's arguments
/// as they are (else `$HOME` or `$$` in one would reach the job rewritten).
pub fn scope_command(
    systemd_run: &str,
    version: Option<u32>,
    unit: &str,
    limits: &Limits,
    argv: &[String],
) -> Vec<String> {
    let mut c: Vec<String> = [systemd_run, "--user", "--scope", "--quiet", "--collect"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if version.is_some_and(|v| v >= EXPAND_ENVIRONMENT_SINCE) {
        c.push("--expand-environment=no".into());
    }
    c.push("--unit".into());
    c.push(unit.into());
    for p in limits.properties() {
        c.push("-p".into());
        c.push(p);
    }
    c.extend(
        ["--", "/bin/sh", "-c", SCOPE_WRAPPER, WRAPPER_NAME]
            .iter()
            .map(|s| s.to_string()),
    );
    c.extend(argv.iter().cloned());
    c
}

/// is_wrapper says whether a process's argv (/proc/<pid>/cmdline, split
/// at NULs) is SCOPE_WRAPPER's.
pub fn is_wrapper(argv: &[String]) -> bool {
    argv.len() >= 4 && argv[1] == "-c" && argv[2] == SCOPE_WRAPPER && argv[3] == WRAPPER_NAME
}

/// show_limits_command reads what systemd applied to a job's scope.
pub fn show_limits_command(systemctl: &str, unit: &str) -> Vec<String> {
    [
        systemctl,
        "--user",
        "show",
        "-p",
        "LoadState",
        "-p",
        "MemoryHigh",
        "-p",
        "MemoryMax",
        "-p",
        "MemorySwapMax",
        "-p",
        "ControlGroup",
        "--",
        unit,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// stop_command stops a job's scope: systemd signals every process in it
/// (SIGTERM, then SIGKILL after its stop timeout) and waits for them.
pub fn stop_command(systemctl: &str, unit: &str) -> Vec<String> {
    [systemctl, "--user", "stop", "--", unit]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// ShownLimits is what `systemctl show` said about a scope's limits. Sizes
/// are bytes, u64::MAX for infinity; None when not shown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShownLimits {
    pub load_state: String,
    pub control_group: String,
    pub memory_high: Option<u64>,
    pub memory_max: Option<u64>,
    pub memory_swap_max: Option<u64>,
}

/// parse_size_value reads a size as systemd and cgroupfs print it: bytes,
/// or "infinity" / "max".
pub fn parse_size_value(v: &str) -> Option<u64> {
    match v.trim() {
        "infinity" | "max" => Some(u64::MAX),
        v => v.parse().ok(),
    }
}

/// parse_shown_limits reads show_limits_command's output.
pub fn parse_shown_limits(out: &str) -> ShownLimits {
    let mut s = ShownLimits::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "LoadState" => s.load_state = v.to_string(),
            "ControlGroup" => s.control_group = v.to_string(),
            "MemoryHigh" => s.memory_high = parse_size_value(v),
            "MemoryMax" => s.memory_max = parse_size_value(v),
            "MemorySwapMax" => s.memory_swap_max = parse_size_value(v),
            _ => {}
        }
    }
    s
}

/// is_active_command asks systemd whether unit runs.
pub fn is_active_command(unit: &str) -> Vec<String> {
    ["systemctl", "--user", "is-active", "--", unit]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// run_command runs argv and says what it printed.
pub fn run_command(argv: &[String]) -> Result<CmdOutput> {
    run(argv)
}

/// unit_active says whether unit is running: Some(true) for a unit that is
/// active (or getting there or leaving), Some(false) for one that isn't (or
/// isn't loaded), None when systemd can't be asked (no user manager here).
pub fn unit_active(unit: &str) -> Option<bool> {
    let out = run(&is_active_command(unit)).ok()?;
    parse_is_active(&out.stdout)
}

pub(crate) fn parse_is_active(stdout: &str) -> Option<bool> {
    match stdout.trim() {
        "active" | "activating" | "deactivating" | "reloading" | "refreshing" => Some(true),
        "inactive" | "failed" | "maintenance" => Some(false),
        _ => None,
    }
}

/// list_termd_units_command lists this user's g3d-termd services.
pub fn list_termd_units_command() -> Vec<String> {
    [
        "systemctl",
        "--user",
        "list-units",
        "--type=service",
        "--all",
        "--plain",
        "--no-legend",
        "--no-pager",
        "g3d-termd-*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// show_preference_command reads units' ManagedOOMPreference and cgroup.
pub fn show_preference_command(units: &[String]) -> Vec<String> {
    let mut c: Vec<String> = [
        "systemctl",
        "--user",
        "show",
        "--property=Id,ManagedOOMPreference,ControlGroup",
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    c.extend(units.iter().cloned());
    c
}

/// set_avoid_command asks systemd to make unit oomd's last choice, until the
/// next reboot (the units are per boot anyway).
pub fn set_avoid_command(unit: &str) -> Vec<String> {
    [
        "systemctl",
        "--user",
        "set-property",
        "--runtime",
        "--",
        unit,
        "ManagedOOMPreference=avoid",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// is_termd_unit says whether name is a g3d-termd service as g3d-termd names
/// them: `^g3d-termd-[A-Za-z0-9_-]+-[0-9]+\.service$`. protect_termd acts on
/// nothing else, whatever list-units prints (an alias's primary unit, say).
pub fn is_termd_unit(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("g3d-termd-")
        .and_then(|r| r.strip_suffix(".service"))
    else {
        return false;
    };
    let Some((ns, n)) = rest.rsplit_once('-') else {
        return false;
    };
    !ns.is_empty()
        && ns
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
}

/// ProtectReport says what protect_termd did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtectReport {
    /// Units set to ManagedOOMPreference=avoid now.
    pub protected: Vec<String>,
    /// Units that already had it.
    pub already: Vec<String>,
    /// Units systemd took it for whose cgroup doesn't carry it
    /// (user.oomd_avoid), so oomd doesn't see it.
    pub unheeded: Vec<String>,
    /// Why it couldn't do (all of) it, in systemd's words where it gave any.
    pub problem: Option<String>,
}

impl ProtectReport {
    /// message is one line for a person.
    pub fn message(&self) -> String {
        let mut parts = Vec::new();
        if !self.protected.is_empty() {
            parts.push(format!(
                "set ManagedOOMPreference=avoid on {}",
                self.protected.join(", ")
            ));
        }
        if !self.already.is_empty() {
            parts.push(format!("already avoid: {}", self.already.join(", ")));
        }
        if let Some(p) = &self.problem {
            parts.push(format!("g3d-termd isn't spared: {p}"));
        }
        if parts.is_empty() {
            "no g3d-termd units to protect".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// CmdOutput is what a command printed and whether it succeeded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

fn run(argv: &[String]) -> Result<CmdOutput> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("running {}", argv[0]))?;
    Ok(CmdOutput {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// protect_termd asks systemd (best effort, Linux only) to treat this user's
/// g3d-termd services as oomd's last choice (ManagedOOMPreference=avoid), so
/// a job that runs memory out is killed rather than the terminals. It never
/// fails: the report says what it did, or why it couldn't.
pub fn protect_termd() -> ProtectReport {
    if !cfg!(target_os = "linux") {
        return ProtectReport {
            problem: Some("not Linux: no systemd-oomd".into()),
            ..Default::default()
        };
    }
    protect_termd_with(&mut |argv| run(argv), &cgroup_has_avoid)
}

/// The xattr systemd puts on a unit's cgroup for ManagedOOMPreference=avoid,
/// which is what systemd-oomd reads.
pub const AVOID_XATTR: &str = "user.oomd_avoid";

/// cgroup_has_avoid says whether the cgroup (a ControlGroup path, under
/// /sys/fs/cgroup) carries AVOID_XATTR; None when that can't be told.
pub fn cgroup_has_avoid(cgroup: &str) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        let path = std::ffi::CString::new(format!("/sys/fs/cgroup{cgroup}")).ok()?;
        let name = std::ffi::CString::new(AVOID_XATTR).ok()?;
        // SAFETY: both are valid C strings; a null buffer of size 0 asks only
        // for the value's size.
        let n = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if n >= 0 {
            return Some(true);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENODATA) => Some(false),
            _ => None,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cgroup;
        None
    }
}

/// protect_termd_with is protect_termd over a command runner and an xattr
/// check (tests pass fakes, so they never touch the live units).
pub fn protect_termd_with(
    run: &mut dyn FnMut(&[String]) -> Result<CmdOutput>,
    has_avoid: &dyn Fn(&str) -> Option<bool>,
) -> ProtectReport {
    let mut report = ProtectReport::default();
    let said = |o: &CmdOutput| {
        let s = o.stderr.trim();
        if s.is_empty() {
            "systemctl failed".to_string()
        } else {
            s.lines().next().unwrap_or(s).to_string()
        }
    };
    let units = match run(&list_termd_units_command()) {
        Ok(o) if o.success => parse_unit_list(&o.stdout),
        Ok(o) => {
            report.problem = Some(said(&o));
            return report;
        }
        Err(e) => {
            report.problem = Some(format!("{e:#}"));
            return report;
        }
    };
    if units.is_empty() {
        return report;
    }
    let mut problems = Vec::new();
    let current = match run(&show_preference_command(&units)) {
        Ok(o) if o.success => parse_preferences(&o.stdout),
        Ok(o) => {
            problems.push(format!(
                "couldn't read their ManagedOOMPreference: {}",
                said(&o)
            ));
            Vec::new()
        }
        Err(e) => {
            problems.push(format!("couldn't read their ManagedOOMPreference: {e:#}"));
            Vec::new()
        }
    };
    // Units whose cgroup couldn't be looked at, so whether oomd sees the
    // preference is unknown.
    let mut unchecked = Vec::new();
    for unit in units {
        let info = current.iter().find(|u| u.id == unit);
        if info.is_some_and(|u| u.preference == "avoid") {
            report.already.push(unit.clone());
        } else {
            match run(&set_avoid_command(&unit)) {
                Ok(o) if o.success => report.protected.push(unit.clone()),
                Ok(o) => {
                    problems.push(format!("{unit}: {}", said(&o)));
                    continue;
                }
                Err(e) => {
                    problems.push(format!("{unit}: {e:#}"));
                    continue;
                }
            }
        }
        // systemd taking the property isn't oomd heeding it: oomd reads the
        // cgroup's xattr, which systemd may not write (a user manager, here).
        match info
            .map(|u| u.cgroup.as_str())
            .filter(|c| !c.is_empty())
            .map(has_avoid)
        {
            Some(Some(true)) => {}
            Some(Some(false)) => report.unheeded.push(unit),
            None | Some(None) => unchecked.push(unit),
        }
    }
    if !report.unheeded.is_empty() {
        problems.push(format!(
            "systemd took ManagedOOMPreference=avoid for {} but their cgroups carry no {AVOID_XATTR}, so systemd-oomd won't see it (it heeds that only on cgroups owned by root, or by the owner of the cgroup it watches)",
            report.unheeded.join(", ")
        ));
    }
    if !unchecked.is_empty() {
        problems.push(format!(
            "couldn't check that systemd-oomd sees it for {}: no cgroup to read {AVOID_XATTR} on, or it can't be read",
            unchecked.join(", ")
        ));
    }
    if !problems.is_empty() {
        report.problem = Some(problems.join("; "));
    }
    report
}

/// UnitInfo is a unit's line in `show`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnitInfo {
    pub id: String,
    pub preference: String,
    pub cgroup: String,
}

/// parse_unit_list reads the units from `list-units --plain --no-legend`
/// (name, load state, ...) that are g3d-termd's own and loaded: a unit an
/// alias led to, or a not-found stub (a unit only named in another's
/// After=), is left out.
pub(crate) fn parse_unit_list(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let (name, load) = (cols.next()?, cols.next()?);
            (is_termd_unit(name) && load == "loaded").then(|| name.to_string())
        })
        .collect()
}

/// parse_preferences reads `show --property=Id,ManagedOOMPreference,
/// ControlGroup` for several units: blocks of Key=value lines.
pub(crate) fn parse_preferences(out: &str) -> Vec<UnitInfo> {
    let mut res = Vec::new();
    let mut cur = UnitInfo::default();
    for line in out.lines().chain(std::iter::once("")) {
        if let Some(v) = line.strip_prefix("Id=") {
            cur.id = v.to_string();
        } else if let Some(v) = line.strip_prefix("ManagedOOMPreference=") {
            cur.preference = v.to_string();
        } else if let Some(v) = line.strip_prefix("ControlGroup=") {
            cur.cgroup = v.to_string();
        } else if line.trim().is_empty() && !cur.id.is_empty() {
            res.push(std::mem::take(&mut cur));
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scope_command_line() {
        let limits = Limits {
            memory_high: "20G".into(),
            memory_max: "24G".into(),
            memory_swap_max: String::new(),
        };
        let tail = s(&[
            "--unit",
            "agent-job-0a1b2c3d-17.scope",
            "-p",
            "MemoryHigh=20G",
            "-p",
            "MemoryMax=24G",
            "--",
            "/bin/sh",
            "-c",
            SCOPE_WRAPPER,
            "agent-job",
            "/w/b.sh",
            "x y",
        ]);
        let head = s(&["systemd-run", "--user", "--scope", "--quiet", "--collect"]);
        let argv = s(&["/w/b.sh", "x y"]);
        for v in [None, Some(253)] {
            assert_eq!(
                scope_command(
                    "systemd-run",
                    v,
                    "agent-job-0a1b2c3d-17.scope",
                    &limits,
                    &argv
                ),
                [head.clone(), tail.clone()].concat()
            );
        }
        for v in [254, 262] {
            assert_eq!(
                scope_command(
                    "systemd-run",
                    Some(v),
                    "agent-job-0a1b2c3d-17.scope",
                    &limits,
                    &argv
                ),
                [head.clone(), s(&["--expand-environment=no"]), tail.clone()].concat()
            );
        }
        let wrapper = scope_command("systemd-run", None, "u", &limits, &argv)[12..].to_vec();
        assert!(is_wrapper(&wrapper), "{wrapper:?}");
        assert!(!is_wrapper(&s(&["systemd-run", "--user"])));
        assert!(!is_wrapper(&s(&[
            "/bin/sh",
            "-c",
            "kill -STOP $$",
            "agent-job"
        ])));
        assert!(!SCOPE_WRAPPER.contains("$$") && !SCOPE_WRAPPER.contains("${"));
        assert_eq!(
            parse_version("systemd 262 (262.1-1-arch)\n+PAM +AUDIT\n"),
            Some(262)
        );
        assert_eq!(parse_version("systemd 249\n"), Some(249));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("Failed to connect\n"), None);
        assert_eq!(
            show_limits_command("systemctl", "u.scope"),
            s(&[
                "systemctl",
                "--user",
                "show",
                "-p",
                "LoadState",
                "-p",
                "MemoryHigh",
                "-p",
                "MemoryMax",
                "-p",
                "MemorySwapMax",
                "-p",
                "ControlGroup",
                "--",
                "u.scope"
            ])
        );
        assert_eq!(
            stop_command("systemctl", "u.scope"),
            s(&["systemctl", "--user", "stop", "--", "u.scope"])
        );
    }

    /// The wrapper stops before anything of the job runs, and once continued
    /// is the job (its exit code, its argv).
    #[test]
    fn scope_wrapper_stops_then_becomes_the_job() {
        let dir = tempfile::Builder::new()
            .prefix("agent-fleet-wrap-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let out = dir.path().join("out");
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                SCOPE_WRAPPER,
                WRAPPER_NAME,
                "sh",
                "-c",
                "echo \"$0 $1\" > \"$2\"; exit 9",
                "job",
                "a b",
            ])
            .arg(&out)
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let mut st = 0;
        // SAFETY: waitpid on our own child.
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
        assert_eq!(r, pid);
        assert!(libc::WIFSTOPPED(st), "status {st:#x}");
        assert!(!out.exists(), "the job ran before it was continued");
        // SAFETY: signalling our own stopped child.
        unsafe { libc::kill(pid, libc::SIGCONT) };
        assert_eq!(child.wait().unwrap().code(), Some(9));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "job a b\n");
    }

    #[test]
    fn protect_commands() {
        assert_eq!(
            list_termd_units_command(),
            s(&[
                "systemctl",
                "--user",
                "list-units",
                "--type=service",
                "--all",
                "--plain",
                "--no-legend",
                "--no-pager",
                "g3d-termd-*"
            ])
        );
        assert_eq!(
            set_avoid_command("g3d-termd-agents-1.service"),
            s(&[
                "systemctl",
                "--user",
                "set-property",
                "--runtime",
                "--",
                "g3d-termd-agents-1.service",
                "ManagedOOMPreference=avoid"
            ])
        );
        assert_eq!(
            show_preference_command(&s(&["a.service", "b.service"])),
            s(&[
                "systemctl",
                "--user",
                "show",
                "--property=Id,ManagedOOMPreference,ControlGroup",
                "--",
                "a.service",
                "b.service"
            ])
        );
        assert_eq!(
            is_active_command("agent-job-3.scope"),
            s(&[
                "systemctl",
                "--user",
                "is-active",
                "--",
                "agent-job-3.scope"
            ])
        );
    }

    /// review-3 1.6: only g3d-termd's own loaded units, by their exact name.
    #[test]
    fn termd_units_by_exact_name() {
        for ok in [
            "g3d-termd-agents-1.service",
            "g3d-termd-a_b-c-1000.service",
            "g3d-termd-W-0.service",
        ] {
            assert!(is_termd_unit(ok), "{ok}");
        }
        for bad in [
            "unrelated-app.service",
            "g3d-termd-agents.service",
            "g3d-termd--1.service",
            "g3d-termd-agents-1x.service",
            "g3d-termd-agents-.service",
            "g3d-termd-agents-1.scope",
            "g3d-termd-ag.ents-1.service",
            "xg3d-termd-agents-1.service",
            "g3d-termd-agents-1.service.bak",
        ] {
            assert!(!is_termd_unit(bad), "{bad}");
        }
        // An alias leads list-units to its primary unit (column 1), and a
        // unit only named in an After= is a not-found stub: neither is ours.
        let list = "unrelated-app.service loaded active running Unrelated\n\
                    g3d-termd-old-3.service not-found inactive dead g3d-termd-old-3.service\n\
                    g3d-termd-agents-1.service loaded active running g3d-termd namespace agents\n\
                    g3d-termd-idle-2.service loaded inactive dead g3d-termd namespace idle\n";
        assert_eq!(
            parse_unit_list(list),
            s(&["g3d-termd-agents-1.service", "g3d-termd-idle-2.service"])
        );
        let (mut f, log) = fake(list, "", None);
        let r = protect_termd_with(&mut f, &|_| Some(true));
        let touched: Vec<String> = log
            .borrow()
            .iter()
            .filter(|c| c[2] == "set-property")
            .map(|c| c[5].clone())
            .collect();
        assert_eq!(
            touched,
            s(&["g3d-termd-agents-1.service", "g3d-termd-idle-2.service"])
        );
        assert_eq!(r.protected, touched);
    }

    #[test]
    fn parses_shown_limits() {
        let shown = parse_shown_limits(
            "LoadState=loaded\nMemoryHigh=50331648\nMemoryMax=67108864\nMemorySwapMax=0\nControlGroup=/user.slice/x.scope\n",
        );
        assert_eq!(
            shown,
            ShownLimits {
                load_state: "loaded".into(),
                control_group: "/user.slice/x.scope".into(),
                memory_high: Some(50331648),
                memory_max: Some(67108864),
                memory_swap_max: Some(0),
            }
        );
        let shown = parse_shown_limits("LoadState=not-found\nMemoryMax=infinity\nControlGroup=\n");
        assert_eq!(
            (
                shown.memory_max,
                shown.memory_high,
                shown.control_group.as_str()
            ),
            (Some(u64::MAX), None, "")
        );
        assert_eq!(parse_size_value("max\n"), Some(u64::MAX));
        assert_eq!(parse_size_value("12x"), None);
    }

    #[test]
    fn parses() {
        let list = "g3d-termd-agents-1.service loaded active running g3d-termd namespace agents\n\
                    g3d-termd-wall-2.service   loaded active running g3d-termd namespace wall\n";
        assert_eq!(
            parse_unit_list(list),
            s(&["g3d-termd-agents-1.service", "g3d-termd-wall-2.service"])
        );
        assert!(parse_unit_list("").is_empty());
        let show = "Id=a.service\nManagedOOMPreference=avoid\nControlGroup=/app.slice/a.service\n\nId=b.service\nManagedOOMPreference=none\nControlGroup=\n";
        let info = |id: &str, preference: &str, cgroup: &str| UnitInfo {
            id: id.into(),
            preference: preference.into(),
            cgroup: cgroup.into(),
        };
        assert_eq!(
            parse_preferences(show),
            vec![
                info("a.service", "avoid", "/app.slice/a.service"),
                info("b.service", "none", "")
            ]
        );
        assert_eq!(parse_is_active("active\n"), Some(true));
        assert_eq!(parse_is_active("inactive\n"), Some(false));
        assert_eq!(parse_is_active(""), None);
    }

    type Log = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

    /// A fake systemctl: records the commands, answers from a script.
    fn fake(
        units: &'static str,
        show: &'static str,
        set_fails: Option<&'static str>,
    ) -> (impl FnMut(&[String]) -> Result<CmdOutput>, Log) {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let l = log.clone();
        let f = move |argv: &[String]| -> Result<CmdOutput> {
            l.borrow_mut().push(argv.to_vec());
            let ok = |out: &str| {
                Ok(CmdOutput {
                    success: true,
                    stdout: out.into(),
                    stderr: String::new(),
                })
            };
            match argv[2].as_str() {
                "list-units" => ok(units),
                "show" => ok(show),
                "set-property" => match set_fails {
                    Some(e) => Ok(CmdOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: e.into(),
                    }),
                    None => ok(""),
                },
                other => panic!("unexpected {other}"),
            }
        };
        (f, log)
    }

    const TWO_UNITS: &str = "g3d-termd-agents-1.service loaded active running x\ng3d-termd-wall-2.service loaded active running y\n";
    const TWO_SHOWN: &str = "Id=g3d-termd-agents-1.service\nManagedOOMPreference=none\nControlGroup=/u/app.slice/g3d-termd-agents-1.service\n\n\
                             Id=g3d-termd-wall-2.service\nManagedOOMPreference=avoid\nControlGroup=/u/app.slice/g3d-termd-wall-2.service\n";

    #[test]
    fn protect_sets_only_what_needs_it() {
        let (mut f, log) = fake(TWO_UNITS, TWO_SHOWN, None);
        let r = protect_termd_with(&mut f, &|_| Some(true));
        assert_eq!(r.protected, s(&["g3d-termd-agents-1.service"]));
        assert_eq!(r.already, s(&["g3d-termd-wall-2.service"]));
        assert!(r.unheeded.is_empty());
        assert_eq!(r.problem, None);
        let sets: Vec<Vec<String>> = log
            .borrow()
            .iter()
            .filter(|c| c[2] == "set-property")
            .cloned()
            .collect();
        assert_eq!(sets, vec![set_avoid_command("g3d-termd-agents-1.service")]);
        assert!(
            r.message()
                .starts_with("set ManagedOOMPreference=avoid on g3d-termd-agents-1.service")
        );
    }

    /// systemd takes the property, but the cgroup has no xattr for oomd to
    /// read (a user manager's units, as on the machine this was written on).
    #[test]
    fn protect_says_when_oomd_wont_see_it() {
        let (mut f, _) = fake(TWO_UNITS, TWO_SHOWN, None);
        let asked = std::cell::RefCell::new(Vec::new());
        let r = protect_termd_with(&mut f, &|cg| {
            asked.borrow_mut().push(cg.to_string());
            Some(false)
        });
        assert_eq!(
            *asked.borrow(),
            s(&[
                "/u/app.slice/g3d-termd-agents-1.service",
                "/u/app.slice/g3d-termd-wall-2.service"
            ])
        );
        assert_eq!(r.protected, s(&["g3d-termd-agents-1.service"]));
        assert_eq!(
            r.unheeded,
            s(&["g3d-termd-agents-1.service", "g3d-termd-wall-2.service"])
        );
        let p = r.problem.clone().unwrap();
        assert!(
            p.contains("carry no user.oomd_avoid") && p.contains("won't see it"),
            "{p}"
        );
        // Can't tell (no cgroupfs xattrs): said so (review-3 1.6), with no
        // claim that oomd doesn't see it.
        let (mut f, _) = fake(TWO_UNITS, TWO_SHOWN, None);
        let r = protect_termd_with(&mut f, &|_| None);
        assert!(r.unheeded.is_empty());
        let p = r.problem.unwrap();
        assert!(p.starts_with("couldn't check that systemd-oomd sees it for g3d-termd-agents-1.service, g3d-termd-wall-2.service"), "{p}");
    }

    /// review-3 1.6: a show that fails leaves nothing to check the cgroups
    /// by: the report says so rather than claiming all is well.
    #[test]
    fn protect_says_when_show_fails() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let l = log.clone();
        let mut f = move |argv: &[String]| -> Result<CmdOutput> {
            l.borrow_mut().push(argv.to_vec());
            match argv[2].as_str() {
                "list-units" => Ok(CmdOutput {
                    success: true,
                    stdout: "g3d-termd-agents-1.service loaded active running x\n".into(),
                    stderr: String::new(),
                }),
                "show" => Ok(CmdOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "Failed to get properties: Connection timed out\n".into(),
                }),
                _ => Ok(CmdOutput {
                    success: true,
                    ..Default::default()
                }),
            }
        };
        let r = protect_termd_with(&mut f, &|_| panic!("no cgroup is known to look at"));
        assert_eq!(r.protected, s(&["g3d-termd-agents-1.service"]));
        let p = r.problem.unwrap();
        assert!(p.contains("couldn't read their ManagedOOMPreference: Failed to get properties: Connection timed out"), "{p}");
        assert!(
            p.contains("couldn't check that systemd-oomd sees it for g3d-termd-agents-1.service"),
            "{p}"
        );
    }

    #[test]
    fn protect_reports_systemds_refusal() {
        let (mut f, _) = fake(
            "g3d-termd-agents-1.service loaded active running x\n",
            "",
            Some("Failed to set unit properties on g3d-termd-agents-1.service: Access denied\n"),
        );
        let r = protect_termd_with(&mut f, &|_| panic!("no cgroup to look at"));
        assert!(r.protected.is_empty());
        assert_eq!(
            r.problem.as_deref(),
            Some(
                "g3d-termd-agents-1.service: Failed to set unit properties on g3d-termd-agents-1.service: Access denied"
            )
        );

        let mut no_manager = |_: &[String]| -> Result<CmdOutput> {
            Ok(CmdOutput {
                success: false,
                stdout: String::new(),
                stderr: "Failed to connect to user scope bus via local transport: No such file or directory\n".into(),
            })
        };
        let r = protect_termd_with(&mut no_manager, &|_| None);
        assert_eq!(
            r.problem.as_deref(),
            Some(
                "Failed to connect to user scope bus via local transport: No such file or directory"
            )
        );

        let (mut none, _) = fake("", "", None);
        assert_eq!(
            protect_termd_with(&mut none, &|_| None),
            ProtectReport::default()
        );
    }
}

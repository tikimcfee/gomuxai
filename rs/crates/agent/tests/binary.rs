//! The built binary, end to end: `agent run`, `agent task` and `agent history`
//! as a person or agent runs them. The data dir is the one beside the binary
//! (config: `<binary's dir>/.gomuxai`), so each test runs a hard link of the
//! binary (a copy, where linking fails) in a temp dir under /var/tmp, and that
//! dir's .gomuxai is the test's. The environment is cleared but for what a test
//! gives: PATH is a dir with only `sh` in it, so systemd-run (and systemctl) are
//! never found and no job touches the user's systemd; AGENT_HOST names the host.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Bin {
    dir: tempfile::TempDir,
    bin: PathBuf,
    /// The environment every run gets beyond PATH, HOME and AGENT_HOST:
    /// with_systemd's way to the user's systemd.
    env: Vec<(String, String)>,
}

/// What a run printed and how it exited.
struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Bin {
    fn new() -> Bin {
        let dir = tempfile::Builder::new()
            .prefix("agent-cli-e2e-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join("agent");
        let built = env!("CARGO_BIN_EXE_agent");
        if std::fs::hard_link(built, &bin).is_err() {
            std::fs::copy(built, &bin).unwrap();
        }
        let path = dir.path().join("path");
        std::fs::create_dir_all(&path).unwrap();
        std::os::unix::fs::symlink("/bin/sh", path.join("sh")).unwrap();
        for d in ["work", "elsewhere", "home"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        Bin {
            dir,
            bin,
            env: Vec::new(),
        }
    }

    /// A Bin whose PATH has the real systemd-run and systemctl too, with the
    /// environment that reaches the user's systemd, where `systemd-run
    /// --user --scope` works; None elsewhere. Jobs then run in real user
    /// scopes (named for this temp store); --no-protect keeps every run off
    /// the live g3d-termd units.
    fn with_systemd() -> Option<Bin> {
        let mut b = Bin::new();
        for k in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Ok(v) = std::env::var(k) {
                b.env.push((k.to_string(), v));
            }
        }
        let path = b.dir.path().join("path");
        for tool in ["systemd-run", "systemctl"] {
            let found = ["/usr/bin", "/bin", "/usr/local/bin"]
                .iter()
                .map(|d| Path::new(d).join(tool))
                .find(|p| p.is_file())?;
            std::os::unix::fs::symlink(found, path.join(tool)).unwrap();
        }
        let ok = Command::new(path.join("systemd-run"))
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--",
                "/bin/sh",
                "-c",
                ":",
            ])
            .env_clear()
            .env("PATH", &path)
            .envs(b.env.iter().map(|(k, v)| (k, v)))
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("binary: systemd-run --user --scope doesn't work here; scope tests skipped");
        }
        ok.then_some(b)
    }

    /// The system's program name, linked into the PATH dir.
    fn link(&self, name: &str) {
        let found = ["/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("no {name} in /usr/bin or /bin"));
        std::os::unix::fs::symlink(found, self.dir.path().join("path").join(name)).unwrap();
    }

    /// A program in the PATH dir (a fake systemd-run, say).
    fn tool(&self, name: &str, body: &str) {
        let p = self.dir.path().join("path").join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The command for agent with these words in cwd, as run_as runs it.
    fn command(&self, cwd: &Path, env: &[(&str, &str)], words: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(words)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", self.dir.path().join("path"))
            .env("HOME", self.dir.path().join("home"))
            .env("AGENT_HOST", "hub")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(env.iter().copied());
        c
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("bin/.gomuxai")
    }

    /// Runs agent with these words in work/, as a person (no AGENT_NAME).
    fn run(&self, words: &[&str]) -> Out {
        self.run_as(&self.work(), &[], words)
    }

    fn run_as(&self, cwd: &Path, env: &[(&str, &str)], words: &[&str]) -> Out {
        let out = self
            .command(cwd, env, words)
            .output()
            .expect("running agent");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// A script in work/.
    fn script(&self, name: &str, body: &str, exec: bool) -> String {
        let p = self.work().join(name);
        std::fs::write(&p, body).unwrap();
        let mode = if exec { 0o755 } else { 0o644 };
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p.to_string_lossy().into_owned()
    }
}

#[test]
fn run_records_jobs_and_exits_with_their_code() {
    let b = Bin::new();
    b.script(
        "ok.sh",
        "echo \"ok: $*\"\necho \"tmp=$TMPDIR jobs=$CARGO_BUILD_JOBS\"\n",
        false,
    );
    b.script("fail.sh", "#!/bin/sh\nexit 3\n", true);

    // Everything after the script is its own, flags included.
    let o = b.run(&["run", "--no-protect", "ok.sh", "a", "--b", "-q", "--list"]);
    assert_eq!(o.code, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "ok: a --b -q --list\ntmp=/var/tmp jobs=12\n");
    assert_eq!(
        o.stderr,
        "agent run: systemd-run can't be run (no such file or directory); job 1 runs without memory limits\n"
    );
    assert!(
        b.data_dir().join("gomuxai.db").is_file(),
        "the store is beside the binary"
    );

    let o = b.run(&["run", "--no-protect", "fail.sh"]);
    assert_eq!(o.code, 3);
    assert!(
        o.stderr
            .ends_with("Error: job 2 (fail.sh) exited 3 (agent run --show 2 has its record)\n"),
        "{}",
        o.stderr
    );

    let o = b.run_as(
        &b.work(),
        &[("AGENT_NAME", "lead")],
        &["run", "--no-protect", "ok.sh"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = b.run(&["run", "--list"]);
    assert_eq!(
        o.stdout,
        "No jobs running (agent run --list --all shows finished ones too).\n"
    );
    let o = b.run(&["run", "--list", "--all"]);
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{}", o.stdout);
    let cols = |l: &str| l.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    assert_eq!(
        cols(lines[0]),
        ["ID", "STARTED", "TOOK", "EXIT", "BY", "SCRIPT"]
    );
    // ID, date, time, took, exit, by, script...
    let row = |i: usize| {
        let c = cols(lines[i]);
        (c[0].clone(), c[4].clone(), c[5].clone(), c[6..].join(" "))
    };
    assert_eq!(
        row(1),
        (
            "1".into(),
            "0".into(),
            "user".into(),
            "ok.sh a --b -q --list".into()
        )
    );
    assert_eq!(
        row(2),
        ("2".into(), "3".into(), "user".into(), "fail.sh".into())
    );
    assert_eq!(
        row(3),
        ("3".into(), "0".into(), "lead".into(), "ok.sh".into())
    );

    let o = b.run(&["run", "--show", "2"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let work = b.work().to_string_lossy().into_owned();
    let data = b.data_dir().to_string_lossy().into_owned();
    for want in [
        "Job 2: fail.sh\n".to_string(),
        "Status:   exited 3 after 0s\n".to_string(),
        format!(" by user on hub, in {work}\n"),
        format!("Ran:      {data}/jobs/2/script (a copy of {work}/fail.sh)\n"),
        "Scope:    none (no memory limits)\n".to_string(),
        "Note:     systemd-run can't be run (no such file or directory): ran plainly, no memory limits\n".to_string(),
        "Script:\n#!/bin/sh\nexit 3\n".to_string(),
    ] {
        assert!(o.stdout.contains(&want), "--show lacks {want:?}:\n{}", o.stdout);
    }
    let o = b.run(&["run", "--show", "2", "--json"]);
    let job: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(job["exit_code"], 3);
    assert_eq!(job["by"], "user");
    assert_eq!(job["host"], "hub");
    assert_eq!(job["unit"], "");
    assert_eq!(job["ran"], format!("{data}/jobs/2/script"));
    assert_eq!(job["not_run"], false);
    assert!(
        job["ended_at"].is_string() && job["found_gone_at"].is_null(),
        "{job}"
    );
    let o = b.run(&["run", "--list", "--all", "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(jobs.len(), 3);
    assert_eq!(
        jobs[0]["args"],
        serde_json::json!(["a", "--b", "-q", "--list"])
    );

    let o = b.run(&["run", "--show", "9"]);
    assert_eq!(o.code, 1);
    assert_eq!(
        o.stderr,
        "Error: no job 9 (agent run --list --all lists them)\n"
    );
    let o = b.run(&["run", "--no-protect", "nope.sh"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .starts_with("Error: no script \"nope.sh\": not a file in "),
        "{}",
        o.stderr
    );

    // The runs are in the command log, with the job's exit code.
    let o = b.run(&["history", "--failed"]);
    assert!(
        o.stdout
            .contains("  3 run --no-protect fail.sh  # job 2 (fail.sh) exited 3"),
        "{}",
        o.stdout
    );
}

#[test]
fn run_promotes_scripts_to_run_by_name() {
    let b = Bin::new();
    let ok = b.script("ok.sh", "echo \"ok: $*\"\n", false);

    let o = b.run(&["run", "--promote", "ok.sh", "--as", "nightly"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout
            .starts_with(&format!("Promoted {ok} as nightly (sha256 "))
            && o.stdout.ends_with("): agent run nightly runs it.\n"),
        "{}",
        o.stdout
    );
    let promoted = b.data_dir().join("scripts/nightly");
    assert!(promoted.is_file());

    // By name, from anywhere.
    let elsewhere = b.dir.path().join("elsewhere");
    let o = b.run_as(&elsewhere, &[], &["run", "--no-protect", "nightly", "x"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "ok: x\n"), "{}", o.stderr);

    let o = b.run(&["run", "--promote", "ok.sh", "--as", "nightly"]);
    assert!(
        o.stdout
            .starts_with("nightly is already this script (sha256 "),
        "{}",
        o.stdout
    );

    b.script("ok.sh", "echo changed\n", false);
    let o = b.run(&["run", "--promote", "ok.sh", "--as", "nightly"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .contains("a different script is already promoted as nightly"),
        "{}",
        o.stderr
    );
    let o = b.run(&["run", "--promote", "--force", "ok.sh", "--as", "nightly"]);
    assert!(
        o.stdout.starts_with(&format!(
            "Replaced the script promoted as nightly with {ok} (sha256 "
        )),
        "{}",
        o.stdout
    );
    let o = b.run_as(&elsewhere, &[], &["run", "--no-protect", "nightly"]);
    assert_eq!(o.stdout, "changed\n");

    // Edited in place since: refused until promoted again.
    std::fs::write(&promoted, "echo edited\n").unwrap();
    let o = b.run_as(&elsewhere, &[], &["run", "--no-protect", "nightly"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .contains("promoted script nightly changed since it was promoted"),
        "{}",
        o.stderr
    );
    // The file's name names it without --as.
    let o = b.run(&["run", "--promote", "ok.sh"]);
    assert!(o.stdout.contains(" as ok.sh (sha256 "), "{}", o.stdout);
}

#[test]
fn task_and_history() {
    let b = Bin::new();
    let o = b.run(&["task", "set", "review", "Review auth"]);
    assert_eq!(
        (o.code, o.stdout.as_str()),
        (0, "Saved task review (1 line, inline).\n")
    );
    assert_eq!(b.run(&["task", "get", "review"]).stdout, "Review auth\n");
    assert!(b.run(&["task", "list"]).stdout.contains("review "));
    assert_eq!(
        b.run(&["task", "clear", "review"]).stdout,
        "Removed task review.\n"
    );
    let o = b.run(&["task", "get", "review"]);
    assert_eq!(o.code, 1);
    assert_eq!(
        o.stderr,
        "Error: no task \"review\": there are no tasks yet (agent task set <name> -f <file> adds one)\n"
    );
    let o = b.run_as(&b.work(), &[("AGENT_NAME", "lead")], &["help", "wait"]);
    assert_eq!(o.code, 0);

    let o = b.run(&["history"]);
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(lines.len(), 6, "{}", o.stdout);
    assert!(
        lines[0].ends_with(" -      0 task set review 'Review auth'"),
        "{}",
        o.stdout
    );
    assert!(
        lines[4]
            .contains(" -      1 task get review  # no task \"review\": there are no tasks yet"),
        "{}",
        o.stdout
    );
    assert!(lines[5].ends_with(" lead   0 help wait"), "{}", o.stdout);
    let o = b.run(&["history", "--agent", "lead", "--json"]);
    let recs: Vec<serde_json::Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["args"], serde_json::json!(["help", "wait"]));
    assert_eq!(recs[0]["host"], "hub");
}

/// Waits (up to 10s) for path to exist.
fn wait_for_file(path: &Path) {
    let start = std::time::Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// review-3 1.7: run's own flags after `--promote <file>` are refused, not
/// dropped (nothing is promoted); run-only flags with --list are refused;
/// a variable the caller set, even to empty, is the job's as set.
#[test]
fn run_refuses_conflicting_flags_and_keeps_the_callers_env() {
    let b = Bin::new();
    b.script(
        "ok.sh",
        "echo \"tmp=[$TMPDIR] jobs=[$CARGO_BUILD_JOBS]\"\n",
        false,
    );
    for (words, flag) in [
        (&["run", "--promote", "ok.sh", "--list"][..], "--list"),
        (
            &["run", "--promote", "ok.sh", "--memory-max", "4G"],
            "--memory-max",
        ),
        (
            &["run", "--promote", "ok.sh", "--as", "x", "--no-protect"],
            "--no-protect",
        ),
        (&["run", "--promote", "ok.sh", "--show", "1"], "--show"),
    ] {
        let o = b.run(words);
        assert_eq!(o.code, 1, "agent {words:?}: {}", o.stdout);
        assert!(
            o.stderr.starts_with(&format!(
                "Error: {flag} doesn't go with --promote: agent run --promote <file> [--as <name>] [--force]"
            )),
            "agent {words:?}: {}",
            o.stderr
        );
    }
    assert!(
        !b.data_dir().join("scripts").exists(),
        "something was promoted"
    );
    let o = b.run(&["run", "--list", "--no-limit-ok"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .starts_with("Error: --no-limit-ok goes with running a script"),
        "{}",
        o.stderr
    );
    let o = b.run(&["run", "--promote", "ok.sh", "--as", "fine", "--force"]);
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = b.run_as(
        &b.work(),
        &[("TMPDIR", ""), ("CARGO_BUILD_JOBS", "")],
        &["run", "-q", "--no-protect", "ok.sh"],
    );
    assert_eq!(
        (o.code, o.stdout.as_str()),
        (0, "tmp=[] jobs=[]\n"),
        "{}",
        o.stderr
    );
    let o = b.run_as(&b.work(), &[], &["run", "-q", "--no-protect", "ok.sh"]);
    assert_eq!(o.stdout, "tmp=[/var/tmp] jobs=[12]\n");
}

/// review-3 1.1/1.3: an explicit limit that can't be applied (no scope
/// here: no systemd-run on PATH) is an error with nothing run, and the
/// record says so; --no-limit-ok runs it, saying so first.
#[test]
fn run_refuses_an_explicit_limit_it_cant_apply() {
    let b = Bin::new();
    b.script("ok.sh", "echo ran\n", false);
    let o = b.run(&["run", "--no-protect", "--memory-max", "4G", "ok.sh"]);
    assert_eq!((o.code, o.stdout.as_str()), (1, ""), "{}", o.stderr);
    assert_eq!(
        o.stderr,
        "Error: job 1: --memory-max 4G can't be applied: systemd-run can't be run (no such file or directory); nothing ran (--no-limit-ok runs it without limits)\n"
    );
    let o = b.run(&["run", "--show", "1"]);
    assert!(
        o.stdout.contains("Status:   not run\n")
            && o.stdout
                .contains("Note:     not run: --memory-max 4G can't be applied"),
        "{}",
        o.stdout
    );
    let o = b.run(&["run", "--list", "--all"]);
    let row: Vec<&str> = o
        .stdout
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .collect();
    assert_eq!(&row[3..7], ["not", "run", "-", "user"], "{}", o.stdout);
    let o = b.run(&[
        "run",
        "--no-protect",
        "--memory-max",
        "4G",
        "--no-limit-ok",
        "ok.sh",
    ]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "ran\n"), "{}", o.stderr);
    assert_eq!(
        o.stderr,
        "agent run: systemd-run can't be run (no such file or directory); job 2 runs without memory limits\n"
    );
}

/// review-3 1.3: a systemd-run that can't create the scope: nothing claims
/// a scope or limits before (or after) it fails.
#[test]
fn run_says_nothing_of_a_scope_it_didnt_get() {
    let b = Bin::new();
    b.tool(
        "systemd-run",
        "#!/bin/sh\necho 'Failed to start transient scope unit: Unit was already loaded' >&2\nexit 1\n",
    );
    b.script("ok.sh", "echo ran\n", false);
    let o = b.run(&["run", "--no-protect", "ok.sh"]);
    assert_eq!((o.code, o.stdout.as_str()), (0, "ran\n"), "{}", o.stderr);
    assert!(!o.stderr.contains(" in agent-job-"), "{}", o.stderr);
    let lines: Vec<&str> = o.stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{}", o.stderr);
    assert!(
        lines[1].starts_with("agent run: systemd-run couldn't start agent-job-")
            && lines[1].ends_with(".scope (it exited 1); job 1 runs without memory limits"),
        "{}",
        o.stderr
    );
    let o = b.run(&["run", "--show", "1", "--json"]);
    let job: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        (&job["unit"], &job["memory_max"], &job["memory_high"]),
        (
            &serde_json::json!(""),
            &serde_json::json!(""),
            &serde_json::json!("")
        )
    );
}

/// review-3 1.5: SIGTERM or SIGHUP to `agent run` stops the job (here run
/// plainly: its process) and records the end; nothing of the job goes on.
#[test]
fn run_stops_the_job_on_sigterm_and_sighup_plainly() {
    let b = Bin::new();
    run_stops_the_job_on_stop_signals(&b, false);
}

/// The same in a real scope: the scope is stopped.
#[test]
fn run_stops_the_job_on_sigterm_and_sighup_in_a_scope() {
    let Some(b) = Bin::with_systemd() else { return };
    run_stops_the_job_on_stop_signals(&b, true);
}

fn run_stops_the_job_on_stop_signals(b: &Bin, scoped: bool) {
    b.link("sleep");
    b.script(
        "sleeper.sh",
        "#!/bin/sh\n: > \"started-$1\"\nsleep 1.0\necho SURVIVED > \"out-$1\"\n",
        true,
    );
    for (n, (sig, name)) in [(libc::SIGTERM, "SIGTERM"), (libc::SIGHUP, "SIGHUP")]
        .into_iter()
        .enumerate()
    {
        let id = n + 1;
        let tag = name.to_lowercase();
        let child = b
            .command(&b.work(), &[], &["run", "--no-protect", "sleeper.sh", &tag])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_file(&b.work().join(format!("started-{tag}")));
        std::thread::sleep(std::time::Duration::from_millis(100));
        // SAFETY: signalling the agent process this test started.
        unsafe { libc::kill(child.id() as libc::pid_t, sig) };
        let o = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(128 + sig), "{name}: {stderr}");
        let what = if scoped { "(agent-job-" } else { "(pid " };
        assert!(
            stderr.contains(&format!("agent run: got {name}: stopping job {id} {what}"))
                && stderr.contains(&format!(
                    "Error: job {id} (sleeper.sh) stopped: agent run got {name}; it exited "
                )),
            "{name}: {stderr}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(
            !b.work().join(format!("out-{tag}")).exists(),
            "{name}: the job went on"
        );
        let o = b.run(&["run", "--show", &id.to_string(), "--json"]);
        let job: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
        assert!(
            job["ended_at"].is_string() && job["found_gone_at"].is_null(),
            "{job}"
        );
        assert_ne!(job["exit_code"], 0, "{job}");
        assert!(
            job["note"]
                .as_str()
                .unwrap()
                .contains(&format!("stopped: agent run got {name}")),
            "{job}"
        );
        assert!(job["exit_code"].is_i64(), "{job}");
    }
}

/// review-3 1.1/1.2/1.5, in real scopes: a job that empties the runner's
/// $TMPDIR, or runs with a $TMPDIR that doesn't exist, runs once, in its
/// scope (named for this store), with the limits systemd applied; a limit
/// too small to start it ends the job, with nothing run unlimited after; a
/// runner killed outright leaves a job whose end is unknown.
#[test]
fn run_in_real_scopes() {
    let Some(b) = Bin::with_systemd() else { return };
    b.link("sleep");
    b.link("rm");
    let scratch = b.dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch_s = scratch.to_string_lossy().into_owned();
    b.script(
        "cleaner.sh",
        "#!/bin/sh\necho ran >> cnt.txt\nrm -rf \"$TMPDIR\"/* \"$TMPDIR\"/.[!.]*\n",
        true,
    );
    let o = b.run_as(
        &b.work(),
        &[("TMPDIR", &scratch_s)],
        &["run", "--no-protect", "cleaner.sh"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        std::fs::read_to_string(b.work().join("cnt.txt")).unwrap(),
        "ran\n"
    );
    let o = b.run(&["run", "--show", "1", "--json"]);
    let job: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    let unit = job["unit"].as_str().unwrap().to_string();
    assert!(
        unit.starts_with("agent-job-")
            && unit.ends_with("-1.scope")
            && unit.len() == "agent-job-12345678-1.scope".len(),
        "{job}"
    );
    assert_eq!(job["memory_swap_max"], "0");

    // A TMPDIR that doesn't exist, and an explicit limit: once, limited.
    b.script("count.sh", "#!/bin/sh\necho ran >> count.txt\n", true);
    let gone = b
        .dir
        .path()
        .join("no-such-dir")
        .to_string_lossy()
        .into_owned();
    let o = b.run_as(
        &b.work(),
        &[("TMPDIR", &gone)],
        &["run", "--no-protect", "--memory-max", "64M", "count.sh"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("agent run: job 2 in agent-job-")
            && o.stderr.contains("MemoryMax=64M MemorySwapMax=0)"),
        "{}",
        o.stderr
    );
    assert_eq!(
        std::fs::read_to_string(b.work().join("count.txt")).unwrap(),
        "ran\n"
    );
    let job: serde_json::Value =
        serde_json::from_str(&b.run(&["run", "--show", "2", "--json"]).stdout).unwrap();
    assert_eq!(
        (job["memory_max"].as_str(), job["not_run"].as_bool()),
        (Some("64M"), Some(false))
    );

    // Too small to start: the job's end, not a fallback. A real kernel OOM
    // kill, which the desktop shows as a memory warning, so only when asked.
    if std::env::var_os("AGENT_TEST_OOM").is_some() {
        let o = b.run(&[
            "run",
            "--no-protect",
            "--memory-max",
            "4K",
            "--memory-high",
            "4K",
            "count.sh",
        ]);
        assert_eq!(o.code, 137, "{}", o.stderr);
        assert_eq!(
            std::fs::read_to_string(b.work().join("count.txt")).unwrap(),
            "ran\n"
        );
        assert!(
            o.stderr
                .contains("not run: killed by SIGKILL in agent-job-"),
            "{}",
            o.stderr
        );
    }

    // The runner killed outright (SIGKILL: nothing to catch): the job runs
    // on in its scope; once it's gone, its end is unknown, not the time it
    // was found so.
    b.script("slow.sh", "#!/bin/sh\n: > slow.started\nsleep 1\n", true);
    let mut child = b
        .command(&b.work(), &[], &["run", "--no-protect", "slow.sh"])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&b.work().join("slow.started"));
    child.kill().unwrap();
    child.wait().unwrap();
    // Job 4 when the OOM step above ran, else 3.
    let id = if std::env::var_os("AGENT_TEST_OOM").is_some() {
        "4"
    } else {
        "3"
    };
    let o = b.run(&["run", "--list"]);
    assert!(
        o.stdout
            .lines()
            .nth(1)
            .is_some_and(|l| l.starts_with(&format!("{id} "))),
        "{}",
        o.stdout
    );
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let o = b.run(&["run", "--show", id]);
    assert!(
        o.stdout.contains("Status:   ended, when and with what exit code unknown (its runner was killed; found no longer running at "),
        "{}",
        o.stdout
    );
    let job: serde_json::Value =
        serde_json::from_str(&b.run(&["run", "--show", id, "--json"]).stdout).unwrap();
    assert!(
        job["ended_at"].is_null() && job["found_gone_at"].is_string() && job["exit_code"].is_null(),
        "{job}"
    );
}

// help, version and --help need no store: on a fresh binary they create no
// data dir (the command log is written only where one exists). Every other
// command opens it, as Go's setup does, even one that reads nothing there
// (statuses): so a fresh install's first commands are in agent history
// (review 5, finding 5).
#[test]
fn help_and_version_open_nothing() {
    let b = Bin::new();
    for words in [
        &["help"][..],
        &["help", "status"],
        &["version"],
        &["--version"],
        &["run", "--help"],
    ] {
        let o = b.run(words);
        assert_eq!(o.code, 0, "agent {words:?}: {}", o.stderr);
    }
    assert!(!b.data_dir().exists(), "a data dir appeared");

    let o = b.run(&["statuses"]);
    assert_eq!(o.code, 0, "agent statuses: {}", o.stderr);
    assert!(
        b.data_dir().join("gomuxai.db").exists(),
        "statuses opened no store"
    );
    let o = b.run(&[
        "hooks",
        "status",
        "--settings",
        "/nonexistent/settings.json",
    ]);
    assert_eq!(o.code, 0, "agent hooks status: {}", o.stderr);
    let o = b.run(&["history"]);
    let rows: Vec<&str> = o.stdout.lines().collect();
    assert!(
        rows.len() == 2
            && rows[0].ends_with(" statuses")
            && rows[1].ends_with(" hooks status --settings /nonexistent/settings.json"),
        "history: {}",
        o.stdout
    );
}

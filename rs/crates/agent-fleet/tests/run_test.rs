//! Real runs of tiny scripts, under a temp dir in /var/tmp with a temp store.
//! Runs in a systemd scope are tested where `systemd-run --user --scope`
//! works; a scope's name carries its store (the temp data dir), so they
//! never meet a real job's. protect_termd is never run here (start is told
//! not to): its commands are unit-tested against a fake systemctl. Fake
//! systemd-runs and systemctls are scripts in the temp dir, named by path.
//!
//! SIGTERM to the runner is tested on the built binary (agent's
//! tests/binary.rs): here it would stop every test's job in this process.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use agent_fleet::run::{self, PromoteOutcome, StartOptions};
use agent_store::{Job, Store, Time};

struct Env {
    dir: tempfile::TempDir,
    store: Store,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::Builder::new()
            .prefix("agent-fleet-run-")
            .tempdir_in("/var/tmp")
            .unwrap();
        std::fs::create_dir_all(dir.path().join(".gomuxai")).unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        let store = Store::open(dir.path().join(".gomuxai/gomuxai.db")).unwrap();
        Env { dir, store }
    }
    fn data_dir(&self) -> String {
        self.dir
            .path()
            .join(".gomuxai")
            .to_string_lossy()
            .into_owned()
    }
    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }
    fn cwd(&self) -> String {
        self.work().to_string_lossy().into_owned()
    }
    /// A script in work/, mode 0755 unless exec is false.
    fn script(&self, name: &str, body: &str, exec: bool) -> PathBuf {
        let p = self.work().join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(
            &p,
            std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
        )
        .unwrap();
        p
    }
    /// An executable in the temp dir (outside work/): a fake systemd-run or
    /// systemctl.
    fn tool(&self, name: &str, body: &str) -> String {
        let p = self.dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }
    /// Options for a run: quiet, no protect_termd; plain unless systemd is
    /// asked for and works here.
    fn opts(&self, script: &str, args: &[&str], systemd: bool) -> StartOptions {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut o = StartOptions::new(
            script,
            &args,
            "tester",
            "hub",
            &self.cwd(),
            &self.data_dir(),
        );
        o.protect_termd = false;
        o.quiet = true;
        if !systemd {
            o.systemd_run = String::new();
        }
        o
    }
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.work().join(name)).unwrap_or_default()
    }
    fn copy_of(&self, id: i64) -> PathBuf {
        Path::new(&self.data_dir()).join(format!("jobs/{id}/script"))
    }
}

/// Whether `systemd-run --user --scope` works here (a user manager to ask).
fn systemd_works() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            return false;
        }
        let ok = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "run_test: systemd-run --user --scope doesn't work here; scope tests run plainly"
            );
        }
        ok
    })
}

/// Both ways where systemd works, else plainly only.
fn modes() -> Vec<bool> {
    if systemd_works() {
        vec![false, true]
    } else {
        vec![false]
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn exit_codes() {
    let e = Env::new();
    e.script("ok.sh", "#!/bin/sh\nexit 0\n", true);
    e.script("three.sh", "#!/bin/sh\nexit 3\n", true);
    e.script("term.sh", "#!/bin/sh\nkill -TERM $$\n", true);
    e.script("kill.sh", "#!/bin/sh\nkill -KILL $$\n", true);
    for systemd in modes() {
        for (script, want) in [
            ("ok.sh", 0),
            ("three.sh", 3),
            ("term.sh", 143),
            ("kill.sh", 137),
        ] {
            let f = run::start(&e.store, &e.opts(script, &[], systemd)).unwrap();
            assert_eq!(f.exit_code, want, "{script} (systemd {systemd})");
            let rec = e.store.get_job(f.job.id).unwrap().unwrap();
            assert_eq!(rec.exit_code, Some(want));
            assert!(!rec.running() && !rec.not_run);
            assert_eq!(
                !rec.unit.is_empty(),
                systemd,
                "{script}: unit {:?}",
                rec.unit
            );
        }
    }
}

#[test]
fn env_defaults() {
    let e = Env::new();
    e.script(
        "env.sh",
        "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n%s\\n' \"$TMPDIR\" \"$CARGO_BUILD_JOBS\" \"$AGENT_JOB_ID\" \"$AGENT_RUN_SCRIPT\" > env.out\n",
        true,
    );
    // Set in this process's environment (to anything): kept. Unset: the
    // default.
    let want = |k: &str, d: &str| std::env::var(k).unwrap_or(d.to_string());
    for systemd in modes() {
        let f = run::start(&e.store, &e.opts("env.sh", &[], systemd)).unwrap();
        assert_eq!(f.exit_code, 0);
        assert_eq!(
            e.read("env.out"),
            format!(
                "{}\n{}\n{}\n{}\n",
                want("TMPDIR", "/var/tmp"),
                want("CARGO_BUILD_JOBS", "12"),
                f.job.id,
                e.work().join("env.sh").display()
            )
        );
    }
    // What start adds, for an environment with neither, one, or both: a
    // variable set to empty is the caller's too (review-3 1.7).
    let env = |set: &[&str]| {
        let set: Vec<String> = set.iter().map(|s| s.to_string()).collect();
        run::job_env(move |k| set.iter().any(|n| n == k))
    };
    let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
    assert_eq!(
        env(&[]),
        vec![pair("TMPDIR", "/var/tmp"), pair("CARGO_BUILD_JOBS", "12")]
    );
    assert_eq!(env(&["TMPDIR"]), vec![pair("CARGO_BUILD_JOBS", "12")]);
    assert_eq!(env(&["TMPDIR", "CARGO_BUILD_JOBS"]), vec![]);
}

#[test]
fn provenance_is_recorded() {
    let e = Env::new();
    let body = "#!/bin/sh\n# builds things\necho \"$@\" > args.out\nexit 5\n";
    let path = e.script("build.sh", body, true);
    for systemd in modes() {
        let before = Time::now();
        let f = run::start(&e.store, &e.opts("build.sh", &["a b", "--x=1"], systemd)).unwrap();
        assert_eq!(f.exit_code, 5);
        assert_eq!(e.read("args.out"), "a b --x=1\n");
        let j: Job = e.store.get_job(f.job.id).unwrap().unwrap();
        assert_eq!(j.script, "build.sh");
        assert_eq!(j.path, path.to_string_lossy());
        assert_eq!(j.ran, e.copy_of(j.id).to_string_lossy());
        assert_eq!(j.sha256, run::sha256_hex(body.as_bytes()));
        let sha = Command::new("sha256sum").arg(&path).output();
        if let Ok(out) = sha {
            assert!(
                String::from_utf8_lossy(&out.stdout).starts_with(&j.sha256),
                "sha256sum disagrees"
            );
        }
        assert_eq!(j.content, body);
        assert_eq!(
            (j.by.as_str(), j.host.as_str(), j.cwd.as_str()),
            ("tester", "hub", e.cwd().as_str())
        );
        assert_eq!(j.args, vec!["a b", "--x=1"]);
        let ended = j.ended_at.unwrap();
        assert!(!j.started_at.before(&before) && !ended.before(&j.started_at));
        assert_eq!(
            (j.exit_code, j.found_gone_at, j.not_run),
            (Some(5), None, false)
        );
        assert!(j.pid > 0);
        if systemd {
            assert_eq!(j.unit, run::unit_name(&e.data_dir(), j.id));
            assert!(!j.memory_high.is_empty() && !j.memory_max.is_empty());
            assert_eq!(j.memory_swap_max, "0");
            assert_eq!(j.note, "");
            assert!(!j.pid_start.is_empty());
        } else {
            assert_eq!(
                (
                    j.unit.as_str(),
                    j.memory_high.as_str(),
                    j.memory_max.as_str()
                ),
                ("", "", "")
            );
            assert!(j.note.contains("no memory limits"), "{}", j.note);
        }
        assert_eq!(f.job, j);
    }
}

/// systemd-run 254+ rewrites `$$`, `${X}` and a whole-word `$X` in a scope's
/// command line unless told not to: a job's arguments reach it as given,
/// in a scope as plainly.
#[test]
fn arguments_reach_the_job_as_given() {
    let e = Env::new();
    e.script(
        "args.sh",
        "#!/bin/sh\nfor a in \"$@\"; do printf '[%s]' \"$a\"; done > args.out\n",
        true,
    );
    for systemd in modes() {
        let f = run::start(
            &e.store,
            &e.opts("args.sh", &["$HOME", "${HOME}", "a$$b", "$", "*"], systemd),
        )
        .unwrap();
        assert_eq!(f.exit_code, 0);
        assert_eq!(
            e.read("args.out"),
            "[$HOME][${HOME}][a$$b][$][*]",
            "systemd {systemd}"
        );
    }
}

#[test]
fn scripts_without_exec_bit() {
    let e = Env::new();
    e.script(
        "bash.sh",
        "#!/bin/bash\necho \"${BASH_VERSION:+bash}\" > which.out\n",
        false,
    );
    e.script("plain.sh", "echo plain > which.out\nexit 4\n", false);
    e.script("plainx.sh", "echo plainx > which.out\n", true);
    for systemd in modes() {
        assert_eq!(
            run::start(&e.store, &e.opts("bash.sh", &[], systemd))
                .unwrap()
                .exit_code,
            0
        );
        assert_eq!(e.read("which.out"), "bash\n");
        assert_eq!(
            run::start(&e.store, &e.opts("plain.sh", &[], systemd))
                .unwrap()
                .exit_code,
            4
        );
        assert_eq!(e.read("which.out"), "plain\n");
        assert_eq!(
            run::start(&e.store, &e.opts("plainx.sh", &[], systemd))
                .unwrap()
                .exit_code,
            0
        );
        assert_eq!(e.read("which.out"), "plainx\n");
    }
}

#[test]
fn missing_script_and_bad_limits_record_nothing() {
    let e = Env::new();
    let err = run::start(&e.store, &e.opts("nope.sh", &[], false))
        .unwrap_err()
        .to_string();
    assert!(err.contains("no script \"nope.sh\""), "{err}");
    let err = run::start(&e.store, &e.opts("./nope.sh", &[], false))
        .unwrap_err()
        .to_string();
    assert!(err.contains("no such file or directory"), "{err}");
    e.script("x.sh", "exit 0\n", true);
    let mut o = e.opts("x.sh", &[], false);
    o.memory_max = Some("24gb".into());
    assert!(
        run::start(&e.store, &o)
            .unwrap_err()
            .to_string()
            .contains("--memory-max: invalid size")
    );
    assert!(e.store.list_jobs(true).unwrap().is_empty());
}

#[test]
fn promoted_scripts_run_by_name() {
    let e = Env::new();
    let body = "#!/bin/sh\npwd > nightly.out\nexit 2\n";
    e.script("nightly.sh", body, false);
    let p = run::promote(
        &e.store,
        &e.data_dir(),
        &e.cwd(),
        "nightly.sh",
        Some("nightly"),
        "tester",
        false,
    )
    .unwrap();
    assert_eq!(p.outcome, PromoteOutcome::New);
    let dest = Path::new(&e.data_dir()).join("scripts/nightly");
    assert_eq!(p.path, dest.to_string_lossy());
    assert_eq!(mode(&dest), 0o755);
    assert_eq!(p.script.sha256, run::sha256_hex(body.as_bytes()));
    assert_eq!(
        e.store.get_script("nightly").unwrap().unwrap().sha256,
        p.script.sha256
    );

    // Promoting the same again changes nothing; a different script under the
    // name is refused without force.
    let again = run::promote(
        &e.store,
        &e.data_dir(),
        &e.cwd(),
        "nightly.sh",
        Some("nightly"),
        "tester",
        false,
    )
    .unwrap();
    assert_eq!(again.outcome, PromoteOutcome::Unchanged);
    e.script("other.sh", "#!/bin/sh\nexit 0\n", true);
    let err = run::promote(
        &e.store,
        &e.data_dir(),
        &e.cwd(),
        "other.sh",
        Some("nightly"),
        "tester",
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("a different script is already promoted as nightly")
            && err.contains("--force"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), body);

    // Run by name from another directory (not a file there).
    let elsewhere = e.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    for systemd in modes() {
        let mut o = e.opts("nightly", &[], systemd);
        o.cwd = elsewhere.to_string_lossy().into_owned();
        let f = run::start(&e.store, &o).unwrap();
        assert_eq!(f.exit_code, 2);
        assert_eq!(f.job.script, "nightly");
        assert_eq!(f.job.path, dest.to_string_lossy());
        assert_eq!(f.job.sha256, p.script.sha256);
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("nightly.out"))
                .unwrap()
                .trim(),
            elsewhere.to_string_lossy()
        );
    }

    // A file of that name in cwd wins over the promoted script.
    e.script("nightly", "#!/bin/sh\nexit 6\n", true);
    assert_eq!(
        run::start(&e.store, &e.opts("nightly", &[], false))
            .unwrap()
            .exit_code,
        6
    );

    // Edited in place since promotion: refused, until promoted again.
    let mut o = e.opts("nightly", &[], false);
    o.cwd = elsewhere.to_string_lossy().into_owned();
    std::fs::write(&dest, "#!/bin/sh\nexit 9\n").unwrap();
    let err = run::start(&e.store, &o).unwrap_err().to_string();
    assert!(
        err.contains("promoted script nightly changed since it was promoted"),
        "{err}"
    );
    let forced = run::promote(
        &e.store,
        &e.data_dir(),
        &e.cwd(),
        "other.sh",
        Some("nightly"),
        "tester",
        true,
    )
    .unwrap();
    assert_eq!(forced.outcome, PromoteOutcome::Replaced);
    assert_eq!(run::start(&e.store, &o).unwrap().exit_code, 0);

    // The default name is the file's.
    let p = run::promote(
        &e.store,
        &e.data_dir(),
        &e.cwd(),
        "other.sh",
        None,
        "tester",
        false,
    )
    .unwrap();
    assert_eq!(p.script.name, "other.sh");
    assert!(
        run::promote(
            &e.store,
            &e.data_dir(),
            &e.cwd(),
            "other.sh",
            Some("../x"),
            "tester",
            false
        )
        .is_err()
    );

    // A file in the scripts dir that was never promoted isn't run.
    std::fs::write(Path::new(&e.data_dir()).join("scripts/stray"), "exit 0\n").unwrap();
    let mut o = e.opts("stray", &[], false);
    o.cwd = elsewhere.to_string_lossy().into_owned();
    let err = run::start(&e.store, &o).unwrap_err().to_string();
    assert!(
        err.contains("stray is in the scripts dir but was never promoted"),
        "{err}"
    );
}

/// review-3 1.4: what is promoted under a name is what the store records.
/// The file there edited (to another script's bytes) or removed: promoting
/// that other script without --force is refused, and so is running it;
/// promoting the recorded one writes its file back.
#[test]
fn promotion_is_checked_against_the_record_not_the_file() {
    let e = Env::new();
    e.script("v1.sh", "echo v1 > which.out\n", false);
    e.script("v2.sh", "echo v2 > which.out\n", false);
    let dest = Path::new(&e.data_dir()).join("scripts/myjob");
    let promote = |file: &str, force: bool| {
        run::promote(
            &e.store,
            &e.data_dir(),
            &e.cwd(),
            file,
            Some("myjob"),
            "tester",
            force,
        )
    };
    let elsewhere = e.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let run_it = || {
        let mut o = e.opts("myjob", &[], false);
        o.cwd = elsewhere.to_string_lossy().into_owned();
        run::start(&e.store, &o)
    };
    assert_eq!(
        promote("v1.sh", false).unwrap().outcome,
        PromoteOutcome::New
    );
    let v1 = run::sha256_hex(b"echo v1 > which.out\n");

    // The file overwritten with v2's bytes: v2 isn't re-blessed.
    std::fs::copy(e.work().join("v2.sh"), &dest).unwrap();
    assert!(
        run_it()
            .unwrap_err()
            .to_string()
            .contains("changed since it was promoted")
    );
    let err = promote("v2.sh", false).unwrap_err().to_string();
    assert!(
        err.contains("a different script is already promoted as myjob"),
        "{err}"
    );
    assert_eq!(e.store.get_script("myjob").unwrap().unwrap().sha256, v1);
    assert!(
        run_it()
            .unwrap_err()
            .to_string()
            .contains("changed since it was promoted")
    );

    // The file removed: v2 still isn't promoted without force.
    std::fs::remove_file(&dest).unwrap();
    let err = promote("v2.sh", false).unwrap_err().to_string();
    assert!(
        err.contains("a different script is already promoted as myjob"),
        "{err}"
    );
    assert!(!dest.exists());

    // v1 promoted again: its file is back, and it runs.
    let p = promote("v1.sh", false).unwrap();
    assert_eq!(p.outcome, PromoteOutcome::Restored);
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "echo v1 > which.out\n"
    );
    assert_eq!(run_it().unwrap().exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("which.out")).unwrap(),
        "v1\n"
    );

    // With --force, v2 replaces it.
    assert_eq!(
        promote("v2.sh", true).unwrap().outcome,
        PromoteOutcome::Replaced
    );
    assert_eq!(run_it().unwrap().exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("which.out")).unwrap(),
        "v2\n"
    );

    // A file there with no record: replaced only with force, unless it is
    // this very script.
    std::fs::write(Path::new(&e.data_dir()).join("scripts/hand"), "echo hand\n").unwrap();
    let hand = |file: &str, force: bool| {
        run::promote(
            &e.store,
            &e.data_dir(),
            &e.cwd(),
            file,
            Some("hand"),
            "tester",
            force,
        )
    };
    let err = hand("v1.sh", false).unwrap_err().to_string();
    assert!(
        err.contains("holds a different script") && err.contains("never promoted"),
        "{err}"
    );
    e.script("hand.sh", "echo hand\n", false);
    assert_eq!(hand("hand.sh", false).unwrap().outcome, PromoteOutcome::New);
}

/// review-3 1.4: what runs is the job's own copy of what was read, so a
/// script changed between the read and the exec (here by a systemd-run that
/// edits it first) still runs as recorded. Both ways: the scope (a wrapper
/// around the real systemd-run) and the plain run after a failed one.
#[test]
fn what_runs_is_the_recorded_copy() {
    let e = Env::new();
    let orig = e.script(
        "mutable.sh",
        "echo ORIGINAL > out.txt; echo \"$0\" > zero.txt; exit 0\n",
        true,
    );
    let mutate = format!(
        "printf 'echo MUTATED > out.txt; exit 42\\n' > '{}'\n",
        orig.display()
    );
    let failing = e.tool(
        "mutating-failing-systemd-run",
        &format!("#!/bin/sh\n{mutate}exit 1\n"),
    );
    let mut cases = vec![(failing, false)];
    if systemd_works() {
        let real = e.tool(
            "mutating-systemd-run",
            &format!("#!/bin/sh\n{mutate}exec systemd-run \"$@\"\n"),
        );
        cases.push((real, true));
    }
    for (systemd_run, scoped) in cases {
        std::fs::write(
            &orig,
            "echo ORIGINAL > out.txt; echo \"$0\" > zero.txt; exit 0\n",
        )
        .unwrap();
        let mut o = e.opts("mutable.sh", &[], true);
        o.systemd_run = systemd_run;
        let f = run::start(&e.store, &o).unwrap();
        assert_eq!(f.exit_code, 0, "scoped {scoped}");
        assert_eq!(e.read("out.txt"), "ORIGINAL\n");
        assert_eq!(
            std::fs::read_to_string(&orig).unwrap(),
            "echo MUTATED > out.txt; exit 42\n"
        );
        let j = e.store.get_job(f.job.id).unwrap().unwrap();
        assert_eq!(
            j.content,
            "echo ORIGINAL > out.txt; echo \"$0\" > zero.txt; exit 0\n"
        );
        assert_eq!(j.sha256, run::sha256_hex(j.content.as_bytes()));
        assert_eq!(!j.unit.is_empty(), scoped);
        // The copy: what was recorded, private, and $0 is its path.
        let copy = e.copy_of(j.id);
        assert_eq!(j.ran, copy.to_string_lossy());
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), j.content);
        assert_eq!(mode(&copy), 0o700);
        assert_eq!(mode(copy.parent().unwrap()), 0o700);
        assert_eq!(mode(copy.parent().unwrap().parent().unwrap()), 0o700);
        assert_eq!(e.read("zero.txt").trim(), copy.to_string_lossy());
    }
}

#[test]
fn parallel_runs() {
    let e = Env::new();
    e.script(
        "sleep.sh",
        "#!/bin/sh\nsleep 1\necho done > \"sleep-$1.out\"\nexit \"$1\"\n",
        true,
    );
    let systemd = systemd_works();
    let started = Instant::now();
    let db = e.dir.path().join(".gomuxai/gomuxai.db");
    let handles: Vec<_> = (1..=4)
        .map(|i| {
            let o = e.opts("sleep.sh", &[&i.to_string()], systemd);
            let db = db.clone();
            std::thread::spawn(move || {
                // Each its own store, as separate `agent run`s would be.
                let s = Store::open(&db).unwrap();
                run::start(&s, &o).unwrap()
            })
        })
        .collect();
    // While they run, they list as running.
    std::thread::sleep(Duration::from_millis(500));
    let running = run::list(&e.store, false).unwrap();
    assert_eq!(running.len(), 4, "{running:#?}");
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "ran one after another: {:?}",
        started.elapsed()
    );
    let mut ids: Vec<i64> = results.iter().map(|f| f.job.id).collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2, 3, 4]);
    for f in &results {
        assert_eq!(f.exit_code as i64, f.job.args[0].parse::<i64>().unwrap());
    }
    assert!(run::list(&e.store, false).unwrap().is_empty());
    assert_eq!(run::list(&e.store, true).unwrap().len(), 4);
}

/// review-3 1.2: two stores on one account each run their job 1 at the same
/// time, each in its own scope with its limits; and a dead runner's row in
/// one isn't kept "running" by the other's live scope.
#[test]
fn two_stores_job_ids_dont_collide() {
    if !systemd_works() {
        eprintln!("two_stores test: no systemd scope here; skipped");
        return;
    }
    let (a, b) = (Env::new(), Env::new());
    for e in [&a, &b] {
        e.script("s.sh", "#!/bin/sh\nsleep 1.5\n", true);
    }
    let start = |e: &Env| {
        let o = e.opts("s.sh", &[], true);
        let db = e.dir.path().join(".gomuxai/gomuxai.db");
        std::thread::spawn(move || run::start(&Store::open(&db).unwrap(), &o).unwrap())
    };
    // Both stores' job 1 at once: each gets its scope and its limits.
    let (ta, tb) = (start(&a), start(&b));
    let (fa, fb) = (ta.join().unwrap(), tb.join().unwrap());
    assert_eq!((fa.job.id, fb.job.id), (1, 1));
    assert_eq!((fa.exit_code, fb.exit_code), (0, 0));
    assert_eq!(fa.job.unit, run::unit_name(&a.data_dir(), 1));
    assert_eq!(fb.job.unit, run::unit_name(&b.data_dir(), 1));
    assert_ne!(fa.job.unit, fb.job.unit);
    for f in [&fa, &fb] {
        assert!(
            !f.job.memory_max.is_empty() && f.job.note.is_empty(),
            "{:?}",
            f.job
        );
    }

    // While a's job 2 runs in its scope: b's job 2, recorded as running
    // with its own unit, whose runner is gone, isn't running.
    let ta = start(&a);
    std::thread::sleep(Duration::from_millis(600));
    let mut c = Command::new("true").spawn().unwrap();
    let dead = c.id() as i64;
    let dead_start = run::process_start(dead).unwrap_or_default();
    c.wait().unwrap();
    let mut stale = Job {
        script: "x.sh".into(),
        path: "/w/x.sh".into(),
        by: "tester".into(),
        host: "hub".into(),
        cwd: "/w".into(),
        pid: dead,
        pid_start: dead_start,
        started_at: Time::now() - chrono::TimeDelta::minutes(5),
        ..Default::default()
    };
    b.store.create_job(&mut stale).unwrap();
    assert_eq!(stale.id, 2);
    stale.unit = run::unit_name(&b.data_dir(), 2);
    b.store.update_job_launch(&stale).unwrap();
    assert!(
        run::list(&a.store, false)
            .unwrap()
            .iter()
            .any(|j| j.id == 2),
        "a's job 2 should be running"
    );
    assert!(run::list(&b.store, false).unwrap().is_empty());
    assert!(!b.store.get_job(2).unwrap().unwrap().running());
    let fa = ta.join().unwrap();
    assert_eq!((fa.job.id, fa.exit_code), (2, 0));
}

/// review-3 1.1: a scope that kills the job before the script starts (a
/// limit too small to start anything) is the job's end: it isn't taken for
/// a scope that couldn't be created, so it never runs again unlimited.
#[test]
fn a_limit_too_small_to_start_is_not_a_fallback() {
    // A real kernel OOM kill in the job's scope: the desktop shows each one
    // as a memory warning, so this runs only when asked (AGENT_TEST_OOM=1).
    if std::env::var_os("AGENT_TEST_OOM").is_none() {
        eprintln!("OOM-kill test skipped: set AGENT_TEST_OOM=1 to run it");
        return;
    }
    if !systemd_works() {
        eprintln!("tiny limit test: no systemd scope here; skipped");
        return;
    }
    let e = Env::new();
    e.script("count.sh", "#!/bin/sh\necho ran >> count.out\n", true);
    let mut o = e.opts("count.sh", &[], true);
    o.memory_max = Some("4K".into());
    o.memory_high = Some("4K".into());
    let f = run::start(&e.store, &o).unwrap();
    assert_eq!(f.exit_code, 137);
    assert_eq!(e.read("count.out"), "", "the script ran");
    let j = e.store.get_job(f.job.id).unwrap().unwrap();
    assert!(j.not_run && !j.running());
    assert_eq!(j.exit_code, Some(137));
    assert!(
        j.note.contains("killed by SIGKILL") && j.note.contains("before the script started"),
        "{}",
        j.note
    );
    assert_eq!((j.unit.as_str(), j.memory_max.as_str()), ("", ""));
}

/// review-3 1.1/1.3: the defaults fall back to a plain run (said, recorded)
/// when no scope can be created; an explicit limit that can't be applied
/// is an error with nothing run, unless no_limit_ok.
#[test]
fn plain_fallback_when_systemd_run_is_missing_or_fails() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let e = Env::new();
    e.script(
        "count.sh",
        "#!/bin/sh\necho ran >> count.out\nexit 7\n",
        true,
    );

    // No systemd-run at all.
    let mut o = e.opts("count.sh", &[], true);
    o.systemd_run = e
        .dir
        .path()
        .join("no-such-systemd-run")
        .to_string_lossy()
        .into_owned();
    let f = run::start(&e.store, &o).unwrap();
    assert_eq!(f.exit_code, 7);
    assert_eq!(e.read("count.out"), "ran\n");
    assert_eq!((f.job.unit.as_str(), f.job.memory_max.as_str()), ("", ""));
    assert!(
        f.job.note.contains("can't be run") && f.job.note.contains("no memory limits"),
        "{}",
        f.job.note
    );

    // A systemd-run that can't create the scope (no user manager): it exits
    // 1 without running anything, and the job then runs once, plainly.
    let fake = e.tool(
        "fake-systemd-run",
        "#!/bin/sh\necho 'Failed to connect to user scope bus via local transport: No such file or directory' >&2\nexit 1\n",
    );
    o.systemd_run = fake.clone();
    let f = run::start(&e.store, &o).unwrap();
    assert_eq!(f.exit_code, 7);
    assert_eq!(e.read("count.out"), "ran\nran\n");
    let j = e.store.get_job(f.job.id).unwrap().unwrap();
    assert_eq!(
        (
            j.unit.as_str(),
            j.memory_high.as_str(),
            j.memory_max.as_str()
        ),
        ("", "", "")
    );
    assert!(
        j.note.contains("systemd-run couldn't start") && j.note.contains("exited 1"),
        "{}",
        j.note
    );
    assert_eq!(j.exit_code, Some(7));
    assert!(j.pid > 0);

    // An explicit limit with no scope to hold it: refused, nothing ran.
    for (systemd_run, why) in [
        (fake.clone(), "systemd-run couldn't start"),
        (String::new(), "systemd-run not used"),
    ] {
        let mut o = e.opts("count.sh", &[], true);
        o.systemd_run = systemd_run;
        o.memory_max = Some("4G".into());
        let err = run::start(&e.store, &o).unwrap_err().to_string();
        assert!(
            err.contains("--memory-max 4G can't be applied")
                && err.contains(why)
                && err.contains("nothing ran"),
            "{err}"
        );
        assert_eq!(e.read("count.out"), "ran\nran\n");
        let j = e.store.list_jobs(true).unwrap().pop().unwrap();
        assert!(j.not_run && !j.running() && j.exit_code.is_none(), "{j:?}");
        assert!(
            j.note
                .starts_with("not run: --memory-max 4G can't be applied"),
            "{}",
            j.note
        );
        // ... unless no_limit_ok: then it runs, plainly.
        o.no_limit_ok = true;
        assert_eq!(run::start(&e.store, &o).unwrap().exit_code, 7);
        assert_eq!(e.read("count.out").lines().count(), 3);
        std::fs::write(e.work().join("count.out"), "ran\nran\n").unwrap();
    }

    // A job whose own exit is 1 under a working systemd-run is not run again.
    if systemd_works() {
        e.script("one.sh", "#!/bin/sh\necho ran >> one.out\nexit 1\n", true);
        let f = run::start(&e.store, &e.opts("one.sh", &[], true)).unwrap();
        assert_eq!(f.exit_code, 1);
        assert_eq!(e.read("one.out"), "ran\n");
        assert!(!f.job.unit.is_empty());
    }
}

/// review-3 1.3: the limits recorded are what systemd reports for the
/// scope, checked before the script starts. A fake systemctl stands in for
/// systemd's answer (the scope itself is real).
#[test]
fn limits_recorded_are_what_systemd_applied() {
    if !systemd_works() {
        eprintln!("applied limits test: no systemd scope here; skipped");
        return;
    }
    let e = Env::new();
    e.script("count.sh", "#!/bin/sh\necho ran >> count.out\n", true);
    let shows = |high: &str, max: &str, cgroup: &str| {
        format!(
            "#!/bin/sh\ncase \"$2\" in show) printf 'LoadState=loaded\\nMemoryHigh={high}\\nMemoryMax={max}\\nMemorySwapMax=0\\nControlGroup={cgroup}\\n';; *) exec systemctl \"$@\";; esac\n"
        )
    };
    let g = 1u64 << 30;
    // systemd applied 1G where 2G was asked.
    let one_g = e.tool(
        "systemctl-1g",
        &shows(&(g / 2).to_string(), &g.to_string(), ""),
    );
    let mut o = e.opts("count.sh", &[], true);
    o.systemctl = one_g.clone();
    o.memory_high = Some("1G".into());
    o.memory_max = Some("2G".into());
    let err = run::start(&e.store, &o).unwrap_err().to_string();
    assert!(
        err.contains(
            "--memory-max can't be applied: MemoryMax is 1G in its scope, not the 2G asked"
        ),
        "{err}"
    );
    assert!(err.contains("nothing ran"), "{err}");
    assert_eq!(e.read("count.out"), "");
    let j = e.store.list_jobs(true).unwrap().pop().unwrap();
    assert!(
        j.not_run && j.unit.is_empty() && j.memory_max.is_empty(),
        "{j:?}"
    );
    // With no_limit_ok it runs, and records what applied.
    o.no_limit_ok = true;
    let f = run::start(&e.store, &o).unwrap();
    assert_eq!((f.exit_code, e.read("count.out").as_str()), (0, "ran\n"));
    assert_eq!(
        (
            f.job.memory_high.as_str(),
            f.job.memory_max.as_str(),
            f.job.memory_swap_max.as_str()
        ),
        ("512M", "1G", "0")
    );
    assert!(
        f.job
            .note
            .contains("MemoryMax is 1G in its scope, not the 2G asked"),
        "{}",
        f.job.note
    );
    assert!(!f.job.unit.is_empty());

    // Default limits that systemd applied otherwise: said, recorded as
    // applied, and run.
    let mut o = e.opts("count.sh", &[], true);
    o.systemctl = one_g;
    let f = run::start(&e.store, &o).unwrap();
    assert_eq!((f.exit_code, f.job.memory_max.as_str()), (0, "1G"));
    assert!(
        f.job
            .note
            .contains("MemoryMax is 1G in its scope, not the "),
        "{}",
        f.job.note
    );
    assert_eq!(e.read("count.out").lines().count(), 2);

    // A cgroup without the memory files: systemd took the limits, the
    // kernel doesn't enforce them (no delegation).
    let root = e.dir.path().join("cgroupfs");
    std::fs::create_dir_all(root.join("fake.scope")).unwrap();
    let undelegated = e.tool(
        "systemctl-undelegated",
        &shows(&(g / 2).to_string(), &g.to_string(), "/fake.scope"),
    );
    let mut o = e.opts("count.sh", &[], true);
    o.systemctl = undelegated;
    o.cgroup_root = root.to_string_lossy().into_owned();
    o.memory_max = Some("1G".into());
    let err = run::start(&e.store, &o).unwrap_err().to_string();
    assert!(err.contains("--memory-max can't be applied: MemoryMax=1G isn't enforced: its cgroup has no memory.max"), "{err}");
    assert_eq!(e.read("count.out").lines().count(), 2);
    o.memory_max = None;
    let f = run::start(&e.store, &o).unwrap();
    assert_eq!(f.exit_code, 0);
    assert_eq!(
        (
            f.job.memory_high.as_str(),
            f.job.memory_max.as_str(),
            f.job.memory_swap_max.as_str()
        ),
        ("", "", "")
    );
    assert!(f.job.note.contains("isn't enforced"), "{}", f.job.note);

    // systemd can't be asked, nor the cgroup read: an explicit limit isn't
    // taken on trust.
    let broken = e.tool(
        "systemctl-broken",
        "#!/bin/sh\necho 'Failed to connect to bus' >&2\nexit 1\n",
    );
    let mut o = e.opts("count.sh", &[], true);
    o.systemctl = broken;
    o.cgroup_root = e
        .dir
        .path()
        .join("no-cgroupfs")
        .to_string_lossy()
        .into_owned();
    o.memory_max = Some("1G".into());
    let err = run::start(&e.store, &o).unwrap_err().to_string();
    assert!(
        err.contains(
            "couldn't confirm MemoryMax=1G: systemctl show failed: Failed to connect to bus"
        ),
        "{err}"
    );
    assert_eq!(e.read("count.out").lines().count(), 3);
}

#[test]
fn list_settles_jobs_whose_runner_died() {
    let e = Env::new();
    let job = |pid: i64, pid_start: &str, unit: &str| {
        let mut j = Job {
            script: "x.sh".into(),
            path: "/w/x.sh".into(),
            by: "tester".into(),
            host: "hub".into(),
            cwd: "/w".into(),
            unit: unit.into(),
            pid,
            pid_start: pid_start.into(),
            started_at: Time::now() - chrono::TimeDelta::minutes(5),
            ..Default::default()
        };
        e.store.create_job(&mut j).unwrap();
        j.id
    };
    // A process that has ended.
    let mut c = Command::new("true").spawn().unwrap();
    let dead = c.id() as i64;
    let dead_start = run::process_start(dead).unwrap_or_default();
    c.wait().unwrap();
    let gone = job(dead, &dead_start, "");
    // This process: alive, with its start time; and its pid with another
    // start time (a reused pid).
    let me = std::process::id() as i64;
    let alive = job(me, &run::process_start(me).unwrap(), "");
    let reused = job(me, "1", "");
    // Recorded long ago, no process ever.
    let never = job(0, "", "");
    // A unit systemd doesn't have (the scope ended and was collected).
    let unit_gone = if systemd_works() {
        Some(job(
            me,
            &run::process_start(me).unwrap(),
            &run::unit_name(&e.data_dir(), 999),
        ))
    } else {
        None
    };

    let before = Time::now();
    let running: Vec<i64> = run::list(&e.store, false)
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(running, vec![alive]);
    // Found gone: when it ended isn't known (review-3 1.5), only when it
    // was found so.
    for id in [Some(gone), Some(reused), Some(never), unit_gone]
        .into_iter()
        .flatten()
    {
        let j = e.store.get_job(id).unwrap().unwrap();
        assert!(!j.running(), "job {id} still recorded as running");
        assert_eq!(j.exit_code, None, "job {id}: exit code should be unknown");
        assert_eq!(j.ended_at, None, "job {id}: its end isn't known");
        assert!(!j.found_gone_at.unwrap().before(&before));
        assert_eq!(j.note, run::GONE_NOTE);
    }
    assert!(run::show(&e.store, alive).unwrap().unwrap().running());
    assert_eq!(run::show(&e.store, 999).unwrap(), None);
    assert_eq!(
        run::list(&e.store, true).unwrap().len(),
        4 + unit_gone.iter().count()
    );
}

/// The real protect_termd, on this user's live g3d-termd units. Harmless (it
/// only makes oomd prefer other victims) but it changes live units, so it
/// runs only by hand: `cargo test -p agent-fleet --test run_test
/// protect_termd_live -- --ignored --nocapture`.
#[test]
#[ignore = "touches the live g3d-termd units: run by hand"]
fn protect_termd_live() {
    let r = run::protect_termd();
    eprintln!("protect_termd: {r:#?}\n{}", r.message());
}

/// A job that allocates past its scope's MemoryMax is killed by the kernel,
/// and this process (which started it) lives on to record a non-zero exit.
#[test]
fn memory_limit_kills_the_job_not_the_runner() {
    // A real kernel OOM kill in the job's scope: the desktop shows each one
    // as a memory warning, so this runs only when asked (AGENT_TEST_OOM=1).
    if std::env::var_os("AGENT_TEST_OOM").is_none() {
        eprintln!("OOM-kill test skipped: set AGENT_TEST_OOM=1 to run it");
        return;
    }
    if !systemd_works() {
        eprintln!("memory_limit test: no systemd scope here; skipped");
        return;
    }
    if Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .map_or(true, |s| !s.success())
    {
        eprintln!("memory_limit test: no python3; skipped");
        return;
    }
    let e = Env::new();
    // 256M of incompressible data, in 8M steps, past a 64M limit.
    e.script(
        "hog.sh",
        "#!/bin/sh\nexec python3 -c '\nimport os\nb = []\nfor i in range(32):\n    b.append(os.urandom(8 << 20))\nopen(\"hog.out\", \"w\").write(\"survived\")\n'\n",
        true,
    );
    let mut o = e.opts("hog.sh", &[], true);
    // High at max (no throttling zone) and no swap: with swap (zram here)
    // the kernel would swap the excess rather than kill.
    o.memory_high = Some("64M".into());
    o.memory_max = Some("64M".into());
    o.memory_swap_max = Some("0".into());
    let started = Instant::now();
    let f = run::start(&e.store, &o).unwrap();
    eprintln!(
        "memory_limit test: exit {} after {:?}, unit {}, {} {} swap {}",
        f.exit_code,
        started.elapsed(),
        f.job.unit,
        f.job.memory_high,
        f.job.memory_max,
        f.job.memory_swap_max
    );
    assert_ne!(f.exit_code, 0);
    assert_eq!(f.exit_code, 137, "expected SIGKILL from the OOM killer");
    assert_eq!(e.read("hog.out"), "");
    let j = e.store.get_job(f.job.id).unwrap().unwrap();
    assert_eq!(j.exit_code, Some(137));
    assert!(!j.not_run);
    assert_eq!(
        (j.memory_max.as_str(), j.memory_swap_max.as_str()),
        ("64M", "0")
    );
}

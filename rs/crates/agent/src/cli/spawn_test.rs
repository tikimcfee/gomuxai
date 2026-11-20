//! Go: internal/cli/spawn_test.go and spawn_remote_test.go.

use agent_base::config::Config;
use agent_bus::AgentRef;
use agent_service::AGENT_STATUS_WAITING;
use agent_store::{Task, Time};

use super::root::Ctx;
use super::spawn::{self, Readiness, spawn_readiness, spawn_target};
use super::testenv::{TestEnv, parse_args, temp_dir};

/// Writes a --backend-config file (Go: writeBackendConfig); the caller points
/// the flag at it.
fn write_backend_config(dir: &tempfile::TempDir, json: &str) -> String {
    let path = dir.path().join("backend.json");
    std::fs::write(&path, json).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn test_setup_backends() {
    let dir = temp_dir("agent-cli-backend-");
    // A custom backend config names spawn's backend unless something else did.
    let path = write_backend_config(&dir, r#"{"name": "custom", "launch_command": "cat"}"#);
    let (mut ctx, _, _) = Ctx::for_test();
    ctx.globals.backend_config = path;
    let mut cfg = Config::default();
    ctx.setup_backends(&mut cfg);
    assert_eq!(cfg.backend, "custom");
    assert_eq!(
        format!("{:#}", ctx.with_backend_config_err(anyhow::anyhow!("x"))),
        "x",
        "a reason kept for a file that loaded"
    );
    assert!(
        ctx.backends().get("custom").is_ok(),
        "custom backend not registered"
    );

    let (mut ctx, _, _) = Ctx::for_test();
    ctx.globals.backend_config =
        write_backend_config(&dir, r#"{"name": "custom", "launch_command": "cat"}"#);
    let mut cfg = Config {
        backend: "claude".into(),
        ..Default::default()
    };
    ctx.setup_backends(&mut cfg);
    assert_eq!(cfg.backend, "claude", "want the app config's claude");

    // An app config isn't a backend config: no error yet, but the reason is kept.
    let path = write_backend_config(&dir, r#"{"backend": "claude"}"#);
    let (mut ctx, _, _) = Ctx::for_test();
    ctx.globals.backend_config = path.clone();
    let mut cfg = Config {
        backend: "claude".into(),
        ..Default::default()
    };
    ctx.setup_backends(&mut cfg);
    let err = format!(
        "{:#}",
        ctx.with_backend_config_err(anyhow::anyhow!("file does not exist"))
    );
    assert!(
        err.contains(&format!(
            "{path} didn't load as a backend config: backend config must have a name"
        )),
        "err = {err}, want the reason the file didn't load"
    );
}

/// setup_cli (private: AGENT_NAME unset, a person's spawn) with a custom
/// backend, "catty", that runs cat, as spawn's backend, so spawns start no
/// real CLI (Go: setupSpawnCLI).
pub fn setup_spawn_cli() -> TestEnv {
    let mut env = TestEnv::setup_cli_private();
    env.set_env("AGENT_NAME", "");
    let dir = temp_dir("agent-cli-catty-");
    let path = dir.path().join("catty.json");
    std::fs::write(
        &path,
        r#"{"name": "catty", "launch_command": "cat", "init_message_format": "read %s"}"#,
    )
    .unwrap();
    env.ctx
        .backends()
        .load_from_file(&path.to_string_lossy())
        .unwrap();
    env.change_cfg(|c| c.backend = "catty".into());
    env
}

fn spawn_args(words: &[&str]) -> spawn::Args {
    parse_args("spawn", words)
}

// Repro: a custom backend config missing launch_command was skipped
// silently, and spawn failed only with "backend not found".
#[test]
fn test_spawn_says_why_backend_config_didnt_load() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    let dir = temp_dir("agent-cli-backend-");
    env.ctx.globals.backend_config = write_backend_config(&dir, r#"{"name": "custom"}"#);
    let mut cfg = env.ctx.cfg().clone();
    env.ctx.setup_backends(&mut cfg);
    cfg.backend = "custom".into();
    env.ctx.replace_cfg(cfg);

    let err = spawn::run(&env.ctx, &spawn_args(&["new-worker"])).expect_err("not found");
    let err = format!("{err:#}");
    assert!(
        err.contains(r#"backend "custom" not found"#) && err.contains("must have a launch_command"),
        "err = {err}, want not found, with why the backend config didn't load"
    );
}

#[test]
fn test_spawn_rejects_missing_or_directory_task_file() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.change_cfg(|c| c.backend = "agy".into());

    // Stray word (e.g. `agent spawn foo spawn --backend agy`) must fail before spawning.
    let err = format!(
        "{:#}",
        spawn::run(&env.ctx, &spawn_args(&["new-worker", "spawn"])).unwrap_err()
    );
    assert!(
        err.contains(r#"task file "spawn" not found"#) && err.contains("omit [task-file]"),
        "stray arg err = {err}"
    );

    // Stored task name passed positionally instead of --task should point at --task.
    env.store()
        .set_task(&Task {
            name: "audit-auth".into(),
            content: "check auth".into(),
            ..Default::default()
        })
        .unwrap();
    let err = format!(
        "{:#}",
        spawn::run(&env.ctx, &spawn_args(&["new-worker", "audit-auth"])).unwrap_err()
    );
    assert!(
        err.contains("--task audit-auth"),
        "stored task positional err = {err}"
    );

    // A directory is not a task file.
    let dir = temp_dir("agent-cli-dir-");
    let d = dir.path().to_string_lossy().into_owned();
    let err = format!(
        "{:#}",
        spawn::run(&env.ctx, &spawn_args(&["new-worker", &d])).unwrap_err()
    );
    assert!(err.contains("is a directory"), "directory arg err = {err}");

    // Passing both --task and positional [task-file] must fail instead of dropping args[1].
    let err = format!(
        "{:#}",
        spawn::run(
            &env.ctx,
            &spawn_args(&["new-worker", "stray-word", "--task", "audit-auth"])
        )
        .unwrap_err()
    );
    assert!(
        err.contains("cannot specify both --task"),
        "--task + positional arg err = {err}"
    );
}

// Spawn left a Claude agent at its trust prompt saying only "Its status is
// unknown so far" (friction #4): it names the prompt now, and what answers it.
#[test]
fn test_spawn_readiness_names_the_trust_prompt() {
    let waiting = AGENT_STATUS_WAITING;
    let none = Readiness::default();
    let task = Readiness {
        task: true,
        remote: false,
    };
    let remote = Readiness {
        task: false,
        remote: true,
    };
    let tests = [
        (
            "claude",
            "claude",
            "trust: /srv/p",
            none,
            "It is waiting at Claude's workspace trust prompt for /srv/p: agent approve w trusts it, or respawn with --trust-workspace.",
        ),
        (
            "with a task",
            "claude",
            "trust: /srv/p",
            task,
            "It is waiting at Claude's workspace trust prompt for /srv/p: agent approve w trusts it, or respawn with --trust-workspace. Its task waits until the folder is trusted.",
        ),
        (
            "agy on another host",
            "agy",
            "trust: /srv/p",
            remote,
            "It is waiting at agy's workspace trust prompt for /srv/p: agent approve w trusts it.",
        ),
        (
            "a permission prompt",
            "claude",
            "permission: Bash: ls",
            none,
            "It is already waiting on a prompt: agent status w -v shows it.",
        ),
        // A dialog Claude shows on its own (friction #29).
        (
            "a dialog, with a task",
            "claude",
            "dialog: Teach auto mode about your environment?; Enter picks: 1. Yes",
            task,
            "It is waiting at a dialog of Claude's (Teach auto mode about your environment?): agent deny w closes it, agent approve w picks its highlighted option. Its task is held until the dialog is gone.",
        ),
    ];
    for (name, backend, detail, r, want) in tests {
        assert_eq!(
            spawn_readiness("w", backend, &waiting, detail, r),
            want,
            "{name}"
        );
    }
}

// spawn_remote_test.go

fn r(name: &str, host: &str) -> AgentRef {
    AgentRef {
        name: name.into(),
        host: host.into(),
    }
}

#[test]
fn test_spawn_target() {
    let env = TestEnv::new();
    let ok = [
        ("w", "", r("w", ""), false),
        ("w@local", "", r("w", ""), false),
        ("w", "local", r("w", ""), false),
        ("w@local", "local", r("w", ""), false),
        ("w@hub", "", r("w", "hub"), true),
        ("w", "hub", r("w", "hub"), true),
        ("w@hub", "hub", r("w", "hub"), true),
    ];
    for (arg, host, want, want_remote) in ok {
        let got = spawn_target(&env.ctx, arg, host);
        assert_eq!(
            got.map_err(|e| format!("{e:#}")),
            Ok((want, want_remote)),
            "spawn_target({arg:?}, {host:?})"
        );
    }
    let errs = [
        (
            "w@hub",
            "other",
            "w@hub names host hub, but --host says other: give one",
        ),
        (
            "w@local",
            "hub",
            "w@local names host local, but --host says hub: give one",
        ),
        ("w", "bad.host", r#"invalid host name "bad.host""#),
        ("w@", "", "invalid host name"),
    ];
    for (arg, host, want) in errs {
        let err = spawn_target(&env.ctx, arg, host).expect_err("an error");
        assert!(
            format!("{err:#}").contains(want),
            "spawn_target({arg:?}, {host:?}) err = {err:#}"
        );
    }
}

/// Serves every op as h1 with a service that knows itself as h1, as a real
/// bus's does, so it takes lead@local for a parent on another host. It
/// shares this host's store and termd, so records are here to check (Go:
/// serveSpawnAsH1).
fn serve_spawn_as_h1(env: &mut TestEnv) -> bool {
    let svc = env.service_with(|c| c.host = "h1".into());
    env.serve_svc_as("h1", svc, agent_bus::ALL_OPS).is_some()
}

#[test]
fn test_remote_spawn() {
    crate::require_termd!();
    let mut env = setup_spawn_cli();
    if !serve_spawn_as_h1(&mut env) {
        eprintln!("skipping: no agent-busd");
        return;
    }
    env.set_env("AGENT_NAME", "lead");
    let tmp = temp_dir("agent-cli-spawn-");
    let dir = tmp.path().to_string_lossy().into_owned();
    // On h1, relative to --cwd, where h1 checks it is a file.
    std::fs::create_dir(tmp.path().join("tasks")).unwrap();
    std::fs::write(tmp.path().join("tasks/build.md"), "build it\n").unwrap();

    spawn::run(
        &env.ctx,
        &spawn_args(&["builder@h1", "tasks/build.md", "--cwd", &dir]),
    )
    .unwrap_or_else(|e| panic!("spawn builder@h1: {e:#}"));
    let out = env.take_out();
    let task = format!("{dir}/tasks/build.md");
    let want = format!(
        "Spawned agent builder@h1 (session rpc-cli-builder, backend catty) on h1 in {dir}, a helper of lead@local.\nAsked it to read {task}.\n"
    );
    assert!(
        out.starts_with(&want),
        "spawn printed:\n{out}\nwant it to start:\n{want}"
    );
    assert!(
        out.contains("builder@h1") && out.matches('\n').count() == 3,
        "want a readiness line naming builder@h1, got:\n{out}"
    );

    // h1 is this host's own service, so the record is here to check.
    let a = env
        .store()
        .get_agent("builder")
        .unwrap()
        .expect("builder's record");
    assert!(
        a.spawned_by == "lead@local" && a.cwd == dir && a.task_file == task,
        "recorded parent {:?}, cwd {:?}, task {:?}",
        a.spawned_by,
        a.cwd,
        a.task_file
    );

    // --host, a stored task, and no AGENT_NAME: a person's spawn, no parent.
    env.set_env("AGENT_NAME", "");
    env.store()
        .set_task(&Task {
            name: "build".into(),
            content: "build it".into(),
            source_file: "/here/only.md".into(),
            created_at: Time::now(),
        })
        .unwrap();
    spawn::run(
        &env.ctx,
        &spawn_args(&["byperson", "--host", "h1", "--task", "build", "--cwd", &dir]),
    )
    .unwrap_or_else(|e| panic!("spawn byperson --host h1: {e:#}"));
    let out = env.take_out();
    assert!(
        out.starts_with(&format!(
            "Spawned agent byperson@h1 (session rpc-cli-byperson, backend catty) on h1 in {dir}.\nSent it task build.\n"
        )),
        "spawn --host printed:\n{out}"
    );
    let a = env
        .store()
        .get_agent("byperson")
        .unwrap()
        .expect("byperson");
    assert!(
        a.spawned_by.is_empty() && a.task_file.is_empty(),
        "byperson's record = {a:?}, want no parent and no task file (its source is a path on the caller)"
    );

    // Spawn's errors come back from h1, saying it was h1.
    let err =
        spawn::run(&env.ctx, &spawn_args(&["builder@h1", "--cwd", &dir])).expect_err("exists");
    assert!(
        format!("{err:#}").starts_with("h1 couldn't spawn builder: agent builder already exists"),
        "respawn builder@h1: err = {err:#}"
    );
}

#[test]
fn test_remote_spawn_refuses_local_only_flags() {
    crate::require_termd!();
    let mut env = setup_spawn_cli();
    env.forbid_bus();

    for (flag, words) in [
        ("--resume", vec!["--resume", "3f34adff"]),
        ("--continue", vec!["--continue"]),
        ("--attach", vec!["--attach"]),
        ("--skip-permissions", vec!["--skip-permissions"]),
        ("--trust-workspace", vec!["--trust-workspace"]),
        ("--backend-config", vec![]),
    ] {
        env.ctx.globals.backend_config = if flag == "--backend-config" {
            "my-cli.json".into()
        } else {
            String::new()
        };
        let mut line = vec!["x@hub"];
        line.extend(words);
        let err = spawn::run(&env.ctx, &spawn_args(&line)).expect_err("refused");
        let want = format!(
            "{flag} works only for an agent on this host: to spawn x with it, run agent spawn on hub"
        );
        assert_eq!(format!("{err:#}"), want, "{flag}");
    }
    env.ctx.globals.backend_config = String::new();

    let err =
        spawn::run(&env.ctx, &spawn_args(&["x@hub", "--cwd", "src/repo"])).expect_err("relative");
    assert!(
        format!("{err:#}").contains("--cwd src/repo: give an absolute path on hub"),
        "relative --cwd: err = {err:#}"
    );
}

// Review 5, finding 6: a spawn on another host took --task with a
// [task-file] (dropping the file), and a task for a shell agent (typing the
// task into the shell as commands); both are refused here, as a local spawn
// refuses them, before anything crosses the bus.
#[test]
fn test_remote_spawn_checks_its_task_as_a_local_spawn_does() {
    crate::require_termd!();
    let mut env = setup_spawn_cli();
    env.forbid_bus();

    let err = spawn::run(
        &env.ctx,
        &spawn_args(&[
            "w@hub",
            "extra.md",
            "--task",
            "t1",
            "--cwd",
            "/var/tmp/work",
        ]),
    )
    .expect_err("both");
    assert_eq!(
        format!("{err:#}"),
        r#"cannot specify both --task "t1" and positional [task-file] "extra.md""#
    );

    env.change_cfg(|c| c.backend = "shell".into());
    for words in [
        vec!["sh@hub", "--task", "t1", "--cwd", "/var/tmp/work"],
        vec!["sh@hub", "file.md", "--cwd", "/var/tmp/work"],
    ] {
        let err = spawn::run(&env.ctx, &spawn_args(&words)).expect_err("shell");
        assert_eq!(
            format!("{err:#}"),
            r#"backend "shell" does not take an initial --task or [task-file]; use agent send or agent keys after spawning"#,
            "{words:?}"
        );
    }
}

// Review 5, finding 6: h1 checks a [task-file] is a file there, as a local
// spawn checks it here, instead of starting an agent and asking it to read
// nothing.
#[test]
fn test_remote_spawn_task_file_must_exist_there() {
    crate::require_termd!();
    let mut env = setup_spawn_cli();
    if !serve_spawn_as_h1(&mut env) {
        eprintln!("skipping: no agent-busd");
        return;
    }
    let tmp = temp_dir("agent-cli-spawn-");
    let dir = tmp.path().to_string_lossy().into_owned();

    let err = spawn::run(
        &env.ctx,
        &spawn_args(&["w@h1", "nonexistent.md", "--cwd", &dir]),
    )
    .expect_err("missing");
    assert_eq!(
        format!("{err:#}"),
        r#"h1 couldn't spawn w: task file "nonexistent.md" not found (omit [task-file] to spawn w with no initial task)"#
    );
    let err = spawn::run(&env.ctx, &spawn_args(&["w@h1", &dir, "--cwd", &dir])).expect_err("dir");
    assert!(
        format!("{err:#}").ends_with("is a directory, not a file"),
        "a directory: err = {err:#}"
    );
    assert!(
        env.store().get_agent("w").unwrap().is_none(),
        "w was spawned"
    );
}

#[test]
fn test_remote_spawn_host_that_doesnt_serve_spawn() {
    crate::require_termd!();
    let mut env = setup_spawn_cli();
    crate::require_bus!(env.serve_as_h1()); // the default remote ops: no spawn

    let err = spawn::run(&env.ctx, &spawn_args(&["x@h1"])).expect_err("not served");
    assert!(
        format!("{err:#}").contains(r#"no bus answering for host "h1""#),
        "err = {err:#}, want no bus answering"
    );
    let hints = env.take_err();
    assert!(
        hints.contains(
            "h1 serves spawn to other hosts only if its bus runs with --remote-ops listing spawn"
        ),
        "hint = {hints:?}"
    );
    assert!(
        env.store().get_agent("x").unwrap().is_none(),
        "x was spawned"
    );
}

/// Changes this process's directory until dropped (Go: t.Chdir). Only in a
/// private env, which holds the environment lock.
pub struct Chdir(std::path::PathBuf);

impl Chdir {
    pub fn to(dir: &std::path::Path) -> Chdir {
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Chdir(old)
    }
}

impl Drop for Chdir {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[test]
fn test_local_spawn_cwd() {
    crate::require_termd!();
    let env = setup_spawn_cli();
    env.forbid_bus();
    let tmp = temp_dir("agent-cli-spawn-");
    let dir = tmp.path().to_string_lossy().into_owned();

    // The task file is relative to spawn's directory, and must exist there.
    let wd_dir = temp_dir("agent-cli-wd-");
    let _cd = Chdir::to(wd_dir.path());
    std::fs::write("task.md", "build it\n").unwrap();
    spawn::run(
        &env.ctx,
        &spawn_args(&["here@local", "task.md", "--cwd", &dir]),
    )
    .unwrap_or_else(|e| panic!("spawn: {e:#}"));
    let out = env.take_out();
    let wd = std::env::current_dir().unwrap();
    let task = wd.join("task.md").to_string_lossy().into_owned(); // relative to spawn's directory, not --cwd
    let want = format!(
        "Spawned agent here (session rpc-cli-here, backend catty) in {dir}.\nAsked it to read {task}.\n"
    );
    assert!(
        out.starts_with(&want),
        "spawn printed:\n{out}\nwant it to start:\n{want}"
    );
    let a = env.store().get_agent("here").unwrap().expect("here");
    assert!(
        a.cwd == dir && a.task_file == task,
        "record = {a:?}, want cwd {dir:?} and task {task:?}"
    );

    let err = spawn::run(
        &env.ctx,
        &spawn_args(&["there", "--cwd", &dir, "--resume", "3f34adff"]),
    )
    .expect_err("refused");
    assert!(
        format!("{err:#}").contains("--cwd and --resume don't go together"),
        "--cwd with --resume: err = {err:#}"
    );
}

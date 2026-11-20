//! Go tests/service/rpc_spawn_test.go.

use std::time::Duration;

use agent_bus::{AgentRef, RpcClient, RpcServer, SpawnRequest, SpawnResponse};
use agent_service::*;

use crate::common::*;
use crate::require_termd;

/// serveSpawn serves env's service as host "h1" with every op, spawn
/// included, and returns a client on a separate connection. Spawns use a
/// custom backend, "catty", that runs cat: no real CLI starts.
pub fn serve_spawn(env: &TestEnv, bus: &TestBus) -> (RpcServer, RpcClient) {
    let cfg_file = env.dir().join("catty.json");
    std::fs::write(
        &cfg_file,
        r#"{"name": "catty", "launch_command": "cat", "init_message_format": "read %s"}"#,
    )
    .unwrap();
    env.backends
        .load_from_file(&cfg_file.display().to_string())
        .unwrap();
    serve(bus, &env.service, "h1", agent_bus::ALL_OPS)
}

#[test]
fn test_rpc_spawn_starts_agent_where_asked() {
    require_termd!();
    let env = setup_test_env();
    let bus = crate::require_bus!();
    let (_server, client) = serve_spawn(&env, &bus);
    let dir = tempdir();
    let dirs = dir.path().display().to_string();
    // The task file is checked on the serving host, as a local spawn checks it.
    std::fs::create_dir(dir.path().join("tasks")).unwrap();
    std::fs::write(dir.path().join("tasks/build.md"), "build it\n").unwrap();

    let resp = client
        .spawn(
            &bg(),
            &AgentRef::new("builder", "h1"),
            SpawnRequest {
                backend: "catty".into(),
                cwd: dirs.clone(),
                task_file: "tasks/build.md".into(), // relative: to Cwd, on the serving host
                spawned_by: "lead@laptop".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Spawn: {e:#}"));
    let session = env.session_name("builder");
    let want = SpawnResponse {
        name: "builder".into(),
        session: session.clone(),
        backend: "catty".into(),
        spawned_by: "lead@laptop".into(),
        cwd: dirs.clone(),
        task_file: format!("{dirs}/tasks/build.md"),
        status: resp.status.clone(),
        ..Default::default()
    };
    assert_eq!(resp, want);
    assert!(
        !resp.status.is_empty() && resp.status != "ended",
        "status = {:?}, want the live agent's",
        resp.status
    );

    let stored = env
        .store
        .get_agent("builder")
        .unwrap()
        .expect("stored agent");
    assert!(
        stored.cwd == dirs
            && stored.spawned_by == "lead@laptop"
            && stored.task_file == want.task_file,
        "stored cwd {:?}, parent {:?}, task {:?}",
        stored.cwd,
        stored.spawned_by,
        stored.task_file
    );
    assert_eq!(
        pane_path(&env, &session),
        real_path(&dirs),
        "pane starts in"
    );

    // The remote parent shows in list, for agent list --host.
    let agents = client.list(&bg(), "h1").unwrap();
    assert!(
        agents.len() == 1 && agents[0].spawned_by == "lead@laptop",
        "List = {agents:?}"
    );
}

#[test]
fn test_rpc_spawn_without_cwd_starts_in_home() {
    require_termd!();
    let mut env = setup_test_env_private();
    let bus = crate::require_bus!();
    let home = tempdir();
    let homes = home.path().display().to_string();
    env.set_env("HOME", &homes);
    let (_server, client) = serve_spawn(&env, &bus);

    let resp = client
        .spawn(
            &bg(),
            &AgentRef::new("homebody", "h1"),
            SpawnRequest {
                backend: "catty".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Spawn: {e:#}"));
    assert!(
        resp.cwd == homes && resp.spawned_by.is_empty(),
        "cwd {:?}, parent {:?}; want {homes:?} and none (a person spawned it)",
        resp.cwd,
        resp.spawned_by
    );
    assert_eq!(
        pane_path(&env, &resp.session),
        real_path(&homes),
        "pane starts in"
    );
}

#[test]
fn test_rpc_spawn_refusals() {
    require_termd!();
    let env = setup_test_env();
    let bus = crate::require_bus!();
    let (_server, client) = serve_spawn(&env, &bus);
    let d = tempdir();
    let file = d.path().join("plain");
    std::fs::write(&file, "").unwrap();
    let dirs = std::cell::RefCell::new(Vec::new());
    let tmp = || {
        let d = tempdir();
        let p = d.path().display().to_string();
        dirs.borrow_mut().push(d);
        p
    };

    let tests = [
        (
            "relative cwd",
            SpawnRequest {
                backend: "catty".into(),
                cwd: "src/repo".into(),
                ..Default::default()
            },
            r#"working directory "src/repo" is not an absolute path"#,
        ),
        (
            "missing cwd",
            SpawnRequest {
                backend: "catty".into(),
                cwd: "/no/such/dir".into(),
                ..Default::default()
            },
            "no such file or directory",
        ),
        (
            "cwd is a file",
            SpawnRequest {
                backend: "catty".into(),
                cwd: file.display().to_string(),
                ..Default::default()
            },
            "is not a directory",
        ),
        (
            "unknown backend",
            SpawnRequest {
                backend: "nope".into(),
                cwd: tmp(),
                ..Default::default()
            },
            r#"backend "nope" not found"#,
        ),
        (
            "no backend",
            SpawnRequest {
                cwd: tmp(),
                ..Default::default()
            },
            "names no backend",
        ),
        // What a local spawn refuses before it starts anything (review 5,
        // finding 6: the Go took all three from a peer).
        (
            "missing task file",
            SpawnRequest {
                backend: "catty".into(),
                cwd: tmp(),
                task_file: "nonexistent.md".into(),
                ..Default::default()
            },
            r#"task file "nonexistent.md" not found (omit [task-file] to spawn refused with no initial task)"#,
        ),
        (
            "task file is a directory",
            SpawnRequest {
                backend: "catty".into(),
                cwd: d.path().display().to_string(),
                task_file: d.path().display().to_string(),
                ..Default::default()
            },
            "is a directory, not a file",
        ),
        (
            "shell with a task",
            SpawnRequest {
                backend: "shell".into(),
                cwd: tmp(),
                task_content: "rm -rf ~".into(),
                ..Default::default()
            },
            r#"backend "shell" does not take an initial --task or [task-file]"#,
        ),
        (
            "task and task file",
            SpawnRequest {
                backend: "catty".into(),
                cwd: d.path().display().to_string(),
                task_content: "x".into(),
                task_file: "plain".into(),
                ..Default::default()
            },
            r#"cannot specify both --task and positional [task-file] "plain""#,
        ),
    ];
    for (name, req, want) in tests {
        let err = client
            .spawn(&bg(), &AgentRef::new("refused", "h1"), req)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains(want),
            "{name}: err = {err:#}, want it to say {want:?}"
        );
    }
    assert!(
        env.store.get_agent("refused").unwrap().is_none(),
        "a refused spawn left a record"
    );
    if env
        .tmux
        .session_exists(&env.session_name("refused"))
        .unwrap()
    {
        let _ = env.tmux.kill_session(&env.session_name("refused"));
        panic!("a refused spawn left a session");
    }

    // A name in use is refused the way a local spawn refuses it.
    client
        .spawn(
            &bg(),
            &AgentRef::new("taken", "h1"),
            SpawnRequest {
                backend: "catty".into(),
                cwd: tmp(),
                ..Default::default()
            },
        )
        .unwrap();
    let err = client
        .spawn(
            &bg(),
            &AgentRef::new("taken", "h1"),
            SpawnRequest {
                backend: "catty".into(),
                cwd: tmp(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("agent taken already exists"),
        "second spawn of taken: err = {err:#}"
    );
}

// A local spawn with Cwd starts there too.
#[test]
fn test_spawn_cwd() {
    require_termd!();
    let env = setup_test_env();
    let dir = tempdir();
    let dirs = dir.path().display().to_string();
    spawn_fake_with(
        &env,
        "placed",
        SpawnOptions {
            cwd: dirs.clone(),
            ..Default::default()
        },
    );
    let a = env.store.get_agent("placed").unwrap();
    assert!(
        a.as_ref().is_some_and(|a| a.cwd == dirs),
        "stored agent = {a:?}, want cwd {dirs:?}"
    );
    assert_eq!(
        pane_path(&env, &env.session_name("placed")),
        real_path(&dirs),
        "pane starts in"
    );
}

// A peer can't name an agent on the serving host as the parent: the spawn
// came over the bus, so that agent didn't ask for it.
#[test]
fn test_rpc_spawn_refuses_parent_on_serving_host() {
    require_termd!();
    let mut env = setup_test_env();
    let bus = crate::require_bus!();
    env.cfg_mut().host = "h1".into();
    let (_server, client) = serve_spawn(&env, &bus);

    // Host names match ignoring case, so "H1" is this host too.
    for by in ["victim@h1", "victim@H1"] {
        let cwd = tempdir();
        let err = client
            .spawn(
                &bg(),
                &AgentRef::new("imposter", "h1"),
                SpawnRequest {
                    backend: "catty".into(),
                    cwd: cwd.path().display().to_string(),
                    spawned_by: by.into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("names an agent on this host (h1)"),
            "spawned_by {by}: err = {err:#}, want a refusal"
        );
        assert!(
            env.store.get_agent("imposter").unwrap().is_none(),
            "spawned_by {by}: spawned anyway"
        );
    }
}

// A spawn whose context ends mid-startup (a bus stopping cancels the
// requests it is serving) leaves neither its session nor its record.
#[test]
fn test_spawn_canceled_during_startup_leaves_nothing() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().startup_delay = Duration::from_secs(2);
    register_fake_hook(&env);

    let (ctx, _cancel) = Context::with_timeout(&bg(), Duration::from_millis(100));
    let res = env.service.spawn(
        &ctx,
        SpawnOptions {
            name: "stopped".into(),
            backend: "fakehooks".into(),
            ..Default::default()
        },
    );
    if res.is_ok() {
        let _ = env.tmux.kill_session(&env.session_name("stopped"));
        panic!("spawn succeeded after its context ended");
    }
    assert!(
        env.store.get_agent("stopped").unwrap().is_none(),
        "a canceled spawn left its record"
    );
    if env
        .tmux
        .session_exists(&env.session_name("stopped"))
        .unwrap()
    {
        let _ = env.tmux.kill_session(&env.session_name("stopped"));
        panic!("a canceled spawn left its session");
    }
}

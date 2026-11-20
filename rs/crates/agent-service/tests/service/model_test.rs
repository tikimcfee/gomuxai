//! Go tests/service/model_test.go.

use agent_service::*;

use crate::common::*;
use crate::require_termd;
use crate::rpc_spawn_test::serve_spawn;

// spawn --model goes on the CLI's launch line, ahead of the exit report agy's
// line ends with, and resume relaunches on the same model.
#[test]
fn test_spawn_model_is_passed_and_kept_on_resume() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/gomuxai/agent".into();
    let resumed_fake = register_fake_agy(&env);
    let spawned = spawn_fake_agy(
        &env,
        SpawnOptions {
            name: "m".into(),
            model: "gemini-3.8-flash-high".into(),
            ..Default::default()
        },
    );

    let launch = spawned.launched_cmd();
    let model = launch.find(" --model 'gemini-3.8-flash-high'");
    let exit = launch.find("; ");
    assert!(
        model.is_some() && exit.is_some() && model < exit,
        "launch line {launch:?}: want --model before the exit report"
    );
    assert_eq!(
        env.state_value("m", STATE_KEY_MODEL),
        "gemini-3.8-flash-high"
    );

    // The same through resume, on the real agy registration.
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "r".into(),
                backend: "agy".into(),
                model: "claude-opus-5-5-high".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        env.service.status_info("r").unwrap().model,
        "claude-opus-5-5-high",
        "StatusInfo: want its model"
    );
    env.tmux.kill_session(&env.session_name("r")).unwrap();
    env.service
        .resume(
            &bg(),
            ResumeOptions {
                name: "r".into(),
                session_id: "conv-9".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Resume: {e:#}"));
    let launched = resumed_fake
        .lock()
        .unwrap()
        .as_ref()
        .map(|f| f.launched_cmd());
    assert!(
        launched
            .as_deref()
            .is_some_and(|c| c.contains("--model 'claude-opus-5-5-high'")),
        "resumed with {launched:?}, want --model claude-opus-5-5-high"
    );
    assert_eq!(
        env.state_value("r", STATE_KEY_MODEL),
        "claude-opus-5-5-high",
        "after resume"
    );
}

// A backend that takes no model refuses --model rather than start on its
// default, as does a name that doesn't read as a model.
#[test]
fn test_spawn_model_refusals() {
    require_termd!();
    let env = setup_test_env();
    let cfg_path = env.dir().join("backend.json");
    std::fs::write(
        &cfg_path,
        r#"{"name": "custom", "launch_command": "echo custom"}"#,
    )
    .unwrap();
    env.backends
        .load_from_file(&cfg_path.display().to_string())
        .unwrap();
    for (backend, model, want) in [
        ("shell", "opus", "backend shell takes no --model"),
        ("claude", "opus; rm -rf ~", "invalid model"),
        ("claude", "-p", "invalid model"),
        ("custom", "opus", "backend custom takes no --model"),
    ] {
        let err = env
            .service
            .spawn(
                &bg(),
                SpawnOptions {
                    name: "x".into(),
                    backend: backend.into(),
                    model: model.into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            err.to_string().contains(want),
            "spawn --backend {backend} --model {model:?}: {err:#}, want {want:?}"
        );
        assert!(
            !env.tmux.session_exists(&env.session_name("x")).unwrap(),
            "--model {model:?} left a session behind"
        );
    }
}

#[test]
fn test_validate_model() {
    for ok in [
        "opus",
        "sonnet",
        "claude-fable-5-1",
        "claude-opus-5-5[1m]",
        "gemini-3.8-flash-high",
        "openrouter/x-ai/grok-4",
        "gpt-5@2026-01",
    ] {
        validate_model(ok).unwrap_or_else(|e| panic!("ValidateModel({ok:?}) = {e:#}"));
    }
    let long = "a".repeat(129);
    for bad in [
        "",
        "-p",
        "--dangerously-skip-permissions",
        "a b",
        "$(id)",
        "x'y",
        "a\nb",
        long.as_str(),
    ] {
        assert!(
            validate_model(bad).is_err(),
            "ValidateModel({bad:?}) passed"
        );
    }
}

// A spawn over the bus carries --model; the serving host checks it as a
// local spawn does, and says in its response which model the agent runs on.
#[test]
fn test_rpc_spawn_model() {
    require_termd!();
    let env = setup_test_env();
    let bus = crate::require_bus!();
    let (_server, client) = serve_spawn(&env, &bus);
    register_fake_agy(&env);

    let cwd = tempdir();
    let resp = client
        .spawn(
            &bg(),
            &agent_bus::AgentRef::new("m", "h1"),
            agent_bus::SpawnRequest {
                backend: "agy".into(),
                cwd: cwd.path().display().to_string(),
                model: "gemini-3.8-flash-high".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Spawn: {e:#}"));
    assert_eq!(resp.model, "gemini-3.8-flash-high", "response model");
    assert_eq!(
        env.state_value("m", STATE_KEY_MODEL),
        "gemini-3.8-flash-high"
    );
    let st = client
        .status(&bg(), &agent_bus::AgentRef::new("m", "h1"))
        .unwrap();
    assert_eq!(st.model, "gemini-3.8-flash-high", "Status: want its model");

    for (model, want) in [
        ("x; touch /tmp/pwned", "invalid model"),
        ("opus", "backend catty takes no --model"),
    ] {
        let cwd = tempdir();
        let err = client
            .spawn(
                &bg(),
                &agent_bus::AgentRef::new("bad", "h1"),
                agent_bus::SpawnRequest {
                    backend: "catty".into(),
                    cwd: cwd.path().display().to_string(),
                    model: model.into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains(want),
            "spawn --model {model:?} over the bus: {err:#}, want {want:?}"
        );
    }
}

//! Go tests/service/resume_test.go.

use agent_backend::END_GONE;
use agent_service::*;
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, StateEntry, Time};

use crate::common::*;
use crate::require_termd;

fn spawn_agy(env: &TestEnv, name: &str, spawned_by: &str) {
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: name.into(),
                backend: "agy".into(),
                spawned_by: spawned_by.into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn {name}: {e:#}"));
}

// A reboot (or a killed tmux server) takes every agent's session with it and
// fires no hook: an agent whose last report is idle is ended (gone).
#[test]
fn test_status_no_tmux_session_is_ended_gone() {
    require_termd!();
    let env = setup_test_env();
    register_fake_agy(&env);
    spawn_agy(&env, "ghost", "");
    env.service
        .report(
            "ghost",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        env.service.status_info("ghost").unwrap().status,
        AGENT_STATUS_IDLE,
        "status with its session"
    );

    env.tmux.kill_session(&env.session_name("ghost")).unwrap();
    let info = env.service.status_info("ghost").unwrap();
    assert!(
        info.status == AGENT_STATUS_ENDED && info.end_reason == END_GONE,
        "status without its session = {} ({}), want ended (gone)",
        info.status,
        info.end_reason.as_str()
    );
    assert!(
        info.detail.contains("agent resume ghost"),
        "detail {:?} doesn't say how to resume it",
        info.detail
    );

    let err = env
        .service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "ghost".into(),
                backend: "agy".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("agent resume ghost"),
        "spawning over the ended agent: {err:#}, want a refusal pointing at agent resume"
    );
}

#[test]
fn test_resume_relaunches_ended_agent_in_place() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/gomuxai/agent".into();
    let resumed_fake = register_fake_agy(&env);

    spawn_agy(&env, "lead", "boss");
    let old_spawn_id = env.state_value("lead", STATE_KEY_SPAWN_ID);
    env.store
        .create_agent(&Agent {
            name: "lead-h".into(),
            session_name: env.session_name("lead-h"),
            backend: "agy".into(),
            spawned_by: "lead".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.service
        .report(
            "lead",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                result: "old reply".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let now = Time::now();
    env.store
        .set_states(&[StateEntry {
            agent_name: "lead".into(),
            key: "notes".into(),
            value: "keep me".into(),
            updated_at: now,
        }])
        .unwrap();
    env.store
        .create_message(&mut Message {
            sender_id: "boss".into(),
            receiver_id: "lead".into(),
            content: "are you there?".into(),
            status: MESSAGE_STATUS_PENDING,
            created_at: now,
            ..Default::default()
        })
        .unwrap();

    env.tmux.kill_session(&env.session_name("lead")).unwrap();
    let agent = env
        .service
        .resume(
            &bg(),
            ResumeOptions {
                name: "lead".into(),
                session_id: "conv-123".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Resume: {e:#}"));

    assert!(
        agent.spawned_by == "boss" && agent.backend == "agy",
        "resumed agent = {agent:?}, want boss's agy agent still"
    );
    assert!(
        env.tmux.session_exists(&env.session_name("lead")).unwrap(),
        "resume started no session"
    );
    let launched = resumed_fake
        .lock()
        .unwrap()
        .as_ref()
        .map(|f| f.launched_cmd());
    assert!(
        launched
            .as_deref()
            .is_some_and(|c| c.contains("--conversation 'conv-123'")),
        "launched {launched:?}, want agy --conversation conv-123"
    );
    for (key, want) in [
        (STATE_KEY_SESSION_ID, "conv-123"),
        (STATE_KEY_RESULT, "old reply"),
        ("notes", "keep me"),
        (STATE_KEY_STATUS, ""),
        (STATE_KEY_END_REASON, ""),
    ] {
        assert_eq!(env.state_value("lead", key), want, "{key} after resume");
    }
    let id = env.state_value("lead", STATE_KEY_SPAWN_ID);
    assert!(
        !id.is_empty() && id != old_spawn_id,
        "spawn id after resume = {id:?}, want a new one (was {old_spawn_id:?})"
    );
    let info = env.service.status_info("lead").unwrap();
    assert_ne!(
        info.status, AGENT_STATUS_ENDED,
        "status after resume: want it no longer ended"
    );
    assert_eq!(
        env.delivering(),
        vec!["lead".to_string()],
        "deliverers started, want one for lead's held message"
    );
    let h = env.store.get_agent("lead-h").unwrap();
    assert!(
        h.is_some_and(|h| h.spawned_by == "lead"),
        "helper after resume, want still lead's"
    );

    let tree = env.store.read_tree(true).unwrap();
    let ended: Vec<String> = tree
        .ended
        .iter()
        .map(|r| format!("{}:{}:{}", r.name, r.status, r.end_reason))
        .collect();
    assert_eq!(
        ended,
        vec!["lead:ended:gone".to_string()],
        "ended runs, want lead's first run, ended (gone)"
    );
}

#[test]
fn test_resume_refuses_live_agent() {
    require_termd!();
    let env = setup_test_env();
    register_fake_agy(&env);
    spawn_agy(&env, "busy", "");
    env.service
        .report(
            "busy",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                ..Default::default()
            },
        )
        .unwrap();
    let err = env
        .service
        .resume(
            &bg(),
            ResumeOptions {
                name: "busy".into(),
                session_id: "conv-1".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("not ended"),
        "Resume of a processing agent: {err:#}, want a refusal"
    );
    assert!(
        env.tmux.session_exists(&env.session_name("busy")).unwrap(),
        "a refused resume killed the live agent's session"
    );
}

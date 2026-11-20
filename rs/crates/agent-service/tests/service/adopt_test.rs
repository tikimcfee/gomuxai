//! Go tests/service/adopt_test.go.

use std::collections::HashMap;
use std::sync::Arc;

use agent_backend::{END_QUIT, PaneSession, Status};
use agent_base::await_;
use agent_service::*;

use crate::common::*;
use crate::require_termd;

struct KillOnDrop(Arc<agent_termd::Client>, String);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill_session(&self.1);
    }
}

fn user_session(env: &TestEnv, suffix: &str) -> (String, KillOnDrop) {
    let s = format!("{}{suffix}", env.cfg().session_prefix);
    env.tmux
        .create_session(&s, &HashMap::new())
        .unwrap_or_else(|e| panic!("CreateSession: {e:#}"));
    (s.clone(), KillOnDrop(Arc::clone(&env.tmux), s))
}

#[test]
fn test_adopt_existing_shell_and_cleanup_leaves_session_running() {
    require_termd!();
    let env = setup_test_env();
    let (user, _k) = user_session(&env, "user-dev");

    let res = env
        .service
        .adopt(AdoptOptions {
            name: "dev-box".into(),
            session_name: user.clone(),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("Adopt: {e:#}"));
    assert!(
        res.agent.adopted && res.agent.session_name == user && res.agent.backend == "shell",
        "Adopt agent = {:?}, want Adopted=true SessionName={user} Backend=shell",
        res.agent
    );

    // Reject adopting the same session under a second agent name.
    let err = env
        .service
        .adopt(AdoptOptions {
            name: "dev-box-2".into(),
            session_name: user.clone(),
            ..Default::default()
        })
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("already registered as agent dev-box"),
        "duplicate session Adopt err = {err:#}"
    );

    // Cleanup unregisters the agent without killing the user's pre-existing session.
    env.service.cleanup("dev-box").unwrap();
    assert!(
        env.tmux.session_exists(&user).unwrap(),
        "after Cleanup of adopted agent, the session should still exist"
    );
}

#[test]
fn test_adopt_live_claude_in_existing_session() {
    require_termd!();
    let env = setup_test_env();
    let (user, _k) = user_session(&env, "claude-pane");

    let fake = FakeHook::default();
    fake.set(|s| {
        s.session_id = "claude-sess-77".into();
        s.native_found = true;
        s.native = Status::Idle;
        s.pane_session = Some(PaneSession {
            id: "claude-sess-77".into(),
            status: Status::Idle,
            detail: "idle".into(),
            pid: 0,
        });
    });
    register_claude_fake_as(
        &env.backends,
        "claude",
        &Arc::new(fake_hook_overrides("fakehooks", &fake)),
    );

    let res = env
        .service
        .adopt(AdoptOptions {
            name: "adopted-claude".into(),
            session_name: user,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("Adopt: {e:#}"));
    assert!(
        res.agent.backend == "claude" && res.agent.adopted && res.session_id == "claude-sess-77",
        "Adopt = {res:?}, want claude session claude-sess-77"
    );
    assert_eq!(
        env.state_value("adopted-claude", STATE_KEY_SESSION_ID),
        "claude-sess-77"
    );
    let info = env.service.status_info("adopted-claude").unwrap();
    assert_eq!(
        info.status, AGENT_STATUS_IDLE,
        "StatusInfo = {info:?}; want idle"
    );
}

// An adopted agent runs in the person's own session: resume refuses it,
// typing nothing into their shell and leaving the session as it was.
#[test]
fn test_adopt_resume_refuses_adopted_agent() {
    require_termd!();
    let mut env = setup_test_env();
    let user = format!("{}keep-me", env.cfg().session_prefix);
    env.tmux.create_session(&user, &HashMap::new()).unwrap();
    let _k = KillOnDrop(Arc::clone(&env.tmux), user.clone());
    env.tmux.send_keys(&user, "echo person-was-here").unwrap();
    env.tmux.send_special_key(&user, "Enter").unwrap();
    let before = await_::until("the person's command to print", || {
        let out = env.tmux.capture(&user, 20).unwrap_or_default();
        let ok = out.split('\n').any(|l| l == "person-was-here");
        (out, ok)
    });

    let get_resumed = register_fake_agy(&env);
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    env.service
        .adopt(AdoptOptions {
            name: "adopted-agy".into(),
            session_name: user.clone(),
            backend: "agy".into(),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("Adopt: {e:#}"));
    // The CLI in it quit (e.g. the person ran /exit).
    env.service
        .report(
            "adopted-agy",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                end_reason: END_QUIT,
                event: "SessionEnd".into(),
                ..Default::default()
            },
        )
        .unwrap();

    let err = env
        .service
        .resume(
            &bg(),
            ResumeOptions {
                name: "adopted-agy".into(),
                session_id: "conv-initial".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    let want = format!(
        "agent adopted-agy is an adopted session (session {user}): resume doesn't relaunch CLIs in sessions gomuxai didn't start; start the CLI there yourself (agy -c continues its last conversation), and adopt keeps tracking it"
    );
    assert_eq!(err.to_string(), want);
    assert!(
        get_resumed.lock().unwrap().is_none(),
        "a launch was built for the adopted agent"
    );
    // Its prompt may redraw (a git status in it), so the screen is checked
    // for what a launch would type, not compared whole.
    let after = env.tmux.capture(&user, 20).unwrap_or_default();
    for typed in ["launch.sh", "agy", "conv-initial", "AGENT_NAME"] {
        assert!(
            !after.contains(typed),
            "{typed:?} was typed into the person's session:\nbefore:\n{before}\nafter:\n{after}"
        );
    }
    assert!(
        !std::path::Path::new(&format!("{}/launch.sh", env.agent_dir("adopted-agy"))).exists(),
        "a launch script was written"
    );
    let ag = env.store.get_agent("adopted-agy").unwrap();
    assert!(
        ag.as_ref()
            .is_some_and(|a| a.adopted && a.session_name == user),
        "agent = {ag:?}; want its adopted record unchanged"
    );

    // With the person's session gone, resume still makes none of its own.
    env.tmux.kill_session(&user).unwrap();
    let err = env
        .service
        .resume(
            &bg(),
            ResumeOptions {
                name: "adopted-agy".into(),
                session_id: "conv-initial".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains(&format!("(session {user}, which is gone)"))
            && s.contains(
                "agent cleanup adopted-agy, then agent spawn adopted-agy --backend agy --continue"
            ),
        "Resume with the session gone: err = {s}"
    );
    assert!(
        !env.tmux
            .session_exists(&env.session_name("adopted-agy"))
            .unwrap(),
        "resume created a session of its own for the adopted agent"
    );
}

#[test]
fn test_adopt_shell_upgrades_to_claude_when_started_later() {
    require_termd!();
    let env = setup_test_env();
    let (user, _k) = user_session(&env, "shell-to-claude");

    let fake = FakeHook::default();
    register_claude_fake_as(
        &env.backends,
        "claude",
        &Arc::new(fake_hook_overrides("fakehooks", &fake)),
    );

    let res = env
        .service
        .adopt(AdoptOptions {
            name: "shell-later".into(),
            session_name: user,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("Adopt: {e:#}"));
    assert_eq!(res.agent.backend, "shell", "initial Backend");

    // Simulate starting Claude Code later in that adopted shell pane.
    fake.set(|s| {
        s.session_id = "claude-later-99".into();
        s.native_found = true;
        s.native = Status::Idle;
        s.pane_session = Some(PaneSession {
            id: "claude-later-99".into(),
            status: Status::Idle,
            detail: "idle".into(),
            pid: 0,
        });
    });

    let info = env.service.status_info("shell-later").unwrap();
    assert_eq!(
        info.status, AGENT_STATUS_IDLE,
        "StatusInfo after Claude started = {info:?}; want idle"
    );
    let updated = env.store.get_agent("shell-later").unwrap();
    assert!(
        updated.as_ref().is_some_and(|a| a.backend == "claude"),
        "stored agent after upgrade = {updated:?}; want Backend=claude"
    );
    assert_eq!(
        env.state_value("shell-later", STATE_KEY_SESSION_ID),
        "claude-later-99"
    );
}

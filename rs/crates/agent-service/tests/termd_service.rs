//! Go tests/termd/service_test.go's TestAgentServiceEndToEndOnTermd, and the
//! ClaudeBackend.InputText check at the end of tests/termd/client_test.go's
//! TestLargeBracketedPasteAndCaptureStyled. They need agent-service and
//! agent-backend over a real g3d-termd, so they live here rather than in
//! agent-termd (whose dev-dependencies can't reach agent-service without a
//! cycle).

#[path = "service/common.rs"]
mod common;

use std::sync::Arc;

use agent_backend::InputReader;
use agent_base::await_;
use agent_base::config::Config;
use agent_service::{AdoptOptions, AgentService, Context, SendOptions, SpawnOptions};
use agent_store::Store;
use agent_termd::Ops;
use agent_termd::testing;

use common::tempdir;

#[test]
fn test_agent_service_end_to_end_on_termd() {
    require_termd!();
    let _srv = testing::require();
    let tc = Arc::new(testing::new_test_client("agents-svc"));
    let ctx = Context::background();

    let root = tempdir();
    let bin_dir = root.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_agent_bin = bin_dir.join("agent");
    std::fs::write(&fake_agent_bin, "#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_agent_bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let data = root.path().join(".gomuxai");
    let p = |s: &str| data.join(s).to_string_lossy().into_owned();
    let cfg = Config {
        backend: "shell".into(),
        termd_namespace: "agents-svc".into(),
        database_path: p("gomuxai.db"),
        log_dir: p("logs"),
        state_dir: p("state"),
        session_prefix: "agent-".into(),
        default_scrollback: 200,
        startup_delay: std::time::Duration::ZERO,
        submit_wait: std::time::Duration::ZERO,
        agent_binary: fake_agent_bin.to_string_lossy().into_owned(),
        host: "test-host".into(),
        ..Default::default()
    };
    cfg.ensure_dirs().unwrap();
    let store = Arc::new(Store::open(&cfg.database_path).unwrap());
    let client = agent_termd::Client::new(agent_termd::Options {
        namespace: "agents-svc".into(),
        ..Default::default()
    })
    .unwrap();
    let svc = AgentService::new(
        cfg.clone(),
        store,
        Arc::new(client) as Arc<dyn Ops>,
        Arc::new(agent_backend::new_registry()),
    );

    let work_dir = tempdir();
    let agent = svc
        .spawn(
            &ctx,
            SpawnOptions {
                name: "helper".into(),
                backend: "shell".into(),
                cwd: work_dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Spawn on termd: {e:#}"));

    // The session exists in termd and carries AGENT_NAME and GOMUXAI_TERMD_NS.
    assert_eq!(tc.session_env(&agent.session_name, "AGENT_NAME"), "helper");
    assert_eq!(
        tc.session_env(&agent.session_name, "GOMUXAI_TERMD_NS"),
        "agents-svc"
    );

    // caller_agent walks process ancestors up to the termd terminal's root PID.
    let pane = tc.first_pane(&agent.session_name).expect("FirstPane");
    let caller = svc.caller_agent(&[999_999, pane.pid, 1]);
    assert!(
        matches!(&caller, Ok((name, true)) if name == "helper"),
        "CallerAgent = {caller:?}, want (helper, true)"
    );

    // Send pastes the text and presses Enter in the shell; Capture reads it back.
    svc.send(
        &ctx,
        "helper",
        "echo termd-svc-$((40+2))",
        SendOptions::default(),
    )
    .unwrap_or_else(|e| panic!("Send: {e:#}"));
    await_::until("termd-svc-42 in Capture", || {
        match svc.capture("helper", 50, false) {
            Ok(out) => {
                let ok = out.contains("termd-svc-42");
                (out, ok)
            }
            Err(e) => (format!("{e:#}"), false),
        }
    });

    // Adopt an existing termd terminal by its id (CurrentPane, $G3D_TERMD_ID),
    // and Cleanup leaves the adopted terminal running.
    let user_term = "dev-shell";
    tc.create_session_in(
        user_term,
        &work_dir.path().to_string_lossy(),
        &Default::default(),
    )
    .expect("CreateSessionIn(dev-shell)");
    let adopted = svc
        .adopt(AdoptOptions {
            name: "adopted-dev".into(),
            current_pane: user_term.into(),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("Adopt on termd: {e:#}"));
    assert!(
        adopted.agent.adopted && !adopted.agent.cwd.starts_with("file://"),
        "Adopt result = {:?}",
        adopted.agent
    );
    svc.cleanup("adopted-dev")
        .unwrap_or_else(|e| panic!("Cleanup(adopted-dev): {e:#}"));
    assert!(
        tc.session_exists(user_term).unwrap_or(false),
        "Cleanup killed an adopted termd terminal; must leave it running"
    );
    let _ = tc.kill_session(user_term);

    // Cleanup of the spawned agent snapshots its scrollback to its log and
    // kills its terminal.
    svc.cleanup("helper")
        .unwrap_or_else(|e| panic!("Cleanup(helper): {e:#}"));
    assert!(
        !tc.session_exists(&agent.session_name).unwrap_or(true),
        "spawned termd terminal still exists after Cleanup"
    );
    let log = std::fs::read_to_string(cfg.log_path("helper"));
    assert!(
        log.as_ref().is_ok_and(|l| l.contains("termd-svc-42")),
        "post-cleanup terminal log = {log:?}, want termd-svc-42"
    );
}

/// Styled capture keeps the dim SGR of a placeholder, so ClaudeBackend's
/// input_text reads the input line as empty rather than as the placeholder.
#[test]
fn test_capture_styled_claude_backend_input_text() {
    require_termd!();
    let _srv = testing::require();
    let tc = testing::new_test_client(agent_termd::DEFAULT_NAMESPACE);
    let sess = format!("styled-{}", std::process::id());
    tc.create_session(&sess, &Default::default()).unwrap();
    struct Kill<'a>(&'a agent_termd::Client, &'a str);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill_session(self.1);
        }
    }
    let _kill = Kill(&tc, &sess);

    let box_cmd = r#"printf '────────────────────\n❯ \033[2mTry "fix lint"\033[0m\n────────────────────\nDONE-%d\n' $((40+2))"#;
    tc.send_keys(&sess, box_cmd).unwrap();
    tc.send_special_key(&sess, "Enter").unwrap();
    let styled = await_::until("DONE-42 on its own line", || {
        let s = tc.capture_styled(&sess, 0).unwrap_or_default();
        let ok = s.lines().any(|l| l.trim_end() == "DONE-42");
        (s, ok)
    });
    assert!(
        !styled.contains("\r\n"),
        "capture_styled must normalize \\r\\n to \\n: {styled:?}"
    );
    // The box sits at the bottom once the lines from DONE-42 on are cut off.
    let idx = styled
        .find("DONE-42")
        .expect("DONE-42 in the styled capture");
    let input = agent_backend::namedagents::new_claude_backend().input_text(&styled[..idx]);
    assert_eq!(
        input.as_deref(),
        Some(""),
        "ClaudeBackend input_text on a dim placeholder; screen:\n{:?}",
        &styled[..idx]
    );
}

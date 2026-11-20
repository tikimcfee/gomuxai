//! Go: internal/cli/adopt_test.go (TestSenderIsTheAgentTheProcessesShow is
//! send's, group B's).

use std::collections::HashMap;

use super::testenv::TestEnv;
use super::{adopt, cleanup, resume};

pub fn adopt_args(name: &str, session: &str) -> adopt::Args {
    adopt::Args {
        session: session.into(),
        args: vec![name.into()],
    }
}

pub fn resume_args(name: &str) -> resume::Args {
    resume::Args {
        all: false,
        attach: false,
        dry_run: false,
        skip_permissions: false,
        trust_workspace: false,
        args: vec![name.into()],
    }
}

/// Makes this test's commands run, as far as caller_agent can tell, in a
/// process under pid (a pane's process, say) (Go: standUnder). The Go's
/// leave() is stand_under(env, None): the real process tree again.
pub fn stand_under(env: &TestEnv, pid: Option<i32>) {
    match pid {
        Some(pid) => env
            .ctx
            .set_caller_chain(move || Ok(vec![std::process::id() as i32, pid])),
        None => env
            .ctx
            .set_caller_chain(|| agent_base::util::proc_ancestors(std::process::id() as i32)),
    }
}

#[test]
fn test_adopt_and_cleanup_preserves_session() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    env.set_env("AGENT_NAME", "");
    let user_sess = format!("rpc-cli-user-{}", std::process::id());
    env.termd_ops()
        .create_session(&user_sess, &HashMap::new())
        .unwrap();
    env.set_env("G3D_TERMD_ID", &user_sess);

    adopt::run(&env.ctx, &adopt_args("my-adopted", "")).unwrap_or_else(|e| panic!("adopt: {e:#}"));
    let out = env.take_out();
    assert!(
        out.contains("Adopted agent my-adopted") && out.contains(&user_sess),
        "adopt output = {out:?}, want Adopted agent my-adopted and session {user_sess}"
    );

    let agent = env
        .store()
        .get_agent("my-adopted")
        .unwrap()
        .expect("stored");
    assert!(
        agent.adopted && agent.session_name == user_sess,
        "stored agent = {agent:?}"
    );

    // A process under the adopted session's pane is the adopted agent's,
    // even without AGENT_NAME set.
    let pane = env.termd_ops().first_pane(&user_sess).expect("first pane");
    stand_under(&env, Some(pane.pid));
    assert_eq!(env.ctx.caller_agent(), "my-adopted");
    stand_under(&env, None);

    // Cleaning up the adopted agent (from outside its pane) leaves its session running.
    env.set_env("G3D_TERMD_ID", "");
    cleanup::run(
        &env.ctx,
        &cleanup::Args {
            dry_run: false,
            ended: false,
            args: vec!["my-adopted".into()],
        },
    )
    .unwrap_or_else(|e| panic!("cleanup: {e:#}"));
    let clean_out = env.take_out();
    assert!(
        clean_out.contains(&format!("left session {user_sess} running")),
        "cleanup output = {clean_out:?}"
    );
    assert!(
        env.termd_ops().session_exists(&user_sess).unwrap_or(false),
        "adopted session {user_sess} was killed by cleanup"
    );
}

#[test]
fn test_adopt_requires_termd_session() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    env.set_env("G3D_TERMD_ID", "");
    let err = adopt::run(&env.ctx, &adopt_args("no-termd", "")).expect_err("no termd session");
    assert!(
        format!("{err:#}").contains("not in a termd session"),
        "err = {err:#}"
    );
}

// agent resume refuses an adopted agent before it looks for a session or
// enters a directory: its session is the person's own.
#[test]
fn test_resume_refuses_adopted_agent() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    env.set_env("AGENT_NAME", "");
    env.set_env("G3D_TERMD_ID", "");
    let user_sess = format!("rpc-cli-user-resume-{}", std::process::id());
    env.termd_ops()
        .create_session(&user_sess, &HashMap::new())
        .unwrap();
    let mut a = adopt_args("mine", &user_sess);
    a.session = user_sess.clone();
    // Go: backendName = "claude".
    env.ctx.globals.backend = "claude".into();
    adopt::run(&env.ctx, &a).unwrap();
    let wd = std::env::current_dir().unwrap();

    let err = resume::run(&env.ctx, &resume_args("mine")).expect_err("a refusal");
    let want = format!(
        "agent mine is an adopted session (session {user_sess}): resume doesn't relaunch CLIs in sessions gomuxai didn't start; start the CLI there yourself (claude -c continues its last conversation), and adopt keeps tracking it"
    );
    assert_eq!(format!("{err:#}"), want);
    assert_eq!(std::env::current_dir().unwrap(), wd, "resume moved");
    assert!(
        env.termd_ops().session_exists(&user_sess).unwrap_or(false),
        "the person's session is gone"
    );
}

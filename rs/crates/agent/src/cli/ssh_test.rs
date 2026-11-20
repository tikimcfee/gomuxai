//! Go: internal/cli/ssh_test.go and termd_attach_test.go.
//!
//! Not ported here: TestAttachLocal_ExecsG3DUnderTermd (a stand-in termd
//! attacher: test_attach_local_execs_g3d covers the exec through the real
//! client's attach command) and TestTermdLogsAndExportDeps (logs and
//! export, group C's commands).

use agent_store::CommandFilter;

use super::attach::{self, gone_attach_error};
use super::exec::ExecCall;
use super::resume;
use super::root::Ctx;
use super::ssh::{self, ssh_dest_for};
use super::testenv::{TestEnv, parse_args};

fn call(path: &str, argv: &[&str]) -> ExecCall {
    ExecCall {
        path: path.into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
    }
}

fn attach_args(target: &str) -> attach::Args {
    parse_args("attach", &[target])
}

/// Serves h1 (every op) with a service whose agent binary is bin (Go:
/// serveAsH1, then cfg.AgentBinary = bin).
fn serve_h1_with_binary(env: &mut TestEnv, bin: &str) -> bool {
    let bin = bin.to_string();
    let svc = env.service_with(move |c| c.agent_binary = bin);
    let mut ops = vec![agent_bus::OP_KEYS];
    ops.extend_from_slice(agent_bus::DEFAULT_REMOTE_OPS);
    env.serve_svc_as("h1", svc, &ops).is_some()
}

// agent attach name@host runs ssh to the host's destination, running the
// binary the host names in its list reply, quoted, with attach <name>.
#[test]
fn test_attach_remote_runs_ssh() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    if !serve_h1_with_binary(&mut env, "/home/me/.local/bin/agent") {
        eprintln!("skipping: no agent-busd");
        return;
    }
    let calls = env.stub_exec();

    attach::run(&env.ctx, &attach_args("worker@h1")).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(
        *calls.lock().unwrap(),
        vec![call(
            "/usr/bin/ssh",
            &[
                "ssh",
                "-t",
                "--",
                "h1",
                "'/home/me/.local/bin/agent' attach 'worker'"
            ]
        )]
    );
    let hints = env.take_err();
    assert!(
        hints.contains(r#"it needs "ssh h1" to work from here"#),
        "no word that this is ssh, not the bus: {hints:?}"
    );

    // A destination set for the host replaces its name.
    ssh::run_bus_ssh(&env.ctx, &parse_args("bus ssh", &["h1", "me@h1.lan"])).unwrap();
    calls.lock().unwrap().clear();
    attach::run(&env.ctx, &attach_args("worker@h1")).unwrap();
    {
        let c = calls.lock().unwrap();
        assert!(
            c.len() == 1 && c[0].argv[3] == "me@h1.lan",
            "exec = {c:?}, want ssh to me@h1.lan"
        );
    }

    // resume --attach runs that host's own resume --attach.
    calls.lock().unwrap().clear();
    resume::run(&env.ctx, &parse_args("resume", &["worker@h1", "--attach"])).unwrap();
    {
        let c = calls.lock().unwrap();
        assert!(
            c.len() == 1 && c[0].argv[4] == "'/home/me/.local/bin/agent' resume 'worker' --attach",
            "exec = {c:?}, want ssh running resume 'worker' --attach"
        );
    }
    // The permission flags are still refused for another host's agent.
    let err = resume::run(
        &env.ctx,
        &parse_args("resume", &["worker@h1", "--attach", "--skip-permissions"]),
    )
    .expect_err("refused");
    assert!(
        format!("{err:#}").contains("--skip-permissions works only for an agent on this host"),
        "resume --attach --skip-permissions: {err:#}"
    );
}

// Attach over ssh refuses, without running ssh: no terminal to hand over,
// an agent the host doesn't have, a host that doesn't say where its binary
// is (older), or a path that isn't safe to put on a command line.
#[test]
fn test_attach_remote_refusals() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    let calls = env.stub_exec();

    for (name, binary, agent, want, terminal) in [
        (
            "no terminal",
            "/opt/agent",
            "worker",
            "needs one: run it in a terminal (agent capture worker@h1",
            false,
        ),
        (
            "no such agent",
            "/opt/agent",
            "ghost",
            r#"agent "ghost" not found on host h1 (1 agent there: agent list --host h1)"#,
            true,
        ),
        (
            "older host",
            "",
            "worker",
            "h1's agent predates attach over ssh and doesn't say where its binary is",
            true,
        ),
        (
            "unsafe path",
            "/opt/my agent;rm -rf ~",
            "worker",
            "won't run h1's agent over ssh",
            true,
        ),
        (
            "relative path",
            "bin/agent",
            "worker",
            "isn't an absolute path",
            true,
        ),
    ] {
        if !serve_h1_with_binary(&mut env, binary) {
            eprintln!("skipping: no agent-busd");
            return;
        }
        env.ctx.attach_terminal = Some(terminal);
        let err = attach::run(&env.ctx, &attach_args(&format!("{agent}@h1"))).expect_err("refused");
        assert!(
            format!("{err:#}").contains(want),
            "{name}: err = {err:#}, want {want:?}"
        );
    }
    assert!(
        calls.lock().unwrap().is_empty(),
        "ran {:?} despite refusing",
        calls.lock().unwrap()
    );
}

// Local attach becomes g3d rather than running it as a child, and the
// command-log row is written first, since nothing runs after.
#[test]
fn test_attach_local_execs_g3d() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.forbid_bus();
    let calls = env.stub_exec();
    let (bin, argv) = match env.termd_ops().attach_command("rpc-cli-worker") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: {e:#}");
            return;
        }
    };
    assert_eq!(&argv[..2], ["g3d", "--ns"]);
    assert_eq!(&argv[3..], ["attach", "--take", "rpc-cli-worker"]);

    attach::run(&env.ctx, &attach_args("worker")).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(*calls.lock().unwrap(), vec![ExecCall { path: bin, argv }]);
    let rows = env
        .store()
        .list_commands(&CommandFilter::default())
        .unwrap();
    assert!(
        env.ctx
            .run_recorded
            .load(std::sync::atomic::Ordering::Relaxed)
            && !rows.is_empty(),
        "{} command-log rows: want the run written before the exec",
        rows.len()
    );
}

#[test]
fn test_bus_ssh() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();
    let show = || {
        ssh::run_bus_ssh(&env.ctx, &parse_args("bus ssh", &["hub"])).unwrap();
        env.take_out()
    };
    assert_eq!(show(), "hub: ssh to hub (the default: its host name)\n");
    ssh::run_bus_ssh(&env.ctx, &parse_args("bus ssh", &["hub", "me@hub.lan"])).unwrap();
    env.take_out();
    assert_eq!(show(), "hub: ssh to me@hub.lan\n");
    assert_eq!(ssh_dest_for(&env.ctx, "hub"), "me@hub.lan");
    ssh::run_bus_ssh(&env.ctx, &parse_args("bus ssh", &["hub", "--clear"])).unwrap();
    env.take_out();
    assert_eq!(ssh_dest_for(&env.ctx, "hub"), "hub", "after --clear");

    // Never something ssh would read as an option, or a second word.
    for bad in ["-oProxyCommand=sh", "me@hub lan", "hub;ls", "", "hub\n"] {
        let args = ssh::BusSshArgs {
            clear: false,
            args: vec!["hub".into(), bad.into()],
        };
        assert!(
            ssh::run_bus_ssh(&env.ctx, &args).is_err(),
            "destination {bad:?} accepted"
        );
    }
    let err =
        ssh::run_bus_ssh(&env.ctx, &parse_args("bus ssh", &["local", "x"])).expect_err("this host");
    assert!(
        format!("{err:#}").contains("is this host"),
        "this host: {err:#}"
    );
}

#[test]
fn test_validate_agent_binary() {
    for (path, ok) in [
        ("/home/me/.local/bin/agent", true),
        ("/Users/me/src/gomuxai/bin/agent", true),
        ("/opt/gomux-1.2_rc+3/agent", true),
        ("bin/agent", false),
        ("", false),
        ("/opt/my agent", false),
        ("/opt/agent';rm -rf ~;'", false),
        ("/opt/$(id)/agent", false),
        ("/opt/`id`", false),
        ("/opt/agent\nrm", false),
        ("/opt/agent\x1b]0;x\x07", false),
        ("/home/me/agent\"", false),
        ("/home/é/agent", false),
        ("/home/me/../me/.local/bin/agent", true), // quoted; the shell doesn't treat .. specially
        ("/home/me/.local/bin/agent --no-bus", false),
    ] {
        assert_eq!(
            agent_bus::validate_agent_binary(path).is_ok(),
            ok,
            "validate_agent_binary({path:?})"
        );
    }
}

// Attach to an agent whose session is gone (a reboot ended it) says so, here
// or over ssh, instead of handing g3d a session it can't find; resume
// --attach over ssh still runs, since it starts the session first.
#[test]
fn test_attach_session_gone() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    if !serve_h1_with_binary(&mut env, "/opt/agent") {
        eprintln!("skipping: no agent-busd");
        return;
    }
    let calls = env.stub_exec();
    env.termd_ops().kill_session("rpc-cli-worker").unwrap();

    for target in ["worker", "worker@h1"] {
        let err = format!(
            "{:#}",
            attach::run(&env.ctx, &attach_args(target)).expect_err("gone")
        );
        for want in [
            format!("{target} has ended (gone)"),
            "its session rpc-cli-worker is gone".to_string(),
            format!("agent logs {target}"),
        ] {
            assert!(
                err.contains(&want),
                "attach {target}: {err}, want it to say {want:?}"
            );
        }
    }
    assert!(
        calls.lock().unwrap().is_empty(),
        "exec = {:?}, want none for a gone session",
        calls.lock().unwrap()
    );

    resume::run(&env.ctx, &parse_args("resume", &["worker@h1", "--attach"]))
        .unwrap_or_else(|e| panic!("{e:#}"));
    let c = calls.lock().unwrap();
    assert!(
        c.len() == 1 && c[0].path == "/usr/bin/ssh",
        "resume --attach exec = {c:?}, want ssh"
    );
}

// An agent resume can continue gets the command that brings it back.
#[test]
fn test_gone_attach_error_resumable() {
    let err = gone_attach_error("bot@mac", "agent-bot", "gone", true);
    assert_eq!(
        err.to_string(),
        "bot@mac has ended (gone): its session agent-bot is gone, so there is nothing to attach to. agent resume bot@mac --attach starts it again on its last session and opens it"
    );
    let err = gone_attach_error("bot", "agent-bot", "", false).to_string();
    assert!(
        !err.contains("()") && err.contains("bot has ended: "),
        "no reason: {err:?}"
    );
}

// termd_attach_test.go

#[test]
fn test_setup_refuses_reserved_termd_namespace() {
    let mut guard = agent_termd::testing::EnvGuard::lock();
    for reserved in ["default", "wall"] {
        guard.set("GOMUXAI_TERMD_NS", reserved);
        for path in ["list", "adopt"] {
            let (mut ctx, _, _) = Ctx::for_test();
            let err = ctx
                .setup(&super::root::command(path))
                .expect_err("a reserved namespace");
            assert!(
                format!("{err:#}").contains("reserved"),
                "setup({path}) with GOMUXAI_TERMD_NS={reserved} = {err:#}, want reserved refusal"
            );
        }
    }
}

//! Go: internal/cli/bus_test.go, and a test of bus run (the Go had none: its
//! bus ran in-process; this one runs agent-busd).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::cli::friction_links::friction_links_test::NoTerm;
use crate::cli::testenv::{TestEnv, busd_binary};

/// Puts the bus session's name back when the test ends.
pub struct SessionName;

impl SessionName {
    pub fn set(name: &str) -> SessionName {
        set_test_bus_session(Some(name));
        SessionName
    }
}

impl Drop for SessionName {
    fn drop(&mut self) {
        set_test_bus_session(None);
    }
}

fn default_start_args() -> BusStartArgs {
    BusStartArgs {
        client_port: 7420,
        listen: String::new(),
        remote_ops: ["list", "status", "capture", "send", "respond", "friction", "logs"]
            .map(String::from)
            .to_vec(),
        args: Vec::new(),
    }
}

// The bus session's name is the same for every data dir, so stop and start
// must leave a session alone when this data dir's bus isn't the one in it.
#[test]
fn bus_stop_leaves_another_data_dirs_bus() {
    crate::require_termd!();
    let env = TestEnv::with_termd();
    let _name = SessionName::set("rpc-cli-gomux-bus");
    env.termd_ops()
        .create_session(&bus_session(), &HashMap::new())
        .unwrap();
    env.no_bus();

    let runs: [(&str, &dyn Fn() -> anyhow::Result<()>); 2] = [
        ("stop", &|| run_bus_stop(&env.ctx, &BusStopArgs { args: Vec::new() })),
        ("start", &|| run_bus_start(&env.ctx, &default_start_args())),
    ];
    for (name, run) in runs {
        let err = run().err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(
            err.contains("another agent binary"),
            "bus {name}: err = {err:?}, want a refusal naming another binary's bus"
        );
        assert!(
            env.termd_ops().session_exists(&bus_session()).unwrap(),
            "bus {name} killed the session"
        );
    }
}

// Status with no bus says how to get one: init first, then start.
#[test]
fn bus_status_without_a_bus() {
    let env = TestEnv::new();
    env.ctx.set_termd(Arc::new(NoTerm));
    env.no_bus();
    bus_status(&env.ctx, false).unwrap();
    assert_eq!(env.take_out(), "local: bus not running\n");
    assert_eq!(
        env.take_err(),
        "hint: set it up first: agent bus init (see agent bus --help)\n"
    );

    let env = TestEnv::new();
    env.ctx.set_termd(Arc::new(NoTerm));
    env.no_bus();
    run_bus_init(
        &env.ctx,
        &BusInitArgs {
            url: String::new(),
            args: Vec::new(),
        },
    )
    .unwrap();
    let out = env.take_out();
    assert!(out.starts_with("Created bus identity for host \"local\" in "), "{out}");
    assert!(
        run_bus_init(&env.ctx, &BusInitArgs { url: String::new(), args: Vec::new() })
            .is_err_and(|e| format!("{e:#}").contains("already has a bus identity")),
        "init twice"
    );
    bus_status(&env.ctx, false).unwrap();
    assert_eq!(env.take_out(), "local: bus not running\n");
    assert_eq!(
        env.take_err(),
        "hint: start it: agent bus start (add --listen 0.0.0.0:7422 on the hub)\n"
    );
    bus_status(&env.ctx, true).unwrap();
    let v: serde_json::Value = serde_json::from_str(&env.take_out()).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"host": "local", "set_up": true, "running": false, "linked_peers": 0,
            "trusted": [], "remote_ops": [], "ssh": {}})
    );
}

// bus run starts agent-busd, answers the status subject, serves the RPC ops,
// and stops on its signal, taking agent-busd with it.
#[test]
fn bus_run_serves_until_stopped() {
    let Some(busd) = busd_binary() else {
        eprintln!("skipping: no agent-busd (go isn't installed)");
        return;
    };
    let env = TestEnv::new();
    env.ctx.set_termd(Arc::new(NoTerm));
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    let cfg = env.ctx.cfg().clone();
    agent_bus::init_identity(cfg.bus_dir(), "local").unwrap();
    let args = BusRunArgs {
        client_port: -1,
        listen: String::new(),
        remote_ops: vec!["list".into(), "status".into()],
        args: Vec::new(),
    };
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let ctx = &env.ctx;
        let run = s.spawn(|| serve_bus(ctx, &args, Some(busd), &|| stop.load(Ordering::SeqCst)));
        let deadline = Instant::now() + Duration::from_secs(10);
        let info = loop {
            match query_bus(ctx, Duration::from_secs(1)) {
                Ok(info) => break info,
                Err(e) if Instant::now() > deadline => panic!("the bus never answered: {e:#}"),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        assert_eq!(info.host, "local");
        assert!(info.client_url.starts_with("tls://127.0.0.1:"), "{info:?}");
        assert_eq!(info.client_port, -1);
        assert_eq!(info.remote_ops, ["list", "status"]);
        assert!(info.trusted.is_empty());

        // Its RPC ops answer for this host.
        let nc = ctx.bus_connect().unwrap();
        let agents = agent_bus::RpcClient::new(nc, Duration::from_secs(5))
            .list(&agent_bus::Context::background(), "local")
            .expect("list over the bus");
        assert!(agents.is_empty());

        bus_status(ctx, false).unwrap();
        stop.store(true, Ordering::SeqCst);
        run.join().unwrap().expect("bus run");
    });
    let out = env.take_out();
    assert!(out.starts_with("agent bus for \"local\": clients on tls://127.0.0.1:"), "{out}");
    assert!(out.contains("; trusted peers: \n"), "{out}");
    assert!(out.contains("local: bus running (clients on tls://127.0.0.1:"), "{out}");
    assert!(out.contains("  remote ops allowed: list, status\n"), "{out}");
    assert!(out.ends_with("agent bus stopping\n"), "{out}");
    let url = std::path::Path::new(&cfg.bus_dir()).join(agent_bus::BUS_URL_FILE);
    assert!(!url.exists(), "bus.url left behind");
    assert!(
        query_bus(&env.ctx, Duration::from_secs(1)).is_err_and(|e| agent_bus::is_no_bus(&e)),
        "the bus still answers"
    );
}

#[test]
fn bus_run_command_line() {
    let env = TestEnv::new();
    let mut cfg = env.ctx.cfg().clone();
    agent_bus::init_identity(cfg.bus_dir(), "local").unwrap();
    cfg.agent_binary = "/opt/my agent/agent".into();
    let ctx = Ctx::new(Box::new(std::io::sink()), Box::new(std::io::sink()));
    ctx.set_cfg(cfg);
    let s = BusSettings {
        listen: "0.0.0.0:7422".into(),
        client_port: 7420,
        remote_ops: vec!["list".into(), "send".into()],
    };
    assert_eq!(
        bus_run_command(&ctx, "local", &s).unwrap(),
        "env AGENT_HOST='local' '/opt/my agent/agent' bus run --client-port 7420 --remote-ops 'list,send' --listen '0.0.0.0:7422'"
    );
    let e = bus_run_command(&ctx, "elsewhere", &s).unwrap_err();
    assert!(agent_bus::is_no_identity(&e) || format!("{e:#}").contains("elsewhere"), "{e:#}");
}

/// Short timing for wait_for_bus's own tests.
fn quick_timing() -> BusWaitTiming {
    BusWaitTiming {
        timeout: Duration::from_millis(600),
        note_after: Duration::from_millis(200),
        unseen_grace: Duration::from_millis(300),
        poll: Duration::from_millis(10),
    }
}

// The wait returns as soon as the bus answers, says once that it's still
// starting when it goes past note_after, and tells a bus that exited (seen
// running, then not; done, as the screen shows; or never seen within the
// grace) from one that still runs, or one whose processes can't be read, at
// the timeout.
#[test]
fn wait_for_bus_outcomes() {
    use std::cell::Cell;
    let t = quick_timing();
    let long = BusWaitTiming { unseen_grace: Duration::from_secs(5), timeout: Duration::from_secs(5), ..t };

    // Up at once: no note.
    let notes = Cell::new(0);
    let got = wait_for_bus(t, &|| true, &|| BusProc::Running, &mut || notes.set(notes.get() + 1));
    assert_eq!((got, notes.get()), (BusWait::Up, 0));

    // Up after note_after: one note, however many polls pass after it.
    let start = Instant::now();
    let got = wait_for_bus(
        BusWaitTiming { timeout: Duration::from_secs(5), ..t },
        &|| start.elapsed() > Duration::from_millis(450),
        &|| BusProc::Running,
        &mut || notes.set(notes.get() + 1),
    );
    assert_eq!((got, notes.get()), (BusWait::Up, 1));
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());

    // Seen running, then gone: exited, at once (well before the grace).
    let polls = Cell::new(0);
    let start = Instant::now();
    let got = wait_for_bus(
        long,
        &|| false,
        &|| {
            polls.set(polls.get() + 1);
            if polls.get() < 3 { BusProc::Running } else { BusProc::NotSeen }
        },
        &mut || {},
    );
    assert_eq!(got, BusWait::Exited);
    assert!(start.elapsed() < Duration::from_millis(200), "{:?}", start.elapsed());

    // Never seen running, but done (the shell is back at its prompt): exited,
    // at once.
    let polls = Cell::new(0);
    let start = Instant::now();
    let got = wait_for_bus(
        long,
        &|| false,
        &|| {
            polls.set(polls.get() + 1);
            if polls.get() < 3 { BusProc::NotSeen } else { BusProc::Done }
        },
        &mut || {},
    );
    assert_eq!(got, BusWait::Exited);
    assert!(start.elapsed() < Duration::from_millis(200), "{:?}", start.elapsed());

    // Never seen: exited once the grace has passed, not before.
    let start = Instant::now();
    let got = wait_for_bus(t, &|| false, &|| BusProc::NotSeen, &mut || {});
    assert_eq!(got, BusWait::Exited);
    let took = start.elapsed();
    assert!(took >= t.unseen_grace && took < t.timeout, "{took:?}");

    // Never seen, but up by the time it's found gone: up.
    let got = wait_for_bus(t, &{
        let start = Instant::now();
        move || start.elapsed() >= Duration::from_millis(300)
    }, &|| BusProc::NotSeen, &mut || {});
    assert_eq!(got, BusWait::Up);

    // Still running at the timeout: still starting, after one note.
    notes.set(0);
    let start = Instant::now();
    let got = wait_for_bus(t, &|| false, &|| BusProc::Running, &mut || notes.set(notes.get() + 1));
    assert_eq!((got, notes.get()), (BusWait::StillStarting, 1));
    assert!(start.elapsed() >= t.timeout);

    // Can't tell: waits the whole timeout, and says it can't tell.
    let got = wait_for_bus(t, &|| false, &|| BusProc::Unknown, &mut || {});
    assert_eq!(got, BusWait::NoAnswer);
}

// The shell is back at its prompt after the command: its echo line, after
// the prompt, is new since it was typed, and the prompt alone is last. Not
// while it runs, nor before the shell has read it, nor on a restarted
// session's last run, nor with a prompt that changed.
#[test]
fn shell_back_after_the_command() {
    let run = "env AGENT_HOST='hub' '/opt/agent' bus run --client-port 7420";
    let before = "[me@box work]$ ";
    let ran = format!("{run}\n[me@box work]$ {run}\nError: listen tcp: address already in\nuse\n[me@box work]$\n\n");
    assert!(shell_back_after(&ran, before, run));
    assert!(shell_back_after(&ran, "", run));

    let running = format!("[me@box work]$ {run}\nagent bus for \"hub\": clients on ...\n");
    assert!(!shell_back_after(&running, before, run));
    let unread = format!("{run}\n");
    assert!(!shell_back_after(&unread, "", run));
    let echoed = format!("[me@box work]$ {run}\n");
    assert!(!shell_back_after(&echoed, before, run));
    // Restart: the screen still shows the last run, ended; the new one isn't
    // read yet.
    assert!(!shell_back_after(&ran, &ran, run));
    let restarted = format!("{ran}[me@box work]$ {run}\nError: x\n[me@box work]$\n");
    assert!(shell_back_after(&restarted, &ran, run));
    // A prompt that changed (an exit status in it): not recognized.
    let changed = format!("0 $ {run}\nError: x\n1 $\n");
    assert!(!shell_back_after(&changed, "", run));
    assert!(!shell_back_after(&ran, before, ""));
}

// agent-busd's own wait for its ready file (the macOS launch) gives up
// before bus start's does, so start shows its error rather than a timeout.
#[test]
fn bus_start_waits_longer_than_busd() {
    assert!(agent_bus::BUSD_READY_TIMEOUT < BUS_START_TIMEOUT);
    assert!(BUS_START_TIMEOUT >= Duration::from_secs(30));
    assert_eq!(BusWaitTiming::default().note_after, Duration::from_secs(5));
}

#[test]
fn bus_run_args_match() {
    let words = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert!(is_bus_run_args(&words("env AGENT_HOST=local /opt/agent bus run --client-port 7420")));
    assert!(is_bus_run_args(&words("/opt/agent bus run")));
    assert!(!is_bus_run_args(&words("/opt/agent bus status")));
    assert!(!is_bus_run_args(&words("-zsh")));
    assert!(!is_bus_run_args(&words("agent-busd --dir /x --host local")));
}

// run_bus_in in a real termd session, with stand-ins for agent bus run (a
// command line with the words "bus run"): one that fails after a moment, as
// a taken port does, is reported at once with the session's output, saying
// it exited; one that keeps starting is reported, after one "still
// starting..." line, as still starting, with how to see whether it came up.
#[test]
fn run_bus_in_tells_exited_from_still_starting() {
    crate::require_termd!();
    let env = TestEnv::with_termd();
    let _name = SessionName::set("wait-cli-gomux-bus");
    env.no_bus();
    let termd = env.termd_ops();

    termd.create_session(&bus_session(), &HashMap::new()).unwrap();
    let start = Instant::now();
    let err = run_bus_in(
        &env.ctx,
        "local",
        "sh -c 'sleep 1; echo listen tcp 127.0.0.1:7422: address already in use; exit 1' bus run",
    )
    .unwrap_err();
    let took = start.elapsed();
    let err = format!("{err:#}");
    assert!(took < Duration::from_secs(4), "a failing bus took {took:?}: {err}");
    assert!(
        err.starts_with("the bus exited in session wait-cli-gomux-bus without coming up; session output:\n"),
        "{err}"
    );
    assert!(err.contains("\nlisten tcp 127.0.0.1:7422: address already in use"), "{err}");
    assert_eq!(env.take_err(), "");

    termd.respawn_pane(&bus_session()).unwrap();
    let timing = BusWaitTiming {
        timeout: Duration::from_secs(3),
        note_after: Duration::from_secs(1),
        ..BusWaitTiming::default()
    };
    // (": done" keeps sh from exec'ing sleep, which would drop "bus run".)
    let err = run_bus_in_with(&env.ctx, "local", "sh -c 'echo starting; sleep 60; : done' bus run", timing)
        .unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.starts_with("the bus hasn't answered within 3s, but it is still starting in session wait-cli-gomux-bus: it may yet come up (agent bus status shows whether it has); session output:\n"),
        "{err}"
    );
    assert!(err.contains("\nstarting"), "{err}");
    assert_eq!(env.take_err(), "still starting...\n");
    assert_eq!(bus_runs_in(termd.as_ref(), &bus_session()), Some(true));
    termd.kill_session(&bus_session()).unwrap();
    assert_eq!(bus_runs_in(termd.as_ref(), &bus_session()), Some(false));

    // A bad flag exits in milliseconds, between two looks for the process:
    // the shell back at its prompt after it says it ran (well before the 5s
    // grace for a prompt that changes between commands).
    termd.create_session(&bus_session(), &HashMap::new()).unwrap();
    let start = Instant::now();
    let err = run_bus_in(&env.ctx, "local", "sh -c 'echo unknown flag --bogus; exit 2' bus run")
        .unwrap_err();
    let took = start.elapsed();
    let err = format!("{err:#}");
    assert!(took < Duration::from_secs(4), "a bad flag took {took:?}: {err}");
    assert!(err.starts_with("the bus exited in session wait-cli-gomux-bus"), "{err}");
    assert!(err.contains("\nunknown flag --bogus"), "{err}");
}

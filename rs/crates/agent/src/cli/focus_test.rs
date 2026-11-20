//! focus's cases: set, show and clear here; another host's; --follow over
//! the bus (a change made on the other host) and without one; cleanup
//! clearing it.
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_bus::FocusEvent;
use agent_store::{Agent, Time};

use crate::cli::fleet::fleet_test::TwoHosts;
use crate::cli::root::{CtxShare, SharedBuf, run_words};
use crate::cli::testenv::TestEnv;
use crate::cli::watch::{INTERRUPT_TESTS, interrupt_until};

/// Runs a command line on another thread, over share, until its notice
/// ("... Ctrl-C stops.") is out.
fn start(share: CtxShare, words: &[&str]) -> (SharedBuf, SharedBuf, JoinHandle<anyhow::Result<()>>) {
    let (out, err) = (SharedBuf::default(), SharedBuf::default());
    let (o, e) = (out.clone(), err.clone());
    let line: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    let h = std::thread::spawn(move || {
        let mut ctx = share.ctx(o.writer(), e.writer());
        let words: Vec<&str> = line.iter().map(String::as_str).collect();
        run_words(&mut ctx, &words)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !err.contents().contains("Ctrl-C stops.") {
        assert!(!h.is_finished(), "returned early: {}", err.contents());
        assert!(Instant::now() < deadline, "no notice: {:?}", err.contents());
        std::thread::sleep(Duration::from_millis(10));
    }
    (out, err, h)
}

/// Waits until out has n lines, and returns them.
fn lines(out: &SharedBuf, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = out.contents();
        let l: Vec<String> = s.lines().map(str::to_string).collect();
        if l.len() >= n {
            return l;
        }
        assert!(Instant::now() < deadline, "want {n} lines, have {s:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn live(name: &str, session: &str) -> Agent {
    Agent {
        name: name.into(),
        session_name: session.into(),
        backend: "claude".into(),
        created_at: Time::now(),
        ..Default::default()
    }
}

#[test]
fn test_focus_set_show_clear() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.no_bus();

    let (out, res) = env.run(&["focus"]);
    res.unwrap();
    assert_eq!(out, "local: nothing has focus\n");

    let (out, res) = env.run(&["focus", "worker"]);
    res.unwrap();
    eprintln!("agent focus worker:\n{out}");
    assert!(
        out.starts_with("local: worker (session rpc-cli-worker), set by user at "),
        "{out}"
    );
    assert_eq!(
        env.take_err(),
        "hint: no agent bus runs here, so no follower was told (agent fleet and agent focus read it all the same)\n"
    );
    let (again, res) = env.run(&["focus"]);
    res.unwrap();
    assert_eq!(again, out, "focus shows what was set");
    let (out, res) = env.run(&["focus", "--json"]);
    res.unwrap();
    let ev: FocusEvent = serde_json::from_str(&out).unwrap();
    assert_eq!((ev.host.as_str(), ev.agent.as_str(), ev.by.as_str()), ("local", "worker", "user"));

    for (words, want) in [
        (vec!["focus", "--clear", "worker"], "give an agent to focus, or --clear, not both"),
        (vec!["focus", "worker@h2"], "focus is set only on its own host: run agent focus worker on h2"),
        (vec!["focus", "worker", "--host", "h2"], "focus is set only on its own host: run agent focus on h2"),
        (vec!["focus", "--clear", "--host", "h2"], "focus is set only on its own host: run agent focus --clear on h2"),
        (vec!["focus", "worker", "--follow"], "--follow only shows focus: set it first, then follow"),
    ] {
        let (_, res) = env.run(&words);
        assert_eq!(format!("{:#}", res.unwrap_err()), want, "{words:?}");
    }
    let (_, res) = env.run(&["focus", "nobody"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("nobody"), "{err}");
    let (out, res) = env.run(&["focus"]);
    res.unwrap();
    assert!(out.starts_with("local: worker "), "refusals changed focus: {out}");

    let (out, res) = env.run(&["focus", "--clear"]);
    res.unwrap();
    assert!(out.starts_with("local: nothing has focus, cleared by user at "), "{out}");
    assert!(agent_fleet::focus::get(env.store()).unwrap().is_none());
}

// review-4 W5: setting focus is the store's and the bus's only: it works with
// no termd, and an unknown name still gets close names.
#[test]
fn test_focus_set_without_termd() {
    let mut env = TestEnv::new();
    env.no_bus();
    env.store().create_agent(&live("worker", "rpc-cli-worker")).unwrap();
    let (out, res) = env.run(&["focus", "worker"]);
    res.unwrap_or_else(|e| panic!("focus worker without termd: {e:#}"));
    // Who set it isn't this test's: TestEnv::new doesn't isolate the caller,
    // and another test's AGENT_NAME can be in the environment for a moment.
    assert!(out.starts_with("local: worker (session rpc-cli-worker), set by "), "{out}");
    let (_, res) = env.run(&["focus", "workr"]);
    assert_eq!(
        format!("{:#}", res.unwrap_err()),
        "agent \"workr\" not found on host local; did you mean worker?"
    );
}

// review-4 W3: a focus change the bus didn't take (its bus went away after the
// connection was made, so the publish only buffered and the flush failed) is
// still made, and says it wasn't published.
#[test]
fn test_focus_unpublished_says_so() {
    let mut env = TestEnv::new();
    let Some(bus) = crate::cli::testenv::TestBus::start() else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    let nc = bus.connect();
    let crate::cli::testenv::TestBus { daemon, dir: _dir, conn, .. } = bus;
    drop(daemon);
    conn.close();
    env.ctx.set_bus_connect(move || Ok(nc.clone()));
    env.store().create_agent(&live("worker", "rpc-cli-worker")).unwrap();

    let (out, res) = env.run(&["focus", "worker"]);
    res.unwrap_or_else(|e| panic!("focus worker: {e:#}"));
    assert!(out.starts_with("local: worker "), "{out}");
    let hints = env.take_err();
    assert!(
        hints.starts_with("hint: the change is stored, but wasn't published on the bus: flushing focus events: "),
        "{hints:?}"
    );
    assert_eq!(agent_fleet::focus::get(env.store()).unwrap().unwrap().agent, "worker");
}

// review-4 W4: another host's focus, when that host answers nothing, isn't put
// down to an older agent; one that answers list but not fleet is.
#[test]
fn test_focus_remote_hint_only_for_a_host_that_answers() {
    crate::require_termd!();
    let Some(mut h) = TwoHosts::new(false) else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    h.hub.take_err();
    let (_, res) = h.hub.run(&["focus", "--host", "gone"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("no bus answering for host \"gone\""), "{err}");
    assert_eq!(h.hub.take_err(), "", "no hint for a host that answers nothing");
    let (_, res) = h.hub.run(&["focus", "--host", "old"]);
    res.unwrap_err();
    assert_eq!(
        h.hub.take_err(),
        "hint: old serves focus only if its bus allows fleet (agent bus status there shows what it allows; agent list --host old lists its agents)\n"
    );
}

// Cleaning up the focused agent clears focus; cleaning up another leaves it.
#[test]
fn test_cleanup_clears_focus() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.no_bus();
    env.create_live_agent(&live("other", "rpc-cli-other"));
    env.run(&["focus", "worker"]).1.unwrap();

    let (out, res) = env.run(&["cleanup", "other"]);
    res.unwrap();
    assert!(!out.contains("focus"), "{out}");
    assert_eq!(agent_fleet::focus::get(env.store()).unwrap().unwrap().agent, "worker");

    let (out, res) = env.run(&["cleanup", "worker"]);
    res.unwrap();
    assert!(
        out.starts_with("cleaned up \"worker\": killed session rpc-cli-worker; removed its record, state, queued messages, and files; its log stays (agent logs worker); it had focus, so nothing has now\n"),
        "{out}"
    );
    assert!(agent_fleet::focus::get(env.store()).unwrap().is_none());
}

// The leaf follows the hub's focus: the current value, then each change made
// on the hub, as it is made; and the hub's own follower sees them as JSON.
#[test]
fn test_focus_follow_across_hosts() {
    crate::require_termd!();
    let _serial = INTERRUPT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut h) = TwoHosts::new(false) else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    h.hub.create_live_agent(&live("lead", "rpc-cli-lead"));

    // Another host's focus, shown.
    let (out, _, res) = h.leaf.run(&["focus", "--host", "hub"]);
    res.unwrap();
    assert_eq!(out, "hub: nothing has focus\n");

    let (out, err, follow) = start(h.leaf.ctx.share().unwrap(), &["focus", "--follow", "--host", "hub"]);
    assert_eq!(err.contents(), "Following focus on host hub. Ctrl-C stops.\n");
    let (jout, _, jfollow) = start(h.hub.ctx.share().unwrap(), &["focus", "--follow", "--json"]);
    assert_eq!(lines(&out, 1), ["hub: nothing has focus"]);
    assert_eq!(lines(&jout, 1), ["{\"host\":\"hub\",\"agent\":\"\"}"]);

    h.hub.run(&["focus", "lead"]).1.unwrap();
    let got = lines(&out, 2);
    assert!(got[1].starts_with("hub: lead (session rpc-cli-lead), set by user at "), "{got:?}");
    h.hub.run(&["focus", "--clear"]).1.unwrap();
    let got = lines(&out, 3);
    assert!(got[2].starts_with("hub: nothing has focus, cleared by user at "), "{got:?}");
    eprintln!("agent focus --follow --host hub (on leaf):\n{}", out.contents());

    let got = lines(&jout, 3);
    let ev: FocusEvent = serde_json::from_str(&got[1]).unwrap();
    assert_eq!((ev.host.as_str(), ev.agent.as_str(), ev.session.as_str()), ("hub", "lead", "rpc-cli-lead"));
    let ev: FocusEvent = serde_json::from_str(&got[2]).unwrap();
    assert_eq!(ev.agent, "");

    interrupt_until(|| follow.is_finished() && jfollow.is_finished());
    follow.join().unwrap().expect("follow's result");
    jfollow.join().unwrap().expect("follow --json's result");

    // review-4 D2, over the bus: following from after a clear, another
    // clear still shows, who cleared it and when.
    let (out, _, follow) = start(h.hub.ctx.share().unwrap(), &["focus", "--follow"]);
    assert_eq!(lines(&out, 1), ["hub: nothing has focus"]);
    h.hub.run(&["focus", "--clear"]).1.unwrap();
    let got = lines(&out, 2);
    assert!(got[1].starts_with("hub: nothing has focus, cleared by user at "), "{got:?}");
    interrupt_until(|| follow.is_finished());
    follow.join().unwrap().expect("follow's result");
    // The leaf's focus is its own: setting the hub's from there is refused.
    let (_, _, res) = h.leaf.run(&["focus", "lead@hub"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("run agent focus lead on hub"));
}

// With no bus, --follow reads the store again: a change still shows.
#[test]
fn test_focus_follow_without_bus() {
    crate::require_termd!();
    let _serial = INTERRUPT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut env = TestEnv::setup_cli();
    env.no_bus();
    let (out, err, follow) = start(env.ctx.share().unwrap(), &["focus", "--follow"]);
    assert_eq!(
        err.contents(),
        "Following focus on host local (no agent bus here: reading the store every second). Ctrl-C stops.\n"
    );
    assert_eq!(lines(&out, 1), ["local: nothing has focus"]);
    env.run(&["focus", "worker"]).1.unwrap();
    let got = lines(&out, 2);
    assert!(got[1].starts_with("local: worker (session rpc-cli-worker), set by user at "), "{got:?}");
    // review-4 D2: a clear shows as over the bus, who cleared it and when,
    // and so does a clear when nothing had focus.
    env.run(&["focus", "--clear"]).1.unwrap();
    let got = lines(&out, 3);
    assert!(got[2].starts_with("local: nothing has focus, cleared by user at "), "{got:?}");
    env.run(&["focus", "--clear"]).1.unwrap();
    let got = lines(&out, 4);
    assert!(got[3].starts_with("local: nothing has focus, cleared by user at "), "{got:?}");
    interrupt_until(|| follow.is_finished());
    follow.join().unwrap().expect("follow's result");

    // Another host's needs the bus.
    let (_, res) = env.run(&["focus", "--follow", "--host", "hub"]);
    let e = format!("{:#}", res.unwrap_err());
    assert!(e.contains("reaching other hosts needs a running agent bus"), "{e}");
}

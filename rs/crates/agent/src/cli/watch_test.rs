//! `agent watch` run as the CLI runs it: what it says it watches, the hint
//! when another watcher runs, the lock it holds while it runs, and stopping
//! on Ctrl-C.

use std::time::{Duration, Instant};

use crate::cli::root::SharedBuf;
use crate::cli::testenv::TestEnv;

use super::*;

/// Runs watch (interval 20ms) on another thread over env's store and
/// service, until its banner is out; returns its output buffers and thread.
fn start_watch(
    env: &TestEnv,
) -> (SharedBuf, SharedBuf, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (out, err) = (SharedBuf::default(), SharedBuf::default());
    let share = env.ctx.share().unwrap();
    let (o, e) = (out.clone(), err.clone());
    let h = std::thread::spawn(move || {
        let ctx = share.ctx(o.writer(), e.writer());
        run(
            &ctx,
            &Args {
                interval: Duration::from_millis(20),
                args: Vec::new(),
            },
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !out.contents().contains("Ctrl-C stops.") {
        assert!(!h.is_finished(), "watch returned early: {}", err.contents());
        assert!(Instant::now() < deadline, "no banner: {:?}", out.contents());
        std::thread::sleep(Duration::from_millis(10));
    }
    (out, err, h)
}

fn stop_watch(h: std::thread::JoinHandle<anyhow::Result<()>>) {
    interrupt_until(|| h.is_finished());
    h.join().unwrap().expect("watch's result");
}

#[test]
fn watch_says_what_it_watches_and_stops_on_interrupt() {
    crate::require_termd!();
    let _serial = INTERRUPT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::setup_cli();
    env.service().queue("lead", "worker", "later").unwrap();

    assert_eq!(watcher_running(&env.ctx), (false, true), "a watcher before watch ran");
    let (out, err, h) = start_watch(&env);
    // Running, it holds the lock that tells queue a watcher runs.
    assert_eq!(watcher_running(&env.ctx), (true, true), "no watcher while watch runs");
    // worker waits on a prompt: its message stays queued.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(env.store().get_pending_messages("worker", 10).unwrap().len(), 1);
    stop_watch(h);
    assert_eq!(
        out.contents(),
        "Watching 1 agent on host local every 20ms; 1 message queued. Ctrl-C stops.\n\nWatcher stopped\n"
    );
    assert_eq!(err.contents(), "", "a hint with no other watcher");
    assert_eq!(watcher_running(&env.ctx), (false, true), "the lock outlived watch");
}

#[test]
fn watch_hints_when_another_watcher_runs() {
    crate::require_termd!();
    let _serial = INTERRUPT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::with_termd();
    let other = hold_watch_lock(&env.ctx).unwrap();
    let (out, err, h) = start_watch(&env);
    stop_watch(h);
    assert_eq!(
        out.contents(),
        "Watching 0 agents on host local every 20ms; 0 messages queued. Ctrl-C stops.\n\nWatcher stopped\n"
    );
    assert_eq!(
        err.contents(),
        "hint: another watcher is already running for this data dir; one is enough\n"
    );
    drop(other);
}

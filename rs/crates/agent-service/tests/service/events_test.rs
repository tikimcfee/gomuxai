//! Go tests/service/events_test.go.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Once, mpsc};
use std::time::Duration;

use agent_bus::{StatusEvent, Time as BusTime};
use agent_service::*;
use agent_store::Time;

use crate::common::*;
use crate::require_termd;
use crate::trust_test::observe;

const EVENTS_HOST: &str = "evthost";

/// setupEventsEnv is setup_test_env with a bus host name and evt- sessions.
fn setup_events_env() -> TestEnv {
    let mut env = setup_test_env();
    env.cfg_mut().host = EVENTS_HOST.into();
    env.cfg_mut().session_prefix += "evt-";
    env
}

/// failingPublisher stands in for a bus that is down or misbehaving.
#[derive(Default)]
struct FailingPublisher {
    calls: Arc<AtomicI32>,
}

impl EventPublisher for FailingPublisher {
    fn publish_status(&self, _: &StatusEvent) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("bus exploded")
    }
}

/// A publisher shared with the test, so it can flush it.
struct Shared(Arc<agent_bus::Publisher>);

impl EventPublisher for Shared {
    fn publish_status(&self, ev: &StatusEvent) -> anyhow::Result<()> {
        self.0.publish_status(ev)
    }
}

#[test]
fn test_report_publishes_status_event() {
    require_termd!();
    let mut env = setup_events_env();
    let bus = crate::require_bus!();
    spawn_fake(&env, "worker");

    let mut sub = bus
        .conn
        .subscribe(&agent_bus::subject_status(EVENTS_HOST, "worker"))
        .unwrap();
    bus.conn.flush().unwrap();
    let publisher = Arc::new(bus.publisher());
    env.svc_mut()
        .set_event_publisher(Some(Box::new(Shared(Arc::clone(&publisher)))));

    let before = Time::now();
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_WAITING,
                event: "PermissionRequest".into(),
                detail: "permission: Bash".into(),
                tool: "Bash".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Report: {e:#}"));
    publisher
        .flush(Duration::from_secs(1))
        .unwrap_or_else(|e| panic!("Flush: {e:#}"));

    let msg = sub
        .next_msg(Duration::from_secs(2))
        .unwrap_or_else(|e| panic!("no event: {e:#}"));
    let ev = agent_bus::decode_status_event(&msg).unwrap();
    let want = StatusEvent {
        host: EVENTS_HOST.into(),
        agent: "worker".into(),
        status: "waiting".into(),
        event: "PermissionRequest".into(),
        detail: "permission: Bash".into(),
        tool: "Bash".into(),
        ..Default::default()
    };
    let mut got = ev.clone();
    got.at = BusTime::default();
    assert_eq!(got, want);
    let at = ev.at.0.timestamp_nanos_opt().unwrap();
    assert!(
        at >= before.unix_nano() - 1_000_000_000 && at <= Time::now().unix_nano(),
        "event time {:?} not around the report",
        ev.at.0
    );
}

#[test]
fn test_report_publish_failure_does_not_fail_report() {
    require_termd!();
    let mut env = setup_events_env();
    let bus = crate::require_bus!();
    spawn_fake(&env, "worker");

    // A publisher that errors, and one whose connection has died.
    let calls = Arc::new(AtomicI32::new(0));
    let closed = bus.publisher();
    let _ = closed.close();

    let publishers: Vec<(&str, Box<dyn EventPublisher>)> = vec![
        (
            "failing",
            Box::new(FailingPublisher {
                calls: Arc::clone(&calls),
            }),
        ),
        ("closed", Box::new(closed)),
    ];
    for (name, publisher) in publishers {
        env.svc_mut().set_event_publisher(Some(publisher));
        env.service
            .report(
                "worker",
                StatusReport {
                    status: AGENT_STATUS_IDLE,
                    event: name.into(),
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("{name} publisher: Report failed: {e:#}"));
        let info = env.service.status_info("worker").unwrap();
        assert!(
            info.status == AGENT_STATUS_IDLE && info.event == name,
            "{name} publisher: status = {}/{}, want the report recorded",
            info.status,
            info.event
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "failing publisher calls");
}

// Hook events that carry no status change record nothing, so they must not
// open a bus connection either: hooks run on every agent event.
#[test]
fn test_report_hook_ignored_event_does_not_touch_bus() {
    require_termd!();
    let mut env = setup_events_env();
    spawn_fake(&env, "worker");

    let connects = Arc::new(AtomicI32::new(0));
    let c = Arc::clone(&connects);
    env.svc_mut()
        .set_event_publisher(Some(Box::new(agent_bus::Publisher::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Err(agent_bus::ErrNoBus.into())
        }))));
    env.service
        .report_hook(
            "worker",
            "",
            "",
            br#"{"hook_event_name":"SubagentStart","session_id":"s"}"#,
        )
        .unwrap_or_else(|e| panic!("ReportHook: {e:#}"));
    assert_eq!(
        connects.load(Ordering::SeqCst),
        0,
        "connected for an ignored hook event"
    );
}

// End to end: wait on a local agent while its hooks report.
#[test]
fn test_wait_local_agent_reports_idle() {
    require_termd!();
    let mut env = setup_events_env();
    let bus = crate::require_bus!();
    let (ctx, _cancel) = Context::with_timeout(&bg(), Duration::from_secs(5));
    spawn_fake(&env, "worker");
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                event: "UserPromptSubmit".into(),
                ..Default::default()
            },
        )
        .unwrap();
    env.svc_mut()
        .set_event_publisher(Some(Box::new(bus.publisher())));

    // The hook fires once wait has subscribed and checked (it subscribes
    // first), so only the event can end the wait.
    let (checked_tx, checked_rx) = mpsc::channel::<()>();
    let once = Once::new();
    let status = std::thread::scope(|s| {
        let svc = &env.service;
        let reporter = s.spawn(move || {
            let _ = checked_rx.recv();
            svc.report_hook(
                "worker",
                "",
                "",
                br#"{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"done"}"#,
            )
        });
        let status = agent_bus::wait(
            &ctx,
            Some(&bus.conn),
            agent_bus::WaitOptions {
                host: EVENTS_HOST.into(),
                agent: "worker".into(),
                for_statuses: vec!["idle".into()],
                interval: Duration::from_secs(3600),
                current: Some(Box::new(|_| {
                    let obs = observe(&env, "worker");
                    once.call_once(|| {
                        let _ = checked_tx.send(());
                    });
                    obs
                })),
                fail_statuses: Vec::new(),
                next: false,
                after: BusTime::default(),
            },
        );
        reporter
            .join()
            .unwrap()
            .unwrap_or_else(|e| panic!("ReportHook: {e:#}"));
        status
    });
    assert_eq!(
        status.map_err(|(s, e)| format!("{s}: {e:#}")),
        Ok("idle".to_string())
    );
}

#[test]
fn test_wait_local_agent_without_bus() {
    require_termd!();
    let env = setup_events_env();
    let (ctx, _cancel) = Context::with_timeout(&bg(), Duration::from_secs(5));
    spawn_fake(&env, "worker");
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                ..Default::default()
            },
        )
        .unwrap();

    // The report comes once wait has checked, so its polling finds it.
    let (checked_tx, checked_rx) = mpsc::channel::<()>();
    let once = Once::new();
    let status = std::thread::scope(|s| {
        let svc = &env.service;
        let reporter = s.spawn(move || {
            let _ = checked_rx.recv();
            svc.report(
                "worker",
                StatusReport {
                    status: AGENT_STATUS_IDLE,
                    ..Default::default()
                },
            )
        });
        let status = agent_bus::wait(
            &ctx,
            None,
            agent_bus::WaitOptions {
                host: EVENTS_HOST.into(),
                agent: "worker".into(),
                for_statuses: vec!["idle".into()],
                interval: Duration::from_millis(20),
                current: Some(Box::new(|_| {
                    let obs = observe(&env, "worker");
                    once.call_once(|| {
                        let _ = checked_tx.send(());
                    });
                    obs
                })),
                fail_statuses: Vec::new(),
                next: false,
                after: BusTime::default(),
            },
        );
        reporter
            .join()
            .unwrap()
            .unwrap_or_else(|e| panic!("Report: {e:#}"));
        status
    });
    assert_eq!(
        status.map_err(|(s, e)| format!("{s}: {e:#}")),
        Ok("idle".to_string())
    );
}

// Send and a turn-starting report record the markers wait --next uses, so a
// turn that started after the send and ended before the wait still counts.
#[test]
fn test_wait_next_catches_turn_that_ended_before_wait() {
    require_termd!();
    let env = setup_events_env();
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "quick".into(),
                backend: "claude".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("Spawn: {e:#}"));
    let report = |r: StatusReport| {
        env.service
            .report("quick", r)
            .unwrap_or_else(|e| panic!("Report: {e:#}"))
    };
    report(StatusReport {
        status: AGENT_STATUS_IDLE,
        event: "Stop".into(),
        ..Default::default()
    }); // the previous turn
    env.service
        .send(&bg(), "quick", "say hi", SendOptions::default())
        .unwrap_or_else(|e| panic!("Send: {e:#}"));
    report(StatusReport {
        status: AGENT_STATUS_PROCESSING,
        event: "UserPromptSubmit".into(),
        clear_result: true,
        ..Default::default()
    });
    report(StatusReport {
        status: AGENT_STATUS_IDLE,
        event: "Stop".into(),
        result: "hi".into(),
        ..Default::default()
    });

    let info = env.service.status_info("quick").unwrap();
    assert!(
        info.sent_at.before(&info.turn_at) && info.turn_at.before(&info.reported_at),
        "want sent < turn < reported, got {:?}, {:?}, {:?}",
        info.sent_at.0,
        info.turn_at.0,
        info.reported_at.0
    );

    let (wait_ctx, _cancel) = Context::with_timeout(&bg(), Duration::from_secs(2));
    let status = agent_bus::wait(
        &wait_ctx,
        None,
        agent_bus::WaitOptions {
            host: String::new(),
            agent: "quick".into(),
            for_statuses: vec!["idle".into()],
            next: true,
            current: Some(Box::new(|_| observe(&env, "quick"))),
            interval: Duration::from_millis(50),
            fail_statuses: Vec::new(),
            after: BusTime::default(),
        },
    );
    assert_eq!(
        status.map_err(|(s, e)| format!("{s}: {e:#}")),
        Ok("idle".to_string()),
        "want idle from the turn that already ended"
    );

    // A report that doesn't start a turn (e.g. the next tool call) moves
    // nothing.
    report(StatusReport {
        status: AGENT_STATUS_PROCESSING,
        event: "PreToolUse".into(),
        ..Default::default()
    });
    let again = env.service.status_info("quick").unwrap();
    assert!(
        again.turn_at.equal(&info.turn_at),
        "TurnAt moved on a report that starts no turn: {:?} -> {:?}",
        info.turn_at.0,
        again.turn_at.0
    );
}

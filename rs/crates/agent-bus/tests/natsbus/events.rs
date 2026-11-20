//! Port of `tests/natsbus/events_test.go`.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_bus::{
    Context, DEFAULT_FLUSH_TIMEOUT, ErrNoBus, Publisher, StatusEvent, Subscription, WaitOptions,
    connect_local, decode_status_event, is_deadline_exceeded, is_no_bus, publish_status,
    subject_all_status, wait,
};

use crate::common::{StatusSource, TEST_HOST, TestBus, publish, test_tempdir, wait_opts};

fn subscribe_status(nc: &agent_bus::Conn) -> Subscription {
    let sub = nc.subscribe(&subject_all_status(TEST_HOST)).unwrap();
    nc.flush().unwrap();
    sub
}

fn next_event(sub: &mut Subscription) -> StatusEvent {
    let msg = sub
        .next_msg(Duration::from_secs(2))
        .expect("no status event");
    decode_status_event(&msg).expect("decode status event")
}

fn test_ctx() -> Context {
    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_secs(5));
    ctx
}

fn tick() {
    std::thread::sleep(Duration::from_millis(2));
}

#[test]
fn test_publish_status_rejects_unsafe_names() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    for (host, agent) in [("", "a"), ("air.local", "a"), (TEST_HOST, "a.b")] {
        let ev = StatusEvent {
            host: host.to_string(),
            agent: agent.to_string(),
            status: "idle".to_string(),
            ..Default::default()
        };
        assert!(
            publish_status(&bus.conn, &ev).is_err(),
            "publish_status({ev:?}) succeeded, want error"
        );
    }
}

#[test]
fn test_publisher_connects_only_on_first_publish() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let mut sub = subscribe_status(&bus.conn);

    let connects = Arc::new(AtomicUsize::new(0));
    let connects_cl = Arc::clone(&connects);
    let bus_dir = bus.host.dir().to_path_buf();
    let bus_name = bus.host.name.clone();
    let p = Publisher::new(move || {
        connects_cl.fetch_add(1, Ordering::SeqCst);
        connect_local(&bus_dir, &bus_name)
    });

    p.flush(Duration::from_secs(1))
        .expect("Flush before any publish");
    assert_eq!(connects.load(Ordering::SeqCst), 0);

    for status in ["processing", "idle"] {
        p.publish_status(&StatusEvent {
            host: TEST_HOST.to_string(),
            agent: "w".to_string(),
            status: status.to_string(),
            ..Default::default()
        })
        .unwrap();
    }
    p.close().expect("Close");
    assert_eq!(connects.load(Ordering::SeqCst), 1);
    assert_eq!(next_event(&mut sub).status, "processing");
    assert_eq!(next_event(&mut sub).status, "idle");
}

#[test]
fn test_publisher_no_bus_is_tried_once() {
    let connects = Arc::new(AtomicUsize::new(0));
    let connects_cl = Arc::clone(&connects);
    let p = Publisher::new(move || {
        connects_cl.fetch_add(1, Ordering::SeqCst);
        Err(ErrNoBus.into())
    });
    for _ in 0..3 {
        let err = p
            .publish_status(&StatusEvent {
                host: TEST_HOST.to_string(),
                agent: "w".to_string(),
                status: "idle".to_string(),
                ..Default::default()
            })
            .unwrap_err();
        assert!(is_no_bus(&err), "err = {err}, want ErrNoBus");
    }
    assert_eq!(connects.load(Ordering::SeqCst), 1);
    p.close().expect("Close with no connection");
}

/// Transparent TCP proxy whose sockets can be shut down abruptly without TLS
/// close_notify or NATS-level close, matching Go's `killableDialer`.
struct KillableProxy {
    addr: std::net::SocketAddr,
    conns: Arc<Mutex<Vec<TcpStream>>>,
}

impl KillableProxy {
    fn start(target_addr: String) -> Self {
        let ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ln.local_addr().unwrap();
        let conns: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let conns_cl = Arc::clone(&conns);

        std::thread::spawn(move || {
            while let Ok((client, _)) = ln.accept() {
                let Ok(upstream) = TcpStream::connect(&target_addr) else {
                    break;
                };
                if let (Ok(c_clone), Ok(u_clone)) = (client.try_clone(), upstream.try_clone()) {
                    let mut guard = conns_cl.lock().unwrap();
                    guard.push(c_clone);
                    guard.push(u_clone);
                }
                let mut c_read = client.try_clone().unwrap();
                let mut u_write = upstream.try_clone().unwrap();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = c_read.read(&mut buf) {
                        if n == 0 || u_write.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = u_write.shutdown(Shutdown::Both);
                });
                let mut u_read = upstream;
                let mut c_write = client;
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = u_read.read(&mut buf) {
                        if n == 0 || c_write.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = c_write.shutdown(Shutdown::Both);
                });
            }
        });

        Self { addr, conns }
    }

    fn kill(&self) {
        for stream in self.conns.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

#[test]
fn test_publisher_flush_delivers_before_abrupt_exit() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let mut sub = subscribe_status(&bus.conn);

    let real_url = bus.daemon.client_url();
    let target_host_port = real_url
        .strip_prefix("tls://")
        .or_else(|| real_url.strip_prefix("nats://"))
        .unwrap()
        .to_string();
    let proxy = KillableProxy::start(target_host_port);

    // Clone the identity directory with bus.url pointing at the killable proxy.
    let proxy_dir = test_tempdir();
    for file in ["host", "host.nk", "local.nk", "tls.crt", "tls.key"] {
        let src = bus.host.dir().join(file);
        let dst = proxy_dir.path().join(file);
        std::fs::copy(&src, &dst).unwrap();
        std::fs::set_permissions(&dst, std::fs::metadata(&src).unwrap().permissions()).unwrap();
    }
    std::fs::write(
        proxy_dir.path().join("bus.url"),
        format!("tls://{}", proxy.addr),
    )
    .unwrap();

    let dir_path = proxy_dir.path().to_path_buf();
    let host_name = bus.host.name.clone();
    let p = Publisher::new(move || connect_local(&dir_path, &host_name));
    p.publish_status(&StatusEvent {
        host: TEST_HOST.to_string(),
        agent: "hooked".to_string(),
        status: "idle".to_string(),
        event: "Stop".to_string(),
        ..Default::default()
    })
    .unwrap();
    p.flush(DEFAULT_FLUSH_TIMEOUT).unwrap();
    proxy.kill(); // abrupt exit: socket torn down immediately

    let ev = next_event(&mut sub);
    assert_eq!(ev.agent, "hooked");
    assert_eq!(ev.status, "idle");
    assert_eq!(ev.event, "Stop");
}

#[test]
fn test_wait_returns_on_event() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let pub_conn = bus.connect_another();
    let src = StatusSource::new("processing");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        std::thread::spawn(move || {
            publish(&pub_conn, "other", "idle");
            publish(&pub_conn, "worker", "waiting");
            src_bg.set("idle");
            publish(&pub_conn, "worker", "idle");
        });
    });

    let start = Instant::now();
    let status = wait(
        &test_ctx(),
        Some(&bus.conn),
        wait_opts(&src, Duration::from_secs(3600), &["idle"]),
    )
    .expect("Wait");
    assert_eq!(status, "idle");
    assert!(start.elapsed() <= Duration::from_secs(2));
    assert_eq!(src.call_count(), 2);
}

#[test]
fn test_wait_unconfirmed_event_does_not_end_wait() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let pub_conn = bus.connect_another();
    let src = StatusSource::new("processing");
    src.set_on_first(move || {
        std::thread::spawn(move || {
            publish(&pub_conn, "worker", "idle"); // forged
        });
    });

    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_millis(2500));
    let res = wait(
        &ctx,
        Some(&bus.conn),
        wait_opts(&src, Duration::from_secs(3600), &["idle"]),
    );
    assert!(
        res.is_err(),
        "Wait returned {res:?} on an unconfirmed event"
    );
    assert!(src.call_count() >= 3, "calls = {}", src.call_count());
}

#[test]
fn test_wait_lagging_status_is_rechecked_soon() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let pub_conn = bus.connect_another();
    let src = StatusSource::new("processing");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        std::thread::spawn(move || {
            publish(&pub_conn, "worker", "idle");
            src_bg.await_calls(2);
            src_bg.set("idle");
        });
    });

    let start = Instant::now();
    let status = wait(
        &test_ctx(),
        Some(&bus.conn),
        wait_opts(&src, Duration::from_secs(3600), &["idle"]),
    )
    .expect("Wait");
    assert_eq!(status, "idle");
    assert!(start.elapsed() <= Duration::from_secs(3));
}

#[test]
fn test_wait_already_in_target_status() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let src = StatusSource::new("waiting");
    let status = wait(
        &test_ctx(),
        Some(&bus.conn),
        wait_opts(&src, Duration::from_secs(3600), &["idle", "waiting"]),
    )
    .unwrap();
    assert_eq!(status, "waiting");
}

#[test]
fn test_wait_catches_change_between_subscribe_and_check() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let pub_conn = bus.connect_another();
    let src = StatusSource::new("processing");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        src_bg.set("idle");
        publish(&pub_conn, "worker", "idle");
    });

    let status = wait(
        &test_ctx(),
        Some(&bus.conn),
        wait_opts(&src, Duration::from_secs(3600), &["idle"]),
    )
    .unwrap();
    assert_eq!(status, "idle");
}

#[test]
fn test_wait_backstop_catches_change_without_event() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let src = StatusSource::new("waiting");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        std::thread::spawn(move || {
            src_bg.set("idle");
        });
    });

    let status = wait(
        &test_ctx(),
        Some(&bus.conn),
        wait_opts(&src, Duration::from_millis(20), &["idle"]),
    )
    .unwrap();
    assert_eq!(status, "idle");
}

#[test]
fn test_wait_no_bus_polls() {
    let src = StatusSource::new("processing");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        std::thread::spawn(move || {
            src_bg.set("idle");
        });
    });

    let status = wait(
        &test_ctx(),
        None,
        wait_opts(&src, Duration::from_millis(20), &["idle"]),
    )
    .unwrap();
    assert_eq!(status, "idle");
}

#[test]
fn test_wait_timeout() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let src = StatusSource::new("processing");
    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_millis(100));

    let (last, err) = wait(
        &ctx,
        Some(&bus.conn),
        wait_opts(&src, Duration::from_millis(20), &["idle"]),
    )
    .unwrap_err();
    assert!(
        is_deadline_exceeded(&err),
        "err = {err:#}, want DeadlineExceeded"
    );
    assert_eq!(last, "processing");
}

#[test]
fn test_wait_fail_status_ends_wait() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let pub_conn = bus.connect_another();
    let src = StatusSource::new("processing");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        src_bg.set("exited");
        publish(&pub_conn, "worker", "exited");
    });

    let (last, _err) = wait(
        &test_ctx(),
        Some(&bus.conn),
        wait_opts(&src, Duration::from_secs(3600), &["idle"]),
    )
    .unwrap_err();
    assert_eq!(last, "exited");
}

#[test]
fn test_wait_current_error_ends_wait() {
    let (_, err) = wait(
        &test_ctx(),
        None,
        WaitOptions {
            host: String::new(),
            agent: "worker".to_string(),
            for_statuses: vec!["idle".to_string()],
            fail_statuses: Vec::new(),
            next: false,
            after: agent_bus::Time::zero(),
            current: Some(Box::new(|_| anyhow::bail!("agent \"worker\" not found"))),
            interval: Duration::from_secs(3600),
        },
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "agent \"worker\" not found");
}

#[test]
fn test_wait_next_waits_for_the_next_arrival() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let pub_conn = bus.connect_another();
    let src = StatusSource::new("idle");
    let came_back = Arc::new(AtomicBool::new(false));
    let came_back_bg = Arc::clone(&came_back);
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        std::thread::spawn(move || {
            publish(&pub_conn, "worker", "idle");
            src_bg.record("processing");
            publish(&pub_conn, "worker", "processing");
            src_bg.await_calls(2);
            came_back_bg.store(true, Ordering::SeqCst);
            src_bg.record("idle");
            publish(&pub_conn, "worker", "idle");
        });
    });

    let mut opts = wait_opts(&src, Duration::from_secs(3600), &["idle"]);
    opts.next = true;
    let status = wait(&test_ctx(), Some(&bus.conn), opts).unwrap();
    assert_eq!(status, "idle");
    assert!(came_back.load(Ordering::SeqCst));
}

#[test]
fn test_wait_next_armed_by_current_status() {
    let src = StatusSource::new("processing");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        std::thread::spawn(move || {
            src_bg.set("idle");
        });
    });

    let mut opts = wait_opts(&src, Duration::from_millis(20), &["idle"]);
    opts.next = true;
    let status = wait(&test_ctx(), None, opts).unwrap();
    assert_eq!(status, "idle");
}

#[test]
fn test_wait_next_armed_by_newer_report_between_checks() {
    let src = StatusSource::new("");
    src.record("idle");
    let src_bg = Arc::clone(&src);
    src.set_on_first(move || {
        std::thread::spawn(move || {
            tick();
            src_bg.record("idle");
        });
    });

    let mut opts = wait_opts(&src, Duration::from_millis(30), &["idle"]);
    opts.next = true;
    let status = wait(&test_ctx(), None, opts).unwrap();
    assert_eq!(status, "idle");
}

#[test]
fn test_wait_next_not_armed_by_same_report() {
    let src = StatusSource::new("");
    src.record("idle");
    let mut opts = wait_opts(&src, Duration::from_millis(20), &["idle"]);
    opts.next = true;

    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_millis(300));
    assert!(wait(&ctx, None, opts).is_err());
}

#[test]
fn test_wait_next_armed_by_turn_after_send() {
    for end in ["idle", "waiting"] {
        let src = StatusSource::new("");
        src.record("idle");
        tick();
        src.send();
        tick();
        src.start_turn();
        tick();
        src.record(end);

        let mut opts = wait_opts(&src, Duration::from_secs(3600), &["idle", "waiting"]);
        opts.next = true;
        let status = wait(&test_ctx(), None, opts).unwrap();
        assert_eq!(status, end);
    }
}

#[test]
fn test_wait_next_not_armed_by_turn_before_send() {
    let src = StatusSource::new("");
    src.start_turn();
    tick();
    src.send();
    tick();
    src.record("idle");

    let mut opts = wait_opts(&src, Duration::from_millis(20), &["idle"]);
    opts.next = true;
    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_millis(300));
    assert!(wait(&ctx, None, opts).is_err());
}

#[test]
fn test_wait_next_not_armed_by_turn_start_alone() {
    let src = StatusSource::new("");
    src.send();
    tick();
    src.start_turn();
    src.set("idle");

    let mut opts = wait_opts(&src, Duration::from_millis(20), &["idle"]);
    opts.next = true;
    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_millis(300));
    assert!(wait(&ctx, None, opts).is_err());
}

#[test]
fn test_wait_next_after_counts_only_newer_arrivals() {
    let src = StatusSource::new("");
    src.send();
    tick();
    src.start_turn();
    tick();
    src.record("waiting");
    let seen = src.reported_at();

    let mut opts = wait_opts(&src, Duration::from_millis(20), &["waiting"]);
    opts.next = true;
    opts.after = seen;
    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_millis(200));
    assert!(wait(&ctx, None, opts).is_err());

    tick();
    src.record("waiting");
    let mut opts = wait_opts(&src, Duration::from_millis(20), &["waiting"]);
    opts.next = true;
    opts.after = seen;
    let status = wait(&test_ctx(), None, opts).unwrap();
    assert_eq!(status, "waiting");
}

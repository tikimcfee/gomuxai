//! The built binary on a bus, end to end: two data dirs (two hard links of
//! the binary, hosts "hub" and "laptop"), each with its own bus identity, on
//! random ports, linked through the hub. tests/e2e/harness.rs says how each
//! runs isolated from the live system (and its bus is stopped on drop).

#[path = "e2e/harness.rs"]
mod harness;

use harness::{Host, free_port, wait_for};

/// bus init on h, checking what it says; returns the public bundle's path.
fn bus_init(h: &Host, url: Option<&str>) -> String {
    h.install_busd();
    let mut words = vec!["bus", "init"];
    if let Some(u) = url {
        words.extend(["--url", u]);
    }
    let o = h.run(&words).ok("bus init");
    let bus_dir = h.data_dir().join("bus");
    let bundle = bus_dir.join(format!("{}.pub.json", h.name));
    assert!(
        o.stdout.starts_with(&format!(
            "Created bus identity for host \"{}\" in {}\nPublic bundle: {}\n  key ",
            h.name,
            bus_dir.display(),
            bundle.display()
        )),
        "{}",
        o.stdout
    );
    bundle.to_string_lossy().into_owned()
}

/// Starts h's bus, checking what start says.
fn bus_start(h: &Host, args: &[&str]) {
    let mut words = vec!["bus", "start"];
    words.extend_from_slice(args);
    let o = h.run(&words);
    h.bus_started.set(true);
    let o = o.ok("bus start");
    assert!(
        o.stdout
            .starts_with("Bus started in session \"gomux-bus\" ("),
        "{}",
        o.stdout
    );
}

/// Links laptop to hub (each trusting the other, buses started on random
/// ports), and waits until the hub sees the link.
fn link(hub: &Host, laptop: &Host) {
    let (hub_port, leaf_port, lap_port) = (free_port(), free_port(), free_port());
    let hub_bundle = bus_init(hub, Some(&format!("tls://127.0.0.1:{leaf_port}")));
    let lap_bundle = bus_init(laptop, None);
    hub.run(&["bus", "trust", &lap_bundle]).ok("bus trust");
    laptop.run(&["bus", "trust", &hub_bundle]).ok("bus trust");
    bus_start(
        hub,
        &[
            "--client-port",
            &hub_port.to_string(),
            "--listen",
            &format!("127.0.0.1:{leaf_port}"),
        ],
    );
    bus_start(laptop, &["--client-port", &lap_port.to_string()]);
    wait_for("the laptop to link to the hub", || {
        let s = hub.run(&["bus", "status"]).stdout;
        let ok = s.contains("linked peers: 1;");
        (s, ok)
    });
}

// fleet, focus and template across the two data dirs: each host's bus (its
// `agent bus run`, in a termd pane) answers fleet and template, and carries
// focus changes to a follower on the other host.
#[test]
fn e2e_fleet_focus_template_two_hosts() {
    require_termd!();
    require_busd!();
    let hub = Host::new("hub");
    let laptop = Host::new("laptop");
    link(&hub, &laptop);

    hub.run(&["spawn", "w", "--backend", "shell"]).ok("spawn");
    hub.type_line("w", "echo ready-$((40+2))");
    hub.wait_line("w", "ready-42");
    hub.run(&["report", "idle", "--as", "w"]).ok("report");
    let o = hub.run(&["focus", "w"]).ok("focus w");
    assert!(
        o.stdout
            .starts_with("hub: w (session agent-w), set by user at "),
        "{}",
        o.stdout
    );
    assert_eq!(o.stderr, "");

    // fleet on the laptop: itself, then the hub, through the hub's bus.
    let o = laptop.run(&["fleet"]).ok("fleet");
    eprintln!("agent fleet (on laptop):\n{}", o.stdout);
    assert!(o.stdout.starts_with("laptop (build "), "{}", o.stdout);
    assert!(
        o.stdout.contains("\nhub (build ")
            && o.stdout.contains("  focus: w (agent-w) by user\n")
            && o.stdout.contains("  w: idle (shell)\n"),
        "{}",
        o.stdout
    );
    let o = laptop
        .run(&["fleet", "--json", "--host", "hub"])
        .ok("fleet --json --host");
    let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["hosts"][0]["host"], "hub");
    assert_eq!(v["hosts"][0]["focus"]["agent"], "w");
    let o = laptop.run(&["fleet", "--report"]).ok("fleet --report");
    eprintln!("agent fleet --report (on laptop):\n{}", o.stdout);
    assert!(
        o.stdout.contains("  agents by backend: shell: 1\n"),
        "{}",
        o.stdout
    );

    // focus: the hub's, from the laptop; then followed as it changes.
    let o = laptop.run(&["focus", "--host", "hub"]).ok("focus --host");
    assert!(
        o.stdout
            .starts_with("hub: w (session agent-w), set by user at "),
        "{}",
        o.stdout
    );
    let mut follow = laptop.start("follow", &["focus", "--follow", "--host", "hub"]);
    wait_for("the follower's first line", || {
        let s = follow.stdout();
        let ok = s.lines().count() >= 1;
        (s, ok)
    });
    hub.run(&["focus", "--clear"]).ok("focus --clear");
    let lines = wait_for("the change on the laptop", || {
        let s = follow.stdout();
        let ok = s.lines().count() >= 2;
        (s, ok)
    });
    eprintln!("agent focus --follow --host hub (on laptop):\n{lines}");
    assert!(
        lines
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("hub: nothing has focus, cleared by user at "),
        "{lines}"
    );
    assert_eq!(
        follow.stderr(),
        "Following focus on host hub. Ctrl-C stops.\n"
    );
    let code = follow.stop(libc::SIGINT);
    assert!(
        code == 0 || code == -libc::SIGINT,
        "focus --follow on SIGINT: {code}"
    );

    // template: the hub's own, started from the laptop.
    let work = hub.work();
    std::fs::write(work.join("brief.md"), "build it\n").unwrap();
    let dir = work.to_string_lossy().into_owned();
    hub.run(&[
        "template",
        "add",
        "builder",
        "--backend",
        "shell",
        "--cwd",
        &dir,
        "--brief",
        "brief.md",
    ])
    .ok("template add");
    let o = laptop
        .run(&["template", "list", "--host", "hub"])
        .ok("template list --host");
    assert_eq!(
        o.stdout,
        format!("builder: shell\n  cwd: {dir}\n  brief: brief.md\n")
    );
    let o = laptop
        .run(&["template", "start", "builder", "--host", "hub"])
        .ok("template start --host");
    eprintln!(
        "agent template start builder --host hub (on laptop):\n{}",
        o.stdout
    );
    assert!(
        o.stdout.starts_with(&format!(
            "Started agent builder@hub (session agent-builder, backend shell) from its template on hub, in {dir}.\n"
        )),
        "{}",
        o.stdout
    );
    let o = hub.run(&["list"]).ok("list");
    assert!(o.stdout.contains("builder: "), "{}", o.stdout);
    let o = laptop.run(&["template", "start", "builder", "--host", "hub"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.starts_with(
            "Error: hub couldn't start template builder: template \"builder\" is already running"
        ),
        "{}",
        o.stderr
    );
    let o = laptop.run(&["template", "remove", "builder", "--host", "hub"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .contains("templates are edited only on their own host"),
        "{}",
        o.stderr
    );
}

#[test]
fn e2e_bus_two_hosts() {
    require_termd!();
    require_busd!();
    let hub = Host::new("hub");
    let laptop = Host::new("laptop");
    let (hub_port, leaf_port, lap_port) = (free_port(), free_port(), free_port());

    let o = hub.run(&["bus", "status"]).ok("bus status");
    assert_eq!(o.stdout, "hub: bus not running\n");

    let hub_bundle = bus_init(&hub, Some(&format!("tls://127.0.0.1:{leaf_port}")));
    let lap_bundle = bus_init(&laptop, None);
    let o = hub.run(&["bus", "trust", &lap_bundle]).ok("bus trust");
    assert!(
        o.stdout.starts_with("Trusted \"laptop\": key "),
        "{}",
        o.stdout
    );
    let o = laptop.run(&["bus", "trust", &hub_bundle]).ok("bus trust");
    assert!(
        o.stdout.contains(&format!(
            "This host will link to it at tls://127.0.0.1:{leaf_port} (unless it is the hub).\n"
        )),
        "{}",
        o.stdout
    );

    bus_start(
        &hub,
        &[
            "--client-port",
            &hub_port.to_string(),
            "--listen",
            &format!("127.0.0.1:{leaf_port}"),
        ],
    );
    bus_start(&laptop, &["--client-port", &lap_port.to_string()]);
    let status = wait_for("the laptop to link to the hub", || {
        let s = hub.run(&["bus", "status"]).stdout;
        let ok = s.contains("linked peers: 1;");
        (s, ok)
    });
    assert!(
        status.starts_with(&format!(
            "hub: bus running (clients on tls://127.0.0.1:{hub_port})\n  hub, accepting peer links on 127.0.0.1:{leaf_port}\n  linked peers: 1; trusted: laptop\n"
        )),
        "{status}"
    );

    // An agent on the hub, reached from the laptop.
    hub.run(&["spawn", "w", "--backend", "shell"]).ok("spawn");
    hub.type_line("w", "echo ready-$((40+2))");
    hub.wait_line("w", "ready-42");
    hub.run(&["report", "idle", "--as", "w"]).ok("report");

    let o = laptop.run(&["list", "--host", "hub"]).ok("list --host");
    assert!(
        o.stdout
            .starts_with("w: idle (shell)\n  session: agent-w\n"),
        "{}",
        o.stdout
    );
    let o = laptop
        .run(&["status", "w@hub", "-v"])
        .ok("status name@host");
    assert!(o.stdout.starts_with("w@hub: idle\n"), "{}", o.stdout);
    let o = laptop
        .run(&["send", "w@hub", "; echo remote-$((40+2))"])
        .ok("send name@host");
    assert_eq!(o.stdout, "Sent to w@hub (it was idle).\n");
    let screen = hub.wait_line("w", "remote-42");
    assert!(
        screen.contains("[from a person on laptop] ; echo remote-$((40+2))"),
        "{screen}"
    );
    let o = laptop
        .run(&["capture", "w@hub", "--lines", "20"])
        .ok("capture name@host");
    assert!(o.stdout.contains("remote-42"), "{}", o.stdout);

    // Status events, as the bus carries them.
    let mut events = hub.start("events", &["events", "--json"]);
    wait_for("events to subscribe", || {
        let s = events.stderr();
        let ok = s.contains("Watching status events");
        (s, ok)
    });
    hub.run(&["report", "processing", "running tests", "--as", "w"])
        .ok("report");
    let line = wait_for("the status event", || {
        let s = events.stdout();
        let ok = s.contains("\"processing\"");
        (s, ok)
    });
    let v: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(
        (
            &v["host"],
            &v["agent"],
            &v["status"],
            &v["event"],
            &v["detail"]
        ),
        (
            &"hub".into(),
            &"w".into(),
            &"processing".into(),
            &"report".into(),
            &"running tests".into()
        ),
        "{line}"
    );
    assert_eq!(
        events.stderr(),
        "Watching status events of every agent on host hub. Ctrl-C stops.\n"
    );
    let code = events.stop(libc::SIGINT);
    assert!(
        code == 0 || code == -libc::SIGINT,
        "events on SIGINT: {code}"
    );

    // attach to another host's agent runs ssh, so it needs a terminal.
    let o = laptop.run(&["attach", "w@hub"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert_eq!(
        o.stderr,
        "Error: attach w@hub opens its session in this terminal, over ssh, so it needs one: run it in a terminal (agent capture w@hub shows its screen from here)\n"
    );

    for h in [&laptop, &hub] {
        let o = h.run(&["bus", "stop"]).ok("bus stop");
        assert_eq!(o.stdout, "Bus stopped\n");
        h.bus_started.set(false);
    }
    let o = hub.run(&["bus", "status"]).ok("bus status");
    assert!(
        o.stdout.starts_with("hub: bus not running\n"),
        "{}",
        o.stdout
    );
}

// A bus that fails as it starts is reported as soon as it exits, saying so,
// with what it printed: a leafnode port that is taken (agent-busd exits),
// and no agent-busd at all. Both exit within milliseconds, between two looks
// for the process: the shell back at its prompt says it ran (with a prompt
// that changes between commands, the wait's 5s grace would say it instead).
#[test]
fn e2e_bus_start_reports_a_failing_bus_at_once() {
    require_termd!();
    require_busd!();
    let h = Host::new("hub");
    bus_init(&h, None);
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = held.local_addr().unwrap().port();
    let start = std::time::Instant::now();
    let o = h.run(&[
        "bus",
        "start",
        "--client-port",
        &free_port().to_string(),
        "--listen",
        &format!("127.0.0.1:{taken}"),
    ]);
    let took = start.elapsed();
    assert_eq!(o.code, 1, "{o:?}");
    assert!(
        o.stderr.starts_with(
            "Error: the bus exited in session gomux-bus without coming up; session output:\n"
        ),
        "{}",
        o.stderr
    );
    // (The screen wraps at 120 columns, so only the start of the reason.)
    assert!(
        o.stderr.contains("Error listening on leafnode port"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("still starting"), "{}", o.stderr);
    assert!(
        took < std::time::Duration::from_secs(4),
        "a taken port took {took:?}"
    );
    drop(held);

    let h = Host::new("hub");
    h.run(&["bus", "init"]).ok("bus init"); // no agent-busd beside it, nor on PATH
    let start = std::time::Instant::now();
    let o = h.run(&["bus", "start", "--client-port", &free_port().to_string()]);
    let took = start.elapsed();
    assert_eq!(o.code, 1, "{o:?}");
    assert!(
        o.stderr.starts_with(
            "Error: the bus exited in session gomux-bus without coming up; session output:\n"
        ),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("cannot find agent-busd"), "{}", o.stderr);
    assert!(
        took < std::time::Duration::from_secs(4),
        "no agent-busd took {took:?}"
    );
}

// The first launch of a newly installed agent-busd can take seconds (on
// macOS, signature checks): start waits it out, saying once that it's still
// starting, rather than giving up at 5s while the bus comes up after.
#[test]
fn e2e_bus_start_waits_out_a_slow_launch() {
    require_termd!();
    require_busd!();
    let h = Host::new("hub");
    h.run(&["bus", "init"]).ok("bus init");
    let busd = harness::busd_binary().unwrap();
    harness::write_script(
        &h.root.path().join("bin/agent-busd"),
        &format!("#!/bin/sh\nsleep 7\nexec '{}' \"$@\"\n", busd.display()),
    );
    let start = std::time::Instant::now();
    let o = h.run(&["bus", "start", "--client-port", &free_port().to_string()]);
    h.bus_started.set(true);
    let took = start.elapsed();
    let o = o.ok("bus start");
    assert!(
        o.stdout
            .starts_with("Bus started in session \"gomux-bus\" ("),
        "{}",
        o.stdout
    );
    assert!(o.stderr.starts_with("still starting...\n"), "{}", o.stderr);
    assert_eq!(
        o.stderr.matches("still starting").count(),
        1,
        "{}",
        o.stderr
    );
    assert!(took >= std::time::Duration::from_secs(7), "{took:?}");
    let o = h.run(&["bus", "stop"]).ok("bus stop");
    assert_eq!(o.stdout, "Bus stopped\n");
    h.bus_started.set(false);
}

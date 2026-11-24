//! Tests of `agent bus service` (no Go file). None reaches the user's
//! systemd: systemctl, loginctl and journalctl are stand-ins, and the units
//! go in the test's own directory.
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::cli::bus::BusInfo;
use crate::cli::bus::bus_test::SessionName;
use crate::cli::testenv::{TestBus, TestEnv, temp_dir};

#[test]
fn units_render_and_read_back() {
    let bus = bus_unit_text(
        "/opt/gomuxai/agent",
        "/usr/local/bin:/usr/bin",
        "/opt/gomuxai",
    );
    for want in [
        "ExecStart=/opt/gomuxai/agent bus run --saved\n",
        "WorkingDirectory=/opt/gomuxai\n",
        "Environment=PATH=/usr/local/bin:/usr/bin\n",
        "Restart=on-failure\n",
        "KillMode=process\n",
        "WantedBy=default.target\n",
    ] {
        assert!(bus.contains(want), "no {want:?} in:\n{bus}");
    }
    assert_eq!(unit_binary(&bus).as_deref(), Some("/opt/gomuxai/agent"));
    assert_eq!(unit_resumes(&bus), None);

    let names = Resume::Names(vec!["lead".into(), "lead-build".into()]);
    let res = resume_unit_text(
        "/opt/gomuxai/agent",
        "/usr/bin",
        "/opt/gomuxai",
        "gomuxai-bus.service",
        &names,
    );
    for want in [
        "After=gomuxai-bus.service\n",
        "Wants=gomuxai-bus.service\n",
        "Type=oneshot\n",
        "RemainAfterExit=yes\n",
        "ExecStart=-/opt/gomuxai/agent resume lead\nExecStart=-/opt/gomuxai/agent resume lead-build\n",
    ] {
        assert!(res.contains(want), "no {want:?} in:\n{res}");
    }
    assert_eq!(unit_binary(&res).as_deref(), Some("/opt/gomuxai/agent"));
    assert_eq!(unit_resumes(&res), Some(names));

    let all = resume_unit_text(
        "/a/agent",
        "/usr/bin",
        "/a",
        "gomuxai-bus.service",
        &Resume::All,
    );
    assert!(all.contains("ExecStart=-/a/agent resume --all\n"), "{all}");
    assert_eq!(unit_resumes(&all), Some(Resume::All));

    assert_eq!(unique_path("/a:/b:/a::/c:/b"), "/a:/b:/c");

    let steps = [
        Step::Write("/units/x.service".into(), "[Unit]\nA=b\n".into()),
        Step::Run(systemctl(&["daemon-reload"]), false),
        Step::Remove("/units/y.service".into()),
    ];
    assert_eq!(
        format_steps(&steps),
        "Would write /units/x.service:\n  [Unit]\n  A=b\nWould run: systemctl --user daemon-reload\nWould remove /units/y.service\n"
    );
}

/// A stand-in systemd for this test thread: it records the commands, and a
/// start brings "the bus" up (a test bus the CLI then reaches), a stop
/// takes it down.
struct FakeSystemd {
    cmds: Rc<RefCell<Vec<String>>>,
    units: tempfile::TempDir,
    _bus: Option<(TestBus, agent_bus::CallbackSubscription)>,
}

impl FakeSystemd {
    /// With bus: the CLI's bus answers while "the unit" is started. None
    /// (skip the test) when it is wanted and agent-busd can't be built.
    fn new(env: &TestEnv, with_bus: bool) -> Option<FakeSystemd> {
        let up = Arc::new(AtomicBool::new(false));
        let mut bus = None;
        if with_bus {
            let b = TestBus::start()?;
            let info = BusInfo {
                host: env.ctx.cfg().host.clone(),
                client_url: "tls://127.0.0.1:7420".into(),
                client_port: 7420,
                remote_ops: vec!["list".into(), "status".into()],
                ..Default::default()
            };
            let data = serde_json::to_vec(&info).unwrap();
            let sub = b
                .conn
                .subscribe_callback(
                    &agent_bus::subject_local(&env.ctx.cfg().host, "bus"),
                    move |m| {
                        let _ = m.respond(data.clone());
                    },
                )
                .unwrap();
            b.conn.flush().unwrap();
            let (dir, host, up) = (b.dir.path().to_path_buf(), b.host.clone(), Arc::clone(&up));
            env.ctx.set_bus_connect(move || {
                if up.load(Ordering::SeqCst) {
                    agent_bus::connect_local(&dir, &host)
                } else {
                    Err(agent_bus::ErrNoBus.into())
                }
            });
            bus = Some((b, sub));
        } else {
            env.no_bus();
        }
        let cmds: Rc<RefCell<Vec<String>>> = Rc::default();
        let got = Rc::clone(&cmds);
        let units = temp_dir("agent-cli-units-");
        set_test_units(
            Some(units.path().to_path_buf()),
            Some(Box::new(move |c| {
                let line = c.join(" ");
                got.borrow_mut().push(line.clone());
                let has = |w: &str| c.iter().any(|a| a == w);
                if c[0] == "loginctl" {
                    return (true, "yes".into());
                }
                if c[0] == "journalctl" {
                    return (true, String::new());
                }
                if has("is-active") {
                    let on = up.load(Ordering::SeqCst);
                    return (on, if on { "active" } else { "inactive" }.into());
                }
                if has("is-enabled") {
                    return (true, "enabled".into());
                }
                let about_bus = c.last().is_some_and(|u| u.ends_with("-bus.service"));
                if about_bus && (has("start") || has("restart") || has("--now") && has("enable")) {
                    up.store(true, Ordering::SeqCst);
                }
                if about_bus && (has("stop") || has("--now") && has("disable")) {
                    up.store(false, Ordering::SeqCst);
                }
                (true, String::new())
            })),
        );
        Some(FakeSystemd {
            cmds,
            units,
            _bus: bus,
        })
    }

    fn dir(&self) -> &Path {
        self.units.path()
    }

    /// The commands run since the last look, emptied.
    fn ran(&self) -> Vec<String> {
        std::mem::take(&mut *self.cmds.borrow_mut())
    }
}

impl Drop for FakeSystemd {
    fn drop(&mut self) {
        set_test_units(None, None);
    }
}

fn run(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    res.unwrap_or_else(|e| panic!("{words:?}: {e:#}"));
    out
}

fn run_err(env: &mut TestEnv, words: &[&str]) -> String {
    let (_, res) = env.run(words);
    format!("{:#}", res.expect_err(&format!("{words:?} succeeded")))
}

// Installed, the unit runs the bus, and start, stop, restart and status go
// through it instead of the bus session.
#[test]
#[cfg(not(target_os = "macos"))]
fn service_runs_the_bus() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd();
    let _name = SessionName::set("svc-cli-gomux-bus");
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    env.change_cfg(|c| c.agent_binary = "/opt/gomuxai/agent".into());
    let cfg = env.ctx.cfg().clone();
    let binary = cfg.agent_binary.clone();

    // No identity: nothing to run.
    let Some(sd) = FakeSystemd::new(&env, true) else {
        return;
    };
    let e = run_err(&mut env, &["bus", "service", "install"]);
    assert!(e.contains("no bus identity"), "{e}");
    agent_bus::init_identity(cfg.bus_dir(), &cfg.host).unwrap();

    let out = run(&mut env, &["bus", "service"]);
    assert!(out.starts_with("No bus service is installed"), "{out}");
    let e = run_err(&mut env, &["bus", "service", "enable"]);
    assert!(e.contains("install, remove or status"), "{e}");
    let e = run_err(
        &mut env,
        &["bus", "service", "install", "--resume-all", "--no-resume"],
    );
    assert!(e.contains("give one of"), "{e}");
    let e = run_err(
        &mut env,
        &["bus", "service", "install", "--resume", "nobody"],
    );
    assert!(e.contains("no agent named \"nobody\""), "{e}");

    // --dry-run prints the unit and the commands, and writes nothing.
    let bus_path = sd.dir().join("gomuxai-bus.service");
    let res_path = sd.dir().join("gomuxai-resume.service");
    sd.ran();
    let out = run(
        &mut env,
        &["bus", "service", "install", "--resume-all", "--dry-run"],
    );
    for want in [
        format!("Would write {}:\n", bus_path.display()),
        format!("  ExecStart={binary} bus run --saved\n"),
        format!("Would write {}:\n", res_path.display()),
        format!("  ExecStart=-{binary} resume --all\n"),
        "Would run: systemctl --user daemon-reload\n".to_string(),
        "Would run: systemctl --user enable --now gomuxai-bus.service\n".to_string(),
        "Would run: systemctl --user enable gomuxai-resume.service\n".to_string(),
        "The bus would run with --client-port 7420 --remote-ops ".to_string(),
    ] {
        assert!(out.contains(&want), "no {want:?} in:\n{out}");
    }
    assert!(!bus_path.exists() && !res_path.exists());
    assert!(sd.ran().iter().all(|c| !c.contains("enable")));

    // install, with no bus running: the units are written, the settings
    // saved, and the bus started under the unit.
    let out = run(&mut env, &["bus", "service", "install", "--resume-all"]);
    assert!(out.contains("Installed gomuxai-bus.service"), "{out}");
    assert!(out.contains("The bus is started under it now."), "{out}");
    assert!(
        out.contains("gomuxai-resume.service: at boot, after the bus, it resumes every agent"),
        "{out}"
    );
    assert!(!out.contains("Linger is off"), "{out}");
    let ran = sd.ran();
    for want in [
        "systemctl --user daemon-reload",
        "systemctl --user enable --now gomuxai-bus.service",
        "systemctl --user enable gomuxai-resume.service",
    ] {
        assert!(ran.iter().any(|c| c == want), "no {want:?} in {ran:?}");
    }
    assert!(bus_path.exists() && res_path.exists());
    let saved = load_bus_settings(&env.ctx)
        .unwrap()
        .expect("settings saved");
    assert_eq!(saved.client_port, 7420);
    assert_eq!(own_unit(&env.ctx).as_deref(), Some("gomuxai-bus.service"));

    let out = run(&mut env, &["bus", "status"]);
    assert!(
        out.contains("run by systemd unit gomuxai-bus.service"),
        "{out}"
    );
    let out = run(&mut env, &["bus", "service", "status"]);
    for want in [
        "gomuxai-bus.service: active, enabled",
        "saved settings: --client-port 7420",
        "gomuxai-resume.service: enabled; at boot it resumes every agent",
        "Linger is on",
    ] {
        assert!(out.contains(want), "no {want:?} in:\n{out}");
    }

    // start refuses a running bus; restart restarts the unit with the new
    // settings saved for it.
    let e = run_err(&mut env, &["bus", "start"]);
    assert!(e.contains("already running"), "{e}");
    sd.ran();
    let out = run(&mut env, &["bus", "restart", "--remote-ops", "list,send"]);
    assert!(
        out.contains("Bus restarted under systemd unit gomuxai-bus.service"),
        "{out}"
    );
    assert!(
        sd.ran()
            .iter()
            .any(|c| c == "systemctl --user restart gomuxai-bus.service")
    );
    let saved = load_bus_settings(&env.ctx).unwrap().unwrap();
    assert_eq!(saved.remote_ops, ["list", "send"]);

    // Installing again leaves the running bus and the resume unit alone.
    let out = run(&mut env, &["bus", "service", "install"]);
    assert!(out.contains("It runs the bus now.\n"), "{out}");
    assert!(res_path.exists());
    let ran = sd.ran();
    assert!(
        ran.iter()
            .any(|c| c == "systemctl --user enable gomuxai-bus.service")
            && ran.iter().all(|c| !c.contains("--now")),
        "{ran:?}"
    );
    let out = run(&mut env, &["bus", "service", "install", "--no-resume"]);
    assert!(out.contains("Removed gomuxai-resume.service."), "{out}");
    assert!(!res_path.exists());

    // stop and start go through the unit.
    sd.ran();
    let out = run(&mut env, &["bus", "stop"]);
    assert!(
        out.contains("Bus stopped (systemd unit gomuxai-bus.service"),
        "{out}"
    );
    assert_eq!(
        sd.ran().last().map(String::as_str),
        Some("systemctl --user stop gomuxai-bus.service")
    );
    let out = run(&mut env, &["bus", "start"]);
    assert!(
        out.contains("Bus started under systemd unit gomuxai-bus.service"),
        "{out}"
    );
    assert!(
        sd.ran()
            .iter()
            .any(|c| c == "systemctl --user start gomuxai-bus.service")
    );

    // remove stops the bus the unit ran, and deletes the units.
    let out = run(&mut env, &["bus", "service", "remove"]);
    assert!(out.contains("Removed the bus service"), "{out}");
    assert!(out.contains("is stopped"), "{out}");
    assert!(!bus_path.exists());
    assert!(
        sd.ran()
            .iter()
            .any(|c| c == "systemctl --user disable --now gomuxai-bus.service")
    );
    let out = run(&mut env, &["bus", "service", "remove"]);
    assert!(out.contains("nothing to remove"), "{out}");
}

// A unit that runs another agent binary's bus isn't this one's: it is never
// started, stopped, replaced or removed from here.
#[test]
#[cfg(not(target_os = "macos"))]
fn service_leaves_another_binarys_unit() {
    let mut env = TestEnv::new();
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    env.change_cfg(|c| c.agent_binary = "/opt/gomuxai/agent".into());
    let cfg = env.ctx.cfg().clone();
    agent_bus::init_identity(cfg.bus_dir(), &cfg.host).unwrap();
    let sd = FakeSystemd::new(&env, false).unwrap();
    let path = sd.dir().join("gomuxai-bus.service");
    let text = bus_unit_text("/elsewhere/agent", "/usr/bin", "/elsewhere");
    std::fs::write(&path, &text).unwrap();

    assert_eq!(own_unit(&env.ctx), None);
    assert_eq!(running_unit(&env.ctx), None);
    let e = run_err(&mut env, &["bus", "service", "install"]);
    assert!(
        e.contains("another agent binary's bus (/elsewhere/agent)"),
        "{e}"
    );
    let e = run_err(&mut env, &["bus", "service", "remove"]);
    assert!(
        e.contains("another agent binary's bus (/elsewhere/agent)"),
        "{e}"
    );
    let out = run(&mut env, &["bus", "service", "status"]);
    assert!(
        out.contains("another agent binary's bus, not this one's"),
        "{out}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    let ran = sd.ran();
    assert!(
        ran.iter().all(
            |c| !c.contains(" start ") && !c.contains(" stop ") && !c.contains("able")
                || c.contains("is-enabled")
        ),
        "{ran:?}"
    );
}

// bus run --saved takes the saved settings for the flags it isn't given.
#[test]
fn run_saved_settings() {
    use crate::cli::bus::{BusRunArgs, run_settings};
    let env = TestEnv::new();
    let args = BusRunArgs {
        client_port: 7420,
        listen: String::new(),
        remote_ops: vec!["list".into()],
        saved: true,
        args: Vec::new(),
    };
    std::fs::create_dir_all(env.ctx.cfg().bus_dir()).unwrap();
    let none = |_: &str| false;
    // Nothing saved: the flags as they are.
    let got = run_settings(&env.ctx, &args, &none).unwrap();
    assert_eq!((got.client_port, got.listen.as_str()), (7420, ""));

    save_bus_settings(
        &env.ctx,
        &BusSettings {
            listen: "0.0.0.0:7422".into(),
            client_port: 7499,
            remote_ops: vec!["list".into(), "send".into()],
        },
    )
    .unwrap();
    let got = run_settings(&env.ctx, &args, &none).unwrap();
    assert_eq!(got.listen, "0.0.0.0:7422");
    assert_eq!(got.client_port, 7499);
    assert_eq!(got.remote_ops, ["list", "send"]);
    // A flag given wins; without --saved the file isn't read.
    let got = run_settings(&env.ctx, &args, &|f| f == "client-port").unwrap();
    assert_eq!(
        (got.client_port, got.listen.as_str()),
        (7420, "0.0.0.0:7422")
    );
    let plain = BusRunArgs {
        saved: false,
        ..args.clone()
    };
    let got = run_settings(&env.ctx, &plain, &none).unwrap();
    assert_eq!((got.client_port, got.listen.as_str()), (7420, ""));
}

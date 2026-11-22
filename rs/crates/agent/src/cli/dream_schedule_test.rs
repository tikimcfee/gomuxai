//! Tests of `agent dream schedule` (docs/DREAMING.md; no Go file). None runs
//! systemctl: they render, dry-run, and read status with no timer written.
use std::path::Path;

use agent_dream::schedule::Recurrence;

use super::*;
use crate::cli::testenv::TestEnv;

#[test]
fn plans_render_units_and_commands() {
    let dir = Path::new("/units");
    let r = Recurrence::Daily { hour: 4, minute: 0 };
    let p = plan_on(dir, "/opt/gomuxai/bin/agent", "/usr/local/bin:/usr/bin", &r).unwrap();
    assert_eq!(p.write.len(), 2);
    assert_eq!(p.write[0].0, dir.join("gomuxai-dream.service"));
    let service = &p.write[0].1;
    assert!(
        service.contains("ExecStart=/opt/gomuxai/bin/agent dream run\n"),
        "{service}"
    );
    assert!(
        service.contains("WorkingDirectory=/opt/gomuxai/bin\n"),
        "{service}"
    );
    assert!(
        service.contains("Environment=PATH=/usr/local/bin:/usr/bin\n"),
        "{service}"
    );
    assert_eq!(p.write[1].0, dir.join("gomuxai-dream.timer"));
    assert!(p.write[1].1.contains("OnCalendar=*-*-* 04:00:00\n"));
    assert_eq!(described(&p.write[1].1).as_deref(), Some("daily at 04:00"));
    let cmds: Vec<String> = p.commands.iter().map(|c| c.join(" ")).collect();
    assert_eq!(
        cmds,
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable --now gomuxai-dream.timer",
            "systemctl --user restart gomuxai-dream.timer",
        ]
    );

    let off = format_plan(&plan_off(dir));
    assert_eq!(
        off,
        "Would run: systemctl --user disable --now gomuxai-dream.timer\n\
         Would remove /units/gomuxai-dream.timer\n\
         Would remove /units/gomuxai-dream.service\n\
         Would run: systemctl --user daemon-reload\n"
    );
}

#[test]
fn plan_refuses_what_a_unit_would_misread() {
    let r = Recurrence::EveryHours(6);
    let dir = Path::new("/units");
    for (bin, path) in [
        ("/opt/my apps/agent", "/usr/bin"),
        ("/opt/agent", "/usr/bin:/opt/50%"),
        ("/opt/agent", ""),
        ("/opt/a\"gent", "/usr/bin"),
    ] {
        assert!(plan_on(dir, bin, path, &r).is_err(), "{bin:?} {path:?}");
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

#[test]
#[cfg(not(target_os = "macos"))]
fn schedule_dry_run_status_and_refusals() {
    let mut env = TestEnv::new();
    let units = unit_dir().unwrap();

    // Nothing scheduled: status says so without asking systemd.
    let none =
        "Dreams aren't scheduled. agent dream schedule daily HH:MM (or every Nh) turns it on.\n";
    assert_eq!(run(&mut env, &["dream", "schedule"]), none);
    assert_eq!(run(&mut env, &["dream", "schedule", "status"]), none);

    // A bad schedule, and an empty scope, are refused.
    let e = run_err(&mut env, &["dream", "schedule", "weekly"]);
    assert!(e.contains("daily HH:MM, every Nh, off or status"), "{e}");
    let e = run_err(&mut env, &["dream", "schedule", "daily", "4"]);
    assert!(e.contains("like 04:00"), "{e}");
    let e = run_err(&mut env, &["dream", "schedule", "every", "6h", "--dry-run"]);
    assert!(e.contains("nothing is in the dream scope"), "{e}");

    // With a scope, --dry-run prints both units and the commands, and
    // writes nothing.
    let proj = env.dir.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    run(
        &mut env,
        &["dream", "scope", "add", &proj.to_string_lossy()],
    );
    let out = run(&mut env, &["dream", "schedule", "every", "6h", "--dry-run"]);
    let service = units.join("gomuxai-dream.service");
    let timer = units.join("gomuxai-dream.timer");
    for want in [
        format!("Would write {}:\n", service.display()),
        format!("Would write {}:\n", timer.display()),
        " dream run\n".to_string(),
        "  OnCalendar=*-*-* 00/6:00:00\n".to_string(),
        "Would run: systemctl --user daemon-reload\n".to_string(),
        "Would run: systemctl --user enable --now gomuxai-dream.timer\n".to_string(),
    ] {
        assert!(out.contains(&want), "missing {want:?} in:\n{out}");
    }
    assert!(!service.exists() && !timer.exists());

    let out = run(&mut env, &["dream", "schedule", "off", "--dry-run"]);
    assert!(
        out.contains(&format!("Would remove {}\n", timer.display())),
        "{out}"
    );
    assert_eq!(
        run(&mut env, &["dream", "schedule", "off"]),
        "Dreams aren't scheduled; nothing to turn off.\n"
    );
}

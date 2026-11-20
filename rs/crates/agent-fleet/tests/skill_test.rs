//! Hermetic tests for `agent_fleet::skill`, strictly under temp `HOME`
//! directories in `/var/tmp` (never touching real `~/.claude` or `~/.gemini`).

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agent_backend::namedagents::{CLAUDE_HOOK_EVENTS, HookState, claude_hooks_status};
use agent_fleet::skill::{self, Harness, InstallEnv, SKILL_DESCRIPTION, SKILL_NAME, SkillStatus};

fn temp_dir(prefix: &str) -> tempfile::TempDir {
    let base = if Path::new("/var/tmp").is_dir() {
        PathBuf::from("/var/tmp")
    } else {
        std::env::temp_dir()
    };
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .unwrap()
}

fn make_executable(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn show_matches_service_skill_text_and_frontmatter_is_valid() {
    let body = skill::show("/opt/gomux/agent");
    assert_eq!(body, agent_service::skill_text("/opt/gomux/agent"));

    let md = skill::skill_markdown("/opt/gomux/agent");
    assert!(
        md.starts_with(&format!(
            "---\nname: {SKILL_NAME}\ndescription: {SKILL_DESCRIPTION}\n---\n\n"
        )),
        "unexpected frontmatter:\n{md}"
    );
    assert!(md.ends_with(&body));
}

#[test]
fn presence_detection_uses_path_or_config_dirs() {
    let tmp = temp_dir("agent-skill-detect-");
    let home = tmp.path().join("home");
    let empty_bin = tmp.path().join("empty-bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&empty_bin).unwrap();

    let agent_bin = tmp.path().join("bin/agent");
    make_executable(&agent_bin);
    let agent_bin_s = agent_bin.to_str().unwrap();

    let mut env = InstallEnv {
        claude_config_dir: None,
        path_env: Some(OsString::from(empty_bin.as_os_str())),
        backup_dir: None,
        force_hooks: false,
    };

    // Nothing on PATH and no config dirs -> nothing installed, nothing created.
    let none = skill::install_with(None, &home, agent_bin_s, &env).unwrap();
    assert!(none.is_empty());
    assert!(!home.join(".claude").exists());
    assert!(!home.join(".gemini").exists());

    // Fake `claude` on PATH -> installs Claude only.
    let cli_bin = tmp.path().join("cli-bin");
    make_executable(&cli_bin.join("claude"));
    env.path_env = Some(OsString::from(cli_bin.as_os_str()));

    let claude_only = skill::install_with(None, &home, agent_bin_s, &env).unwrap();
    assert_eq!(claude_only.len(), 1);
    assert_eq!(claude_only[0].harness, Harness::Claude);
    assert_eq!(claude_only[0].status, SkillStatus::Created);
    assert!(!home.join(".gemini").exists());

    // Reset home, place `.gemini/antigravity-cli` with empty PATH -> detects agy from config dir.
    let home2 = tmp.path().join("home2");
    fs::create_dir_all(home2.join(".gemini/antigravity-cli")).unwrap();
    env.path_env = Some(OsString::from(empty_bin.as_os_str()));

    let agy_only = skill::install_with(None, &home2, agent_bin_s, &env).unwrap();
    assert_eq!(agy_only.len(), 1);
    assert_eq!(agy_only[0].harness, Harness::Agy);
    assert_eq!(
        agy_only[0].skill_path,
        home2.join(".gemini/config/skills/agent/SKILL.md")
    );
    assert_eq!(agy_only[0].status, SkillStatus::Created);
    assert!(agy_only[0].hooks.is_none());
}

/// `to` (absolute) as a path relative to this process's directory.
fn relative_to_cwd(to: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap();
    let mut rel = PathBuf::new();
    for _ in cwd.components().skip(1) {
        rel.push("..");
    }
    rel.join(to.strip_prefix("/").unwrap())
}

// review-4 S3: a relative PATH entry ("." and the like, or an empty one) is
// never where a harness is found: an executable named claude in whatever
// directory skill install runs from doesn't count. Go's exec.LookPath
// refuses such a hit (exec.ErrDot), as agent-termd's look_path does.
#[test]
fn presence_detection_ignores_relative_path_entries() {
    let tmp = temp_dir("agent-skill-relpath-");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let agent_bin = tmp.path().join("bin/agent");
    make_executable(&agent_bin);
    let here = tmp.path().join("here");
    make_executable(&here.join("claude"));
    make_executable(&here.join("agy"));
    let rel = relative_to_cwd(&here);
    assert!(
        rel.is_relative() && rel.join("claude").is_file(),
        "{}",
        rel.display()
    );

    let elsewhere = tmp.path().join("elsewhere");
    make_executable(&elsewhere.join("claude"));
    for (path, want) in [
        (std::env::join_paths([rel.clone()]).unwrap(), 0),
        (
            OsString::from(format!(":{}", tmp.path().join("no-bins").display())),
            0,
        ),
        // The first entry with one decides, as for Go's exec.LookPath.
        (
            std::env::join_paths([rel.clone(), elsewhere.clone()]).unwrap(),
            0,
        ),
        (
            std::env::join_paths([elsewhere.clone(), rel.clone()]).unwrap(),
            1,
        ),
    ] {
        let env = InstallEnv {
            claude_config_dir: None,
            path_env: Some(path.clone()),
            backup_dir: None,
            force_hooks: false,
        };
        let got = skill::install_with(None, &home, agent_bin.to_str().unwrap(), &env).unwrap();
        assert_eq!(got.len(), want, "PATH={path:?}: {got:?}");
    }
}

#[test]
fn claude_and_agy_install_idempotency_hooks_and_claude_config_dir() {
    let tmp = temp_dir("agent-skill-install-");
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".gemini/config")).unwrap();

    // Pre-populate Claude settings.json with user settings and another hook to verify preservation.
    let settings_path = home.join(".claude/settings.json");
    fs::write(
        &settings_path,
        r#"{
  "model": "opus",
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {"type": "command", "command": "echo custom-stop", "timeout": 5}
        ]
      }
    ]
  }
}
"#,
    )
    .unwrap();

    let bin1 = tmp.path().join("v1/agent");
    make_executable(&bin1);
    let bin1_s = bin1.to_str().unwrap();

    let empty_path = tmp.path().join("no-bins");
    fs::create_dir_all(&empty_path).unwrap();
    let env = InstallEnv {
        claude_config_dir: None,
        path_env: Some(OsString::from(empty_path.as_os_str())),
        backup_dir: None,
        force_hooks: false,
    };

    // 1. First install writes both skills and registers missing Claude hook events.
    let first = skill::install_with(None, &home, bin1_s, &env).unwrap();
    assert_eq!(first.len(), 2);

    let cl = &first[0];
    assert_eq!(cl.harness, Harness::Claude);
    assert_eq!(cl.skill_path, home.join(".claude/skills/agent/SKILL.md"));
    assert_eq!(cl.status, SkillStatus::Created);
    let hooks = cl.hooks.as_ref().unwrap();
    assert!(hooks.changed);
    assert_eq!(hooks.added, CLAUDE_HOOK_EVENTS.len());
    assert_eq!(hooks.repointed, 0);
    assert!(
        !hooks.backup.is_empty(),
        "existing settings.json should be backed up"
    );

    // Verify existing user settings and custom hook survived intact.
    let raw_settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(raw_settings["model"], "opus");
    assert_eq!(raw_settings["hooks"]["Stop"].as_array().unwrap().len(), 2);

    // Verify every CLAUDE_HOOK_EVENTS entry is now Current.
    let status = claude_hooks_status(settings_path.to_str().unwrap(), bin1_s).unwrap();
    for &ev in CLAUDE_HOOK_EVENTS {
        assert_eq!(
            status.events.get(ev),
            Some(&HookState::Current),
            "event {ev}"
        );
    }

    let ag = &first[1];
    assert_eq!(ag.harness, Harness::Agy);
    assert_eq!(
        ag.skill_path,
        home.join(".gemini/config/skills/agent/SKILL.md")
    );
    assert_eq!(ag.status, SkillStatus::Created);
    assert_eq!(
        fs::read_to_string(&ag.skill_path).unwrap(),
        skill::skill_markdown(bin1_s)
    );

    // 2. Second install with the same binary is completely idempotent ("unchanged").
    let second = skill::install_with(None, &home, bin1_s, &env).unwrap();
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].status, SkillStatus::Unchanged);
    assert_eq!(second[0].hooks.as_ref().unwrap().summary(), "unchanged");
    assert!(!second[0].hooks.as_ref().unwrap().changed);
    assert_eq!(second[1].status, SkillStatus::Unchanged);

    // 3. Moving binary (old binary deleted, new binary installed) updates SKILL.md and re-points hooks.
    fs::remove_file(&bin1).unwrap();
    let bin2 = tmp.path().join("v2/agent");
    make_executable(&bin2);
    let bin2_s = bin2.to_str().unwrap();

    let third = skill::install_with(Some(Harness::Claude), &home, bin2_s, &env).unwrap();
    assert_eq!(third.len(), 1);
    assert_eq!(third[0].status, SkillStatus::Updated);
    let hooks3 = third[0].hooks.as_ref().unwrap();
    assert!(hooks3.changed);
    assert_eq!(hooks3.repointed, CLAUDE_HOOK_EVENTS.len());
    assert!(
        fs::read_to_string(&third[0].skill_path)
            .unwrap()
            .contains(bin2_s)
    );

    // 4. CLAUDE_CONFIG_DIR override places both SKILL.md and settings.json under that directory.
    let custom_claude = tmp.path().join("custom-claude-dir");
    let custom_env = InstallEnv {
        claude_config_dir: Some(custom_claude.clone()),
        path_env: Some(OsString::from(empty_path.as_os_str())),
        backup_dir: None,
        force_hooks: false,
    };
    let custom = skill::install_with(Some(Harness::Claude), &home, bin2_s, &custom_env).unwrap();
    assert_eq!(
        custom[0].skill_path,
        custom_claude.join("skills/agent/SKILL.md")
    );
    assert_eq!(
        custom[0].hooks.as_ref().unwrap().settings_path,
        custom_claude.join("settings.json")
    );
    assert!(custom[0].skill_path.is_file());
    assert!(custom_claude.join("settings.json").is_file());
}

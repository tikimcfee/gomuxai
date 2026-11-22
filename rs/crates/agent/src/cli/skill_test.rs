//! skill's cases, under a HOME and Claude config dir of the test's own.
use agent_termd::testing::EnvGuard;

use crate::cli::testenv::{TestEnv, isolate, temp_dir};

#[test]
fn test_skill_install_and_show() {
    isolate();
    let mut env = TestEnv::new();
    env.change_cfg(|c| c.agent_binary = "/opt/agent/bin/agent".into());
    let tmp = temp_dir("agent-skill-");
    let home = tmp.path().join("home");
    let claude = tmp.path().join("claude");
    std::fs::create_dir_all(&home).unwrap();
    let mut guard = EnvGuard::lock();
    guard.set("HOME", &home);
    guard.set("CLAUDE_CONFIG_DIR", &claude);

    // isolate() put stand-in claude and agy on PATH: both are found.
    let (out, res) = env.run(&["skill", "install"]);
    res.unwrap();
    eprintln!("agent skill install:\n{out}");
    let claude_skill = claude.join("skills/agent/SKILL.md");
    let agy_skill = home.join(".gemini/config/skills/agent/SKILL.md");
    let settings = claude.join("settings.json");
    assert!(
        out.starts_with(&format!(
            "Installed the agent skill for claude: {}\n  its hook in {}: 14 event(s) added, 0 pointed at this binary",
            claude_skill.display(),
            settings.display()
        )) && out.ends_with(&format!("Installed the agent skill for agy: {}\n", agy_skill.display())),
        "{out}"
    );
    assert_eq!(env.take_err(), "hint: sessions already running pick it up when restarted\n");
    for p in [&claude_skill, &agy_skill] {
        let text = std::fs::read_to_string(p).unwrap();
        assert!(text.starts_with("---\nname: agent\ndescription: "), "{}: {text}", p.display());
        assert!(text.contains("agent help"), "{}", p.display());
    }
    let s = std::fs::read_to_string(&settings).unwrap();
    assert!(s.contains("/opt/agent/bin/agent"), "{s}");

    // Again: nothing changes, and it says so.
    let (out, res) = env.run(&["skill", "install"]);
    res.unwrap();
    assert_eq!(
        out,
        format!(
            "Already installed the agent skill for claude: {}\n  its hook in {}: already installed\nAlready installed the agent skill for agy: {}\n",
            claude_skill.display(),
            settings.display(),
            agy_skill.display()
        )
    );
    assert_eq!(env.take_err(), "");

    // One harness.
    std::fs::remove_file(&agy_skill).unwrap();
    let (out, res) = env.run(&["skill", "install", "--harness", "agy"]);
    res.unwrap();
    assert_eq!(out, format!("Installed the agent skill for agy: {}\n", agy_skill.display()));
    let (_, res) = env.run(&["skill", "install", "--harness", "vim"]);
    assert_eq!(format!("{:#}", res.unwrap_err()), "unknown harness \"vim\": use claude or agy");

    let (out, res) = env.run(&["skill", "show"]);
    res.unwrap();
    assert!(out.starts_with("# Coordinating agents with `agent`") || out.contains("agent help"), "{out}");
    assert!(!out.starts_with("---"), "show prints the text, not the file: {out}");

    // review-4 C2: with no $HOME, Claude Code's skill still installs where
    // $CLAUDE_CONFIG_DIR says when --harness claude names it; what needs a
    // home (agy's, or finding which harnesses are here) says so.
    guard.remove("HOME");
    std::fs::remove_file(&claude_skill).unwrap();
    let (out, res) = env.run(&["skill", "install", "--harness", "claude"]);
    res.unwrap_or_else(|e| panic!("--harness claude without HOME: {e:#}"));
    assert!(
        out.starts_with(&format!("Installed the agent skill for claude: {}\n", claude_skill.display())),
        "{out}"
    );
    for words in [vec!["skill", "install"], vec!["skill", "install", "--harness", "agy"]] {
        let (_, res) = env.run(&words);
        assert_eq!(
            format!("{:#}", res.unwrap_err()),
            "$HOME isn't set, so there is no home to install the skill in",
            "{words:?}"
        );
    }
    guard.set("CLAUDE_CONFIG_DIR", "");
    let (_, res) = env.run(&["skill", "install", "--harness", "claude"]);
    assert_eq!(
        format!("{:#}", res.unwrap_err()),
        "$HOME isn't set, so there is no home to install the skill in"
    );
}

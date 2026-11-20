//! Go tests/backend/agy_hooks_test.go's TestAgyWorksInItsProject. It sets
//! TMPDIR, so it is a test binary of its own: no other test runs beside it.

mod common;

use agent_backend::namedagents::{self, AgyConfig};
use agent_backend::*;
use common::*;

/// agy sorts its workspace folders by path and works in the first: the
/// agent's dir, .gomuxai/agents/<name> beside the binary, sorted before the
/// project ("/home/u/.local/..." before "/home/u/dev/..."), so agy ran its
/// commands there (friction #46). The agent's dir goes in through a symlink
/// whose path sorts after the project.
#[test]
fn test_agy_works_in_its_project() {
    let root = temp_dir();
    let r = root.path().to_str().unwrap();
    let agent_dir = format!("{r}/a/.gomuxai/agents/helper");
    let project = format!("{r}/b/project");
    let tmp = format!("{r}/c");
    for d in [&agent_dir, &project, &tmp] {
        std::fs::create_dir_all(d).unwrap();
    }
    // SAFETY: this binary runs this one test, so no other thread reads the
    // environment while it is set.
    unsafe { std::env::set_var("TMPDIR", &tmp) };
    let provider = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: agent_dir.clone(),
        cwd: project.clone(),
        ..Default::default()
    });
    let cmd = provider.launch_command_with_hooks("agent report --hook --as helper", "s");
    let brief = format!("{agent_dir}/instructions.md");
    std::fs::write(&brief, "# briefing").unwrap();
    let cmd = provider.with_instructions(&cmd, &brief).unwrap();
    assert_eq!(cmd.matches("--add-dir").count(), 1, "--add-dir in {cmd}");
    let agy_args = cmd
        .strip_prefix("env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy ")
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let args = shell_args(agy_args);
    let added = args.last().unwrap().clone();
    assert!(
        added > project,
        "added {added} sorts before the project {project}: agy would work there"
    );
    let target = std::fs::read_link(&added).unwrap_or_else(|e| panic!("added {added}: {e}"));
    assert_eq!(
        target.to_str(),
        Some(agent_dir.as_str()),
        "added {added} isn't a link to the agent's dir"
    );
    assert!(
        added.starts_with(&format!("{tmp}/~gomuxai-")),
        "added {added} isn't under TMPDIR"
    );
    // The bundle is reachable through it.
    assert!(
        std::path::Path::new(&added)
            .join(".agents")
            .join("hooks.json")
            .exists()
    );
    assert!(std::path::Path::new(&added).join("AGENTS.md").exists());

    // A project that already sorts first keeps the agent's dir as it is.
    let early = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: project.clone(),
        cwd: agent_dir.clone(),
        ..Default::default()
    });
    let cmd = early.launch_command_with_hooks("agent report --hook --as helper", "s");
    assert!(
        cmd.contains(&format!("--add-dir '{project}'")),
        "no symlink needed: {cmd}"
    );
}

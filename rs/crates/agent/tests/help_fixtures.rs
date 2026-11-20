//! The binary's help and usage errors against the Go binary's, kept as fixtures
//! in testdata/ (written by tests/help_diff.sh --update from a Go build). Every
//! command path's help, both as `agent help <path>` and `agent <path> --help`,
//! and every line of testdata/errors/cases.txt: exit code, stdout and stderr,
//! byte for byte. Nothing here runs a command that does anything.

use std::path::{Path, PathBuf};
use std::process::Command;

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// Runs the binary with these words: (exit code, stdout, stderr), its data dir
/// shown as "<data dir>" as in the fixtures.
fn run(words: &[&str]) -> (i32, String, String) {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_agent"));
    let data_dir = std::fs::canonicalize(&bin)
        .unwrap()
        .parent()
        .unwrap()
        .join(".gomuxai");
    let out = Command::new(&bin)
        .args(words)
        .output()
        .expect("running agent");
    let norm =
        |b: &[u8]| String::from_utf8_lossy(b).replace(data_dir.to_str().unwrap(), "<data dir>");
    (
        out.status.code().unwrap_or(-1),
        norm(&out.stdout),
        norm(&out.stderr),
    )
}

/// Commands the Rust build has and the Go never had, by name: the root help
/// lists them, and the Go's doesn't. help_diff.sh keeps the same list.
const RUST_ONLY: &[&str] = &["run", "revive", "fleet", "focus", "template", "skill"];

/// The root help without the lines of RUST_ONLY's commands, and without a group
/// that lists nothing else: what the Go's root help is, if the rest agrees. It
/// panics if one of them isn't there, so the list can't go stale.
fn without_rust_only(help: &str) -> String {
    let mut out = Vec::new();
    for para in help.split("\n\n") {
        let mut lines = para.lines();
        let title = lines.next().unwrap_or_default();
        let cmds: Vec<&str> = lines.collect();
        let is_group = title.ends_with(':') && !title.starts_with(' ') && !cmds.is_empty();
        let rust_only = |l: &&str| {
            let name = l.split_whitespace().next().unwrap_or_default();
            l.starts_with("  ") && RUST_ONLY.contains(&name)
        };
        if !is_group || !cmds.iter().any(rust_only) {
            out.push(para.to_string());
            continue;
        }
        let kept: Vec<&str> = cmds.iter().copied().filter(|l| !rust_only(l)).collect();
        if !kept.is_empty() {
            out.push(format!("{title}\n{}", kept.join("\n")));
        }
    }
    for name in RUST_ONLY {
        let listed = help
            .lines()
            .any(|l| l.starts_with("  ") && l.split_whitespace().next() == Some(name));
        assert!(
            listed,
            "RUST_ONLY names {name}, which the root help doesn't list"
        );
    }
    out.join("\n\n")
}

/// Help fixtures whose text the Rust build changes on purpose, by name: for
/// each, testdata/rust_changed/<name>.go holds the Go's lines and <name>.rust
/// what the Rust prints instead. help_diff.sh keeps the same list. The Go's
/// lines are replaced once, and must be there, so the list can't go stale.
/// friction, friction_add: the friction kinds brief, handover and skill
/// (docs/FLEET.md). spawn: --handoff (docs/FLEET.md, "Revive"). bus, bus_run,
/// bus_start, bus_restart: --remote-ops defaults to agent_bus's
/// DEFAULT_REMOTE_OPS, which adds fleet and template.
const RUST_CHANGED: &[&str] = &[
    "friction",
    "friction_add",
    "spawn",
    "bus",
    "bus_run",
    "bus_start",
    "bus_restart",
];

/// The Go's help for the fixture name, with RUST_CHANGED's change to it made.
fn with_rust_changes(name: &str, want: String) -> String {
    if !RUST_CHANGED.contains(&name) {
        return want;
    }
    let dir = testdata().join("rust_changed");
    let from = read(&dir.join(format!("{name}.go")));
    let to = read(&dir.join(format!("{name}.rust")));
    assert!(
        want.contains(&from),
        "rust_changed/{name}.go isn't in the Go's help for {name}"
    );
    want.replacen(&from, &to, 1)
}

#[test]
fn rust_changed_fixtures_are_all_named() {
    let dir = testdata().join("rust_changed");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut want: Vec<String> = RUST_CHANGED
        .iter()
        .flat_map(|n| [format!("{n}.go"), format!("{n}.rust")])
        .collect();
    want.sort();
    assert_eq!(
        names, want,
        "testdata/rust_changed holds only RUST_CHANGED's files"
    );
}

#[test]
fn rust_only_commands_are_left_out_by_name() {
    let help = "Usage:\n  agent [command]\n\nAgents:\n  resume w\n  revive v\n\nFleet:\n  fleet    f\n  focus    g\n  template t\n\nState and tasks:\n  state  x\n\nJobs:\n  run    y\n\nAbout this tool:\n  history h\n  skill   s\n\nMachines (bus):\n  bus    z\n";
    assert_eq!(
        without_rust_only(help),
        "Usage:\n  agent [command]\n\nAgents:\n  resume w\n\nState and tasks:\n  state  x\n\nAbout this tool:\n  history h\n\nMachines (bus):\n  bus    z\n"
    );
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn help_matches_go() {
    let paths = read(&testdata().join("help/paths.txt"));
    let mut failures = Vec::new();
    let mut n = 0;
    for path in paths.lines() {
        let name = if path.is_empty() {
            "agent".to_string()
        } else {
            path.replace(' ', "_")
        };
        let want = with_rust_changes(&name, read(&testdata().join(format!("help/{name}.txt"))));
        let words: Vec<&str> = path.split_whitespace().collect();
        let mut help = vec!["help"];
        help.extend(&words);
        let mut flag = words.clone();
        flag.push("--help");
        for line in [help, flag] {
            n += 1;
            let (code, mut out, err) = run(&line);
            if path.is_empty() {
                out = without_rust_only(&out);
            }
            if code != 0 || out != want || !err.is_empty() {
                failures.push(format!(
                    "agent {}: exit {code}, stderr {err:?}, stdout:\n{out}",
                    line.join(" ")
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {n} differ from Go:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn usage_errors_match_go() {
    let cases = read(&testdata().join("errors/cases.txt"));
    let mut failures = Vec::new();
    for (i, line) in cases.lines().enumerate() {
        let want = read(&testdata().join(format!("errors/{}.txt", i + 1)));
        let (code, mut out, err) = run(&line.split(' ').collect::<Vec<_>>());
        // A line can print the root help (agent -h send).
        if out.contains("Use \"agent [command] --help\"") {
            out = without_rust_only(&out);
        }
        let got = format!("{code}\n--- stdout\n{out}--- stderr\n{err}");
        if got != want {
            failures.push(format!("agent {line}:\n got: {got:?}\nwant: {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

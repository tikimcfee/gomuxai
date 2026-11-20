//! The binary's help and usage errors, kept as fixtures in testdata/. Every
//! command path in testdata/help/paths.txt, both as `agent help <path>` and
//! `agent <path> --help`, and every line of testdata/errors/cases.txt: exit
//! code, stdout and stderr, byte for byte. Nothing here runs a command that
//! does anything.
//!
//! After changing help text on purpose, rewrite the fixtures and read the diff:
//!
//!   UPDATE_HELP_FIXTURES=1 cargo test -p agent --test help_fixtures

use std::path::{Path, PathBuf};
use std::process::Command;

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

fn updating() -> bool {
    std::env::var_os("UPDATE_HELP_FIXTURES").is_some_and(|v| !v.is_empty())
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

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The fixture's text; when updating, `got` is written there first.
fn fixture(p: &Path, got: &str) -> String {
    if updating() {
        std::fs::write(p, got).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    }
    read(p)
}

/// The commands a help text lists: under "Available Commands:", and in the
/// root help (`root`) under each of its groups ("Agents:", "Fleet:", ...).
fn listed_commands(help: &str, root: bool) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_list = false;
    for line in help.lines() {
        if !line.starts_with(' ') {
            in_list = line == "Available Commands:"
                || (root
                    && line.ends_with(':')
                    && !matches!(line, "Usage:" | "Flags:" | "Examples:"));
            continue;
        }
        if !in_list {
            continue;
        }
        if let Some(name) = line.split_whitespace().next() {
            names.push(name.to_string());
        }
    }
    names
}

/// paths.txt names every command that any help it covers lists, so a new
/// command or subcommand can't go without a fixture.
#[test]
fn paths_cover_every_listed_command() {
    let paths = read(&testdata().join("help/paths.txt"));
    let have: Vec<&str> = paths.lines().collect();
    let mut seen = 0;
    for path in &have {
        let mut words = vec!["help"];
        words.extend(path.split_whitespace());
        let (code, out, _) = run(&words);
        assert_eq!(code, 0, "agent help {path}");
        for name in listed_commands(&out, path.is_empty()) {
            seen += 1;
            let sub = format!("{path} {name}");
            let sub = sub.trim_start();
            assert!(
                have.contains(&sub),
                "testdata/help/paths.txt has no line for {sub:?}"
            );
        }
    }
    assert!(seen > 40, "commands found in the help texts: {seen}");
}

#[test]
fn help_matches_fixtures() {
    let paths = read(&testdata().join("help/paths.txt"));
    let mut failures = Vec::new();
    let mut n = 0;
    for path in paths.lines() {
        let name = if path.is_empty() {
            "agent".to_string()
        } else {
            path.replace(' ', "_")
        };
        let file = testdata().join(format!("help/{name}.txt"));
        let words: Vec<&str> = path.split_whitespace().collect();
        let mut help = vec!["help"];
        help.extend(&words);
        let mut flag = words.clone();
        flag.push("--help");
        for (i, line) in [help, flag].into_iter().enumerate() {
            n += 1;
            let (code, out, err) = run(&line);
            // Both spellings print the same text, so only the first writes.
            let want = if i == 0 {
                fixture(&file, &out)
            } else {
                read(&file)
            };
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
        "{} of {n} differ from testdata/help (UPDATE_HELP_FIXTURES=1 rewrites it):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn usage_errors_match_fixtures() {
    let cases = read(&testdata().join("errors/cases.txt"));
    let mut failures = Vec::new();
    for (i, line) in cases.lines().enumerate() {
        let (code, out, err) = run(&line.split(' ').collect::<Vec<_>>());
        let got = format!("{code}\n--- stdout\n{out}--- stderr\n{err}");
        let want = fixture(&testdata().join(format!("errors/{}.txt", i + 1)), &got);
        if got != want {
            failures.push(format!("agent {line}:\n got: {got:?}\nwant: {want:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "differ from testdata/errors (UPDATE_HELP_FIXTURES=1 rewrites it):\n{}",
        failures.join("\n")
    );
}

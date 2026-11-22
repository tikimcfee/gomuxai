use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use super::*;
use crate::memory::{self, Finding};

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// A small invented command tree: `agent send`, `agent wait`, `agent dream
/// memory check`, `agent dream note`, with a global --json.
fn commands() -> AgentCommands {
    let mut c = AgentCommands::default();
    let mut add = |path: &str, subs: &[&str], needs_sub: bool, flags: &[&str]| {
        let mut f = set(flags);
        f.insert("json".into());
        f.insert("help".into());
        c.commands.insert(
            path.to_string(),
            AgentCommand {
                subs: set(subs),
                needs_sub,
                flags: f,
            },
        );
    };
    add("", &["send", "wait", "dream"], true, &["quiet"]);
    add("send", &[], false, &["now"]);
    add("wait", &[], false, &["for", "next", "timeout"]);
    add("dream", &["memory", "note"], true, &[]);
    add("dream memory", &["check"], true, &["dir"]);
    add("dream memory check", &[], false, &["dir", "repo"]);
    add("dream note", &[], false, &[]);
    c
}

struct Fixture {
    _t: tempfile::TempDir,
    mem: std::path::PathBuf,
    repo: std::path::PathBuf,
    other: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let t = tempfile::tempdir().unwrap();
    let mem = t.path().join("memory");
    let repo = t.path().join("proj");
    let other = t.path().join("tools");
    fs::create_dir_all(&mem).unwrap();
    write(&repo, "src/main.rs", "fn main() {}\n");
    write(&repo, "rs/crates/core/src/lib.rs", "\n");
    write(&repo, "docs/GUIDE.md", "# guide\n");
    write(
        &repo,
        "Makefile",
        "VERSION := 1\nall: build\nbuild test: deps\n\tcargo build\n%.o: %.c\ninclude extra.mk\n",
    );
    write(&repo, "extra.mk", "lint:\n\ttrue\n");
    write(&other, "scripts/run.sh", "\n");
    Fixture {
        _t: t,
        mem,
        repo,
        other,
    }
}

fn repos(f: &Fixture) -> Repos {
    Repos {
        roots: vec![f.repo.clone(), f.other.clone()],
        agent: Some(commands()),
    }
}

fn found(f: &Fixture) -> Vec<String> {
    check(&f.mem, &repos(f))
        .unwrap()
        .iter()
        .map(|x| x.to_string())
        .collect()
}

#[test]
fn paths_are_checked_against_every_repo() {
    let f = fixture();
    let abs_ok = f.repo.join("docs/GUIDE.md");
    let abs_gone = f.repo.join("docs/OLD.md");
    write(
        &f.mem,
        "a.md",
        &format!(
            "---\nname: a\n---\n\
             Lives in `src/main.rs:12`, `src/main.rs:main` and `scripts/run.sh`; `docs/` and `rs/crates/core/`.\n\
             Moved: `src/old.rs`, `docs/gone/`.\n\
             Tail of a path: `crates/core/src/lib.rs`, and `crates/core/src/nope.rs`.\n\
             Bare: `lib.rs`, `missing.rs`, `a.md`, `MEMORY.md`.\n\
             Absolute: `{}` and `{}`; outside: `/etc/nothing/x.conf`, `~/.config/x.toml`.\n\
             Not paths: `origin/main`, `and/or`, `https://example.com/x.rs`, `docs/<name>.md`,\n\
             `src/*.rs`, `$HOME/x.rs`, `memory::check`, `0.8`, `../up/x.rs`, `v1.2`.\n\
             Unsure: `peer.pub.json`, `notes.md`, `memory/`, `lib/other/x.rs` (another\n\
             project's), `.cache/state/x.db`; a tail deeper down: `core/src/lib.rs`.\n",
            abs_ok.display(),
            abs_gone.display()
        ),
    );
    let got = found(&f);
    let want = vec![
        "a.md:5: `src/old.rs`: not in any repo".to_string(),
        "a.md:5: `docs/gone/`: not in any repo".into(),
        "a.md:6: `crates/core/src/nope.rs`: not in any repo, though its dir is".into(),
        "a.md:7: `missing.rs`: no file of that name in any repo".into(),
        format!(
            "a.md:8: `{}`: no such file in the repo it is under",
            abs_gone.display()
        ),
    ];
    let mut want_sorted = want.clone();
    want_sorted.sort();
    let mut got_sorted = got.clone();
    got_sorted.sort();
    assert_eq!(got_sorted, want_sorted, "{got:#?}");
}

#[test]
fn a_checkout_inside_a_repo_isnt_the_repo() {
    let f = fixture();
    write(&f.repo, "wt/feature/.git", "gitdir: elsewhere\n");
    write(&f.repo, "wt/feature/src/only_here.rs", "");
    write(&f.mem, "w.md", "`only_here.rs`, `src/only_here.rs`\n");
    assert_eq!(
        found(&f),
        vec![
            "w.md:1: `only_here.rs`: no file of that name in any repo",
            "w.md:1: `src/only_here.rs`: not in any repo",
        ]
    );
}

#[test]
fn paths_the_repo_ignores_are_never_stale() {
    let f = fixture();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&f.repo)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return; // no git here: nothing to test
    }
    write(&f.repo, ".gitignore", "/local/\n*.local.json\n");
    write(&f.repo, "local/other.db", "");
    write(
        &f.mem,
        "i.md",
        "`local/data/x.db`, `src/settings.local.json`, `src/settings.json`\n",
    );
    assert_eq!(
        found(&f),
        vec!["i.md:1: `src/settings.json`: not in any repo"]
    );
}

#[test]
fn make_targets_come_from_the_makefile_and_its_includes() {
    let f = fixture();
    write(
        &f.mem,
        "m.md",
        "---\nname: m\n---\n`make build`, `make test lint`, `make VERBOSE=1 all`\n\
         `make install`, `make -C sub anything`, `make <target>`, `make`\n",
    );
    assert_eq!(
        found(&f),
        vec!["m.md:5: `make install`: no Makefile defines that target"]
    );
    // Without a Makefile anywhere, make isn't checked.
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    assert!(found(&f).is_empty());
}

#[test]
fn make_is_unsure_when_a_target_is_a_variable() {
    let f = fixture();
    write(&f.repo, "Makefile", "$(BIN): x\nbuild:\n");
    write(&f.mem, "m.md", "`make whatever`\n");
    assert!(found(&f).is_empty());
}

#[test]
fn agent_subcommands_and_flags() {
    let f = fixture();
    write(
        &f.mem,
        "g.md",
        "`agent send helper \"hi\" --now`, `agent wait x --for idle --next`\n\
         `agent sned helper`, `agent dream memroy check`, `agent dream memory check --repo . --json`\n\
         `agent send x --later`, `agent -q send x`, `agent --backend claude list`\n\
         `agent dream memory check` takes `--repo` and `--bogus`\n\
         `agent send` and `cargo test` take `--release`\n\
         `agent <cmd>`, `agent NAME`, `agent`, `./bin/agent wait --for idle`, `bin/agent sned`\n",
    );
    assert_eq!(
        found(&f),
        vec![
            "g.md:2: `agent dream memroy check`: agent dream has no subcommand \"memroy\"",
            "g.md:2: `agent sned helper`: agent has no subcommand \"sned\"",
            "g.md:3: `agent send x --later`: agent send has no --later flag",
            "g.md:4: `--bogus`: agent dream memory check has no --bogus flag",
            "g.md:6: `bin/agent sned`: agent has no subcommand \"sned\"",
        ]
    );
}

#[test]
fn agent_is_unchecked_without_commands_and_fences_are_skipped() {
    let f = fixture();
    write(
        &f.mem,
        "g.md",
        "```\n`agent sned` `src/gone.rs`\n```\n`agent sned`\n",
    );
    let mut r = repos(&f);
    r.agent = None;
    assert!(check(&f.mem, &r).unwrap().is_empty());
    assert_eq!(found(&f).len(), 1);
}

#[test]
fn check_with_repos_adds_stale_findings_sorted_and_as_json() {
    let f = fixture();
    write(
        &f.mem,
        "b.md",
        "---\nname: b\ndescription: \"b\"\nmetadata:\n  type: project\n---\n\n`src/gone.rs` and `make nope`\n`agent sned`\n",
    );
    write(&f.mem, memory::INDEX, "- [B](b.md) — b\n");
    let r = repos(&f);
    let a = memory::check_with(&f.mem, Some(&r)).unwrap();
    assert_eq!(a, memory::check_with(&f.mem, Some(&r)).unwrap());
    let kinds: Vec<String> = a
        .iter()
        .map(|x| {
            serde_json::to_value(x).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds, vec!["stale-make", "stale-path", "stale-agent"]);
    let j = serde_json::to_value(&a[0]).unwrap();
    assert_eq!(j["file"], "b.md");
    assert_eq!(j["line"], 8);
    assert_eq!(j["token"], "make nope");
    assert!(matches!(a[2], Finding::StaleAgent { line: 9, .. }));
    // Without repos, none of it.
    assert!(memory::check(&f.mem).unwrap().is_empty());
}

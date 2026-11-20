//! Go: internal/cli/friction_test.go.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent_store::{CommandRecord, Friction, Time};
use agent_termd::testing::EnvGuard;
use chrono::TimeDelta;

use super::friction_similar::{SIMILAR_MIN_SCORE, summary_similarity, summary_words};
use super::*;
use crate::cli::exit::exit_code;
use crate::cli::root::execute_ctx;
use crate::cli::testenv::{TestEnv, test_config};

/// A CLI on host with a store of its own, as a test runs it (Go: setupStore,
/// or a frictionHost's cfg and store).
pub struct Host {
    pub env: TestEnv,
}

impl Host {
    pub fn new(host: &str) -> Host {
        let mut env = TestEnv::new();
        let mut cfg = test_config(env.dir.path());
        cfg.host = host.to_string();
        env.cfg = cfg.clone();
        let store = Arc::clone(env.ctx.store().expect("the test store"));
        let mut ctx = Ctx::new(env.out.writer(), env.err.writer());
        ctx.base_cfg = Some(cfg.clone());
        ctx.set_cfg(cfg);
        ctx.set_store(store);
        // The caller is AGENT_NAME's, whatever processes this test runs under.
        ctx.set_caller_chain(|| Ok(Vec::new()));
        env.ctx = ctx;
        Host { env }
    }

    /// Runs "agent args..." here: its stdout, and its error (Go: runAgent).
    pub fn on(&mut self, args: &[&str]) -> (String, anyhow::Result<()>) {
        self.env.out.clear();
        self.env.ctx.hinted.store(false, Ordering::Relaxed);
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let res = execute_ctx(&mut self.env.ctx, &argv);
        (self.env.take_out(), res)
    }

    /// on, which must succeed (Go: mustRun).
    pub fn must(&mut self, args: &[&str]) -> String {
        let (out, res) = self.on(args);
        if let Err(e) = res {
            panic!(
                "on {}: agent {}: {e:#}",
                self.env.cfg.host,
                args.join(" ")
            );
        }
        out
    }

    /// The error of on, which must fail.
    pub fn err(&mut self, args: &[&str]) -> String {
        match self.on(args).1 {
            Ok(()) => panic!("agent {}: no error", args.join(" ")),
            Err(e) => format!("{e:#}"),
        }
    }

    pub fn entry(&self, id: i64) -> Friction {
        self.env
            .store()
            .get_friction(id)
            .unwrap()
            .unwrap_or_else(|| panic!("on {}: no #{id}", self.env.cfg.host))
    }

    pub fn stderr(&self) -> String {
        self.env.err.contents()
    }
}

/// Holds the environment lock with AGENT_NAME set to name ("" unsets it)
/// (Go: t.Setenv("AGENT_NAME", ...)).
pub fn agent_name(name: &str) -> EnvGuard {
    let mut g = EnvGuard::lock();
    g.set("AGENT_NAME", name);
    g
}

/// Records a failed command by agent ("" for a person, in this directory)
/// (Go: logFailure).
fn log_failure(h: &Host, agent: &str, args: &[&str]) {
    let cwd = if agent.is_empty() {
        agent_base::gostd::os::getwd().unwrap()
    } else {
        h.env.dir.path().join("elsewhere").display().to_string()
    };
    h.env
        .store()
        .log_command(&mut CommandRecord {
            started_at: Time::now(),
            agent: agent.to_string(),
            cwd,
            args: args.iter().map(|s| s.to_string()).collect(),
            exit_code: 1,
            error: "it failed".to_string(),
            ..Default::default()
        })
        .unwrap();
}

#[test]
fn summary_similarity_with_help() {
    // "help" is one of the root's commands, but no command word: summaries
    // that name only help aren't about the same command.
    let score = summary_similarity(
        &summary_words("help for wait is confusing"),
        &summary_words("help for send is confusing"),
    );
    assert!(score < SIMILAR_MIN_SCORE, "score {score}");
}

#[test]
fn friction_add() {
    let _env = agent_name("lead");
    let mut h = Host::new("local");

    let out = h.must(&[
        "friction",
        "add",
        "wait --timeout doesn't say what status it last saw",
        "--kind",
        "error",
        "--detail",
        " more ",
    ]);
    assert_eq!(out, "Filed friction #1 (error).\n");
    let f = h.entry(1);
    assert!(
        f.filed_by == "lead"
            && f.kind == "error"
            && f.detail == "more"
            && f.status == "open"
            && f.host == "local"
            && !f.build.is_empty()
            && f.uid.starts_with("local-")
            && f.uid.len() == "local-".len() + 12
            && f.command.is_none(),
        "filed {f:?}"
    );

    let (out, res) = h.on(&[
        "friction",
        "add",
        "wait timeout doesn't say the last status it saw",
    ]);
    let err = res.expect_err("a similar summary was filed");
    let msg = format!("{err:#}");
    assert_eq!(exit_code(&err), FRICTION_SIMILAR_EXIT_CODE, "{msg}");
    assert!(msg.contains("agent friction +1 1") && msg.contains("--new"), "{msg}");
    assert!(out.contains("#1 error +0 wait --timeout"), "printed {out:?}");

    let out = h.must(&[
        "friction",
        "add",
        "wait timeout doesn't say the last status it saw",
        "--new",
    ]);
    assert_eq!(out, "Filed friction #2 (other).\n");

    // The kinds new in the Rust build.
    for kind in ["brief", "handover", "skill"] {
        let summary = format!("a {kind} entry, number {kind}");
        let out = h.must(&["friction", "add", &summary, "--kind", kind, "--new"]);
        assert!(out.ends_with(&format!("({kind}).\n")), "{out}");
    }

    for args in [
        &["friction", "add", "  "][..],
        &["friction", "add", "two\nlines"],
        &["friction", "add", "x", "--kind", "nope"],
        &["friction", "add", "x", "--detail", "a", "--detail-file", "b"],
    ] {
        h.err(args);
    }
    assert_eq!(h.env.store().last_friction_id().unwrap(), 5, "refused adds filed entries");
    assert_eq!(
        h.err(&["friction", "add", "x", "--kind", "skil"]),
        r#"unknown kind "skil"; did you mean skill? (want other, help, error, missing, bug, docs, brief, handover, skill)"#
    );
}

#[test]
fn friction_add_kind_help_lists_the_kinds() {
    let cmd = <FrictionAddArgs as clap::Args>::augment_args(clap::Command::new("add"));
    let kind = cmd
        .get_arguments()
        .find(|a| a.get_id() == "kind")
        .expect("a --kind flag");
    assert_eq!(
        kind.get_help().map(|h| h.to_string()),
        Some(format!("Kind: {}", FRICTION_KINDS.join(", ")))
    );
}

#[test]
fn friction_attaches_the_last_failed_command() {
    let mut env = agent_name("lead");
    let mut h = Host::new("local");
    log_failure(&h, "lead", &["wait", "helper", "--for", "idel"]);
    log_failure(&h, "other", &["send", "x", "hi"]); // someone else's
    log_failure(&h, "lead", &["friction", "add", "something"]); // friction's own refusal

    let out = h.must(&["friction", "add", "wait --for takes no typos"]);
    assert_eq!(
        out,
        "Filed friction #1 (other), with your failed command from just now: agent wait helper --for idel (exit 1).\n"
    );
    let f = h.entry(1);
    let c = f.command.expect("an attached command");
    assert_eq!(c.args.join(" "), "wait helper --for idel");
    assert_eq!(c.error, "it failed");

    let out = h.must(&["friction", "+1", "1", "--no-command"]);
    assert_eq!(out, "Voted on friction #1 (other, now +1).\n");

    // A person's failures count from the same directory only.
    env.set("AGENT_NAME", "");
    let out = h.must(&["friction", "+1", "#1", "me too"]);
    assert_eq!(out, "Voted on friction #1 (other, now +2).\n", "want nothing attached");
    log_failure(&h, "", &["list", "--host", "nowhere"]);
    let out = h.must(&["friction", "+1", "1"]);
    assert!(
        out.contains("agent list --host nowhere (exit 1)"),
        "person's +1 printed {out:?}, want their failure from this directory attached"
    );
    let votes = h.env.store().list_friction_votes(1).unwrap();
    assert_eq!(votes.len(), 3);
    assert_eq!(votes[1].voter, "user");
    assert_eq!(votes[1].note, "me too");
}

#[test]
fn friction_lifecycle() {
    let _env = agent_name("");
    let mut h = Host::new("local");
    h.must(&[
        "friction",
        "add",
        "send's help doesn't say it types into the terminal",
        "--kind",
        "help",
    ]);
    h.must(&[
        "friction",
        "add",
        "no way to wait for several agents at once",
        "--kind",
        "missing",
    ]);
    h.must(&["friction", "add", "wait for many agents", "--new"]);
    h.must(&["friction", "+1", "3"]);

    assert_eq!(
        h.must(&["friction", "accept", "1", "--note", "soon"]),
        "Accepted friction #1 (help): send's help doesn't say it types into the terminal\n"
    );
    let e = h.err(&["friction", "accept", "1"]);
    assert!(e.contains("already accepted (by user)"), "accepting twice: {e}");
    assert_eq!(
        h.must(&["friction", "resolve", "3", "--dup", "2"]),
        "Closed friction #3 as a duplicate of #2 (now +2).\n"
    );
    // A vote on the duplicate goes to the original.
    assert_eq!(
        h.must(&["friction", "+1", "3"]),
        "Voted on friction #2 (missing, now +3; #3 is a duplicate of it).\n"
    );
    let e = h.err(&["friction", "resolve", "1"]);
    assert!(e.contains("--note is required"), "{e}");
    let e = h.err(&["friction", "resolve", "2", "--dup", "3"]);
    assert!(e.contains("itself"), "{e}");
    assert_eq!(
        h.must(&["friction", "resolve", "1", "--note", "send's help says so (abc1234)"]),
        "Resolved friction #1 (help).\n"
    );
    assert_eq!(
        h.must(&["friction", "resolve", "2", "--wontfix", "--note", "use a loop"]),
        "Closed friction #2 (missing) as wontfix.\n"
    );
    let e = h.err(&["friction", "resolve", "1", "--note", "again"]);
    assert!(e.contains("already closed as resolved (by user)"), "{e}");
    let e = h.err(&["friction", "+1", "1"]);
    assert!(e.contains("closed as resolved"), "{e}");

    assert_eq!(
        h.must(&["friction", "list"]),
        "No open or accepted entries (3 entries in all: agent friction list --status all).\n"
    );
    let e = h.err(&["friction", "list", "--kind", "halp"]);
    assert!(e.contains(r#"unknown kind "halp"; did you mean help?"#), "{e}");
    let out = h.must(&["friction", "list", "--status", "all"]);
    let lines: Vec<&str> = out.trim().split('\n').collect();
    assert!(lines.len() == 4 && lines[1].starts_with("#2 wontfix"), "list --status all printed:\n{out}");
    let e = h.err(&["friction", "list", "--status", "closed"]);
    assert!(e.contains(r#"unknown status "closed""#), "{e}");

    let export = h.must(&["friction", "export"]);
    for want in [
        "# Friction on local",
        "## Resolved\n\n### #1 send's help",
        "Resolved by user",
        ": send's help says so (abc1234).",
        "## Won't fix",
        "## Duplicates\n\n### #3",
    ] {
        assert!(export.contains(want), "export lacks {want:?}:\n{export}");
    }
    assert!(!export.contains("## Open"), "export has an empty section:\n{export}");

    let show = h.must(&["friction", "show", "2"]);
    for want in [
        "#2 missing, wontfix, +3: no way",
        "Won't fix, by user",
        "Votes:\n",
        "Duplicates:\n  #3 +1 by user: wait for many agents",
    ] {
        assert!(show.contains(want), "show lacks {want:?}:\n{show}");
    }
}

#[test]
fn friction_id_errors() {
    let _env = agent_name("");
    let mut h = Host::new("local");
    let e = h.err(&["friction", "show", "1"]);
    assert!(e.contains("nothing has been filed"), "{e}");
    h.must(&["friction", "add", "x y z"]);
    assert_eq!(
        h.err(&["friction", "show", "99"]),
        "no friction #99 (latest is #1); agent friction list --status all lists them"
    );
    let e = h.err(&["friction", "show", "twelve"]);
    assert!(e.contains("ids are numbers"), "{e}");
}

// An empty box says so, and how to file; an unknown subcommand suggests one.
#[test]
fn friction_list_on_an_empty_box() {
    let _env = agent_name("");
    let mut h = Host::new("local");
    assert_eq!(
        h.must(&["friction", "list"]),
        "The friction box on host local is empty.\n"
    );
    let hints = h.stderr();
    assert!(
        hints.starts_with(r#"hint: if agent gets in your way, file it: agent friction add "what happened""#),
        "hints = {hints:?}"
    );
    let e = h.err(&["friction", "lst"]);
    assert!(
        e.contains(r#"unknown command "lst" for "agent friction"; did you mean list?"#),
        "{e}"
    );
}

#[test]
fn format_friction_list_columns() {
    let now = Time::date_utc(2026, 10, 24, 14, 0, 0, 0);
    let mut entries = vec![
        Friction {
            id: 12,
            kind: "help".into(),
            votes: 3,
            created_at: now - TimeDelta::hours(2),
            filed_by: "lead".into(),
            summary: "a".into(),
            status: "open".into(),
            ..Default::default()
        },
        Friction {
            id: 9,
            kind: "missing".into(),
            votes: 0,
            created_at: now - TimeDelta::hours(72),
            filed_by: "user".into(),
            summary: "b".into(),
            status: "open".into(),
            ..Default::default()
        },
    ];
    let none = std::collections::HashMap::new();
    let want = "ID  KIND    VOTES  AGE BY   SUMMARY
#12 help       +3   2h lead a
#9  missing    +0   3d user b
";
    assert_eq!(format_friction_list(&entries, &none, &now), want, "all open");
    entries[1].status = "accepted".into();
    let want = "ID  STATUS    KIND    VOTES  AGE BY   SUMMARY
#12           help       +3   2h lead a
#9  accepted  missing    +0   3d user b
";
    assert_eq!(format_friction_list(&entries, &none, &now), want, "one accepted");
}

#[test]
fn markdown_code_fences() {
    for (s, want) in [
        ("agent list", "`agent list`"),
        ("agent send x 'a`b'", "``agent send x 'a`b'``"),
        ("`x`", "`` `x` ``"),
        ("agent send x 'a``b`'", "```agent send x 'a``b`'```"),
    ] {
        assert_eq!(markdown_code(s), want, "markdown_code({s:?})");
    }
}

#[test]
fn ages() {
    assert_eq!(age(TimeDelta::seconds(59)), "now");
    assert_eq!(age(TimeDelta::seconds(-5)), "now");
    assert_eq!(age(TimeDelta::minutes(5)), "5m");
    assert_eq!(age(TimeDelta::hours(47)), "47h");
    assert_eq!(age(TimeDelta::hours(49)), "2d");
}

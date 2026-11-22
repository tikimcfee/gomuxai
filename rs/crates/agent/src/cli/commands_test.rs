//! The command tree's own tests (Go: internal/cli/commands_test.go).

use clap::ArgAction;

use super::command::{Cmd, Def};
use super::flags;
use super::root::{check_args, find, parse_flags, positionals, tree};

fn cmd(path: &str) -> Cmd<'static> {
    Cmd::root(tree())
        .lookup(path)
        .unwrap_or_else(|| panic!("no command {path}"))
}

/// Parses `args` as the command's flags and arguments and checks them against
/// its argument rule, as cobra's ValidateArgs does (without the usage line).
fn validate(path: &str, args: &[&str]) -> anyhow::Result<()> {
    let c = cmd(path);
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let m = parse_flags(&c, &args)?;
    c.def().doc.args.check(&c, &m, &positionals(&m))
}

#[test]
fn command_args() {
    let tests: &[(&str, &[&str], bool)] = &[
        ("spawn", &[], false),
        ("spawn", &["researcher"], true),
        ("spawn", &["researcher", "tasks/research.md"], true),
        (
            "spawn",
            &["researcher", "tasks/research.md", "extra"],
            false,
        ),
        ("send", &["agent"], false),
        ("send", &["agent", "please", "analyze"], true),
        ("queue", &["agent"], false),
        ("queue", &["agent", "fix", "the", "bug"], true),
        ("capture", &[], false),
        ("capture", &["agent"], true),
        ("list", &[], true),
        ("list", &["unexpected"], false),
        ("sessions", &[], true),
        ("sessions", &["unexpected"], false),
        ("status", &["agent"], true),
        ("status", &["a", "b"], false),
        ("cleanup", &[], false),
        ("cleanup", &["agent"], true),
        ("cleanup", &["a", "b"], true),
        ("watch", &["unexpected"], false),
        ("session new", &["my-session"], true),
        ("session kill", &[], false),
        ("session list", &[], true),
        ("keys", &["my-session"], false),
        ("keys", &["my-session", "echo", "hello"], true),
        ("key", &["my-session"], false),
        ("key", &["my-session", "Enter"], true),
        ("key", &["my-session", "Enter", "extra"], false),
        ("task set", &["name"], true), // with -f
        ("task set", &["name", "content"], true),
        ("task set", &["name", "content", "extra"], false),
        ("task get", &["name"], true),
        ("task list", &["unexpected"], false),
        ("task clear", &[], false),
        ("state set", &["agent", "key"], false),
        ("state set", &["agent", "key", "value"], true),
        ("state get", &["agent", "key"], true),
        ("state list", &["agent"], true),
        ("state clear", &[], false),
        ("logs", &[], false),
        ("logs", &["agent"], true),
        ("report", &[], true), // with --hook
        ("report", &["idle", "detail", "extra"], false),
        ("approve", &["agent"], true),
        ("deny", &[], false),
        ("wait", &["agent"], true),
        ("events", &[], true),
        ("events", &["a", "b"], false),
        ("bus trust", &[], false),
        ("bus start", &["unexpected"], false),
        // The rules that depend on flags.
        ("resume", &["lead"], true),
        ("resume", &["--all"], true),
        ("resume", &["--all", "--dry-run"], true),
        ("resume", &["--dry-run", "lead"], false),
        ("resume", &["--all", "lead"], false),
        ("resume", &["--all", "--attach"], false),
        ("cleanup", &["--ended"], true),
        ("cleanup", &["--ended", "a"], false),
        ("cleanup", &["--dry-run", "a"], false),
    ];
    for (path, args, ok) in tests {
        let err = validate(path, args);
        assert_eq!(
            err.is_ok(),
            *ok,
            "agent {path} {args:?}: err = {err:?}, want ok = {ok}"
        );
    }
}

#[test]
fn custom_arg_rules_say_why() {
    let msg = |path: &str, args: &[&str]| validate(path, args).unwrap_err().to_string();
    assert_eq!(
        msg("resume", &["--dry-run", "x"]),
        "--dry-run goes with --all: agent resume --all --dry-run"
    );
    assert_eq!(
        msg("resume", &["--all", "lead@hub"]),
        "resume --all resumes this host's agents: run agent resume --all on hub to resume its own, or agent resume lead@hub for that one"
    );
    assert_eq!(
        msg("cleanup", &["--ended", "a"]),
        "--ended picks the agents itself: name agents, or pass --ended, not both"
    );
}

/// pflag's DefValue of a flag: its default as text, lists as "[a,b]".
fn def_value(path: &str, flag: &str) -> Option<String> {
    let parser = cmd(path).def().clap_command();
    let arg = parser
        .get_arguments()
        .find(|a| a.get_long() == Some(flag))?;
    let values: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    Some(match arg.get_action() {
        ArgAction::Append => format!("[{}]", values.join(",")),
        _ => values.join(","),
    })
}

#[test]
fn flag_defaults() {
    let tests = [
        ("capture", "lines", "50"),
        ("capture", "raw", "false"),
        ("logs", "lines", "50"),
        ("watch", "interval", "2s"),
        ("keys", "no-enter", "false"),
        ("wait", "timeout", "0s"),
        ("wait", "interval", "0s"),
        (
            "bus start",
            "remote-ops",
            // agent_bus::DEFAULT_REMOTE_OPS: the Go's, and fleet and
            // template (docs/FLEET.md).
            "[list,status,capture,send,respond,friction,logs,fleet,template,manage,roster]",
        ),
    ];
    for (path, flag, want) in tests {
        assert_eq!(
            def_value(path, flag).as_deref(),
            Some(want),
            "agent {path} --{flag}"
        );
    }
}

#[test]
fn flags_parse_as_pflag_does() {
    use crate::cli::{capture, sessions, status};
    use clap::FromArgMatches;
    let parse = |path: &str, line: &str| {
        let words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        parse_flags(&cmd(path), &words)
    };
    // Bools take =value, and never the next word.
    let m = parse("status", "x --json=false -v y").unwrap();
    let a = status::Args::from_arg_matches(&m).unwrap();
    assert!(!a.json && a.verbose);
    assert_eq!(a.args, ["x", "y"]);
    // Short bools cluster; values may start with a dash; the last of a flag
    // given twice wins.
    let a = status::Args::from_arg_matches(&parse("status", "-qv x").unwrap()).unwrap();
    assert!(a.verbose);
    let m = parse("capture", "x --lines 5 --lines=7").unwrap();
    assert_eq!(capture::Args::from_arg_matches(&m).unwrap().lines, 7);
    let m = parse("sessions", "--project -x -n 0x10").unwrap();
    let a = sessions::Args::from_arg_matches(&m).unwrap();
    assert_eq!((a.project.as_str(), a.limit), ("-x", 16));
    // Globals anywhere.
    let m = parse("status", "x --backend claude -q").unwrap();
    let g = crate::cli::root::Globals::from_arg_matches(&m).unwrap();
    assert!(g.quiet && g.backend == "claude");
    // "--" ends the flags.
    let a = status::Args::from_arg_matches(&parse("status", "-- -v").unwrap()).unwrap();
    assert!(!a.verbose && a.args == ["-v"]);
}

#[test]
fn flag_rules_are_checked_after_args() {
    // After setup too, as cobra checks them after PersistentPreRunE: setup
    // opens the store, so these lines get a data dir of their own.
    let dir = super::testenv::temp_dir("agent-cli-");
    let cfg = super::testenv::test_config(dir.path());
    let run = |args: &[&str]| -> (String, String, Option<String>) {
        let mut ctx = super::root::Ctx::new(
            super::root::SharedBuf::default().writer(),
            super::root::SharedBuf::default().writer(),
        );
        ctx.base_cfg = Some(cfg.clone());
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let res = super::root::execute_ctx(&mut ctx, &argv);
        (
            String::new(),
            String::new(),
            res.err().map(|e| format!("{e:#}")),
        )
    };
    let (_, _, err) = run(&["wait", "x", "--next"]);
    assert_eq!(err.as_deref(), Some(r#"required flag(s) "for" not set"#));
    let (_, _, err) = run(&[
        "friction",
        "add",
        "summary",
        "--detail",
        "a",
        "--detail-file",
        "b",
    ]);
    assert_eq!(
        err.as_deref(),
        Some(
            "if any flags in the group [detail detail-file] are set none of the others can be; [detail detail-file] were all set"
        )
    );
    // Without a config of the test's own, setup refuses the store beside the
    // test binary, before any flag rule.
    let (_, _, err) = self::run(&["wait", "x", "--for", "idle"]);
    let err = err.unwrap_or_default();
    assert!(
        err.contains("a test opened the store beside the test binary"),
        "err = {err:?}"
    );
}

#[test]
fn spawn_requires_backend() {
    let (_, _, err) = run(&["spawn", "helper"]);
    let err = err.unwrap_or_default();
    assert!(
        err.contains("--backend") && err.contains("--backend-config"),
        "err = {err:?}, want one naming --backend and --backend-config"
    );
}

/// Runs a command line: (stdout, stderr, the error's text).
fn run(args: &[&str]) -> (String, String, Option<String>) {
    let (out, err) = (
        super::root::SharedBuf::default(),
        super::root::SharedBuf::default(),
    );
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let res = super::execute_with(&argv, out.writer(), err.writer());
    (
        out.contents(),
        err.contents(),
        res.err().map(|e| format!("{e:#}")),
    )
}

/// Every command in the tree, with its place.
fn all_commands() -> Vec<Cmd<'static>> {
    fn walk(c: Cmd<'static>, out: &mut Vec<Cmd<'static>>) {
        out.push(c.clone());
        for sub in &c.def().subs {
            walk(c.child(sub), out);
        }
    }
    let mut out = Vec::new();
    walk(Cmd::root(tree()), &mut out);
    out
}

fn is_completion(c: &Cmd) -> bool {
    c.command_path().starts_with("agent completion")
}

// Every example in the commands' help must name a real command, use only its
// flags, and pass an accepted number of arguments, so the help can't drift from
// what the commands take.
#[test]
fn help_examples_parse() {
    let mut errors = Vec::new();
    for c in all_commands() {
        let doc = c.def().doc;
        if doc.example.is_empty() && !doc.hidden && c.name() != "help" && !is_completion(&c) {
            errors.push(format!("{} has no Example", c.command_path()));
        }
        let text = format!("{}\n{}", doc.long, doc.example);
        for line in text.lines().filter(|l| l.starts_with("  ")) {
            for example in line.split("&&") {
                let words = example_words(example);
                if words.first().map(String::as_str) == Some("agent")
                    && let Err(e) = check_example(&words[1..])
                {
                    errors.push(format!(
                        "{} help: agent {}: {e:#}",
                        c.command_path(),
                        words[1..].join(" ")
                    ));
                }
            }
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    // And the check refuses what a command doesn't take.
    let words = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
    for bad in [
        "send x",
        "list --bogus",
        "bogus",
        "completion",
        "capture x --lines abc",
    ] {
        assert!(check_example(&words(bad)).is_err(), "agent {bad} passed");
    }
    assert_eq!(
        example_words(r#"agent send x "a b" # why"#),
        ["agent", "send", "x", "a b"],
        "quotes join, comments go"
    );
}

fn check_example(args: &[String]) -> anyhow::Result<()> {
    let (c, rest) = find(tree(), args);
    if !c.def().runnable() {
        anyhow::bail!("no such command");
    }
    let m = parse_flags(&c, &rest)?;
    c.def().doc.args.check(&c, &m, &positionals(&m))
}

/// Splits a help example into shell words, honoring double quotes and dropping a
/// trailing "# comment" or "< input" or "> output" redirect.
fn example_words(s: &str) -> Vec<String> {
    let s = s.split(" < ").next().unwrap_or(s);
    let s = s.split(" > ").next().unwrap_or(s);
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut in_word, mut quoted) = (false, false);
    for r in s.chars() {
        match r {
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            _ if quoted => word.push(r),
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '#' if !in_word => return words,
            _ => {
                word.push(r);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

// Every command's short description is one line, and every command but help is
// in a group, so "agent help" lists it by task.
#[test]
fn help_layout() {
    for sub in &tree().subs {
        assert!(
            !sub.doc.short.is_empty() && !sub.doc.short.contains('\n'),
            "{}: Short = {:?}",
            sub.name(),
            sub.doc.short
        );
        if sub.name() != "help" && sub.name() != "completion" {
            assert!(!sub.group_id.is_empty(), "{} has no group", sub.name());
        }
    }
}

// Each command's parser is one clap accepts, and every flag shows as pflag shows
// it: a bool, or a value named by its pflag type, which help prints after it.
#[test]
fn flags_are_declared_as_pflag_types() {
    const TYPES: [&str; 5] = ["string", "int", "duration", "strings", "stringArray"];
    for c in all_commands() {
        let parser = c.def().clap_command();
        parser.clone().debug_assert();
        for arg in parser.get_arguments() {
            if arg.is_positional() {
                assert_eq!(
                    arg.get_id(),
                    "args",
                    "{}: positional arguments go in args: Vec<String>",
                    c.command_path()
                );
                continue;
            }
            assert!(
                arg.get_long().is_some(),
                "{}: every flag has a long name",
                c.command_path()
            );
            if !flags::is_bool(arg) {
                let name = arg
                    .get_value_names()
                    .and_then(|v| v.first())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                assert!(
                    TYPES.contains(&name.as_str()),
                    "{} --{}: value_name {name:?} isn't a pflag type ({TYPES:?})",
                    c.command_path(),
                    arg.get_long().unwrap_or_default()
                );
            }
        }
    }
}

#[test]
fn every_leaf_runs_or_says_it_is_not_ported() {
    // Each command's Def can parse its own Args struct from an empty line: the
    // leaf! wiring of Doc, Args and run agrees.
    fn leaves(d: &'static Def, c: Cmd<'static>, out: &mut Vec<Cmd<'static>>) {
        if d.subs.is_empty() {
            out.push(c.clone());
        }
        for s in &d.subs {
            leaves(s, c.child(s), out);
        }
    }
    let mut all = Vec::new();
    leaves(tree(), Cmd::root(tree()), &mut all);
    assert!(all.len() > 50, "{} leaves", all.len());
    for c in all {
        let m = parse_flags(&c, &[]).unwrap_or_else(|e| panic!("{}: {e}", c.command_path()));
        let _ = check_args(&c, &m, &positionals(&m));
    }
}

#[test]
fn spawn_summary() {
    let mut agent = agent_store::Agent {
        name: "lead-review".into(),
        session_name: "agent-lead-review".into(),
        backend: "claude".into(),
        ..Default::default()
    };
    let tests = [
        ("", "", "", "", "Spawned agent lead-review (session agent-lead-review, backend claude).
It has no task yet: agent send lead-review \"...\"
"),
        ("lead", "review-auth", "tasks/review.md", "", "Spawned agent lead-review (session agent-lead-review, backend claude), a helper of lead.
Sent it task review-auth.
"),
        ("lead", "", "tasks/review.md", "", "Spawned agent lead-review (session agent-lead-review, backend claude), a helper of lead.
Asked it to read tasks/review.md.
"),
        ("", "", "", "3f34adff", "Spawned agent lead-review (session agent-lead-review, backend claude).
It continues session 3f34adff.
"),
    ];
    for (parent, task, task_file, resume, want) in tests {
        agent.spawned_by = parent.into();
        assert_eq!(
            super::spawn::spawn_summary(&agent, "", task, task_file, resume),
            want,
            "spawn_summary({parent:?}, {task:?}, {task_file:?}, {resume:?})"
        );
    }
}

#[test]
fn check_resumable() {
    use agent_discovery::{ParentSession, STATUS_ENDED, Session, Workgroup};
    let dir = super::testenv::temp_dir("agent-cli-resumable-");
    let d = dir.path().to_string_lossy().into_owned();
    let ok = Session {
        session_id: "s1".into(),
        harness: "claude".into(),
        cwd: d.clone(),
        status: STATUS_ENDED.into(),
        ..Default::default()
    };
    let with = |f: &dyn Fn(&mut Session)| {
        let mut s = ok.clone();
        f(&mut s);
        s
    };
    let tests: Vec<(&str, Session, &str, &str)> = vec![
        ("an exited claude session", ok.clone(), "claude", ""),
        (
            "an exited agy session",
            with(&|s| s.harness = "agy".into()),
            "agy",
            "",
        ),
        (
            "the wrong backend",
            ok.clone(),
            "agy",
            "spawn it with --backend claude",
        ),
        (
            "a harness that can't resume",
            with(&|s| s.harness = "kimi".into()),
            "kimi",
            "continues claude and agy sessions",
        ),
        (
            "still running",
            with(&|s| s.status = "idle".into()),
            "claude",
            "is idle, not ended",
        ),
        (
            "run by a workgroup agent",
            with(&|s| {
                s.status = "processing".into();
                s.workgroup = Some(Workgroup {
                    name: "lead".into(),
                    ..Default::default()
                });
            }),
            "claude",
            "(agent lead runs it)",
        ),
        (
            "unknown is not exited",
            with(&|s| s.status = "unknown".into()),
            "claude",
            "quit the CLI running it first",
        ),
        (
            "a subagent",
            with(&|s| {
                s.parent_session = Some(ParentSession {
                    session_id: "p1".into(),
                    ..Default::default()
                })
            }),
            "claude",
            "resume the session that started it, p1",
        ),
        (
            "no working directory",
            with(&|s| s.cwd = String::new()),
            "claude",
            "no working directory",
        ),
        (
            "a directory that's gone",
            with(&|s| s.cwd = format!("{d}/gone")),
            "claude",
            "no longer a directory",
        ),
    ];
    for (name, s, backend, want) in tests {
        let res = super::spawn::check_resumable(&s, backend);
        match (want, res) {
            ("", Err(e)) => panic!("{name}: check_resumable = {e:#}, want Ok"),
            ("", Ok(())) => {}
            (want, Ok(())) => {
                panic!("{name}: check_resumable = Ok, want an error containing {want:?}")
            }
            (want, Err(e)) => assert!(
                format!("{e:#}").contains(want),
                "{name}: check_resumable = {e:#}, want an error containing {want:?}"
            ),
        }
    }
}

use chrono::TimeZone;

use super::*;
use crate::stale::AgentCommand;

fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 3, day, hour, 0, 0).unwrap()
}

fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|a| a.to_string()).collect()
}

fn tree() -> AgentCommands {
    let mut t = AgentCommands::default();
    let mut add = |path: &str, subs: &[&str]| {
        t.commands.insert(
            path.to_string(),
            AgentCommand {
                subs: subs.iter().map(|s| s.to_string()).collect(),
                needs_sub: !subs.is_empty(),
                flags: Default::default(),
            },
        );
    };
    add("", &["logs", "send", "dream", "wait", "run"]);
    add("logs", &[]);
    add("send", &[]);
    add("wait", &[]);
    add("run", &[]);
    add("dream", &["memory", "commit"]);
    add("dream memory", &["put", "get"]);
    add("dream memory put", &[]);
    add("dream commit", &[]);
    t
}

#[test]
fn normalizes_what_varies() {
    let cases = [
        (
            r#"invalid agent name "/srv/x/0a1b2c3d-1111-2222-3333-444455556666.jsonl": use letters"#,
            r#"invalid agent name "…": use letters"#,
        ),
        (
            "Error: timed out after 9m0s waiting for alpha-beta-gamma to be idle",
            "timed out after NmNs waiting for <name> to be idle",
        ),
        (
            "<tool_use_error>String to replace not found in file.\nString: foo</tool_use_error>",
            "String to replace not found in file.",
        ),
        (
            "job 85 (/srv/app/check.sh) exited 1 (see job 85)",
            "job N (<path>) exited N (see job N)",
        ),
        (
            "unknown commit deadbeef12 in /srv/app, see `git log`",
            "unknown commit <id> in <path>, see `…`",
        ),
        (
            "agent 'zed' not found; did you mean one-two-three, one-two-four, or one-two-five?",
            "agent '…' not found; did you mean <names>?",
        ),
        ("\n\n  first real line\nsecond", "first real line"),
        (
            "typed into lead-build, but use --dry-run or --no-brief",
            "typed into <name>, but use --dry-run or --no-brief",
        ),
        (
            "the user doesn't want to proceed",
            "the user doesn't want to proceed",
        ),
    ];
    for (input, want) in cases {
        assert_eq!(normalize_error(input), want, "{input}");
    }
    assert!(normalize_error("export API_TOKEN=abc failed").starts_with("(withheld"));
    assert!(normalize_error(&"x".repeat(300)).chars().count() <= ERROR_CHARS);
}

#[test]
fn shapes_commands_by_tree_and_flags() {
    let t = tree();
    assert_eq!(
        command_shape(
            &args(&["-q", "logs", "/a/b.jsonl", "--lines", "100"]),
            Some(&t)
        ),
        "agent logs --lines"
    );
    assert_eq!(
        command_shape(
            &args(&[
                "dream",
                "memory",
                "put",
                "f.md",
                "--expect=abc",
                "--dir",
                "/d"
            ]),
            Some(&t)
        ),
        "agent dream memory put --dir --expect"
    );
    // A message that looks like a subcommand isn't one.
    assert_eq!(
        command_shape(&args(&["send", "x", "logs -- -5 --not-a-flag"]), Some(&t)),
        "agent send"
    );
    assert_eq!(
        command_shape(&args(&["frob", "nicate", "x", "--all"]), None),
        "agent frob nicate --all"
    );
}

#[test]
fn shapes_shell_commands() {
    let cases = [
        ("cargo test -p x", "cargo test"),
        ("cd /srv/app && cargo +nightly clippy --all", "cargo clippy"),
        ("git -C /srv/app log --oneline", "git log"),
        (
            "FOO=1 RUST_LOG=x /home/u/.local/bin/agent -q send helper hi",
            "agent send",
        ),
        ("timeout 30 ./bin/agent wait x --next", "agent wait"),
        ("ls /tmp | head", "ls"),
        ("export X=1; make test", "make test"),
        ("(cd x && npm run build)", "npm run"),
        ("set -euo pipefail\n# build it\ncd x\nmake", "make"),
        ("until agent wait x --next; do true; done", "agent wait"),
        ("for f in a b; do rm \"$f\"; done", "rm"),
        ("if ! grep -q x f; then echo no; fi", "grep"),
        ("[ -f x ] && rm x", "rm"),
        ("CARGO_TARGET_DIR=/t \\\n  cargo test", "cargo test"),
        ("", ""),
    ];
    for (cmd, want) in cases {
        assert_eq!(shell_shape(cmd), want, "{cmd}");
    }
}

#[test]
fn tool_keys_and_noise() {
    assert_eq!(
        tool_key("Bash", "pkill -f server", "Exit code 144\n"),
        Some(("Bash pkill".into(), "exit 144".into()))
    );
    // Builds and tests failing are the work, not a trip.
    for dev in [
        "cargo test -p x",
        "make",
        "make check",
        "go test ./...",
        "npm run build",
    ] {
        assert_eq!(tool_key("Bash", dev, "Exit code 2\n"), None, "{dev}");
    }
    assert!(tool_key("Bash", "maker x", "Exit code 2\n").is_some());
    assert_eq!(tool_key("Bash", "grep -n foo x.rs", "Exit code 1\n"), None);
    // agy's shell tool says it its own way.
    assert_eq!(
        tool_key("run_command", "ls /nowhere", "Command exited with code 2."),
        Some(("run_command ls".into(), "exit 2".into()))
    );
    assert_eq!(
        tool_key("run_command", "rg x", "Command exited with code 1."),
        None
    );
    assert_eq!(
        tool_key(
            "run_command",
            "cargo test",
            "The tool's output was truncated due to"
        ),
        None
    );
    assert_eq!(
        tool_key("Bash", "agent wait x --next", "Exit code 2\n"),
        None
    );
    assert_eq!(
        tool_key(
            "Bash",
            "agent wait x --next",
            "Exit code 1\nError: no agent"
        ),
        Some(("Bash agent wait".into(), "exit 1".into()))
    );
    assert_eq!(
        tool_key(
            "Bash",
            "rm -rf build",
            "Permission for this action was denied. Reason: [Destructive]"
        ),
        Some((
            "Bash rm".into(),
            "Permission for this action was denied. Reason: [Destructive]".into()
        ))
    );
    assert_eq!(
        tool_key(
            "Edit",
            "",
            "<tool_use_error>File has not been read yet.</tool_use_error>"
        ),
        Some(("Edit".into(), "File has not been read yet.".into()))
    );
}

#[test]
fn groups_commands_rank_multi_source_first_and_drop_noise() {
    let t = tree();
    let fail = |agent: &str, a: &[&str], code: i64, err: &str, day: u32| FailedCommand {
        agent: agent.into(),
        args: args(a),
        exit_code: code,
        error: err.into(),
        at: at(day, 9),
    };
    let cmds = vec![
        fail(
            "a",
            &["logs", "/x/1.jsonl"],
            1,
            r#"invalid agent name "/x/1.jsonl""#,
            1,
        ),
        fail(
            "b",
            &["logs", "/x/2.jsonl"],
            1,
            r#"invalid agent name "/x/2.jsonl""#,
            3,
        ),
        fail(
            "b",
            &["logs", "/x/3.jsonl"],
            1,
            r#"invalid agent name "/x/3.jsonl""#,
            2,
        ),
        fail(
            "c",
            &["send", "nope-nope-x", "hi"],
            1,
            r#"agent "nope-nope-x" not found"#,
            2,
        ),
        fail(
            "c",
            &["send", "nope-nope-y", "hi"],
            1,
            r#"agent "nope-nope-y" not found"#,
            3,
        ),
        fail(
            "c",
            &["send", "nope-nope-z", "hi"],
            1,
            r#"agent "nope-nope-z" not found"#,
            4,
        ),
        fail("a", &["wait", "x", "--next"], 2, "timed out after 9m0s", 2),
        fail("a", &["run", "t.sh"], 101, "job 4 (t.sh) exited 101", 2),
        fail("", &["dream", "commit"], 1, "fix them", 2),
    ];
    let (groups, n) = group_commands(&cmds, Some(&t));
    assert_eq!(n, 7);
    assert_eq!(groups.len(), 3);
    // Seen from two agents leads, though the send group has as many.
    assert_eq!(groups[0].what, "agent logs");
    assert_eq!(groups[0].error, r#"invalid agent name "…""#);
    assert_eq!(groups[0].count, 3);
    assert_eq!(groups[0].sources, vec!["a", "b"]);
    assert_eq!((groups[0].first, groups[0].last), (at(1, 9), at(3, 9)));
    assert!(!groups[0].single());
    assert_eq!(groups[1].what, "agent send");
    assert!(groups[1].single());
    assert_eq!(groups[2].sources, vec!["(person)"]);
}

#[test]
fn bursts_are_a_scripts_not_an_agents() {
    let fail = |agent: &str, ms: i64| FailedCommand {
        agent: agent.into(),
        args: args(&["list", "--bogus"]),
        exit_code: 1,
        error: "unknown flag: --bogus".into(),
        at: at(1, 1) + chrono::TimeDelta::milliseconds(ms),
    };
    // Three by one agent within a second are a burst; two aren't, nor are
    // three spread out, nor three by different agents.
    let cmds = vec![
        fail("t", 0),
        fail("t", 10),
        fail("u", 15),
        fail("t", 20),
        fail("u", 5000),
        fail("v", 0),
        fail("v", 1500),
        fail("v", 3000),
        fail("t", 9000),
    ];
    assert_eq!(
        bursts(&cmds),
        [true, true, false, true, false, false, false, false, false]
    );
    let (groups, n) = group_commands(&cmds, None);
    assert_eq!(n, 6);
    assert_eq!(groups[0].sources, vec!["t", "u", "v"]);
}

#[test]
fn groups_are_capped() {
    let tools: Vec<FailedTool> = (0..40)
        .map(|i| FailedTool {
            session: format!("claude:s{}", i % 3),
            tool: format!("Tool{i}"),
            error: "boom".into(),
            at: at(1, 1),
            ..Default::default()
        })
        .collect();
    let (groups, n) = group_tools(&tools);
    assert_eq!(n, 40);
    assert_eq!(groups.len(), MAX_GROUPS);
}

#[test]
fn messages_drop_envelopes() {
    assert_eq!(
        messages(
            "[from lead@hostx] lead: no, use the other branch … [gomuxai message #12, 3.5 KB]"
        ),
        vec!["no, use the other branch"]
    );
    assert_eq!(
        messages(
            "2 messages arrived while you were working, oldest first:\n\n[from a@h] first\n[from b@h] don't push"
        ),
        vec!["first", "don't push"]
    );
    assert_eq!(messages("plain text"), vec!["plain text"]);
}

#[test]
fn finds_corrections() {
    for yes in [
        "No, that's the wrong file",
        "don't run the full suite",
        "Stop editing main.",
        "Actually, keep the old name",
        "I said the worktree, not the checkout",
        "Thanks, but that's not what I asked",
        "Please do not push yet",
        "again: use Edit, not sed",
    ] {
        assert!(is_correction(yes), "{yes}");
    }
    for no in [
        "/clear",
        "Nothing else to do today",
        "Notes are in the doc",
        "Please read and execute the task in tasks/a.md, which covers a long list of things; don't skip",
        "Know what? Build it.",
        "No pending edits here, you are clear",
        "Nope, those are all good. Where are we?",
        "phone: done, the hand button passes full, the others don't.",
        "Reply with exactly the word pong. Do not run any commands.",
    ] {
        assert!(!is_correction(no), "{no}");
    }
}

#[test]
fn corrections_are_capped_per_session_and_scrubbed() {
    let mut prompts = Vec::new();
    for h in 0..5 {
        prompts.push(Prompt {
            session: "claude:one".into(),
            text: format!("no, attempt {h} was wrong"),
            at: at(2, h),
        });
    }
    prompts.push(Prompt {
        session: "claude:two".into(),
        text: "don't paste this\n<pasted_content id=1>\nsecret stuff\n</pasted_content>\nok".into(),
        at: at(1, 0),
    });
    prompts.push(Prompt {
        session: "claude:two".into(),
        text: "great work".into(),
        at: at(1, 1),
    });
    let cs = corrections(&prompts);
    assert_eq!(cs.len(), MAX_CORRECTIONS_PER_SESSION + 1);
    assert_eq!(cs[0].excerpt, "no, attempt 4 was wrong");
    assert_eq!(cs[2].excerpt, "no, attempt 2 was wrong");
    assert_eq!(cs[3].session, "claude:two");
    assert_eq!(cs[3].excerpt, "don't paste this ok");
}

#[test]
fn formats_a_bounded_section() {
    assert_eq!(format(&Evidence::default()), "");
    let g = |what: &str, sources: &[&str]| Group {
        what: what.into(),
        error: "it broke".into(),
        count: 4,
        sources: sources.iter().map(|s| s.to_string()).collect(),
        first: at(1, 1),
        last: at(3, 1),
        projects: Vec::new(),
    };
    let e = Evidence {
        since: Some(at(1, 0)),
        commands_failed: 6,
        commands: vec![g("agent logs", &["a", "b"]), g("agent send", &["c"])],
        sessions_read: 2,
        tools_failed: 4,
        tools: vec![g(
            "Bash cargo test",
            &["claude:1", "claude:2", "claude:3", "claude:4", "claude:5"],
        )],
        corrections: vec![Correction {
            session: "claude:1".into(),
            at: at(2, 5),
            excerpt: "no, the other one".into(),
        }],
    };
    let s = format(&e);
    assert!(s.starts_with("\n## evidence\n"), "{s}");
    assert!(
        s.contains("- 4× `agent logs`: it broke\n  2 agents (03-01 to 03-03): `a`, `b`\n"),
        "{s}"
    );
    assert!(
        s.contains("- Seen in one agent only:\n- 4× `agent send`"),
        "{s}"
    );
    assert!(
        s.contains(
            "5 sessions (03-01 to 03-03): `claude:1`, `claude:2`, `claude:3`, `claude:4`, +1 more"
        ),
        "{s}"
    );
    assert!(
        s.contains("- `claude:1` 03-02 05:00: no, the other one"),
        "{s}"
    );

    // Many long groups stay within the budget.
    let many: Vec<Group> = (0..MAX_GROUPS)
        .map(|i| Group {
            error: "e".repeat(ERROR_CHARS),
            ..g(&format!("agent x{i}"), &["a", "b"])
        })
        .collect();
    let big = Evidence {
        commands: many.clone(),
        tools: many,
        corrections: (0..MAX_CORRECTIONS)
            .map(|i| Correction {
                session: format!("claude:{i}"),
                at: at(1, 1),
                excerpt: "x".repeat(CORRECTION_CHARS),
            })
            .collect(),
        ..Default::default()
    };
    let s = format(&big);
    assert!(s.len() <= MAX_BYTES + 200, "{}", s.len());
    assert!(s.contains("more left out for space"), "{s}");
}

#[test]
fn pooled_groups_name_their_projects() {
    let mut groups = vec![Group {
        what: "Bash pkill".into(),
        sources: vec!["claude:1".into(), "claude:2".into(), "helper".into()],
        ..Default::default()
    }];
    let mut of: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    of.entry("claude:1".into())
        .or_default()
        .insert("/srv/app".into());
    of.entry("claude:2".into())
        .or_default()
        .insert("/srv/tools".into());
    of.entry("helper".into())
        .or_default()
        .insert("/srv/app".into());
    set_projects(&mut groups, &of);
    assert_eq!(groups[0].projects, ["/srv/app", "/srv/tools"]);
    let s = format(&Evidence {
        tools: groups,
        ..Default::default()
    });
    assert!(
        s.contains("  in 2 projects: `/srv/app`, `/srv/tools`\n"),
        "{s}"
    );
}

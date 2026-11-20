use chrono::TimeDelta;

use crate::tree_test::new_test_store;
use crate::*;

#[test]
fn command_log() {
    let (s, _, _dir) = new_test_store();
    s.create_agent(&Agent {
        name: "lead-review".into(),
        session_name: "agent-lead-review".into(),
        backend: "claude".into(),
        spawned_by: "lead".into(),
        created_at: Time::now(),
        ..Default::default()
    })
    .unwrap();

    let start = Time::date_utc(2026, 10, 24, 14, 0, 0, 0);
    let rec =
        |at: Time, agent: &str, cwd: &str, args: &[&str], exit: i64, error: &str| CommandRecord {
            started_at: at,
            agent: agent.into(),
            cwd: cwd.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            exit_code: exit,
            error: error.into(),
            host: "h".into(),
            build: "c7253f9".into(),
            ..Default::default()
        };
    let mut logged = vec![
        rec(start, "", "/w", &["list"], 0, ""),
        rec(
            start + TimeDelta::minutes(1),
            "lead-review",
            "/w",
            &["wait", "x", "--for", "idel"],
            1,
            r#"invalid status "idel""#,
        ),
        rec(
            start + TimeDelta::minutes(2),
            "lead",
            "/w",
            &["send", "lead-review", "hi"],
            0,
            "",
        ),
        rec(start + TimeDelta::minutes(3), "", "/v", &[], 2, "timed out"),
    ];
    for r in &mut logged {
        s.log_command(r).unwrap();
    }
    assert_eq!(
        logged[1].parent, "lead",
        "a spawned agent's command: Parent"
    );
    assert_eq!(
        logged[2].parent, "",
        "an unlisted agent's command: Parent, want none"
    );

    let ids = |f: CommandFilter| -> Vec<i64> {
        s.list_commands(&f).unwrap().iter().map(|r| r.id).collect()
    };
    let tests: Vec<(&str, CommandFilter, Vec<i64>)> = vec![
        (
            "all, newest first",
            CommandFilter::default(),
            vec![4, 3, 2, 1],
        ),
        (
            "limit",
            CommandFilter {
                limit: 2,
                ..Default::default()
            },
            vec![4, 3],
        ),
        (
            "an agent's",
            CommandFilter {
                by_agent: true,
                agent: "lead-review".into(),
                ..Default::default()
            },
            vec![2],
        ),
        (
            "a person's",
            CommandFilter {
                by_agent: true,
                ..Default::default()
            },
            vec![4, 1],
        ),
        (
            "a person's in a directory",
            CommandFilter {
                by_agent: true,
                cwd: "/w".into(),
                ..Default::default()
            },
            vec![1],
        ),
        (
            "failed",
            CommandFilter {
                failed: true,
                ..Default::default()
            },
            vec![4, 2],
        ),
        (
            "since",
            CommandFilter {
                since: start + TimeDelta::seconds(90),
                ..Default::default()
            },
            vec![4, 3],
        ),
    ];
    for (name, f, want) in tests {
        assert_eq!(ids(f), want, "{name}: ids");
    }

    let recs = s
        .list_commands(&CommandFilter {
            limit: 3,
            ..Default::default()
        })
        .unwrap();
    let (got, want) = (&recs[2], &logged[1]);
    assert!(
        got.started_at == want.started_at
            && got.args == want.args
            && got.exit_code == 1
            && got.error == want.error
            && got.agent == "lead-review"
            && got.parent == "lead"
            && got.build == "c7253f9",
        "read back {got:?}, want {want:?}"
    );
    assert!(
        recs[0].args.is_empty(),
        "no args read back as {:?}, want an empty list",
        recs[0].args
    );
}

#[test]
fn command_log_prunes() {
    let (mut s, _, _dir) = new_test_store();
    s.command_log_keep = 10;
    s.command_log_prune_every = 4;
    for _ in 0..25 {
        s.log_command(&mut CommandRecord {
            started_at: Time::now(),
            args: vec!["list".into()],
            ..Default::default()
        })
        .unwrap();
    }
    let recs = s.list_commands(&CommandFilter::default()).unwrap();
    // The last prune ran at id 24 and kept ids 15 to 24; 25 came after.
    assert!(
        recs.len() == 11 && recs[recs.len() - 1].id == 15,
        "kept {} records, oldest id {}; want 11, from 15",
        recs.len(),
        recs[recs.len() - 1].id
    );
}

/// Args are stored as Go's json.Marshal writes them, HTML characters
/// escaped, and text that isn't a JSON list reads back as one argument.
#[test]
fn command_log_args_json() {
    let (s, _, _dir) = new_test_store();
    s.log_command(&mut CommandRecord {
        started_at: Time::now(),
        args: vec!["send".into(), "x".into(), "a && <b>".into()],
        ..Default::default()
    })
    .unwrap();
    {
        let conn = s.conn();
        let args: String = conn
            .query_row("SELECT args FROM command_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            args,
            "[\"send\",\"x\",\"a \\u0026\\u0026 \\u003cb\\u003e\"]"
        );
        conn.execute("UPDATE command_log SET args = 'not json'", [])
            .unwrap();
    }
    let recs = s.list_commands(&CommandFilter::default()).unwrap();
    assert_eq!(recs[0].args, ["not json"]);
}

use chrono::TimeDelta;

use crate::tree_test::new_test_store;
use crate::*;

#[test]
fn friction() {
    let (s, _, _dir) = new_test_store();

    let last = s.last_friction_id().unwrap();
    assert_eq!(last, 0, "LastFrictionID on an empty box");
    let f = s.get_friction(1).unwrap();
    assert!(
        f.is_none(),
        "GetFriction(1) on an empty box = {f:?}; want none"
    );

    let mut now = Time::date_utc(2026, 10, 24, 14, 0, 0, 0);
    let mut file = |uid: &str, by: &str, kind: &str, summary: &str| -> Friction {
        let mut f = Friction {
            uid: uid.into(),
            host: "h".into(),
            filed_by: by.into(),
            build: "c7253f9".into(),
            created_at: now,
            updated_at: now,
            kind: kind.into(),
            summary: summary.into(),
            status: FRICTION_OPEN.into(),
            ..Default::default()
        };
        s.add_friction(&mut f).unwrap();
        now = now + TimeDelta::minutes(1);
        f
    };
    let mut a = file("h-a", "lead", "help", "a");
    let b = file("h-b", "user", "error", "b");
    let mut c = file("h-c", "lead", "error", "c");
    let mut d = file("h-d", "lead", "bug", "d");
    let now = Time::date_utc(2026, 10, 24, 14, 4, 0, 0);
    let vote = |id: i64, uid: &str| {
        s.add_friction_vote(&mut FrictionVote {
            friction_id: id,
            uid: uid.into(),
            host: "h".into(),
            voter: "x".into(),
            created_at: now,
            ..Default::default()
        })
        .unwrap();
    };
    vote(b.id, "v1");
    vote(c.id, "v2");
    vote(c.id, "v3");
    vote(d.id, "v4");

    assert!(
        s.add_friction(&mut Friction {
            uid: "h-a".into(),
            created_at: now,
            updated_at: now,
            status: FRICTION_OPEN.into(),
            ..Default::default()
        })
        .is_err(),
        "filed a second entry with the same uid"
    );

    let ids = |f: FrictionFilter| -> Vec<i64> {
        s.list_friction(&f).unwrap().iter().map(|e| e.id).collect()
    };
    let votes = |id: i64| -> i64 { s.get_friction(id).unwrap().unwrap().votes };

    assert_eq!(
        ids(FrictionFilter::default()),
        [c.id, d.id, b.id, a.id],
        "most votes, then newest"
    );
    assert_eq!(
        ids(FrictionFilter {
            kind: "error".into(),
            filed_by: "lead".into(),
            ..Default::default()
        }),
        [c.id],
        "by kind and filer"
    );

    // d is a duplicate of c, then c of b: b's count takes in both, and d
    // moves onto b.
    let resolve_dup = |f: &mut Friction, of: i64| {
        f.status = FRICTION_DUPLICATE.into();
        f.duplicate_of = of;
        f.resolved_by = "user".into();
        f.resolved_at = now;
        f.updated_at = now;
        s.update_friction(f).unwrap();
    };
    resolve_dup(&mut d, c.id);
    assert_eq!(
        votes(c.id),
        4,
        "after d dup of c: c's votes (its 2, d's 1, and d itself)"
    );
    resolve_dup(&mut c, b.id);
    assert_eq!(
        votes(b.id),
        6,
        "after c dup of b: b's votes (its 1, c's 2 + c, d's 1 + d)"
    );
    assert_eq!(
        ids(FrictionFilter {
            duplicate_of: b.id,
            ..Default::default()
        }),
        [c.id, d.id],
        "duplicates of b"
    );
    assert_eq!(
        ids(FrictionFilter {
            statuses: vec![FRICTION_OPEN.into(), FRICTION_ACCEPTED.into()],
            ..Default::default()
        }),
        [b.id, a.id],
        "open"
    );

    a.status = FRICTION_ACCEPTED.into();
    a.accepted_by = "lead".into();
    a.accepted_at = now;
    a.accept_note = "soon".into();
    // Updates leave the filing alone.
    a.command = Some(AttachedCommand {
        args: vec!["wait".into(), "x".into()],
        exit_code: 2,
        error: "timed out".into(),
        at: now,
    });
    s.update_friction(&a).unwrap();
    let got = s.get_friction(a.id).unwrap().unwrap();
    assert!(
        got.status == FRICTION_ACCEPTED
            && got.accepted_by == "lead"
            && got.accepted_at == now
            && got.accept_note == "soon"
            && got.resolved_at.is_zero()
            && got.duplicate_of == 0
            && got.command.is_none(),
        "accepted entry read back as {got:?}"
    );

    let vs = s.list_friction_votes(c.id).unwrap();
    assert!(
        vs.len() == 2 && vs[0].uid == "v2",
        "c's votes = {vs:?}; want v2, v3"
    );
    assert_eq!(s.last_friction_id().unwrap(), d.id, "LastFrictionID");
}

#[test]
fn friction_attached_command_round_trips() {
    let (s, _, _dir) = new_test_store();
    let at = Time::date_utc(2026, 10, 24, 14, 0, 0, 0);
    let cmd = AttachedCommand {
        args: vec!["send".into(), "x".into(), "hi there".into()],
        exit_code: 1,
        error: r#"agent "x" not found"#.into(),
        at,
    };
    let mut f = Friction {
        uid: "u".into(),
        created_at: at,
        updated_at: at,
        summary: "s".into(),
        status: FRICTION_OPEN.into(),
        command: Some(cmd.clone()),
        ..Default::default()
    };
    s.add_friction(&mut f).unwrap();
    s.add_friction_vote(&mut FrictionVote {
        friction_id: f.id,
        uid: "v".into(),
        created_at: at,
        command: Some(cmd.clone()),
        ..Default::default()
    })
    .unwrap();
    let got = s.get_friction(f.id).unwrap().unwrap();
    let vs = s.list_friction_votes(f.id).unwrap();
    for c in [&got.command, &vs[0].command] {
        assert!(
            matches!(c, Some(c) if c.args == cmd.args && c.exit_code == 1 && c.error == cmd.error && c.at == at),
            "attached command read back as {c:?}, want {cmd:?}"
        );
    }
    // Stored as Go's json.Marshal writes it.
    let stored: String = s
        .conn()
        .query_row("SELECT command FROM friction", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        stored,
        r#"{"args":["send","x","hi there"],"exit_code":1,"error":"agent \"x\" not found","at":"2026-10-24T14:00:00Z"}"#
    );
}

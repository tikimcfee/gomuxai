use chrono::TimeDelta;

use crate::tree_test::new_test_store;
use crate::*;

#[test]
fn roster_entries() {
    let (s, _, _dir) = new_test_store();
    assert!(s.list_roster_entries().unwrap().is_empty());
    assert!(s.get_roster_entry("claude", "m", "").unwrap().is_none());

    let now = Time::date_utc(2026, 11, 22, 10, 0, 0, 0);
    let mut e = RosterEntry {
        harness: "claude".into(),
        model: "claude-haiku-5-5".into(),
        good_at: "small precise edits".into(),
        tasks: vec!["edit".into(), "test".into()],
        speed: "fast".into(),
        cost: "low".into(),
        enabled: true,
        set_by: "user".into(),
        updated_at: now,
        ..Default::default()
    };
    s.put_roster_entry(&e).unwrap();
    // The same model at another effort is its own row.
    let low = RosterEntry {
        effort: "low".into(),
        ..e.clone()
    };
    s.put_roster_entry(&low).unwrap();
    s.put_roster_entry(&RosterEntry {
        harness: "agy".into(),
        model: String::new(),
        enabled: false,
        updated_at: now,
        ..Default::default()
    })
    .unwrap();

    let got = s
        .get_roster_entry("claude", "claude-haiku-5-5", "")
        .unwrap();
    assert_eq!(got.as_ref(), Some(&e));
    let keys: Vec<String> = s
        .list_roster_entries()
        .unwrap()
        .iter()
        .map(|e| format!("{}/{}@{}:{}", e.harness, e.model, e.effort, e.enabled))
        .collect();
    assert_eq!(
        keys,
        [
            "agy/@:false",
            "claude/claude-haiku-5-5@:true",
            "claude/claude-haiku-5-5@low:true"
        ]
    );

    // Putting it again replaces it.
    e.avoid = "deep debugging".into();
    e.tasks = vec![];
    e.updated_at = now + TimeDelta::minutes(1);
    s.put_roster_entry(&e).unwrap();
    let got = s
        .get_roster_entry("claude", "claude-haiku-5-5", "")
        .unwrap()
        .unwrap();
    assert_eq!(got, e);
    assert_eq!(s.list_roster_entries().unwrap().len(), 3);

    assert!(
        s.remove_roster_entry("claude", "claude-haiku-5-5", "low")
            .unwrap()
    );
    assert!(
        !s.remove_roster_entry("claude", "claude-haiku-5-5", "low")
            .unwrap()
    );
    assert_eq!(s.list_roster_entries().unwrap().len(), 2);
}

#[test]
fn roster_notes() {
    let (s, _, _dir) = new_test_store();
    assert!(s.list_roster_notes().unwrap().is_empty());
    assert!(s.get_roster_note(1).unwrap().is_none());

    let now = Time::date_utc(2026, 11, 22, 10, 0, 0, 0);
    let mut a = RosterNote {
        harness: "claude".into(),
        model: "claude-haiku-5-5".into(),
        effort: "low".into(),
        task: "edit".into(),
        verdict: "good".into(),
        text: "did the rename across 12 files cleanly".into(),
        source: "claude:3f3a9c".into(),
        by: "lead".into(),
        created_at: now,
        ..Default::default()
    };
    s.add_roster_note(&mut a).unwrap();
    assert_eq!(a.id, 1);
    let mut b = RosterNote {
        verdict: "bad".into(),
        why: "missed a caller".into(),
        supersedes: a.id,
        effort: String::new(),
        ..a.clone()
    };
    s.add_roster_note(&mut b).unwrap();
    assert_eq!(b.id, 2);

    assert_eq!(s.get_roster_note(2).unwrap().as_ref(), Some(&b));
    assert_eq!(s.list_roster_notes().unwrap(), vec![a.clone(), b.clone()]);
    assert_eq!(
        s.list_roster_notes_for("claude", "claude-haiku-5-5", "low")
            .unwrap(),
        vec![a.clone()]
    );

    // Removing an entry leaves its notes.
    s.put_roster_entry(&RosterEntry {
        harness: "claude".into(),
        model: "claude-haiku-5-5".into(),
        effort: "low".into(),
        enabled: true,
        updated_at: now,
        ..Default::default()
    })
    .unwrap();
    s.remove_roster_entry("claude", "claude-haiku-5-5", "low")
        .unwrap();
    assert_eq!(s.list_roster_notes().unwrap().len(), 2);
}

#[test]
fn roster_picks() {
    let (s, _, _dir) = new_test_store();
    assert!(s.get_roster_pick("quick", 0).unwrap().is_none());
    assert!(s.list_roster_picks("").unwrap().is_empty());

    let now = Time::date_utc(2026, 11, 22, 10, 0, 0, 0);
    let pick = |name: &str, version: i64, model: &str| RosterPick {
        name: name.into(),
        version,
        argv: [
            "agent",
            "spawn",
            "NAME",
            "--backend",
            "claude",
            "--model",
            model,
        ]
        .map(String::from)
        .to_vec(),
        harness: "claude".into(),
        model: model.into(),
        host: "h".into(),
        task: "edit".into(),
        saved_by: "lead".into(),
        saved_at: now,
        ..Default::default()
    };
    let mut a1 = pick("quick", 1, "haiku");
    s.add_roster_pick(&mut a1).unwrap();
    let mut a2 = pick("quick", 2, "sonnet");
    s.add_roster_pick(&mut a2).unwrap();
    let mut b1 = pick("deep", 1, "opus");
    s.add_roster_pick(&mut b1).unwrap();
    assert!(s.add_roster_pick(&mut pick("quick", 2, "x")).is_err());

    assert_eq!(s.get_roster_pick("quick", 0).unwrap().as_ref(), Some(&a2));
    assert_eq!(s.get_roster_pick("quick", 1).unwrap().as_ref(), Some(&a1));
    assert!(s.get_roster_pick("quick", 3).unwrap().is_none());
    assert_eq!(s.list_roster_picks("").unwrap(), vec![b1, a2.clone()]);
    assert_eq!(s.list_roster_picks("quick").unwrap(), vec![a2, a1]);
}

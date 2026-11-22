use crate::tree_test::new_test_store;
use crate::*;

fn item(kind: &str, key: &str, fp: &str) -> DreamItem {
    DreamItem {
        kind: kind.into(),
        key: key.into(),
        fingerprint: fp.into(),
        summary: format!("{kind} {key}"),
        read: String::new(),
    }
}

#[test]
fn dreams_open_commit_and_review() {
    let (s, _, _dir) = new_test_store();
    assert!(s.list_dreams(0).unwrap().is_empty());
    assert!(s.last_committed_dream().unwrap().is_none());
    assert!(!s.is_reviewed("session", "claude:a", "10@t1").unwrap());

    let d = s
        .open_dream(
            "h",
            &[
                item("session", "claude:a", "10@t1"),
                item("friction", "h-1", "t@0"),
            ],
            |id| format!("dream-{id}"),
        )
        .unwrap();
    assert_eq!(d.status, DREAM_OPEN);
    assert_eq!(d.agent, format!("dream-{}", d.id));
    assert_eq!(d.items, 2);
    assert!(d.finished_at.is_zero());
    assert_eq!(s.open_dream_of(&d.agent).unwrap().map(|o| o.id), Some(d.id));
    assert!(s.open_dream_of("someone").unwrap().is_none());
    assert_eq!(s.open_dreams().unwrap().len(), 1);

    // Open: nothing is reviewed yet.
    assert!(!s.is_reviewed("session", "claude:a", "10@t1").unwrap());

    s.append_dream_diary(d.id, "first").unwrap();
    s.append_dream_diary(d.id, "second").unwrap();
    // The dream triaged a friction entry: reviewed as it left it.
    s.set_dream_item(d.id, &item("friction", "h-1", "t2@1"))
        .unwrap();
    s.set_dream_item(d.id, &item("friction", "h-2", "t@0"))
        .unwrap();
    assert_eq!(s.dream_items(d.id).unwrap().len(), 3);

    s.commit_dream(d.id).unwrap();
    let got = s.get_dream(d.id).unwrap().unwrap();
    assert_eq!(got.status, DREAM_COMMITTED);
    assert_eq!(got.diary, "first\nsecond");
    assert!(!got.finished_at.is_zero());
    assert_eq!(s.last_committed_dream().unwrap().map(|d| d.id), Some(d.id));
    assert!(s.open_dream_of(&d.agent).unwrap().is_none());

    assert!(s.is_reviewed("session", "claude:a", "10@t1").unwrap());
    assert!(
        !s.is_reviewed("session", "claude:a", "20@t2").unwrap(),
        "changed since"
    );
    assert!(s.is_reviewed("friction", "h-1", "t2@1").unwrap());
    assert!(
        !s.is_reviewed("friction", "h-1", "t@0").unwrap(),
        "the old fingerprint"
    );

    // Closed dreams take no more notes, items, or closing.
    assert!(s.append_dream_diary(d.id, "late").is_err());
    assert!(s.set_dream_item(d.id, &item("memory", "/m", "1")).is_err());
    let err = s.commit_dream(d.id).unwrap_err();
    assert!(
        format!("{err:#}").contains("is committed, not open"),
        "{err:#}"
    );
    assert!(s.abandon_dream(999, "").is_err());

    // A newer review stands over an older one.
    let d2 = s
        .open_dream("h", &[item("session", "claude:a", "20@t2")], |id| {
            format!("dream-{id}")
        })
        .unwrap();
    s.commit_dream(d2.id).unwrap();
    let reviewed = s.reviewed_fingerprints().unwrap();
    assert_eq!(
        reviewed
            .get(&("session".to_string(), "claude:a".to_string()))
            .map(String::as_str),
        Some("20@t2")
    );
    assert_eq!(reviewed.len(), 3);
}

#[test]
fn dreams_abandoned_and_quiet() {
    let (s, _, _dir) = new_test_store();
    let d = s
        .open_dream("h", &[item("session", "claude:a", "1")], |id| {
            format!("dream-{id}")
        })
        .unwrap();
    s.abandon_dream(d.id, "its agent is gone").unwrap();
    let got = s.get_dream(d.id).unwrap().unwrap();
    assert_eq!(got.status, DREAM_ABANDONED);
    assert_eq!(got.diary, "its agent is gone");
    assert!(
        !s.is_reviewed("session", "claude:a", "1").unwrap(),
        "abandoned reviews nothing"
    );
    assert!(s.reviewed_fingerprints().unwrap().is_empty());

    let q = s.record_quiet_dream("h", "nothing new").unwrap();
    assert_eq!(q.status, DREAM_QUIET);
    assert_eq!(q.agent, "");
    assert_eq!(q.items, 0);
    assert!(s.last_committed_dream().unwrap().is_none());

    let ids: Vec<i64> = s.list_dreams(0).unwrap().iter().map(|d| d.id).collect();
    assert_eq!(ids, [q.id, d.id], "newest first");
    assert_eq!(s.list_dreams(1).unwrap().len(), 1);
    assert_eq!(
        s.dream_agents().unwrap(),
        std::collections::HashSet::from([d.agent.clone()])
    );
}

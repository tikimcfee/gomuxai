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

    let q = s
        .record_quiet_dream("h", &DreamOf::project("/srv/app"), "nothing new")
        .unwrap();
    assert_eq!(
        (q.kind.as_str(), q.project.as_str()),
        ("project", "/srv/app")
    );
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

/// A store from before projects: opening it adds kind and project, and its
/// dreams read as project dreams over the whole scope.
#[test]
fn dreams_migrate_to_kinds_and_projects() {
    let dir = crate::tree_test::temp_dir();
    let path = dir.path().join("gomuxai.db");
    Store::open(&path).unwrap().close().unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    for stmt in [
        "ALTER TABLE dreams DROP COLUMN project",
        "ALTER TABLE dreams DROP COLUMN kind",
        "INSERT INTO dreams (host, agent, started_at, finished_at, status) VALUES ('h', 'dream-1', '2026-11-01T10:00:00Z', '2026-11-01T11:00:00Z', 'committed')",
    ] {
        db.execute_batch(stmt)
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    drop(db);

    let s = Store::open(&path).unwrap();
    let d = s.get_dream(1).unwrap().unwrap();
    assert_eq!(
        (d.kind.as_str(), d.project.as_str()),
        (DREAM_KIND_PROJECT, "")
    );
    // The legacy dream covered every project, and no global one.
    let of = |dir: &str| {
        s.last_committed_dream_of(&DreamOf::project(dir))
            .unwrap()
            .map(|d| d.id)
    };
    assert_eq!(of("/srv/app"), Some(1));
    assert!(
        s.last_committed_dream_of(&DreamOf::global())
            .unwrap()
            .is_none()
    );
}

#[test]
fn dreams_per_project_and_global() {
    let (s, _, _dir) = new_test_store();
    let open = |of: &DreamOf, items: &[DreamItem]| {
        s.open_dream_of_kind("h", of, items, |id| format!("dream-{id}"))
            .unwrap()
    };
    let legacy = open(&DreamOf::project(""), &[]);
    s.commit_dream(legacy.id).unwrap();
    let app = open(
        &DreamOf::project("/srv/app"),
        &[item("memory", "/m/app", "1")],
    );
    assert_eq!(
        (app.kind.as_str(), app.project.as_str()),
        ("project", "/srv/app")
    );
    s.commit_dream(app.id).unwrap();
    let last = |of: DreamOf| s.last_committed_dream_of(&of).unwrap().map(|d| d.id);
    assert_eq!(last(DreamOf::project("/srv/app")), Some(app.id));
    assert_eq!(
        last(DreamOf::project("/srv/tools")),
        Some(legacy.id),
        "the legacy dream covered it"
    );
    assert_eq!(last(DreamOf::global()), None);

    let since = s.get_dream(app.id).unwrap().unwrap().finished_at;
    let ids: Vec<i64> = s
        .committed_project_dreams_since(&since)
        .unwrap()
        .iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, [app.id]);

    let g = open(&DreamOf::global(), &[item("memory", "/m/app", "2")]);
    s.commit_dream(g.id).unwrap();
    assert_eq!(last(DreamOf::global()), Some(g.id));
    assert_eq!(
        last(DreamOf::project("/srv/app")),
        Some(app.id),
        "a global dream is no project's"
    );
    let key = ("memory".to_string(), "/m/app".to_string());
    assert_eq!(s.reviewed_fingerprints().unwrap()[&key], "2");
    assert_eq!(
        s.reviewed_fingerprints_by(DREAM_KIND_PROJECT).unwrap()[&key],
        "1"
    );
    assert_eq!(
        s.reviewed_fingerprints_by(DREAM_KIND_GLOBAL).unwrap()[&key],
        "2"
    );
}

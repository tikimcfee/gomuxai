use std::collections::HashSet;

use crate::tree_test::new_test_store;
use crate::*;

/// A peer renamed old -> new: agents it spawned here and links to its
/// friction entries follow it; other hosts' references stay.
#[test]
fn rename_host() {
    let (s, _, _dir) = new_test_store();
    let now = Time::date_utc(2026, 11, 8, 10, 0, 0, 0);

    for (name, parent) in [
        ("h1", "lead@Old-Mac"),
        ("h2", "lead@old-macro"), // another host
        ("h3", "h1"),             // local parent
    ] {
        s.create_agent(&Agent {
            name: name.into(),
            session_name: format!("agent-{name}"),
            backend: "claude".into(),
            spawned_by: parent.into(),
            created_at: now,
            ..Default::default()
        })
        .unwrap();
    }
    let entry = |uid: &str| Friction {
        uid: uid.into(),
        host: "h".into(),
        filed_by: "x".into(),
        created_at: now,
        updated_at: now,
        kind: "bug".into(),
        summary: "s".into(),
        status: FRICTION_OPEN.into(),
        ..Default::default()
    };
    let (mut f, mut g) = (entry("u1"), entry("u2"));
    s.add_friction(&mut f).unwrap();
    s.add_friction(&mut g).unwrap();
    for (id, host, peer) in [
        (f.id, "old-mac", 34),
        (f.id, "hub", 2),
        // g links to #7 under both names: the old one goes.
        (g.id, "old-mac", 7),
        (g.id, "new-mac", 7),
    ] {
        s.add_friction_link(&mut FrictionLink {
            friction_id: id,
            peer_host: host.into(),
            peer_id: peer,
            linked_by: "x".into(),
            linked_at: now,
            ..Default::default()
        })
        .unwrap();
    }

    let n = s.rename_host("OLD-MAC", "new-mac").unwrap();
    assert!(
        n.agents == 1 && n.friction_links == 2,
        "renamed = {n:?}, want 1 agent and 2 links"
    );
    for (name, parent) in [
        ("h1", "lead@new-mac"),
        ("h2", "lead@old-macro"),
        ("h3", "h1"),
    ] {
        let a = s.get_agent(name).unwrap().unwrap();
        assert_eq!(a.spawned_by, parent, "{name} spawned_by");
    }
    let tree = s.read_tree(false).unwrap();
    let r = tree.runs.get("h1");
    assert!(
        r.is_some_and(|r| r.parent == "lead@new-mac"),
        "h1's run = {r:?}, want parent lead@new-mac"
    );
    let hosts = |id: i64| -> HashSet<String> {
        s.list_friction_links(id)
            .unwrap()
            .into_iter()
            .map(|l| l.peer_host)
            .collect()
    };
    let h = hosts(f.id);
    assert!(
        h.len() == 2 && h.contains("new-mac") && h.contains("hub"),
        "#{} links to {h:?}, want new-mac and hub",
        f.id
    );
    let h = hosts(g.id);
    assert!(
        h.len() == 1 && h.contains("new-mac"),
        "#{} links to {h:?}, want new-mac only",
        g.id
    );
}

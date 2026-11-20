use chrono::TimeDelta;

use crate::tree_test::new_test_store;
use crate::*;

#[test]
fn friction_links() {
    let (s, _, _dir) = new_test_store();
    let now = Time::date_utc(2026, 11, 8, 10, 0, 0, 0);
    let hour = TimeDelta::hours(1);

    let file = |uid: &str| -> Friction {
        let mut f = Friction {
            uid: uid.into(),
            host: "h".into(),
            filed_by: "lead".into(),
            created_at: now,
            updated_at: now,
            kind: "bug".into(),
            summary: uid.into(),
            status: FRICTION_OPEN.into(),
            ..Default::default()
        };
        s.add_friction(&mut f).unwrap();
        f
    };
    let links = |id: i64| -> Vec<FrictionLink> { s.list_friction_links(id).unwrap() };
    let link = |friction_id: i64, host: &str, peer: i64| FrictionLink {
        friction_id,
        peer_host: host.into(),
        peer_id: peer,
        linked_by: "x".into(),
        linked_at: now,
        ..Default::default()
    };
    let (a, mut b, c) = (file("h-a"), file("h-b"), file("h-c"));

    // Recorded while the other host was unreachable: no uid, unconfirmed.
    let mut l = FrictionLink {
        friction_id: a.id,
        peer_host: "Mono".into(),
        peer_id: 34,
        linked_by: "lead".into(),
        linked_at: now,
        note: "same trust prompt".into(),
        ..Default::default()
    };
    let r = s.add_friction_link(&mut l);
    assert!(
        r.is_ok() && l.id != 0,
        "AddFrictionLink = {r:?}, id {}",
        l.id
    );
    // Linked again once it answers, spelled in another case: the same link,
    // now confirmed, keeping who first linked it and the note.
    let mut again = FrictionLink {
        friction_id: a.id,
        peer_host: "mono".into(),
        peer_id: 34,
        peer_uid: "mono-34".into(),
        linked_by: "other".into(),
        linked_at: now + hour,
        confirmed_at: now + hour,
        peer_status: FRICTION_OPEN.into(),
        peer_summary: "trust prompt".into(),
        peer_votes: 2,
        peer_checked_at: now + hour,
        ..Default::default()
    };
    let r = s.add_friction_link(&mut again);
    assert!(
        r.is_ok() && again.id == l.id,
        "relinking = {r:?}, id {}; want the same link, id {}",
        again.id,
        l.id
    );
    let got = links(a.id);
    assert_eq!(got.len(), 1, "links of #{}", a.id);
    let mut g = got[0].clone();
    assert!(
        g.peer_host == "Mono"
            && g.peer_uid == "mono-34"
            && g.linked_by == "lead"
            && g.linked_at == now
            && g.note == "same trust prompt"
            && g.confirmed_at == now + hour
            && g.peer_votes == 2
            && g.peer_summary == "trust prompt",
        "relinked link = {g:?}"
    );

    // An update with no check keeps the cache.
    s.add_friction_link(&mut link(a.id, "mono", 34)).unwrap();
    let g2 = &links(a.id)[0];
    assert!(
        g2.peer_summary == "trust prompt" && g2.peer_uid == "mono-34",
        "an unchecked relink lost the cache: {g2:?}"
    );

    // update_friction_link follows the other end to its original.
    g.peer_id = 30;
    g.peer_status = FRICTION_RESOLVED.into();
    g.peer_checked_at = now + hour * 2;
    s.update_friction_link(&g).unwrap();
    let g3 = &links(a.id)[0];
    assert!(
        g3.peer_id == 30 && g3.peer_status == FRICTION_RESOLVED,
        "updated link = {g3:?}"
    );

    // Closing b as a duplicate of a moves b's links to a; one a already has
    // is dropped.
    for peer in [30, 40] {
        s.add_friction_link(&mut link(b.id, "mono", peer)).unwrap();
    }
    b.status = FRICTION_DUPLICATE.into();
    b.duplicate_of = a.id;
    s.update_friction(&b).unwrap();
    assert_eq!(links(b.id).len(), 0, "the duplicate kept links");
    let peers: Vec<i64> = links(a.id).iter().map(|l| l.peer_id).collect();
    assert_eq!(peers, [30, 40], "the original's links");

    s.add_friction_link(&mut link(c.id, "other", 1)).unwrap();
    assert_eq!(links(0).len(), 3, "all links");
    assert!(
        s.delete_friction_link(a.id, "MONO", 40).unwrap(),
        "DeleteFrictionLink; want true"
    );
    assert!(
        !s.delete_friction_link(a.id, "mono", 40).unwrap(),
        "deleting it again; want false"
    );
    assert_eq!(links(a.id).len(), 1, "after unlinking, #{}'s links", a.id);
}

/// A store from before links gets the table when it's opened.
#[test]
fn friction_links_added_to_old_stores() {
    let (s, path, _dir) = new_test_store();
    s.conn().execute("DROP TABLE friction_links", []).unwrap();
    s.close().unwrap();
    let s = Store::open(&path).unwrap();
    s.list_friction_links(0)
        .expect("listing links in a reopened old store");
}

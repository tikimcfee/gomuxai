//! Bus identities: init, trust, untrust, rename, and `canonical_host`.

use std::os::unix::fs::PermissionsExt;

use agent_bus::{
    OP_STATUS, SameKeysError, canonical_host, load_identity, load_peers, previous_hosts,
    rename_identity, rename_peer, trust_peer, untrust_peer,
};
use x509_parser::extensions::GeneralName;

use crate::common::{
    BusHost, request, request_once, require_busd, serve, wait_interest, wait_linked, wait_log,
};

#[test]
fn test_rename_identity_keeps_keys() {
    let h = BusHost::new("old");
    let before = h.id.public_bundle("");

    let id = rename_identity(h.dir(), "old", "new").expect("rename_identity");
    let after = id.public_bundle("");
    assert_eq!(id.host, "new");
    assert_eq!(after.nkey, before.nkey);
    assert_eq!(after.cert_sha, before.cert_sha);

    let (_, cert) = x509_parser::parse_x509_certificate(&id.cert_der).expect("parse cert");
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|c| c.as_str().ok())
        .unwrap_or("");
    assert_eq!(cn, "gomux bus new");
    let san = cert
        .subject_alternative_name()
        .expect("san parse")
        .expect("san present");
    let dns_names: Vec<&str> = san
        .value
        .general_names
        .iter()
        .filter_map(|gn| match gn {
            GeneralName::DNSName(d) => Some(*d),
            _ => None,
        })
        .collect();
    assert_eq!(dns_names, vec!["new"]);

    assert_eq!(previous_hosts(h.dir()), vec!["old"]);
    let err = load_identity(h.dir(), "old").unwrap_err();
    assert!(
        err.to_string().contains("renamed from"),
        "loading under old name: {err}"
    );
    for f in ["host.nk", "local.nk", "tls.key"] {
        let mode = std::fs::metadata(h.dir().join(f))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{f} mode {mode:o} after rename");
    }
    let left_tmp: Vec<_> = std::fs::read_dir(h.dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(left_tmp.is_empty(), "left behind {left_tmp:?}");

    rename_identity(h.dir(), "new", "old").expect("rename back");
    assert_eq!(previous_hosts(h.dir()), vec!["new"]);
}

#[test]
fn test_rename_identity_refusals() {
    let h = BusHost::new("box");
    let peer = BusHost::new("peer");
    h.trust(peer.id.public_bundle(""));
    let mut b = peer.id.public_bundle("");
    b.host = "peer2".to_string();
    rename_peer(h.dir(), "peer", b).expect("rename_peer");

    for (name, want) in [
        ("box", "already named"),
        ("a.b", "invalid"),
        ("PEER2", "trusted peer peer2"),
        ("Peer", "trusted peer peer2 is (or was) named Peer"),
    ] {
        let err = rename_identity(h.dir(), "box", name).unwrap_err();
        assert!(
            err.to_string().contains(want),
            "rename to {name:?}: {err}, want {want:?}"
        );
    }
    assert!(rename_identity(h.dir(), "other", "x").is_err());
    load_identity(h.dir(), "box").expect("refused rename should not change identity");
}

#[test]
fn test_untrust_peer() {
    let h = BusHost::new("box");
    h.trust(BusHost::new("Hub").id.public_bundle("tls://hub:7422"));
    h.trust(BusHost::new("leaf").id.public_bundle(""));

    let b = untrust_peer(h.dir(), "hub").expect("untrust_peer");
    assert_eq!(b.host, "Hub");
    assert_eq!(b.url, "tls://hub:7422");

    let peers = load_peers(h.dir()).unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].host, "leaf");

    let err = untrust_peer(h.dir(), "Hub").unwrap_err();
    assert!(
        err.to_string().contains("trusted: leaf"),
        "untrusting again: {err}"
    );
}

#[test]
fn test_trust_peer_same_keys_and_renames() {
    let h = BusHost::new("box");
    let peer = BusHost::new("peer");
    h.trust(peer.id.public_bundle(""));

    let mut renamed = peer.id.public_bundle("tls://peer2:7422");
    renamed.host = "peer2".to_string();
    let err = trust_peer(h.dir(), renamed.clone()).unwrap_err();
    let same = err
        .downcast_ref::<SameKeysError>()
        .expect("expected SameKeysError");
    assert_eq!(same.peer, "peer");
    assert!(err.to_string().contains("--renames peer"), "{err}");

    let stranger = BusHost::new("peer2").id.public_bundle("");
    let err = rename_peer(h.dir(), "peer", stranger).unwrap_err();
    assert!(err.to_string().contains("isn't peer renamed"), "{err}");

    let err = rename_peer(h.dir(), "nobody", renamed.clone()).unwrap_err();
    assert!(err.to_string().contains("no trusted peer"), "{err}");

    let (got, replaced) = rename_peer(h.dir(), "PEER", renamed.clone()).expect("rename_peer");
    assert_eq!(replaced.host, "peer");
    assert_eq!(got.aka, vec!["peer"]);
    assert!(got.names("Peer"));
    assert!(got.names("peer2"));
    assert!(!got.names("box"));
    assert_eq!(load_peers(h.dir()).unwrap(), vec![got]);

    renamed.url.clear();
    h.trust(renamed.clone());
    let peers = load_peers(h.dir()).unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].aka, vec!["peer"]);
    assert_eq!(peers[0].url, "");

    let mut third = renamed;
    third.host = "peer3".to_string();
    let (got, _) = rename_peer(h.dir(), "peer2", third.clone()).expect("second rename");
    assert_eq!(got.aka, vec!["peer", "peer2"]);

    let mut fourth = third;
    fourth.host = "Peer3".to_string();
    let (got, replaced) = rename_peer(h.dir(), "peer3", fourth).expect("case-only rename");
    assert_eq!(replaced.host, "peer3");
    assert_eq!(got.host, "Peer3");
    assert_eq!(got.aka, vec!["peer", "peer2"]);
    let peers = load_peers(h.dir()).unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].host, "Peer3");
}

#[test]
fn test_canonical_host() {
    let h = BusHost::new("HubBox");
    let id = rename_identity(h.dir(), "HubBox", "Studio").unwrap();
    assert_eq!(id.host, "Studio");

    let peer = BusHost::new("OldLaptop");
    h.trust(peer.id.public_bundle(""));
    let mut peer_new = peer.id.public_bundle("");
    peer_new.host = "MacAir".to_string();
    rename_peer(h.dir(), "OldLaptop", peer_new).unwrap();

    assert_eq!(canonical_host(h.dir(), "Studio", ""), "");
    assert_eq!(canonical_host(h.dir(), "Studio", "Studio"), "Studio");
    assert_eq!(canonical_host(h.dir(), "Studio", "studio"), "Studio");
    assert_eq!(canonical_host(h.dir(), "Studio", "hubbox"), "Studio");
    assert_eq!(canonical_host(h.dir(), "Studio", "macair"), "MacAir");
    assert_eq!(canonical_host(h.dir(), "Studio", "oldlaptop"), "MacAir");
    assert_eq!(
        canonical_host(h.dir(), "Studio", "other-leaf"),
        "other-leaf"
    );
}

#[test]
fn test_bus_renamed_leaf_needs_the_hub_to_retrust() {
    if require_busd().is_none() {
        return;
    }
    let hub = BusHost::new("hub");
    let mut leaf = BusHost::new("leaf");
    hub.trust(leaf.id.public_bundle(""));
    let hub_d = hub.start("127.0.0.1:-1", None);
    let hub_addr = hub_d.leaf_addr().to_string();
    leaf.trust(hub.id.public_bundle(&format!("tls://{hub_addr}")));
    let leaf_d = leaf.start("", None);
    wait_linked(&hub_d, 1);

    leaf_d.shutdown();
    let id = rename_identity(leaf.dir(), "leaf", "leaf2").expect("rename leaf");
    leaf.name = "leaf2".to_string();
    leaf.id = id;
    let _leaf_d2 = leaf.start("", None);
    wait_linked(&hub_d, 1);
    let leaf_conn = leaf.connect();
    let _srv = serve(&leaf_conn, "leaf2");
    let hub_conn = hub.connect();
    wait_interest(&leaf_conn, &hub_conn, "hub");
    assert!(
        request_once(&hub_conn, "leaf2", OP_STATUS).is_err(),
        "hub reached leaf2 before re-trusting it"
    );

    rename_peer(hub.dir(), "leaf", leaf.id.public_bundle("")).expect("hub rename_peer");
    hub_d.shutdown();
    let hub_d2 = hub.start(&hub_addr, None);
    wait_linked(&hub_d2, 1);
    let hub_conn2 = hub.connect();
    let got = request(&hub_conn2, "leaf2", OP_STATUS).expect("hub -> leaf2 after re-trust");
    assert_eq!(got, "ran gomux.leaf2.rpc.status");
    wait_interest(&hub_conn2, &leaf_conn, "leaf2");
}

#[test]
fn test_bus_untrust_cuts_the_link() {
    if require_busd().is_none() {
        return;
    }
    for side in ["hub", "leaf"] {
        let hub = BusHost::new("hub");
        let leaf = BusHost::new("leaf");
        hub.trust(leaf.id.public_bundle(""));
        let hub_d = hub.start("127.0.0.1:-1", None);
        let hub_addr = hub_d.leaf_addr().to_string();
        leaf.trust(hub.id.public_bundle(&format!("tls://{hub_addr}")));
        let leaf_d = leaf.start("", None);
        wait_linked(&hub_d, 1);

        if side == "hub" {
            untrust_peer(hub.dir(), "leaf").unwrap();
            hub_d.shutdown();
            let _ = std::fs::remove_file(hub.dir().join("bus.log"));
            let hub_d2 = hub.start(&hub_addr, None);
            wait_log(&hub, "Leafnode connection closed");
            assert_eq!(hub_d2.linked_peers(), 0, "links after untrusting on hub");
        } else {
            untrust_peer(leaf.dir(), "hub").unwrap();
            leaf_d.shutdown();
            let _leaf_d2 = leaf.start("", None);
            wait_linked(&hub_d, 0);
            assert_eq!(hub_d.linked_peers(), 0, "links after untrusting on leaf");
        }
    }
}

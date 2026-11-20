//! Port of `tests/natsbus/identity_test.go`, plus `canonical_host` and
//! bidirectional Go <-> Rust identity compatibility tests.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use agent_bus::{
    DaemonConfig, OP_STATUS, PeerBundle, SameKeysError, canonical_host, load_identity, load_peers,
    previous_hosts, rename_identity, rename_peer, trust_peer, untrust_peer,
};
use x509_parser::extensions::GeneralName;

use crate::common::{
    BusHost, has_go, legacy_go_root, request, request_once, require_busd, serve, test_temp_root,
    test_tempdir, wait_interest, wait_linked, wait_log,
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

/// Verifies both directions required by `bus.md`:
/// 1. An identity written (and renamed) by Rust loads in Go (`agent-busd` and
///    `internal/natsbus.LoadIdentity`).
/// 2. An identity written by Go (`internal/natsbus.InitIdentity`, which writes
///    SEC 1 `EC PRIVATE KEY`) loads and renames in Rust, with identical public
///    nkey and `spkiSHA256` pin, and links over TLS 1.3 with a Rust-initialized
///    hub.
#[test]
fn test_identity_rust_and_go_bidirectional_compat() {
    let Some(legacy) = legacy_go_root() else {
        eprintln!("skipping Go <-> Rust identity interop test: no legacy/ Go source here");
        return;
    };
    if !has_go() || require_busd().is_none() {
        eprintln!("skipping Go <-> Rust identity interop test: `go` is missing");
        return;
    }

    let tmp = test_tempdir();
    let go_dir = tmp.path().join("go-init");
    let rust_dir = tmp.path().join("rust-init");
    let helper_src = tmp.path().join("compat_helper.go");

    agent_bus::init_identity(&rust_dir, "rustbox").expect("rust init_identity");

    // Rust must write tls.key as SEC 1 ("EC PRIVATE KEY") matching Go, so Go's
    // `natsbus.RenameIdentity` (`x509.ParseECPrivateKey`) succeeds on it.
    let rust_key_pem = std::fs::read_to_string(rust_dir.join("tls.key")).unwrap();
    assert!(
        rust_key_pem.starts_with("-----BEGIN EC PRIVATE KEY-----\n"),
        "expected SEC 1 EC PRIVATE KEY header, got:\n{rust_key_pem}"
    );

    // Small Go program run inside the legacy Go module (legacy/) that:
    // 1) Loads rust_dir with Go's internal/natsbus.LoadIdentity, then renames it
    //    with Go's internal/natsbus.RenameIdentity (which calls x509.ParseECPrivateKey)
    //    and renames back to "rustbox", printing its PublicBundle JSON.
    // 2) Initializes go_dir with Go's internal/natsbus.InitIdentity and prints its PublicBundle JSON.
    std::fs::write(
        &helper_src,
        r#"package main

import (
	"encoding/json"
	"fmt"
	"os"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

func main() {
	rustDir, goDir := os.Args[1], os.Args[2]
	if _, err := natsbus.LoadIdentity(rustDir, "rustbox"); err != nil {
		fmt.Fprintf(os.Stderr, "Go LoadIdentity on Rust dir failed: %v\n", err)
		os.Exit(1)
	}
	if _, err := natsbus.RenameIdentity(rustDir, "rustbox", "rustbox_tmp"); err != nil {
		fmt.Fprintf(os.Stderr, "Go RenameIdentity on Rust tls.key failed: %v\n", err)
		os.Exit(1)
	}
	rid, err := natsbus.RenameIdentity(rustDir, "rustbox_tmp", "rustbox")
	if err != nil {
		fmt.Fprintf(os.Stderr, "Go RenameIdentity back failed: %v\n", err)
		os.Exit(1)
	}
	gid, err := natsbus.InitIdentity(goDir, "gobox")
	if err != nil {
		fmt.Fprintf(os.Stderr, "Go InitIdentity failed: %v\n", err)
		os.Exit(1)
	}
	out, _ := json.Marshal([]natsbus.PeerBundle{rid.PublicBundle(""), gid.PublicBundle("")})
	os.Stdout.Write(out)
}
"#,
    )
    .unwrap();

    let root = legacy;
    let virtual_main = root
        .join("internal")
        .join("natsbus_compat_helper")
        .join("main.go");
    let overlay_path = tmp.path().join("overlay.json");
    let overlay_json = serde_json::json!({
        "Replace": {
            virtual_main.to_string_lossy().to_string(): helper_src.to_string_lossy().to_string(),
        }
    });
    std::fs::write(&overlay_path, serde_json::to_vec(&overlay_json).unwrap()).unwrap();

    let out = Command::new("go")
        .arg("run")
        .arg("-overlay")
        .arg(&overlay_path)
        .arg("./internal/natsbus_compat_helper")
        .arg(&rust_dir)
        .arg(&go_dir)
        .current_dir(&root)
        .env("TMPDIR", test_temp_root())
        .output()
        .expect("running Go identity compat helper");
    assert!(
        out.status.success(),
        "Go compat helper failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bundles: Vec<PeerBundle> = serde_json::from_slice(&out.stdout).unwrap();
    let go_saw_rust = &bundles[0];
    let go_created = &bundles[1];

    // Direction 1: Rust-initialized identity loaded in Go has the exact same pin & nkey.
    let rust_loaded = load_identity(&rust_dir, "rustbox").unwrap();
    assert_eq!(rust_loaded.public_bundle(""), *go_saw_rust);

    // Direction 2: Go-initialized identity loaded in Rust has the exact same pin & nkey,
    // can be renamed by Rust, and links over TLS 1.3 via agent-busd!
    let go_loaded_in_rust = load_identity(&go_dir, "gobox").expect("Rust load_identity on Go dir");
    assert_eq!(go_loaded_in_rust.public_bundle(""), *go_created);

    let go_renamed =
        rename_identity(&go_dir, "gobox", "gobox2").expect("Rust rename_identity on Go SEC1 key");
    assert_eq!(go_renamed.cert_sha, go_created.cert_sha);
    assert_eq!(go_renamed.host_public_key(), go_created.nkey);

    // Link rustbox (hub) and gobox2 (leaf) through agent-busd and pass an RPC!
    let bin = require_busd().unwrap();
    trust_peer(&rust_dir, go_renamed.public_bundle("")).unwrap();
    let hub_d = agent_bus::start_daemon(DaemonConfig {
        dir: rust_dir.clone(),
        host: "rustbox".to_string(),
        client_port: -1,
        leaf_listen: "127.0.0.1:-1".to_string(),
        remote_ops: None,
        busd_path: Some(bin.clone()),
    })
    .unwrap();
    trust_peer(
        &go_dir,
        rust_loaded.public_bundle(&format!("tls://{}", hub_d.leaf_addr())),
    )
    .unwrap();
    let _leaf_d = agent_bus::start_daemon(DaemonConfig {
        dir: go_dir.clone(),
        host: "gobox2".to_string(),
        client_port: -1,
        leaf_listen: String::new(),
        remote_ops: None,
        busd_path: Some(bin),
    })
    .unwrap();
    wait_linked(&hub_d, 1);

    let hub_conn = agent_bus::connect_local(&rust_dir, "rustbox").unwrap();
    let leaf_conn = agent_bus::connect_local(&go_dir, "gobox2").unwrap();
    let _srv = serve(&leaf_conn, "gobox2");
    let got = request(&hub_conn, "gobox2", OP_STATUS).unwrap();
    assert_eq!(got, "ran gomux.gobox2.rpc.status");
}

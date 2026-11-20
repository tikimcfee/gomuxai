//! Go: internal/cli/friction_links_test.go.

use std::collections::HashMap;
use std::sync::Arc;

use agent_bus::{FrictionEntry, FrictionRequest, RpcServer};
use agent_store::{FRICTION_OPEN, FRICTION_RESOLVED, FRICTION_WONTFIX, Friction};
use agent_termd::{Ops, Session};

use super::*;
use crate::cli::friction::friction_test::{Host, agent_name};
use crate::cli::testenv::TestBus;

/// A termd that isn't there: the friction tests need the service (to serve
/// the friction op) and no terminals.
pub struct NoTerm;

fn no_term<T>() -> anyhow::Result<T> {
    anyhow::bail!("no termd in this test")
}

impl Ops for NoTerm {
    fn create_session(&self, _: &str, _: &HashMap<String, String>) -> anyhow::Result<()> {
        no_term()
    }
    fn create_session_in(
        &self,
        _: &str,
        _: &str,
        _: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        no_term()
    }
    fn kill_session(&self, _: &str) -> anyhow::Result<()> {
        no_term()
    }
    fn respawn_pane(&self, _: &str) -> anyhow::Result<()> {
        no_term()
    }
    fn session_exists(&self, _: &str) -> anyhow::Result<bool> {
        no_term()
    }
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        no_term()
    }
    fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> {
        no_term()
    }
    fn paste_text(&self, _: &str, _: &str) -> anyhow::Result<()> {
        no_term()
    }
    fn send_special_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        no_term()
    }
    fn capture(&self, _: &str, _: i32) -> anyhow::Result<String> {
        no_term()
    }
    fn capture_styled(&self, _: &str, _: i32) -> anyhow::Result<String> {
        no_term()
    }
    fn pane_pids(&self) -> anyhow::Result<HashMap<i32, String>> {
        no_term()
    }
}

/// A host in a friction test: its own store, served over the test bus while
/// it's up (Go: frictionHost).
struct FHost {
    h: Host,
    server: Option<RpcServer>,
}

/// The test bus every host's CLI reaches.
struct Net {
    bus: TestBus,
}

impl Net {
    /// A CLI on host name with a new, empty store, reaching the test bus.
    fn host(&self, name: &str) -> FHost {
        let h = Host::new(name);
        h.env.ctx.set_termd(Arc::new(NoTerm));
        let (dir, bus_host) = (self.bus.dir.path().to_path_buf(), self.bus.host.clone());
        h.env
            .ctx
            .set_bus_connect(move || agent_bus::connect_local(&dir, &bus_host));
        let mut f = FHost { h, server: None };
        f.up(self);
        f
    }
}

impl FHost {
    /// Serves h's friction op on the bus; down stops it, so h is unreachable.
    fn up(&mut self, net: &Net) {
        self.down();
        let svc = Arc::clone(self.h.env.ctx.service().expect("the host's service"));
        let handler: Arc<dyn agent_bus::Handler> = Arc::new(agent_service::new_rpc_handler(svc));
        let server = agent_bus::serve_rpc(
            &net.bus.conn,
            &self.h.env.cfg.host,
            handler,
            &[agent_bus::OP_FRICTION],
        )
        .expect("serving friction");
        self.server = Some(server);
    }

    fn down(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }

    fn must(&mut self, args: &[&str]) -> String {
        self.h.must(args)
    }

    fn err(&mut self, args: &[&str]) -> String {
        self.h.err(args)
    }

    fn entry(&self, id: i64) -> Friction {
        self.h.entry(id)
    }

    fn links(&self, id: i64) -> Vec<FrictionLink> {
        self.h.env.store().list_friction_links(id).unwrap()
    }
}

impl Drop for FHost {
    fn drop(&mut self) {
        self.down();
    }
}

/// The test bus, or None (skip) without agent-busd.
fn net() -> Option<Net> {
    Some(Net {
        bus: TestBus::start()?,
    })
}

fn want_in(what: &str, out: &str, wants: &[&str]) {
    for w in wants {
        assert!(out.contains(w), "{what} lacks {w:?}:\n{out}");
    }
}

fn want_not_in(what: &str, out: &str, unwanted: &[&str]) {
    for w in unwanted {
        assert!(!out.contains(w), "{what} has {w:?}:\n{out}");
    }
}

#[test]
fn parse_friction_refs() {
    let h = Host::new("local");
    let tests: &[(&str, &str, i64, bool, bool)] = &[
        ("34", "local", 34, false, false),
        ("#34", "local", 34, false, false),
        ("34@h1", "h1", 34, true, false),
        ("#34@h1", "h1", 34, true, false),
        ("34@LOCAL", "local", 34, false, false),
        ("34@", "", 0, false, true),
        ("34@h.1", "", 0, false, true),
        ("x@h1", "", 0, false, true),
        ("0@h1", "", 0, false, true),
    ];
    for &(arg, host, id, remote, bad) in tests {
        match parse_friction_ref(&h.env.ctx, arg) {
            Err(e) => assert!(bad, "parse_friction_ref({arg:?}) err = {e:#}"),
            Ok(got) => {
                assert!(!bad, "parse_friction_ref({arg:?}) = {got:?}, want an error");
                assert_eq!(
                    got,
                    (host.to_string(), id, remote),
                    "parse_friction_ref({arg:?})"
                );
            }
        }
    }
}

#[test]
fn friction_link_across_hosts() {
    let Some(net) = net() else { return };
    let _env = agent_name("");
    let (mut local, mut h1) = (net.host("local"), net.host("h1"));
    local.must(&[
        "friction",
        "add",
        "a claude agent at its trust prompt shows as unknown",
        "--kind",
        "bug",
    ]);
    h1.must(&["friction", "add", "spawn hangs on a slow disk"]);
    h1.must(&[
        "friction",
        "add",
        "trust prompt agent reads unknown, not waiting",
        "--kind",
        "bug",
    ]);
    h1.must(&["friction", "+1", "2"]);

    let e = local.err(&["friction", "link", "1", "2"]);
    assert!(
        e.contains("is on this host"),
        "linking two local entries: {e}"
    );
    let e = local.err(&["friction", "link", "1", "9@h1"]);
    assert!(
        e.contains("no friction #9 on h1 (latest is #2)"),
        "linking to a missing entry: {e}"
    );
    assert_eq!(local.links(1).len(), 0, "a refused link left links here");

    let out = local.must(&["friction", "link", "1", "#2@h1", "--note", "same prompt"]);
    assert_eq!(
        out,
        "Linked #1 and #2@h1 (bug) open, +1 there: trust prompt agent reads unknown, not waiting\nThey're the same problem: resolving either resolves both.\n"
    );
    let (mine, theirs) = (local.links(1), h1.links(2));
    assert!(
        mine.len() == 1
            && mine[0].peer_uid == h1.entry(2).uid
            && !mine[0].confirmed_at.is_zero()
            && mine[0].note == "same prompt",
        "this host's half = {mine:?}"
    );
    assert!(
        theirs.len() == 1
            && theirs[0].peer_host == "local"
            && theirs[0].peer_id == 1
            && theirs[0].peer_uid == local.entry(1).uid
            && theirs[0].linked_by == "user@local"
            && theirs[0].peer_summary == "a claude agent at its trust prompt shows as unknown",
        "h1's half = {theirs:?}"
    );

    let show = local.must(&["friction", "show", "1"]);
    want_in(
        "show",
        &show,
        &[
            "Linked (the same problem on other hosts; resolving #1 resolves these too):\n  #2@h1 (bug) open, +1 there: trust prompt agent reads unknown, not waiting\n    linked by user, ",
        ],
    );
    want_not_in("show", &show, &["!"]);
    want_in(
        "show on h1",
        &h1.must(&["friction", "show", "2"]),
        &["  #1@local (bug) open, +0 there: a claude agent at its trust prompt"],
    );
    want_in(
        "list",
        &local.must(&["friction", "list"]),
        &["a claude agent at its trust prompt shows as unknown [= #2@h1]"],
    );

    let listed: serde_json::Value =
        serde_json::from_str(&local.must(&["friction", "list", "--json"])).unwrap();
    let links = &listed[0]["links"];
    assert!(
        listed.as_array().map(Vec::len) == Some(1)
            && links.as_array().map(Vec::len) == Some(1)
            && links[0]["ref"] == "#2@h1"
            && links[0]["state"] == LINK_OK
            && links[0]["host"] == "h1",
        "list --json = {listed}"
    );

    let out = local.must(&[
        "friction",
        "resolve",
        "1",
        "--note",
        "status reads the trust prompt (abc123)",
    ]);
    assert_eq!(out, "Resolved friction #1 (bug).\nResolved #2@h1 too.\n");
    let f = h1.entry(2);
    assert!(
        f.status == FRICTION_RESOLVED
            && f.resolved_by == "user@local"
            && f.resolution == "status reads the trust prompt (abc123)",
        "h1's #2 = {f:?}"
    );
    assert_eq!(
        h1.entry(1).status,
        FRICTION_OPEN,
        "h1's unlinked #1 was closed"
    );
    want_in(
        "links",
        &local.must(&["friction", "links"]),
        &["#1 resolved = #2@h1: (bug) resolved, +1 there"],
    );
    assert_eq!(
        local.must(&["friction", "links", "--dangling"]),
        "No dangling links (1 in all: agent friction links).\n"
    );
    want_in(
        "export",
        &local.must(&["friction", "export"]),
        &["Linked to #2@h1 (resolved there, "],
    );

    assert_eq!(
        local.must(&["friction", "unlink", "1", "2@h1"]),
        "Unlinked #1 and #2@h1.\n"
    );
    assert!(
        local.links(1).is_empty() && h1.links(2).is_empty(),
        "links left after unlink"
    );
    assert_eq!(
        local.must(&["friction", "links"]),
        "No links to other hosts' friction entries on host local.\n"
    );
}

// A host that can't be reached: link and resolve still work here, and say
// what's left to do; show and links flag the link until it's done.
#[test]
fn friction_link_while_unreachable() {
    let Some(net) = net() else { return };
    let _env = agent_name("");
    let (mut local, mut h1) = (net.host("local"), net.host("h1"));
    local.must(&[
        "friction",
        "add",
        "wait --timeout doesn't name the last status",
        "--kind",
        "error",
    ]);
    h1.must(&["friction", "add", "wait timeout says nothing"]);

    h1.down();
    let out = local.must(&["friction", "link", "1", "1@h1"]);
    want_in(
        "link",
        &out,
        &[
            "Linked #1 to #1@h1 on this host only: couldn't reach h1 (no bus answering for host \"h1\"",
            "once its bus answers: agent friction link 1 1@h1\n",
            "or on h1: agent friction link 1 1@local\n",
        ],
    );
    let l = local.links(1);
    assert!(
        l.len() == 1 && l[0].confirmed_at.is_zero() && l[0].peer_uid.is_empty(),
        "unconfirmed link = {l:?}"
    );
    let show = local.must(&["friction", "show", "1"]);
    want_in(
        "show",
        &show,
        &[
            "  #1@h1 not seen yet: it was linked while h1 was unreachable\n",
            "    ! unreachable: no bus answering for host \"h1\"",
            "      or on h1: agent friction link 1 1@local\n",
        ],
    );
    want_in(
        "list",
        &local.must(&["friction", "list"]),
        &["[= #1@h1 !unreachable]"],
    );

    h1.up(&net);
    want_in(
        "show once h1 answers",
        &local.must(&["friction", "show", "1"]),
        &[
            "  #1@h1 (other) open, +0 there: wait timeout says nothing\n",
            "    ! one-sided: h1 has no link back to #1\n",
            "      agent friction link 1 1@h1\n",
        ],
    );
    // Linking from the other side confirms both halves.
    h1.must(&["friction", "link", "1", "1@local"]);
    let l = local.links(1);
    assert!(
        l.len() == 1 && !l[0].confirmed_at.is_zero() && l[0].peer_uid == h1.entry(1).uid,
        "link confirmed from h1 = {l:?}"
    );
    want_not_in(
        "show once linked both ways",
        &local.must(&["friction", "show", "1"]),
        &["!"],
    );

    h1.down();
    let out = local.must(&["friction", "resolve", "1", "--note", "wait names it"]);
    want_in(
        "resolve",
        &out,
        &[
            "Resolved friction #1 (error).\nCouldn't reach h1 to resolve #1@h1: no bus answering",
            "Left to do, once those hosts' buses answer: agent friction resolve 1 --linked\n",
            "  or on h1: agent friction resolve 1 --note 'wait names it'\n",
        ],
    );
    want_in(
        "links --dangling",
        &local.must(&["friction", "links", "--dangling"]),
        &[
            "#1 resolved = #1@h1: unreachable: no bus answering",
            "; last seen open, ",
        ],
    );

    h1.up(&net);
    want_in(
        "links --dangling",
        &local.must(&["friction", "links", "--dangling"]),
        &[
            "#1 resolved = #1@h1: out of step: open there, resolved here\n    agent friction resolve 1 --linked\n",
        ],
    );
    let e = local.err(&["friction", "resolve", "1", "--linked", "--note", "x"]);
    assert!(e.contains("--linked takes no --note"), "{e}");
    assert_eq!(
        local.must(&["friction", "resolve", "1", "--linked"]),
        "Resolved #1@h1 too.\n"
    );
    let f = h1.entry(1);
    assert!(
        f.status == FRICTION_RESOLVED && f.resolution == "wait names it",
        "h1's #1 = {f:?}"
    );
    // h1 heard how it was left here, without asking.
    let l = h1.links(1);
    assert!(
        l.len() == 1 && l[0].peer_status == FRICTION_RESOLVED,
        "h1's half = {l:?}"
    );
    assert_eq!(
        local.must(&["friction", "resolve", "1", "--linked"]),
        "#1@h1 was already resolved (by user@local).\n"
    );

    local.must(&["friction", "add", "unrelated", "--new"]);
    let e = local.err(&["friction", "resolve", "2", "--linked"]);
    assert!(e.contains("still open"), "{e}");

    h1.down();
    local.must(&["friction", "link", "2", "1@h1"]);
    let out = local.must(&["friction", "unlink", "2", "1@h1"]);
    want_in(
        "unlink while unreachable",
        &out,
        &[
            "Unlinked #2 from #1@h1 here; h1 still has its half",
            "or on h1: agent friction unlink 1 2@local\n",
        ],
    );
    let e = local.err(&["friction", "unlink", "2", "1@h1"]);
    assert!(e.contains("#2 has no link to #1@h1 here"), "{e}");
}

// Links whose other end changed without this host: resolved there (two
// agents in parallel), a different entry under the id (a reset store), or
// closed there as a duplicate.
#[test]
fn friction_links_go_stale() {
    let Some(net) = net() else { return };
    let _env = agent_name("");
    let (mut local, mut h1) = (net.host("local"), net.host("h1"));
    for s in [
        "capture cuts long lines",
        "send loses its enter",
        "status flickers",
    ] {
        local.must(&["friction", "add", s, "--new"]);
        h1.must(&["friction", "add", s, "--new"]);
    }
    for id in ["1", "2", "3"] {
        local.must(&["friction", "link", id, &format!("{id}@h1")]);
    }

    // Resolved on h1 only.
    h1.must(&[
        "friction",
        "resolve",
        "1",
        "--only",
        "--note",
        "fixed there",
    ]);
    assert_eq!(
        local.entry(1).status,
        FRICTION_OPEN,
        "resolve --only on h1 closed this host's #1"
    );
    want_in(
        "show",
        &local.must(&["friction", "show", "1"]),
        &[
            "    ! out of step: resolved there (by user: fixed there), open here\n",
            "      if the same fix covers it here: agent friction resolve 1 --note 'fixed there'\n",
            "      if it isn't the same problem: agent friction unlink 1 1@h1\n",
        ],
    );
    want_in(
        "list",
        &local.must(&["friction", "list"]),
        &["[= #1@h1 !out of step]"],
    );

    // h1 closes its #3 as a duplicate of a new #4: the link follows.
    h1.must(&[
        "friction",
        "add",
        "status flickers between idle and processing",
        "--new",
    ]);
    h1.must(&["friction", "resolve", "3", "--dup", "4"]);
    let show = local.must(&["friction", "show", "3"]);
    want_in(
        "show",
        &show,
        &[
            "  #4@h1 (other) open, +1 there: status flickers between idle",
            "(#3 there, which it links to, is closed as its duplicate)",
        ],
    );
    want_not_in("show", &show, &["!"]);
    let l = local.links(3);
    assert!(
        l.len() == 1 && l[0].peer_id == 4 && l[0].peer_uid == h1.entry(4).uid,
        "link after h1's dup = {l:?}"
    );

    // h1's store is reset (Go: reset, then up): #2 is a different entry,
    // then gone. The old host stays, down, to check its store after.
    h1.down();
    let old = std::mem::replace(&mut h1, net.host("h1"));
    want_in(
        "links",
        &local.must(&["friction", "links", "--dangling"]),
        &["#2 open = #2@h1: not found: h1 has no #2\n    agent friction unlink 2 2@h1\n"],
    );
    h1.must(&["friction", "add", "one"]);
    h1.must(&["friction", "add", "two"]);
    want_in(
        "show",
        &local.must(&["friction", "show", "2"]),
        &["! not found: #2 on h1 is a different entry now (two)"],
    );
    let out = local.must(&["friction", "resolve", "2", "--note", "fixed"]);
    want_in(
        "resolve",
        &out,
        &["#2@h1 wasn't resolved: it's a different entry there now (two)"],
    );
    assert_eq!(
        h1.entry(2).status,
        FRICTION_OPEN,
        "resolve closed the entry that reused #2"
    );
    assert_eq!(old.entry(2).status, FRICTION_OPEN, "the old store changed");
}

// Resolving follows links through other hosts: local-h1-h2.
#[test]
fn friction_resolve_follows_links() {
    let Some(net) = net() else { return };
    let _env = agent_name("");
    let (mut local, mut h1, mut h2) = (net.host("local"), net.host("h1"), net.host("h2"));
    local.must(&["friction", "add", "logs can't show one turn"]);
    h1.must(&["friction", "add", "logs has no --turn"]);
    h2.must(&["friction", "add", "no way to see a single turn in logs"]);
    h2.must(&["friction", "add", "unrelated", "--new"]);
    local.must(&["friction", "link", "1", "1@h1"]);
    h1.must(&["friction", "link", "1", "2@h2"]);
    h2.must(&["friction", "resolve", "1", "--dup", "2"]);

    let out = local.must(&[
        "friction",
        "resolve",
        "1",
        "--wontfix",
        "--note",
        "use transcript",
    ]);
    assert_eq!(
        out,
        "Closed friction #1 (other) as wontfix.\nResolved #1@h1 too.\nResolved #2@h2 too.\n"
    );
    for f in [h1.entry(1), h2.entry(2)] {
        assert!(
            f.status == FRICTION_WONTFIX
                && f.resolution == "use transcript"
                && f.resolved_by == "user@local",
            "linked entry = {f:?}"
        );
    }
}

// A same-host duplicate hands its links to its original.
#[test]
fn friction_dup_moves_links() {
    let Some(net) = net() else { return };
    let _env = agent_name("");
    let (mut local, mut h1) = (net.host("local"), net.host("h1"));
    local.must(&["friction", "add", "keys refuses a read-only client"]);
    local.must(&[
        "friction",
        "add",
        "keys fails when tmux is attached read-only",
        "--new",
    ]);
    h1.must(&["friction", "add", "read-only attach breaks keys"]);
    local.must(&["friction", "link", "2", "1@h1"]);

    let out = local.must(&["friction", "resolve", "2", "--dup", "1"]);
    assert_eq!(
        out,
        "Closed friction #2 as a duplicate of #1 (now +1).\nIts links to other hosts' entries moved to #1.\n"
    );
    // h1 still names #2 here, which stands for #1 now.
    let show = h1.must(&["friction", "show", "1"]);
    want_in(
        "show on h1",
        &show,
        &[
            "  #1@local (other) open, +1 there: keys refuses",
            "(#2 there, which it links to, is closed as its duplicate)",
        ],
    );
    want_not_in("show on h1", &show, &["!"]);
    assert_eq!(
        h1.must(&["friction", "resolve", "1", "--note", "pastes past it"]),
        "Resolved friction #1 (other).\nResolved #1@local too.\n"
    );
    assert_eq!(local.entry(1).status, FRICTION_RESOLVED);
}

// A request over the bus can't claim to come from the serving host itself.
#[test]
fn serve_friction_refuses_its_own_host() {
    let _env = agent_name("");
    let mut local = Host::new("local");
    local.env.ctx.set_termd(Arc::new(NoTerm));
    local.must(&["friction", "add", "x"]);
    let svc = local.env.ctx.service().unwrap();
    let err = svc
        .serve_friction(&FrictionRequest {
            action: agent_bus::FRICTION_LINK.into(),
            id: 1,
            from: Some(FrictionEntry {
                host: "LOCAL".into(),
                id: 1,
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect_err("a link from this host over the bus");
    assert!(format!("{err:#}").contains("which is this host"), "{err:#}");
    assert!(local.env.store().list_friction_links(1).unwrap().is_empty());
}

// Another host resolves only an entry linked to the one the request comes
// from: a mistyped id, or a link this host never recorded, closes nothing.
#[test]
fn friction_resolve_over_the_bus_needs_a_link_back() {
    let Some(net) = net() else { return };
    let _env = agent_name("");
    let (mut local, mut h1) = (net.host("local"), net.host("h1"));
    local.must(&["friction", "add", "capture drops the last line"]);
    h1.must(&["friction", "add", "capture misses its last line"]);
    h1.must(&["friction", "add", "spawn hangs on a slow disk", "--new"]);
    local.must(&["friction", "link", "1", "1@h1"]);

    let svc = Arc::clone(h1.h.env.ctx.service().unwrap());
    let from = FrictionEntry {
        host: "local".into(),
        id: 1,
        uid: local.entry(1).uid,
        ..Default::default()
    };
    let resolve = |id: i64, from: Option<FrictionEntry>| FrictionRequest {
        action: agent_bus::FRICTION_RESOLVE.into(),
        id,
        from,
        status: "resolved".into(),
        note: "x".into(),
        ..Default::default()
    };

    // No From: refused, whatever the id.
    let e = svc
        .serve_friction(&resolve(1, None))
        .expect_err("resolve with no from");
    assert!(
        format!("{e:#}").contains("names no entry it comes from"),
        "{e:#}"
    );
    // A mistyped id, no uid: #2 isn't linked to local's #1.
    let e = svc
        .serve_friction(&resolve(2, Some(from)))
        .expect_err("resolve of an unlinked entry");
    assert!(
        format!("{e:#}").contains("#2 on h1 has no link to #1@local, so it wasn't resolved: link it from this side first (on h1: agent friction link 2 1@local)"),
        "{e:#}"
    );
    for id in [1, 2] {
        assert_eq!(
            h1.entry(id).status,
            FRICTION_OPEN,
            "a refused resolve closed h1's #{id}"
        );
    }

    // A link recorded here while h1 was down is one-sided: h1 refuses, and
    // resolve passes on what to run instead of listing it as left to do.
    h1.down();
    local.must(&["friction", "add", "status flickers", "--new"]);
    local.must(&["friction", "link", "2", "1@h1"]);
    h1.up(&net);
    let out = local.must(&["friction", "resolve", "2", "--note", "debounced"]);
    want_in(
        "resolve",
        &out,
        &[
            "Resolved friction #2 (other).\n",
            "h1 didn't resolve #1@h1: #1 on h1 has no link to #2@local, so it wasn't resolved: link it from this side first (on h1: agent friction link 1 2@local).\n",
        ],
    );
    want_not_in("resolve", &out, &["Left to do"]);
    assert_eq!(
        h1.entry(1).status,
        FRICTION_OPEN,
        "resolving #2 closed h1's #1"
    );

    // The real link still resolves.
    assert_eq!(
        local.must(&["friction", "resolve", "1", "--note", "fixed"]),
        "Resolved friction #1 (other).\nResolved #1@h1 too.\n"
    );
}

// "missing" is a friction kind; a link whose other end is gone is "not
// found", and the other entry's kind is in parentheses, so a linked entry of
// kind missing doesn't read as a dangling link.
#[test]
fn friction_link_state_isnt_a_kind() {
    let Some(net) = net() else { return };
    let _env = agent_name("");
    let (mut local, mut h1) = (net.host("local"), net.host("h1"));
    local.must(&[
        "friction",
        "add",
        "no way to wait for several agents",
        "--kind",
        "missing",
    ]);
    local.must(&["friction", "add", "status flickers", "--new"]);
    h1.must(&[
        "friction",
        "add",
        "wait takes one agent only",
        "--kind",
        "missing",
    ]);
    local.must(&["friction", "link", "1", "1@h1"]);
    h1.down();
    local.must(&["friction", "link", "2", "7@h1"]);
    h1.up(&net);
    h1.must(&["friction", "link", "1", "1@local"]);

    let show = local.must(&["friction", "show", "1"]);
    want_in(
        "show",
        &show,
        &["  #1@h1 (missing) open, +0 there: wait takes one agent only\n"],
    );
    want_not_in("show", &show, &["!"]);
    want_in(
        "list",
        &local.must(&["friction", "list"]),
        &[
            "no way to wait for several agents [= #1@h1]\n",
            "status flickers [= #7@h1 !not found]\n",
        ],
    );
    want_in(
        "links",
        &local.must(&["friction", "links"]),
        &[
            "#1 open = #1@h1: (missing) open, +0 there: wait takes one agent only\n",
            "#2 open = #7@h1: not found: h1 has no #7\n",
        ],
    );
    let dangling: serde_json::Value =
        serde_json::from_str(&local.must(&["friction", "links", "--dangling", "--json"])).unwrap();
    assert!(
        dangling.as_array().map(Vec::len) == Some(1)
            && dangling[0]["ref"] == "#7@h1"
            && dangling[0]["state"] == "not found",
        "links --dangling --json = {dangling}"
    );
}

mod common;

use std::sync::Barrier;

use agent_store::*;
use common::setup_test_db;

/// cascade_delete_holds_on_every_pooled_connection checks that the agents →
/// state cascade (what makes cleanup drop an agent's state) is enforced on
/// every connection, not just the one that ran the startup PRAGMA. SQLite's
/// foreign_keys pragma is per-connection. The Go pools connections; here
/// every thread opens its own store, which is each its own connection.
#[test]
fn cascade_delete_holds_on_every_pooled_connection() {
    let store = setup_test_db();
    const AGENTS: u8 = 16;
    let name = |i: u8| char::from(b'a' + i).to_string();
    for i in 0..AGENTS {
        let n = name(i);
        store
            .create_agent(&Agent {
                name: n.clone(),
                session_name: format!("s-{n}"),
                backend: "claude".into(),
                created_at: Time::now(),
                ..Default::default()
            })
            .expect("create agent");
        store
            .set_state(&StateEntry {
                agent_name: n,
                key: "_status".into(),
                value: "idle".into(),
                updated_at: Time::now(),
            })
            .expect("set state");
    }

    // Delete concurrently, on separate connections and on the shared one.
    let start = Barrier::new(AGENTS as usize);
    std::thread::scope(|scope| {
        for i in 0..AGENTS {
            let (start, store) = (&start, &store);
            scope.spawn(move || {
                let own;
                let s: &Store = if i % 2 == 0 {
                    own = Store::open(&store.path).unwrap();
                    &own
                } else {
                    store
                };
                start.wait();
                if let Err(e) = s.delete_agent(&name(i)) {
                    panic!("delete agent {}: {e:#}", name(i));
                }
            });
        }
    });

    let leaked: Vec<String> = (0..AGENTS)
        .map(name)
        .filter(|n| store.get_state(n, "_status").unwrap().is_some())
        .collect();
    assert!(
        leaked.is_empty(),
        "state survived agent deletion for {}/{AGENTS} agents {leaked:?}: foreign_keys is not on for every connection",
        leaked.len()
    );
}

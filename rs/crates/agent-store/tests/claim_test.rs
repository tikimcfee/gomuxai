mod common;

use std::collections::HashMap;
use std::sync::Mutex;

use agent_store::*;
use common::setup_test_db;

fn hold_message(store: &Store, receiver: &str, content: &str) -> Message {
    let mut msg = Message {
        sender_id: "s".into(),
        receiver_id: receiver.into(),
        content: content.into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        ..Default::default()
    };
    store.create_message(&mut msg).expect("creating message");
    msg
}

/// claim_pending_messages returns the pending messages oldest first, marks
/// them delivered, and applies the state change it is given in the same
/// transaction.
#[test]
fn claim_pending_messages() {
    let store = setup_test_db();
    store
        .create_agent(&Agent {
            name: "worker".into(),
            session_name: "s".into(),
            backend: "claude".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    for c in ["first", "second", "third"] {
        hold_message(&store, "worker", c);
    }
    hold_message(&store, "other", "not mine");

    let mut seen = 0usize;
    let claimed = store
        .claim_pending_messages(
            "worker",
            Some(&mut |c: &[Message]| {
                seen = c.len();
                StateUpdate {
                    entries: vec![StateEntry {
                        agent_name: "worker".into(),
                        key: "_status".into(),
                        value: "processing".into(),
                        updated_at: Time::now(),
                    }],
                    ..Default::default()
                }
            }),
        )
        .unwrap();
    let got: Vec<&str> = claimed.iter().map(|m| m.content.as_str()).collect();
    assert!(
        got == ["first", "second", "third"] && seen == 3,
        "claimed {got:?} (update saw {seen}), want first, second, third"
    );
    let e = store.get_state("worker", "_status").unwrap();
    assert!(
        e.as_ref().is_some_and(|e| e.value == "processing"),
        "state change not applied: {e:?}"
    );
    let left = store.get_pending_messages("worker", 10).unwrap();
    assert_eq!(left.len(), 0, "still pending after the claim");
    let left = store.get_pending_messages("other", 10).unwrap();
    assert_eq!(left.len(), 1, "another agent's message was claimed");

    // Nothing left: the update still runs, with no messages.
    let mut seen: i64 = -1;
    let claimed = store.claim_pending_messages(
        "worker",
        Some(&mut |c: &[Message]| {
            seen = c.len() as i64;
            StateUpdate::default()
        }),
    );
    assert!(
        matches!(&claimed, Ok(c) if c.is_empty()) && seen == 0,
        "second claim: {claimed:?}, update saw {seen}"
    );
}

/// Of several callers racing to deliver the same messages, each message goes
/// to exactly one. Each caller has its own store (its own connection), as
/// the hooks and the watcher are separate processes.
#[test]
fn claim_each_message_once() {
    let store = setup_test_db();
    const N: usize = 20;
    let ids: Vec<i64> = (0..N)
        .map(|_| hold_message(&store, "worker", "m").id)
        .collect();

    let counts: Mutex<HashMap<i64, i32>> = Mutex::new(HashMap::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                // claims everything at once, like a Stop hook
                let s = Store::open(&store.path).unwrap();
                let claimed = s.claim_pending_messages("worker", None).unwrap();
                let mut counts = counts.lock().unwrap();
                for m in claimed {
                    *counts.entry(m.id).or_default() += 1;
                }
            });
            scope.spawn(|| {
                // claims one at a time, like the watcher
                let s = Store::open(&store.path).unwrap();
                for &id in &ids {
                    if s.claim_message(id).unwrap() {
                        *counts.lock().unwrap().entry(id).or_default() += 1;
                    }
                }
            });
        }
    });
    let counts = counts.into_inner().unwrap();
    for id in ids {
        assert_eq!(
            counts.get(&id).copied().unwrap_or(0),
            1,
            "message {id} claimed, want once"
        );
    }
}

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

#[test]
fn replace_supersede_later_filter_and_requeue() {
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

    // Pending messages across topics, plus a `later` chain whose superseded
    // predecessor must NOT leak into a mid-turn `NonLater` claim.
    let mut m1 = Message {
        sender_id: "lead".into(),
        receiver_id: "worker".into(),
        content: "try A".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        topic: "plan".into(),
        ..Default::default()
    };
    let (sup, already) = store.create_message_replacing(&mut m1, false, 0).unwrap();
    assert!(sup.is_empty() && already.is_none());

    let mut m_later_old = Message {
        sender_id: "review".into(),
        receiver_id: "worker".into(),
        content: "end-of-turn v1".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        urgency: URGENCY_LATER.into(),
        ..Default::default()
    };
    store
        .create_message_replacing(&mut m_later_old, false, 0)
        .unwrap();

    let mut m_later_new = Message {
        sender_id: "review".into(),
        receiver_id: "worker".into(),
        content: "end-of-turn v2".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        urgency: URGENCY_LATER.into(),
        ..Default::default()
    };
    let (sup_later, _) = store
        .create_message_replacing(&mut m_later_new, true, 0)
        .unwrap();
    assert_eq!(sup_later, vec![m_later_old.id]);

    let mut m2 = Message {
        sender_id: "lead".into(),
        receiver_id: "worker".into(),
        content: "switch to B".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        topic: "plan".into(),
        ..Default::default()
    };
    let (sup, _) = store.create_message_replacing(&mut m2, true, 0).unwrap();
    assert_eq!(sup, vec![m1.id]);

    // Mid-turn `NonLater` claims m2 + its superseded m1, leaving m_later_new + m_later_old!
    let mid_batch = store
        .claim_pending_batch_matching("worker", ClaimFilter::NonLater, "PostToolBatch", None)
        .unwrap();
    let mid_ids: Vec<i64> = mid_batch.iter().map(|m| m.id).collect();
    assert_eq!(mid_ids, vec![m1.id, m2.id]);

    // Requeue restores m2 to pending and m1's delivery_batch to 0, then re-claim.
    store.requeue_batch(&mid_batch).unwrap();
    let mid_batch2 = store
        .claim_pending_batch_matching("worker", ClaimFilter::NonLater, "PostToolBatch", None)
        .unwrap();
    assert_eq!(
        mid_batch2.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![m1.id, m2.id]
    );

    // Calling `--replace` when nothing is pending from `lead` reports `m2` as already delivered!
    let mut m3 = Message {
        sender_id: "lead".into(),
        receiver_id: "worker".into(),
        content: "switch to C".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        topic: "plan".into(),
        ..Default::default()
    };
    let (sup3, already3) = store.create_message_replacing(&mut m3, true, 0).unwrap();
    assert!(sup3.is_empty());
    assert_eq!(already3.unwrap().id, m2.id);

    // Turn-end `All` claims m_later_old + m_later_new + m3 together.
    let end_batch = store
        .claim_pending_batch_matching("worker", ClaimFilter::All, "Stop", None)
        .unwrap();
    assert_eq!(
        end_batch.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![m_later_old.id, m_later_new.id, m3.id]
    );
}

mod common;

use agent_store::*;
use common::{setup_test_db, temp_dir};
use rusqlite::Connection;

#[test]
fn agent_crud() {
    let store = setup_test_db();

    // Create
    let agent = Agent {
        name: "test-agent".into(),
        session_name: "agent-test-agent".into(),
        backend: "claude".into(),
        task_file: "tasks/test.md".into(),
        log_file: "/var/tmp/test.log".into(),
        spawned_by: "lead".into(),
        created_at: Time::now(),
        ..Default::default()
    };
    store.create_agent(&agent).expect("creating agent");

    // Read
    let retrieved = store.get_agent("test-agent").expect("getting agent");
    let retrieved = retrieved.expect("expected agent, got none");
    assert_eq!(retrieved.name, "test-agent");
    assert_eq!(retrieved.backend, "claude");
    assert_eq!(retrieved.spawned_by, "lead");

    // List
    let agents = store.list_agents().expect("listing agents");
    assert_eq!(agents.len(), 1);

    // Delete
    store.delete_agent("test-agent").expect("deleting agent");
    let retrieved = store
        .get_agent("test-agent")
        .expect("getting deleted agent");
    assert!(retrieved.is_none(), "expected none after delete");
}

/// A database from before spawned_by gains the column on open, and its
/// agents read back with no parent.
#[test]
fn migrate_adds_spawned_by() {
    let dir = temp_dir();
    let path = dir.path().join("old.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "
		CREATE TABLE agents (
			name TEXT PRIMARY KEY,
			session_name TEXT NOT NULL,
			backend TEXT NOT NULL,
			task_file TEXT,
			log_file TEXT,
			created_at TEXT NOT NULL
		);
		INSERT INTO agents VALUES ('old', 'agent-old', 'claude', NULL, NULL, '2026-10-01T09:30:00Z');
	",
    )
    .unwrap();
    drop(db);

    for i in 0..2 {
        // the second open finds the column already there
        let store = Store::open(&path).unwrap_or_else(|e| panic!("open {i}: {e:#}"));
        let old = store.get_agent("old").unwrap();
        assert!(
            old.as_ref().is_some_and(|o| o.spawned_by.is_empty()),
            "open {i}: old agent = {old:?}"
        );
        let name = format!("helper-{i}");
        store
            .create_agent(&Agent {
                name: name.clone(),
                session_name: format!("agent-{name}"),
                backend: "claude".into(),
                spawned_by: "old".into(),
                created_at: Time::now(),
                ..Default::default()
            })
            .unwrap_or_else(|e| panic!("open {i}: create: {e:#}"));
        let h = store.get_agent(&name).unwrap();
        assert!(
            h.as_ref().is_some_and(|h| h.spawned_by == "old"),
            "open {i}: helper = {h:?}"
        );
        store.close().unwrap();
    }
}

#[test]
fn message_crud() {
    let store = setup_test_db();

    // Create message
    let mut msg = Message {
        sender_id: "sender".into(),
        receiver_id: "receiver".into(),
        content: "hello world".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: Time::now(),
        ..Default::default()
    };
    store.create_message(&mut msg).expect("creating message");
    assert_ne!(msg.id, 0, "expected message ID to be set");

    // Get pending
    let messages = store
        .get_pending_messages("receiver", 10)
        .expect("getting pending messages");
    assert_eq!(messages.len(), 1, "expected 1 pending message");

    // Update status
    store
        .update_message_status(msg.id, MESSAGE_STATUS_DELIVERED)
        .expect("updating message status");

    // Should not appear in pending anymore
    let messages = store
        .get_pending_messages("receiver", 10)
        .expect("getting pending messages after update");
    assert_eq!(
        messages.len(),
        0,
        "expected 0 pending messages after delivery"
    );

    // Delete by agent
    store
        .delete_messages_by_agent("sender")
        .expect("deleting messages by agent");
}

/// The watcher delivers the first pending message, so the order must be the
/// order messages were queued, also within one second (created_at has whole
/// seconds).
#[test]
fn pending_messages_fifo() {
    let store = setup_test_db();

    let queued_at = Time::now();
    for content in ["A", "B", "C", "D", "E"] {
        let mut msg = Message {
            sender_id: "sender".into(),
            receiver_id: "receiver".into(),
            content: content.into(),
            status: MESSAGE_STATUS_PENDING,
            created_at: queued_at,
            ..Default::default()
        };
        store
            .create_message(&mut msg)
            .unwrap_or_else(|e| panic!("creating message {content}: {e:#}"));
    }

    for want in ["A", "B", "C", "D", "E"] {
        let pending = store
            .get_pending_messages("receiver", 1)
            .expect("getting pending");
        assert_eq!(pending.len(), 1, "expected 1 message");
        assert_eq!(pending[0].content, want, "next pending message");
        store
            .update_message_status(pending[0].id, MESSAGE_STATUS_DELIVERED)
            .expect("updating status");
    }
}

#[test]
fn pending_messages_per_receiver() {
    let store = setup_test_db();

    for agent in ["agent1", "agent2", "agent3"] {
        for _ in 0..3 {
            let mut msg = Message {
                sender_id: "coordinator".into(),
                receiver_id: agent.into(),
                content: format!("task for {agent}"),
                status: MESSAGE_STATUS_PENDING,
                created_at: Time::now(),
                ..Default::default()
            };
            store.create_message(&mut msg).unwrap();
        }
    }

    // Settling agent1's messages leaves the others' alone.
    let pending = store.get_pending_messages("agent1", 10).unwrap();
    let statuses = [
        MESSAGE_STATUS_DELIVERED,
        MESSAGE_STATUS_FAILED,
        MESSAGE_STATUS_DELIVERED,
    ];
    for (i, msg) in pending.iter().enumerate() {
        store
            .update_message_status(msg.id, statuses[i].clone())
            .unwrap();
    }

    for (agent, n) in [("agent1", 0), ("agent2", 3), ("agent3", 3), ("nobody", 0)] {
        let pending = store.get_pending_messages(agent, 10).unwrap();
        assert_eq!(pending.len(), n, "{agent}: pending");
    }
}

#[test]
fn get_non_existent_agent() {
    let store = setup_test_db();
    let agent = store.get_agent("nonexistent").expect("unexpected error");
    assert!(agent.is_none(), "expected none for nonexistent agent");
}

#[test]
fn task_crud() {
    let store = setup_test_db();

    // Tasks are standalone - no agent required
    let mut task = Task {
        name: "research-auth".into(),
        content: "# Research Auth\n\nAnalyze authentication patterns.".into(),
        source_file: "tasks/research-auth.md".into(),
        created_at: Time::now(),
    };
    store.set_task(&task).expect("setting task");

    // Get task
    let retrieved = store
        .get_task("research-auth")
        .expect("getting task")
        .expect("expected task");
    assert_eq!(retrieved.name, "research-auth");
    assert_eq!(retrieved.content, task.content);
    assert_eq!(retrieved.source_file, task.source_file);

    // Update task (upsert)
    task.content = "# Updated Task\n\nNew content.".into();
    store.set_task(&task).expect("updating task");
    let retrieved = store.get_task("research-auth").unwrap().unwrap();
    assert_eq!(retrieved.content, "# Updated Task\n\nNew content.");

    // Delete task
    store.delete_task("research-auth").expect("deleting task");
    let retrieved = store
        .get_task("research-auth")
        .expect("getting deleted task");
    assert!(retrieved.is_none(), "expected none after delete");
}

#[test]
fn get_non_existent_task() {
    let store = setup_test_db();
    let task = store.get_task("nonexistent").expect("unexpected error");
    assert!(task.is_none(), "expected none for nonexistent task");
}

#[test]
fn list_tasks() {
    let store = setup_test_db();

    // Create standalone tasks - no agents needed
    let tasks = [
        Task {
            name: "research-auth".into(),
            content: "Research authentication".into(),
            source_file: "tasks/auth.md".into(),
            created_at: Time::now(),
        },
        Task {
            name: "build-api".into(),
            content: "Build the API".into(),
            created_at: Time::now(),
            ..Default::default()
        },
        Task {
            name: "write-docs".into(),
            content: "Write documentation".into(),
            source_file: "tasks/docs.md".into(),
            created_at: Time::now(),
        },
    ];
    for task in &tasks {
        store.set_task(task).unwrap();
    }

    let listed = store.list_tasks().expect("listing tasks");
    assert_eq!(listed.len(), 3);
    let found: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
    for name in ["research-auth", "build-api", "write-docs"] {
        assert!(found.contains(&name), "expected to find task {name}");
    }
}

#[test]
fn state_crud() {
    let store = setup_test_db();

    // First create an agent (state has foreign key to agents)
    store
        .create_agent(&Agent {
            name: "test-agent".into(),
            session_name: "agent-test-agent".into(),
            backend: "claude".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .expect("creating agent");

    // Set state entries
    let mut entry1 = StateEntry {
        agent_name: "test-agent".into(),
        key: "status".into(),
        value: "working".into(),
        updated_at: Time::now(),
    };
    store.set_state(&entry1).expect("setting state 1");
    let entry2 = StateEntry {
        agent_name: "test-agent".into(),
        key: "progress".into(),
        value: "50%".into(),
        updated_at: Time::now(),
    };
    store.set_state(&entry2).expect("setting state 2");

    // Get single state
    let retrieved = store
        .get_state("test-agent", "status")
        .unwrap()
        .expect("expected state entry");
    assert_eq!(retrieved.value, "working");

    // Update state (upsert)
    entry1.value = "completed".into();
    store.set_state(&entry1).expect("updating state");
    let retrieved = store.get_state("test-agent", "status").unwrap().unwrap();
    assert_eq!(retrieved.value, "completed");

    // List all state
    let entries = store.list_state("test-agent").expect("listing state");
    assert_eq!(entries.len(), 2);

    // Delete all state for agent
    store.delete_state("test-agent").expect("deleting state");
    let entries = store
        .list_state("test-agent")
        .expect("listing state after delete");
    assert_eq!(entries.len(), 0);
}

#[test]
fn get_non_existent_state() {
    let store = setup_test_db();
    let state = store
        .get_state("nonexistent", "key")
        .expect("unexpected error");
    assert!(state.is_none(), "expected none for nonexistent state");
}

#[test]
fn cascade_delete_on_agent_removal() {
    let store = setup_test_db();

    store
        .create_agent(&Agent {
            name: "cascade-test".into(),
            session_name: "agent-cascade-test".into(),
            backend: "claude".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(&StateEntry {
            agent_name: "cascade-test".into(),
            key: "test-key".into(),
            value: "test-value".into(),
            updated_at: Time::now(),
        })
        .unwrap();

    // Delete agent - should cascade delete state (NOT tasks - they're standalone)
    store.delete_agent("cascade-test").unwrap();

    let states = store
        .list_state("cascade-test")
        .expect("listing state after cascade");
    assert_eq!(states.len(), 0, "expected state to be cascade deleted");
}

#[test]
fn tasks_are_standalone() {
    let store = setup_test_db();

    store
        .set_task(&Task {
            name: "standalone-task".into(),
            content: "This task exists independently".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .expect("setting standalone task");

    let retrieved = store
        .get_task("standalone-task")
        .unwrap()
        .expect("expected standalone task to exist");
    // The task persists regardless of agent lifecycle
    assert_eq!(retrieved.name, "standalone-task");
}

#[test]
fn agent_adopted_round_trip() {
    let store = setup_test_db();

    let mut agent = Agent {
        name: "adopted-dev".into(),
        session_name: "my-dev-session".into(),
        backend: "claude".into(),
        cwd: "/var/tmp/project".into(),
        adopted: true,
        created_at: Time::now().utc().truncate_seconds(),
        ..Default::default()
    };
    store.create_agent(&agent).expect("CreateAgent");
    let got = store.get_agent("adopted-dev").unwrap().expect("GetAgent");
    assert!(
        got.adopted && got.session_name == "my-dev-session",
        "GetAgent = {got:?}, want Adopted=true SessionName=my-dev-session"
    );
    assert_eq!(got.created_at, agent.created_at);

    let tree = store.read_tree(true).expect("ReadTree");
    assert!(
        tree.agents.len() == 1 && tree.agents[0].adopted,
        "ReadTree Agents = {:?}, want 1 adopted agent",
        tree.agents
    );

    // ReplaceAgent preserves Adopted when updated.
    agent.backend = "agy".into();
    store.replace_agent(&agent).expect("ReplaceAgent");
    let got = store.get_agent("adopted-dev").unwrap();
    assert!(
        got.as_ref()
            .is_some_and(|g| g.adopted && g.backend == "agy"),
        "after ReplaceAgent, GetAgent = {got:?}"
    );
}

/// Handover messages: claimed once, by id and receiver; failed only while
/// still awaiting handover; a missing message's status is an error in Go's
/// words.
#[test]
fn handover_claims() {
    let store = setup_test_db();
    let mut msg = Message {
        sender_id: "s".into(),
        receiver_id: "worker".into(),
        content: "long".into(),
        status: MESSAGE_STATUS_HANDOVER,
        created_at: Time::now(),
        ..Default::default()
    };
    store.create_message(&mut msg).unwrap();
    assert!(
        store.claim_handover("other", msg.id).unwrap().is_none(),
        "another receiver claimed it"
    );
    let got = store.claim_handover("worker", msg.id).unwrap();
    assert!(got.is_some_and(|m| m.content == "long" && m.status == MESSAGE_STATUS_DELIVERED));
    assert!(
        store.claim_handover("worker", msg.id).unwrap().is_none(),
        "claimed twice"
    );
    assert!(
        !store.fail_handover(msg.id).unwrap(),
        "failed a delivered handover"
    );
    assert_eq!(
        store.get_message_status(msg.id).unwrap(),
        MESSAGE_STATUS_DELIVERED
    );

    let mut msg2 = Message {
        status: MESSAGE_STATUS_HANDOVER,
        ..msg.clone()
    };
    store.create_message(&mut msg2).unwrap();
    assert!(store.fail_handover(msg2.id).unwrap());
    assert_eq!(
        store.get_message_status(msg2.id).unwrap(),
        MESSAGE_STATUS_FAILED
    );

    let err = store.get_message_status(999).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "reading message 999: sql: no rows in result set"
    );
}

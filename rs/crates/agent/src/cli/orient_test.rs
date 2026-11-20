//! Go: internal/cli/orient_test.go.

use agent_service::{AGENT_STATUS_PROCESSING, StatusReport};
use agent_store::{Agent, Time};

use super::orient::orient;
use super::testenv::TestEnv;

#[test]
fn test_orient() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();
    env.no_bus();
    env.create_live_agent(&Agent {
        name: "lead".into(),
        session_name: "rpc-cli-lead".into(),
        backend: "claude".into(),
        created_at: Time::now(),
        ..Default::default()
    });
    env.create_live_agent(&Agent {
        name: "lead-a".into(),
        session_name: "rpc-cli-lead-a".into(),
        backend: "claude".into(),
        spawned_by: "lead".into(),
        created_at: Time::now(),
        ..Default::default()
    });
    env.service()
        .report(
            "lead-a",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                ..Default::default()
            },
        )
        .unwrap();
    env.service().queue("user", "lead", "hi").unwrap();

    let person = orient(&env.ctx, "").unwrap();
    let want = "You are a person at a terminal, on host local.
Agents here (3): lead unknown (1 held), lead-a processing, worker waiting
Bus: not set up (only needed to reach other hosts: agent bus --help)
";
    assert!(
        person.starts_with(want),
        "person's orientation:\n{person}\nwant it to start:\n{want}"
    );

    let agent = orient(&env.ctx, "lead-a").unwrap();
    for line in [
        "You are agent lead-a on host local, a helper of lead.\n",
        "Your helpers: none (3 agents here: agent list).\n",
    ] {
        assert!(
            agent.contains(line),
            "agent's orientation lacks {line:?}:\n{agent}"
        );
    }

    let lead = orient(&env.ctx, "lead").unwrap();
    for line in [
        "Your helpers (1 of 3 agents here): lead-a processing\n",
        "Queued for you: 1 message, delivered when you're idle, but no watcher is running to deliver (agent watch).\n",
    ] {
        assert!(
            lead.contains(line),
            "lead's orientation lacks {line:?}:\n{lead}"
        );
    }
    let n = person.matches('\n').count();
    assert!(n <= 10, "orientation is {n} lines, want at most 10");
}

// "agent" alone orients; "agent --help" still gives the full help.
#[test]
fn test_root_help_still_full() {
    let out = super::help::render(&super::command::Cmd::root(super::root::tree()));
    for want in ["Agents:", "Low-level sessions", "spawn", "-q, --quiet"] {
        assert!(out.contains(want), "help lacks {want:?}");
    }
}

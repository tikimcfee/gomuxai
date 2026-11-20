//! Go: internal/cli/list_test.go.

use agent_bus::AgentInfo;
use agent_store::{Agent, Time};

use super::list::{self, team_order};
use super::testenv::TestEnv;

fn info(name: &str, by: &str) -> AgentInfo {
    AgentInfo {
        name: name.into(),
        spawned_by: by.into(),
        ..Default::default()
    }
}

#[test]
fn test_team_order() {
    let agents = vec![
        info("lead-review", "lead"),
        info("solo", ""),
        info("lead-build-test", "lead-build"),
        info("lead", ""),
        info("lead-build", "lead"),
        info("stray", "gone"), // its parent was cleaned up
        info("x", "y"),        // a loop, from reused names
        info("y", "x"),
    ];
    let got: Vec<String> = team_order(&agents).into_iter().map(|a| a.name).collect();
    assert_eq!(
        got.join(" "),
        "solo lead lead-review lead-build lead-build-test stray x y"
    );
}

pub fn list_args() -> list::Args {
    list::Args {
        host: String::new(),
        json: false,
        args: Vec::new(),
    }
}

#[test]
fn test_list_shows_parents() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();
    env.forbid_bus();
    env.create_live_agent(&Agent {
        name: "worker-test".into(),
        session_name: "rpc-cli-worker-test".into(),
        backend: "claude".into(),
        spawned_by: "worker".into(),
        created_at: Time::date_utc(2026, 10, 1, 9, 31, 0, 0),
        ..Default::default()
    });

    list::run(&env.ctx, &list_args()).unwrap();
    let want = [
        "worker: waiting (claude)",
        "  session: rpc-cli-worker",
        "  created: 2026-10-01 09:30:00",
        "  └ worker-test: unknown (claude)",
        "      parent:  worker",
        "      session: rpc-cli-worker-test",
        "      created: 2026-10-01 09:31:00",
    ]
    .join("\n")
        + "\n";
    assert_eq!(env.take_out(), want);
}

//! Go: internal/cli/remote_test.go. Its setup helpers (setupCLI,
//! createLiveAgent, serveAs, forbidBus) are testenv.rs's. TestFromAgent tests
//! send.rs's private from_agent, and is left to send's tests (group B).

use std::sync::atomic::Ordering;

use agent_backend::EndReason;
use agent_bus::AgentRef;
use agent_service::{AGENT_STATUS_ENDED, AGENT_STATUS_IDLE, AgentStatus, StatusReport};
use agent_store::{Agent, Time};

use super::list::list_agents;
use super::remote::{is_remote_host, parse_target};
use super::spawn::spawn_target;
use super::testenv::{TestEnv, temp_dir};

fn r(name: &str, host: &str) -> AgentRef {
    AgentRef {
        name: name.into(),
        host: host.into(),
    }
}

#[test]
fn test_parse_target() {
    let env = TestEnv::new();
    let tests: Vec<(&str, AgentRef, bool, bool)> = vec![
        ("worker", r("worker", ""), false, false),
        ("my.session", r("my.session", ""), false, false), // no "@": passed through untouched
        ("worker@local", r("worker", ""), false, false),
        ("worker@h1", r("worker", "h1"), true, false),
        ("worker@", r("", ""), false, true),
        ("worker@h.1", r("", ""), false, true),
        ("a.b@h1", r("", ""), false, true),
    ];
    for (arg, want, want_remote, want_err) in tests {
        let got = parse_target(&env.ctx, arg);
        assert_eq!(got.is_err(), want_err, "parse_target({arg:?}) = {got:?}");
        if let Ok((got, remote)) = got {
            assert_eq!((got, remote), (want, want_remote), "parse_target({arg:?})");
        }
    }
    for (host, want) in [("", false), ("local", false), ("h1", true)] {
        assert_eq!(
            is_remote_host(&env.ctx, host),
            want,
            "is_remote_host({host:?})"
        );
    }
}

#[test]
fn test_local_targets_dont_dial_the_bus() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.forbid_bus();

    for target in ["worker", "worker@local"] {
        let (out, res) = env.run(&["status", "-v", target]);
        assert!(
            res.is_ok() && out.starts_with(&format!("{target}: waiting\n")),
            "status {target} = {out:?}, {res:?}"
        );
    }
    for host in ["", "local"] {
        let (out, res) = env.run(&["list", "--host", host]);
        assert!(
            res.is_ok() && out.contains("rpc-cli-worker"),
            "list --host {host:?} = {out:?}, {res:?}"
        );
    }
    let not_found = r#"agent "ghost" not found on host local (1 agent there: agent list)"#;
    let (_, res) = env.run(&["send", "ghost@local", "hi"]);
    assert_eq!(
        res.map_err(|e| format!("{e:#}")),
        Err(not_found.to_string()),
        "send ghost@local"
    );
    let (_, res) = env.run(&["keys", "ghost@local", "hi"]);
    assert_eq!(
        res.map_err(|e| format!("{e:#}")),
        Err(not_found.to_string()),
        "keys ghost@local"
    );
}

#[test]
fn test_remote_output_matches_local() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    let dials = crate::require_bus!(env.serve_as_h1());

    let mut run = |words: &[&str]| {
        let (out, res) = env.run(words);
        res.unwrap_or_else(|e| panic!("{words:?} failed: {e:#}"));
        out
    };
    let local = run(&["status", "-v", "worker"]);
    let remote = run(&["status", "-v", "worker@h1"]);
    assert_eq!(
        remote,
        local.replacen("worker:", "worker@h1:", 1),
        "status -v differs"
    );
    assert!(
        remote.contains("tool:   Bash"),
        "remote status lost detail:\n{remote}"
    );

    let local = run(&["list"]);
    let remote = run(&["list", "--host", "h1"]);
    assert_eq!(remote, local, "list differs");

    assert_eq!(
        dials.load(Ordering::Relaxed),
        2,
        "bus dials: want 2 (once per remote command)"
    );
}

#[test]
fn test_remote_errors_match_local() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    crate::require_bus!(env.serve_as_h1());

    let pairs: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        (
            "status",
            vec!["status", "ghost"],
            vec!["status", "ghost@h1"],
        ),
        (
            "capture",
            vec!["capture", "ghost"],
            vec!["capture", "ghost@h1"],
        ),
        (
            "send",
            vec!["send", "ghost", "hi"],
            vec!["send", "ghost@h1", "hi"],
        ),
        (
            "keys",
            vec!["keys", "ghost@local", "hi"],
            vec!["keys", "ghost@h1", "hi"],
        ),
        (
            "approve --expect",
            vec!["approve", "worker", "--expect", "Write"],
            vec!["approve", "worker@h1", "--expect", "Write"],
        ),
    ];
    for (name, local, remote) in pairs {
        let (_, local_err) = env.run(&local);
        let (_, remote_err) = env.run(&remote);
        let (Err(l), Err(rem)) = (local_err, remote_err) else {
            panic!("{name}: want both to fail");
        };
        // Listing a remote host's agents takes --host (here the served
        // host's own name, since h1 is this CLI's service).
        assert_eq!(
            format!("{l:#}"),
            format!("{rem:#}").replacen("agent list --host local", "agent list", 1),
            "{name}"
        );
    }
}

#[test]
fn test_remote_needs_bus() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.no_bus();

    for words in [
        vec!["status", "worker@h2"],
        vec!["capture", "worker@h2"],
        vec!["send", "worker@h2", "hi"],
        vec!["keys", "worker@h2", "hi"],
        vec!["deny", "worker@h2"],
        vec!["list", "--host", "h2"],
    ] {
        let (_, res) = env.run(&words);
        let err = res.expect_err("no bus");
        assert!(
            agent_bus::is_no_bus(&err) && format!("{err:#}").contains("needs a running agent bus"),
            "{words:?}: err = {err:#}"
        );
    }
}

/// Writes a trusted peer bundle for each host into the CLI's bus directory,
/// as agent bus trust does (Go: trustPeers).
pub fn trust_peers(env: &TestEnv, hosts: &[&str]) {
    for host in hosts {
        let dir = temp_dir("agent-cli-peer-");
        let id = agent_bus::init_identity(dir.path(), host).unwrap();
        agent_bus::trust_peer(env.ctx.cfg().bus_dir(), id.public_bundle("")).unwrap();
    }
}

#[test]
fn test_remote_host_not_answering() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    crate::require_bus!(env.serve_as_h1());
    trust_peers(&env, &["monolith1", "h1"]);

    let (_, res) = env.run(&["status", "worker@h2"]);
    let err = format!("{:#}", res.expect_err("no answer"));
    assert!(
        err.contains(r#"no bus answering for host "h2""#),
        "err = {err}"
    );
    // It names the hosts it knows, so a misspelled one stands out.
    assert!(
        err.ends_with("; this host is local, and its bus trusts h1, monolith1"),
        "err = {err}, want the known hosts named"
    );
}

// Host names match ignoring case. Bus subjects don't, so a host this one
// knows (itself, or a trusted peer) is addressed in its own spelling: "agent
// list --host dev-box-m2" reached no bus when the peer was "Dev-Box-M2".
#[test]
fn test_host_names_match_ignoring_case() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    let svc = std::sync::Arc::clone(env.service());
    crate::require_bus!(env.serve_svc_as("Dev-Box-M2", svc, agent_bus::DEFAULT_REMOTE_OPS));
    trust_peers(&env, &["Dev-Box-M2"]);

    for (arg, want) in [
        ("worker@LOCAL", r("worker", "")),
        ("worker@dev-box-m2", r("worker", "Dev-Box-M2")),
        ("worker@DEV-BOX-M2", r("worker", "Dev-Box-M2")),
        ("worker@h2", r("worker", "h2")), // unknown here: as given
    ] {
        let got = parse_target(&env.ctx, arg).map(|(r, _)| r);
        assert_eq!(got.ok(), Some(want), "parse_target({arg:?})");
    }
    assert!(!is_remote_host(&env.ctx, "Local"), "Local is this host");

    let (out, res) = env.run(&["status", "-v", "worker@dev-box-m2"]);
    assert!(
        res.is_ok() && out.starts_with("worker@dev-box-m2: waiting\n"),
        "status worker@dev-box-m2 = {out:?}, {res:?}"
    );
    let (out, res) = env.run(&["list", "--host", "dev-box-m2"]);
    assert!(
        res.is_ok() && out.contains("rpc-cli-worker"),
        "list --host dev-box-m2 = {out:?}, {res:?}"
    );
    // Go also checks parseWaitRefs(["worker@dev-box-m2", "worker@Dev-Box-M2"])
    // is one ref on Dev-Box-M2: wait.rs's parse_wait_refs_folds_host_case.
    let (sr, remote) = spawn_target(&env.ctx, "helper", "dev-box-m2").unwrap();
    assert!(
        remote && sr.host == "Dev-Box-M2",
        "spawn_target = {sr:?}, {remote}"
    );
    let hosts = super::export::export_host_list(&env.ctx, "dev-box-m2,LOCAL,Dev-Box-M2").unwrap();
    assert_eq!(hosts, vec!["Dev-Box-M2"]);
}

// --force skips send's dialog check, so it's refused inside agents (and to
// anything without a terminal, as agents' commands are).
#[test]
fn test_send_force_refused_for_agents() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    env.set_env("AGENT_NAME", "lead");
    let (_, res) = env.run(&["send", "--force", "worker", "y"]);
    let err = format!("{:#}", res.expect_err("refused"));
    assert!(err.contains("needs a person"), "err = {err}, want refusal");
    env.set_env("AGENT_NAME", ""); // tests run without a terminal on stdin
    let (_, res) = env.run(&["send", "--force", "worker", "y"]);
    let err = format!("{:#}", res.expect_err("refused"));
    assert!(
        err.contains("needs a person"),
        "without a terminal: err = {err}, want refusal"
    );
}

// Queueing isn't served over the bus: name@<this host> queues locally, and
// another host is refused without dialing it.
#[test]
fn test_queue_targets() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    env.forbid_bus();
    env.set_env("AGENT_NAME", "");

    for (i, target) in ["worker", "worker@local"].iter().enumerate() {
        let (out, res) = env.run(&["queue", target, "hi"]);
        let want = if i == 1 {
            "Queued for worker from user, behind 1 other message (it is waiting).\n"
        } else {
            "Queued for worker from user (it is waiting).\n"
        };
        assert!(
            res.is_ok() && out == want,
            "queue {target} = {out:?}, {res:?}; want {want:?}"
        );
    }
    let (_, res) = env.run(&["queue", "worker@h1", "hi"]);
    let err = format!("{:#}", res.expect_err("refused"));
    assert!(
        err.contains("agent send worker@h1"),
        "queue worker@h1: err = {err}"
    );
    let (_, res) = env.run(&["queue", "worker@", "hi"]);
    assert!(res.is_err(), "queue worker@: want an error");

    let pending = env.store().get_pending_messages("worker", 10).unwrap();
    assert_eq!(
        pending.len(),
        2,
        "pending messages: want one per local target"
    );
}

// The bus's list carries what export shows for the serving host's agents,
// with resumable worked out there by resume's rules.
#[test]
fn test_remote_list_fields() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    crate::require_bus!(env.serve_as_h1());
    env.store()
        .create_agent(&Agent {
            name: "done".into(),
            session_name: "rpc-cli-done".into(),
            backend: "claude".into(),
            cwd: "/srv/w".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.service()
        .report(
            "done",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                end_reason: EndReason::from("quit"),
                session_id: "5f0c".into(),
                transcript: "/home/u/.claude/projects/-srv-w/5f0c.jsonl".into(),
                ..Default::default()
            },
        )
        .unwrap();

    let agents = list_agents(&env.ctx, "h1").unwrap();
    let w = agents.iter().find(|a| a.name == "worker").expect("worker");
    assert!(
        w.attached == Some(true)
            && w.resumable == Some(false)
            && w.detail == "permission: Bash: ls",
        "worker = {w:?}, want attached, not resumable (waiting), its detail"
    );
    let d = agents.iter().find(|a| a.name == "done").expect("done");
    assert!(
        d.attached == Some(false)
            && d.resumable == Some(true)
            && d.session_id == "5f0c"
            && d.cwd == "/srv/w"
            && d.status == "ended"
            && d.end_reason == "quit",
        "done = {d:?}, want detached, resumable, its session id and cwd, ended (quit)"
    );
}

// resume_refusal (what resume says) and types::resumable (what list and
// export say) are the same rules.
#[test]
fn test_resume_rules_agree() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();
    for backend in ["claude", "agy", "aider"] {
        for adopted in [false, true] {
            for session in ["", "5f0c"] {
                for status in [AGENT_STATUS_ENDED, AGENT_STATUS_IDLE] {
                    let a = Agent {
                        name: "x".into(),
                        session_name: "rpc-cli-x".into(),
                        backend: backend.into(),
                        adopted,
                        ..Default::default()
                    };
                    let refused = env
                        .service()
                        .resume_refusal(&a, &AgentStatus::from(status.as_str()), session)
                        .is_err();
                    let ok = agent_base::types::resumable(
                        backend,
                        adopted,
                        session,
                        status == AGENT_STATUS_ENDED,
                    );
                    assert_ne!(
                        ok, refused,
                        "{backend} adopted={adopted} session={session:?} {status}: resumable {ok}, refused {refused}"
                    );
                }
            }
        }
    }
}

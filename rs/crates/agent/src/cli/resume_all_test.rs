//! Go: internal/cli/resume_all_test.go. The Go registers a claude factory
//! that records the session each resume continues; here the agent's
//! launch.sh (the CLI's command line, with --resume <id>) says it, and the
//! claude CLI is testenv's stand-in (`exec cat`).

use std::sync::Mutex;
use std::time::Duration;

use agent_backend::EndReason;
use agent_bus::{Context, ResumeResponse};
use agent_service::{AGENT_STATUS_ENDED, AGENT_STATUS_IDLE, StatusReport};
use agent_store::{Agent, Time};

use super::resume;
use super::resume::resume_agent;
use super::resume_all::{ResumeAllOpts, ResumeOne, run_resume_all_with};
use super::root::Ctx;
use super::spawn_continue_test::{claude_transcript, setup_continue};
use super::testenv::TestEnv;

/// Records an agent whose termd session is gone, as a reboot leaves it: its
/// last report is report (idle if empty), with transcript (or, without one,
/// session id session_id) as its session (Go: goneAgent).
pub fn gone_agent(
    env: &TestEnv,
    name: &str,
    parent: &str,
    backend: &str,
    transcript: &str,
    session_id: &str,
    mut report: StatusReport,
) {
    env.store()
        .create_agent(&Agent {
            name: name.into(),
            session_name: format!("rpc-cli-{name}"),
            backend: backend.into(),
            spawned_by: parent.into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    if report.status.is_empty() {
        report.status = AGENT_STATUS_IDLE;
    }
    report.transcript = transcript.into();
    report.session_id = session_id.into();
    env.service().report(name, report).unwrap();
}

/// The session the agent's CLI was launched to continue, from its
/// launch.sh ("" if none).
pub fn launched_resume(env: &TestEnv, name: &str) -> String {
    let script = std::fs::read_to_string(format!("{}/launch.sh", env.cfg.agent_dir(name)))
        .unwrap_or_default();
    let mut words = script.split_whitespace();
    while let Some(w) = words.next() {
        if w == "--resume" || w == "'--resume'" {
            return words.next().unwrap_or("").trim_matches('\'').to_string();
        }
    }
    String::new()
}

fn opts(resume_one: ResumeOne<'_>, parallel: usize) -> ResumeAllOpts<'_> {
    ResumeAllOpts {
        parallel,
        spacing: Duration::ZERO,
        resume_one,
    }
}

const HOUR: Duration = Duration::from_secs(3600);

// After a reboot, resume --all brings back the agents it ended, parents
// first, and leaves the ones ended on purpose or that can't be resumed,
// saying why; --dry-run says the same and starts nothing.
#[test]
fn test_resume_all() {
    crate::require_termd!();
    let c = setup_continue(None);
    let (env, home, dir) = (&c.env, c.home.as_str(), c.a.as_str());
    let bg = Context::background();

    let (lead_id, helper_id, quit_id) = (
        "11111111-0000-0000-0000-000000000001",
        "11111111-0000-0000-0000-000000000002",
        "11111111-0000-0000-0000-000000000003",
    );
    gone_agent(
        env,
        "lead",
        "",
        "claude",
        &claude_transcript(home, lead_id, dir, "", HOUR),
        "",
        StatusReport::default(),
    );
    gone_agent(
        env,
        "lead-h",
        "lead",
        "claude",
        &claude_transcript(home, helper_id, dir, "", HOUR),
        "",
        StatusReport::default(),
    );
    gone_agent(
        env,
        "quitter",
        "",
        "claude",
        &claude_transcript(home, quit_id, dir, "", HOUR),
        "",
        StatusReport {
            status: AGENT_STATUS_ENDED,
            end_reason: EndReason::from("quit"),
            ..Default::default()
        },
    );
    gone_agent(env, "aid", "", "aider", "", "", StatusReport::default());
    gone_agent(env, "blank", "", "claude", "", "", StatusReport::default());

    let skips = [
        "skipped aid: agent aid runs aider: resume continues claude and agy sessions",
        "skipped blank: no session to resume agent blank on: its CLI never named one (agent logs blank --terminal shows what it did)",
        "skipped quitter: ended (quit), on purpose, not by a reboot: agent resume quitter resumes it",
    ];
    let one = |ctx: &Ctx, c: &Context, n: &str, by: &str, s: bool, t: bool| {
        resume_agent(ctx, c, n, by, s, t)
    };
    run_resume_all_with(&env.ctx, &bg, true, "", false, false, &opts(&one, 4)).unwrap();
    let out = env.take_out();
    let want = [
        skips[0].to_string(),
        skips[1].to_string(),
        format!("would resume lead (claude) on session {lead_id}, in {dir}"),
        skips[2].to_string(),
        format!("would resume lead-h (claude) on session {helper_id}, in {dir}"),
        "Would resume 2 agents; nothing changed (dry run). Skipped 3. 1 agent still running."
            .to_string(),
    ]
    .join("\n")
        + "\n";
    assert_eq!(out, want, "--dry-run");
    for name in ["lead", "lead-h"] {
        assert!(
            !env.termd_ops()
                .session_exists(&format!("rpc-cli-{name}"))
                .unwrap_or(false),
            "--dry-run started {name}"
        );
    }

    run_resume_all_with(&env.ctx, &bg, false, "", false, false, &opts(&one, 4))
        .unwrap_or_else(|e| panic!("resume --all: {e:#}"));
    let out = env.take_out();
    let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
    assert!(
        lines.len() == 6
            && lines[0] == skips[0]
            && lines[1] == skips[1]
            && lines[3] == skips[2]
            && lines[2].starts_with(&format!(
                "resumed lead (claude) on session {lead_id}, in {dir}"
            ))
            && lines[4].starts_with(&format!(
                "resumed lead-h (claude) on session {helper_id}, in {dir}"
            ))
            && lines[5] == "Resumed 2 agents. Skipped 3. 1 agent still running.",
        "resume --all printed:\n{out}"
    );
    assert_eq!(launched_resume(env, "lead"), lead_id);
    assert_eq!(launched_resume(env, "lead-h"), helper_id);
    for name in ["lead", "lead-h"] {
        assert!(
            env.termd_ops()
                .session_exists(&format!("rpc-cli-{name}"))
                .unwrap_or(false),
            "{name} has no termd session after resume --all"
        );
    }
    assert!(
        !env.termd_ops()
            .session_exists("rpc-cli-quitter")
            .unwrap_or(false),
        "resume --all resumed quitter, which ended on purpose"
    );

    // Nothing left that a reboot ended.
    let res = run_resume_all_with(&env.ctx, &bg, false, "", false, false, &opts(&one, 4));
    let out = env.take_out();
    assert!(
        res.is_ok()
            && out.ends_with(
                "No agents here were ended by a reboot or a stopped g3d-termd. Skipped 3. 3 agents still running.\n"
            ),
        "second resume --all: {res:?}\n{out}"
    );
}

// resume --all brings back one depth of the tree at a time, a few at once
// within it, prints its lines in a stable order whatever order they finish
// in, and fails if any resume did. Inside an agent it covers only that
// agent's helpers.
#[test]
fn test_resume_all_order_and_parallel() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();
    let tree = [
        ("p1", ""),
        ("p2", ""),
        ("p3", ""),
        ("p4", ""),
        ("h1", "p1"),
        ("h2", "p1"),
        ("hh", "h1"),
        ("q-h", "p3"),
    ];
    for (name, parent) in tree {
        gone_agent(
            &env,
            name,
            parent,
            "claude",
            "",
            &format!("sess-{name}"),
            StatusReport::default(),
        );
    }

    struct Seen {
        events: Vec<String>,
        callers: Vec<String>,
        active: usize,
        peak: usize,
    }
    let seen = Mutex::new(Seen {
        events: Vec::new(),
        callers: Vec::new(),
        active: 0,
        peak: 0,
    });
    let one = |_: &Ctx,
               _: &Context,
               name: &str,
               caller: &str,
               _: bool,
               _: bool|
     -> anyhow::Result<ResumeResponse> {
        {
            let mut s = seen.lock().unwrap();
            s.events.push(format!("start {name}"));
            s.callers.push(caller.to_string());
            s.active += 1;
            s.peak = s.peak.max(s.active);
        }
        // Stands in for the CLI's startup, so resumes overlap.
        std::thread::sleep(Duration::from_millis(20));
        {
            let mut s = seen.lock().unwrap();
            s.events.push(format!("end {name}"));
            s.active -= 1;
        }
        if name == "p3" {
            anyhow::bail!("boom");
        }
        Ok(ResumeResponse {
            name: name.into(),
            backend: "claude".into(),
            session_id: format!("sess-{name}"),
            cwd: "/w".into(),
            status: "idle".into(),
            ..Default::default()
        })
    };

    let bg = Context::background();
    let res = run_resume_all_with(&env.ctx, &bg, false, "", false, false, &opts(&one, 2));
    assert_eq!(
        res.map_err(|e| format!("{e:#}")),
        Err("1 of 8 resumes failed".to_string())
    );
    let want = "resumed p1 (claude) on session sess-p1, in /w: idle\n\
resumed p2 (claude) on session sess-p2, in /w: idle\n\
failed p3: boom\n\
resumed p4 (claude) on session sess-p4, in /w: idle\n\
resumed h1 (claude) on session sess-h1, in /w: idle\n\
resumed h2 (claude) on session sess-h2, in /w: idle\n\
resumed q-h (claude) on session sess-q-h, in /w: idle\n\
resumed hh (claude) on session sess-hh, in /w: idle\n\
Resumed 7 of 8 agents; 1 failed. 1 agent still running.\n";
    assert_eq!(env.take_out(), want);
    let s = seen.lock().unwrap();
    assert_eq!(s.peak, 2, "most resumed at once, want 2 (the limit)");
    let at = |event: &str| s.events.iter().position(|e| e == event).unwrap();
    for (name, parent) in tree {
        if !parent.is_empty() {
            assert!(
                at(&format!("start {name}")) > at(&format!("end {parent}")),
                "{name} started before its parent {parent} was back: {:?}",
                s.events
            );
        }
    }
    // Each depth finishes before the next starts.
    for p in ["p1", "p2", "p3", "p4"] {
        for h in ["h1", "h2", "q-h"] {
            assert!(
                at(&format!("start {h}")) > at(&format!("end {p}")),
                "{h} started before depth 0's {p} was done: {:?}",
                s.events
            );
        }
    }
    drop(s);

    // Inside agent p1: its helpers and theirs only, resumed as p1. (The
    // stand-in started nothing, so every one is still ended (gone).)
    {
        let mut s = seen.lock().unwrap();
        s.events.clear();
        s.callers.clear();
    }
    run_resume_all_with(&env.ctx, &bg, false, "p1", false, false, &opts(&one, 2)).unwrap();
    let out = env.take_out();
    assert!(
        out.matches("resumed ").count() == 3
            && out.contains("resumed hh ")
            && out.ends_with(
                "Resumed 3 agents. Left 5 more agents here that others spawned, not you (\"p1\") or your helpers.\n"
            ),
        "resume --all as p1:\n{out}"
    );
    let s = seen.lock().unwrap();
    assert!(
        s.callers.iter().all(|c| c == "p1"),
        "resumed as {:?}, want p1 each time",
        s.callers
    );
}

#[test]
fn test_resume_args() {
    for (words, want) in [
        (vec!["--all", "lead"], "--all picks the agents itself"),
        (
            vec!["--all", "lead@hub"],
            "run agent resume --all on hub to resume its own, or agent resume lead@hub for that one",
        ),
        (
            vec!["--all", "--attach"],
            "--attach opens one agent's session, and --all resumes several",
        ),
        (vec!["--dry-run", "lead"], "--dry-run goes with --all"),
        (vec![], "accepts 1 arg"),
    ] {
        let mut env = TestEnv::new();
        let mut line = vec!["resume"];
        line.extend(words.iter().copied());
        let (_, res) = env.run(&line);
        let err = res.expect_err("an argument error");
        assert!(
            format!("{err:#}").contains(want),
            "{words:?}: {err:#}, want {want:?}"
        );
    }
    // The two good shapes pass the argument check (they fail later, here,
    // for want of a termd or an agent).
    for words in [vec!["--all", "--dry-run"], vec!["lead"]] {
        let mut env = TestEnv::new();
        let mut line = vec!["resume"];
        line.extend(words.iter().copied());
        let (_, res) = env.run(&line);
        if let Err(e) = res {
            let e = format!("{e:#}");
            assert!(!e.contains("Usage:"), "{words:?}: an argument error: {e}");
        }
    }
}

// An agy conversation whose workspace agy hasn't recorded yet (its prompts'
// history lines carry no conversation id) resumes in the directory the
// agent's record says its session ran in.
#[test]
fn test_resume_session_agy_without_workspace() {
    crate::require_termd!();
    let c = setup_continue(None);
    let (env, home, dir) = (&c.env, c.home.as_str(), c.a.as_str());
    let conv = "320ae9a3-0000-0000-0000-000000000001";
    let base = format!("{home}/.gemini/antigravity-cli");
    let path = format!("{base}/brain/{conv}/.system_generated/logs/transcript_full.jsonl");
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    // No presence lock for it: agy doesn't hold it open.
    std::fs::create_dir_all(format!("{base}/presence")).unwrap();
    std::fs::write(&path, "{\"type\":\"USER_INPUT\",\"content\":\"hi\"}\n").unwrap();
    std::fs::write(
        format!("{base}/history.jsonl"),
        format!("{{\"display\":\"hi\",\"timestamp\":1,\"workspace\":\"{dir}\"}}\n"),
    )
    .unwrap();
    env.store()
        .create_agent(&Agent {
            name: "ag".into(),
            session_name: "rpc-cli-ag".into(),
            backend: "agy".into(),
            cwd: dir.into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.service()
        .report(
            "ag",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                transcript: path.clone(),
                session_id: conv.into(),
                ..Default::default()
            },
        )
        .unwrap();
    let mut agent = env.service().agent("ag").unwrap();
    let s = resume::resume_session(&env.ctx, &agent).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(
        s.session_id == conv && s.cwd == dir,
        "resume_session = {} in {:?}; want {conv} in {dir}",
        s.session_id,
        s.cwd
    );

    agent.cwd = String::new();
    let err = resume::resume_session(&env.ctx, &agent).expect_err("no cwd");
    assert!(
        format!("{err:#}").contains("recorded no working directory"),
        "with no cwd anywhere: {err:#}"
    );
}

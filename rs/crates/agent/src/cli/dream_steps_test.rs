//! Tests of the global dream's steps and the idea, ideas, judge and done
//! verbs (docs/DREAMING.md; no Go file).
use std::sync::{Arc, Mutex};

use agent_service::SpawnOptions;
use agent_store::{
    DREAM_ABANDONED, DREAM_COMMITTED, DREAM_OPEN, DreamWorker, FRICTION_ACCEPTED, FRICTION_OPEN,
    Friction, ROLE_CRITIC, ROLE_DREAMER, ROLE_GENERATOR, Time, WORKER_DONE, WORKER_FAILED,
    WORKER_RUNNING,
};

use super::{
    CRITIC_MODELS, Presence, SYNTH_MODELS, Step, WORKER_IDLE_LIMIT, WORKER_RUN_LIMIT, given_up,
    next_step,
};
use crate::cli::testenv::TestEnv;

fn run(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    res.unwrap_or_else(|e| panic!("{words:?}: {e:#}\n{out}"));
    out
}

fn run_err(env: &mut TestEnv, words: &[&str]) -> String {
    let (_, res) = env.run(words);
    format!("{:#}", res.expect_err(&format!("{words:?} succeeded")))
}

fn be(env: &mut TestEnv, agent: &str) {
    env.set_env("AGENT_NAME", agent);
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
}

fn file_friction(env: &TestEnv, uid: &str, summary: &str) -> Friction {
    let now = Time::now();
    let mut f = Friction {
        uid: uid.into(),
        host: "local".into(),
        filed_by: "user".into(),
        created_at: now,
        updated_at: now,
        kind: "bug".into(),
        summary: summary.into(),
        status: FRICTION_OPEN.into(),
        ..Default::default()
    };
    env.store().add_friction(&mut f).unwrap();
    f
}

fn worker(agent: &str, role: &str, status: &str) -> DreamWorker {
    DreamWorker {
        status: status.into(),
        ..DreamWorker::new(agent, role, "", "")
    }
}

#[test]
fn the_next_step_follows_the_workers() {
    let alive = |_: &DreamWorker| None;
    let gone = |_: &DreamWorker| Some("is gone".to_string());
    let g = |s: &str| worker("g", ROLE_GENERATOR, s);
    let c = |s: &str| worker("c", ROLE_CRITIC, s);

    assert_eq!(
        next_step(&[g(WORKER_RUNNING)], 0, &alive),
        Step::Wait("g".into())
    );
    assert_eq!(
        next_step(&[g(WORKER_RUNNING)], 2, &gone),
        Step::Abandon {
            why: "its generator g is gone".into(),
            failed: Some("g".into())
        }
    );
    assert_eq!(
        next_step(&[g(WORKER_DONE)], 2, &alive),
        Step::Critic,
        "a critic is due"
    );
    assert_eq!(
        next_step(&[g(WORKER_DONE)], 0, &alive),
        Step::Apply,
        "nothing to judge"
    );
    assert_eq!(
        next_step(&[g(WORKER_DONE), c(WORKER_RUNNING)], 2, &alive),
        Step::Wait("c".into())
    );
    assert_eq!(
        next_step(&[g(WORKER_DONE), c(WORKER_RUNNING)], 2, &gone),
        Step::Abandon {
            why: "its critic c is gone".into(),
            failed: Some("c".into())
        }
    );
    assert_eq!(
        next_step(&[g(WORKER_DONE), c(WORKER_DONE)], 2, &gone),
        Step::Apply,
        "the done aren't checked"
    );
    assert_eq!(
        next_step(&[g(WORKER_DONE), c(WORKER_FAILED)], 2, &alive),
        Step::Abandon {
            why: "its critic c failed".into(),
            failed: None
        }
    );
    assert_eq!(
        next_step(&[c(WORKER_DONE)], 2, &alive),
        Step::Abandon {
            why: "it has no generator".into(),
            failed: None
        }
    );
}

#[test]
fn a_worker_idle_or_running_too_long_is_given_up() {
    let now = Time::now();
    let mins = |m: i64| Time(now.0 - chrono::Duration::minutes(m));
    let w = |started: i64| DreamWorker {
        started_at: mins(started),
        ..worker("g", ROLE_GENERATOR, WORKER_RUNNING)
    };
    let here = |status: &str, idle: Option<i64>| Presence::Here {
        status: status.into(),
        idle_since: idle.map(mins),
    };
    assert_eq!(
        given_up(&w(5), &Presence::Gone, &now).as_deref(),
        Some("is gone")
    );
    assert_eq!(
        given_up(&w(40), &here("idle", Some(10)), &now),
        None,
        "idle, but not for long"
    );
    let why = given_up(&w(40), &here("idle", Some(31)), &now).expect("idle too long");
    assert!(
        why.starts_with("sat idle for 31 minutes without saying done (the limit is 30)"),
        "{why}"
    );
    let why = given_up(&w(35), &here("idle", None), &now).expect("idle since it started");
    assert!(why.starts_with("sat idle for 35 minutes"), "{why}");
    assert_eq!(
        given_up(&w(80), &here("processing", None), &now),
        None,
        "at work, within the limit"
    );
    let why = given_up(&w(91), &here("processing", None), &now).expect("running too long");
    assert_eq!(why, "ran for 91 minutes, past the limit of 90");
    let why = given_up(&w(95), &here("waiting", None), &now).expect("waiting counts as running");
    assert!(why.starts_with("ran for 95"), "{why}");
    assert_eq!(
        (
            WORKER_IDLE_LIMIT.num_minutes(),
            WORKER_RUN_LIMIT.num_minutes()
        ),
        (30, 90)
    );
    // A worker given up is abandoned like a gone one.
    let check = |w: &DreamWorker| given_up(w, &here("idle", Some(45)), &now);
    match next_step(&[w(50)], 1, &check) {
        Step::Abandon { why, failed } => {
            assert!(
                why.starts_with("its generator g sat idle for 45 minutes"),
                "{why}"
            );
            assert_eq!(failed.as_deref(), Some("g"));
        }
        s => panic!("{s:?}"),
    }
}

#[test]
fn run_gives_up_a_worker_idle_too_long() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    let synth = format!("dream-{id}-synth");
    // Its agent's session is there (a shell at its prompt), it says it is
    // idle, and it last reported 40 minutes ago.
    g.env
        .termd_ops()
        .create_session(&format!("rpc-cli-{synth}"), &Default::default())
        .unwrap();
    let long_ago = Time(Time::now().0 - chrono::Duration::minutes(40));
    g.env
        .store()
        .set_state(&agent_store::StateEntry {
            agent_name: synth.clone(),
            key: "_status".into(),
            value: "idle".into(),
            updated_at: long_ago,
        })
        .unwrap();
    let p = presence_of(&g.env, &synth);
    assert!(
        matches!(&p, Presence::Here { status, idle_since: Some(t) } if status == "idle" && t.0 == long_ago.0),
        "{p:?}"
    );
    g.env.set_env("AGENT_NAME", "");
    g.env.ctx.set_caller_chain(|| Ok(Vec::new()));
    let out = run(&mut g.env, &["dream", "run"]);
    assert!(
        out.contains(&format!(
            "Closed dream #{id} as abandoned: its generator {synth} sat idle for 40 minutes"
        )),
        "{out}"
    );
    assert!(out.contains(&format!("Cleaned up {synth}")), "{out}");
    let d = g.env.store().get_dream(id).unwrap().unwrap();
    assert!(
        d.diary.contains("abandoned: its generator") && d.diary.contains("without saying done"),
        "{}",
        d.diary
    );
    assert_eq!(
        g.env.store().dream_workers(id).unwrap()[0].status,
        WORKER_FAILED
    );
}

fn presence_of(env: &TestEnv, agent: &str) -> Presence {
    super::presence(&env.ctx, agent)
}

#[test]
fn the_critic_runs_on_another_family() {
    for seed in [0, 1, 2, 3] {
        let g = super::generator_worker(7, seed);
        let c = super::critic_worker(7, &g, seed / 2);
        assert_eq!(
            (g.agent.as_str(), c.agent.as_str()),
            ("dream-7-synth", "dream-7-critic")
        );
        assert!(
            SYNTH_MODELS.contains(&(g.model.as_str(), g.effort.as_str())),
            "{g:?}"
        );
        assert!(
            CRITIC_MODELS.contains(&(c.model.as_str(), c.effort.as_str())),
            "{c:?}"
        );
        assert_eq!(super::backend_for(&g.model), "claude");
        assert_eq!(super::backend_for(&c.model), "agy");
    }
    assert_ne!(
        super::generator_worker(1, 0).model,
        super::generator_worker(1, 1).model
    );
    let g = super::generator_worker(1, 0);
    assert_ne!(
        super::critic_worker(1, &g, 0).model,
        super::critic_worker(1, &g, 1).model,
        "both critics get picked"
    );
}

const FACT: &str = "---\nname: build\ndescription: build with your own target dir\nmetadata:\n  type: feedback\n---\nEach checkout builds with its own CARGO_TARGET_DIR.\n**Why:** a shared one gives false errors.\n[source: CITES; added: 2026-11-22]\n";

struct Global {
    env: TestEnv,
    spawns: Arc<Mutex<Vec<SpawnOptions>>>,
    id: i64,
    machine: String,
    friction: Friction,
}

/// A host with a scoped project and a friction entry, and a global dream
/// started on it with its workers recorded instead of spawned.
fn start_global() -> Global {
    let mut env = TestEnv::with_termd_private();
    let spawns = Arc::new(Mutex::new(Vec::new()));
    env.ctx.dream_spawns = Some(Arc::clone(&spawns));
    let proj = env.dir.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    run(
        &mut env,
        &["dream", "scope", "add", &proj.to_string_lossy()],
    );
    let friction = file_friction(&env, "local-1", "wait misses a turn");
    let out = run(&mut env, &["dream", "run", "--global"]);
    let d = env.store().list_dreams(1).unwrap().remove(0);
    assert_eq!(d.status, DREAM_OPEN, "{out}");
    assert_eq!(d.agent, format!("dream-{}-synth", d.id));
    assert!(
        out.contains(&format!(
            "its synthesizer dream-{}-synth (generator, ",
            d.id
        )),
        "{out}"
    );
    let machine = super::super::dream::machine_memory_dir(&env.ctx)
        .to_string_lossy()
        .into_owned();
    Global {
        env,
        spawns,
        id: d.id,
        machine,
        friction,
    }
}

impl Global {
    fn dream_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.env.ctx.data_dir())
            .join("dreams")
            .join(self.id.to_string())
    }

    /// Writes an idea (and a draft, for a put) in the dream's directory and
    /// proposes it as the generator.
    fn idea(&mut self, name: &str, json: &str) -> (String, anyhow::Result<()>) {
        let path = self.dream_dir().join(format!("{name}.json"));
        std::fs::write(&path, json).unwrap();
        let p = path.to_string_lossy().into_owned();
        self.env.run(&["dream", "idea", "--from", &p])
    }

    fn draft(&self, name: &str, content: &str) -> String {
        let path = self.dream_dir().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }
}

#[test]
fn a_global_dream_proposes_judges_applies_and_commits() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    let synth = format!("dream-{id}-synth");
    let critic = format!("dream-{id}-critic");

    // The generator was spawned confined, on a model and effort of the pair,
    // with the synthesizer's brief.
    let spawned: Vec<SpawnOptions> = g.spawns.lock().unwrap().clone();
    assert_eq!(spawned.len(), 1);
    let gen_opts = &spawned[0];
    assert_eq!(gen_opts.name, synth);
    assert!(
        SYNTH_MODELS.contains(&(gen_opts.model.as_str(), gen_opts.effort.as_str())),
        "{gen_opts:?}"
    );
    assert_eq!(gen_opts.permission_mode, "dontAsk");
    assert!(gen_opts.confined);
    assert!(
        gen_opts.task_file.ends_with("brief-synth.md"),
        "{}",
        gen_opts.task_file
    );
    let brief = std::fs::read_to_string(&gen_opts.task_file).unwrap();
    assert!(
        brief.starts_with(&format!("You are {synth}, the synthesizer of dream #{id}")),
        "{brief}"
    );
    assert!(
        gen_opts.allow_tools.iter().any(|r| r.starts_with("Edit(")),
        "{:?}",
        gen_opts.allow_tools
    );
    let w = g.env.store().dream_worker(id, &synth).unwrap().unwrap();
    assert_eq!(
        (w.role.as_str(), w.model.as_str(), w.effort.as_str()),
        (
            ROLE_GENERATOR,
            gen_opts.model.as_str(),
            gen_opts.effort.as_str()
        )
    );
    let diary = g.env.store().get_dream(id).unwrap().unwrap().diary;
    assert!(
        diary.contains(&format!("started {synth} (generator, {}", gen_opts.model)),
        "{diary}"
    );

    be(&mut g.env, &synth);
    let draft = g.draft("build.md", &FACT.replace("CITES", "friction:local-1"));
    let machine = g.machine.clone();
    let put = format!(
        r#"{{"op":"put","dir":"{machine}","file":"build.md","expect_version":"new","draft":"{draft}","why":"filed and seen","citations":["friction:local-1"]}}"#
    );
    assert_eq!(
        g.idea("put", &put).0,
        "Recorded idea #1: memory put ".to_string() + &machine + "/build.md.\n"
    );
    let accept = r#"{"op":"accept","id":1,"note":"real","citations":["friction:local-1"]}"#;
    assert_eq!(
        g.idea("accept", accept).0,
        "Recorded idea #2: friction accept #1.\n"
    );
    // Changing the draft afterwards changes nothing: its content was read.
    std::fs::write(&draft, "scribbled over").unwrap();
    let out = run(&mut g.env, &["dream", "ideas"]);
    assert!(
        out.contains("#1  proposed  memory put") && out.contains("    | Each checkout builds"),
        "{out}"
    );

    // The generator's done starts the critic on the other model.
    let out = run(&mut g.env, &["dream", "done"]);
    assert!(
        out.starts_with(&format!(
            "{synth} is done (2 ideas proposed).\nStarted dream #{id}'s critic {critic} (critic, "
        )),
        "{out}"
    );
    let spawned: Vec<SpawnOptions> = g.spawns.lock().unwrap().clone();
    assert_eq!(spawned.len(), 2);
    let crit_opts = &spawned[1];
    assert_eq!(crit_opts.name, critic);
    assert_ne!(crit_opts.model, gen_opts.model);
    // Another family: agy, confined by its fail-closed hook.
    assert_eq!(
        (gen_opts.backend.as_str(), crit_opts.backend.as_str()),
        ("claude", "agy")
    );
    assert!(crit_opts.confined, "{crit_opts:?}");
    assert!(CRITIC_MODELS.contains(&(crit_opts.model.as_str(), crit_opts.effort.as_str())));
    assert!(crit_opts.task_file.ends_with("brief-critic.md"));
    assert!(
        !crit_opts.allow_tools.iter().any(|r| r.starts_with("Edit(")),
        "a critic edits nothing: {:?}",
        crit_opts.allow_tools
    );
    let err = run_err(&mut g.env, &["dream", "done"]);
    assert!(err.contains("is already done"), "{err}");

    be(&mut g.env, &critic);
    let err = run_err(
        &mut g.env,
        &["dream", "judge", "1", "approve", "--why", "looks right"],
    );
    assert!(err.contains("approving needs --checked naming a source you read among the idea's citations (friction:local-1)"), "{err}");
    let err = run_err(
        &mut g.env,
        &[
            "dream",
            "judge",
            "1",
            "approve",
            "--why",
            "x",
            "--checked",
            "claude:other",
        ],
    );
    assert!(err.contains("approving needs --checked"), "{err}");
    let err = run_err(&mut g.env, &["dream", "judge", "1", "maybe", "--why", "x"]);
    assert!(err.contains("\"maybe\" is no verdict"), "{err}");
    let err = run_err(&mut g.env, &["dream", "judge", "9", "reject", "--why", "x"]);
    assert!(
        err.contains(&format!("dream #{id} has no idea #9")),
        "{err}"
    );
    let err = run_err(
        &mut g.env,
        &[
            "dream",
            "judge",
            "1",
            "approve",
            "--why",
            "x",
            "--checked",
            "friction:local-1",
            "--evidence",
            "7",
        ],
    );
    assert!(err.contains("--evidence is 7"), "{err}");
    let out = run(
        &mut g.env,
        &[
            "dream",
            "judge",
            "1",
            "approve",
            "--why",
            "the entry says so",
            "--checked",
            "friction:local-1",
            "--evidence",
            "4",
        ],
    );
    assert_eq!(
        out,
        format!("Approved idea #1: memory put {machine}/build.md.\n")
    );
    let err = run_err(&mut g.env, &["dream", "done"]);
    assert!(
        err.contains("judge every idea first: #2 has no verdict"),
        "{err}"
    );
    run(
        &mut g.env,
        &[
            "dream",
            "judge",
            "2",
            "approve",
            "--why",
            "filed, still happening",
            "--checked",
            "friction:local-1",
        ],
    );

    // The critic's done applies the approved ideas and commits.
    let out = run(&mut g.env, &["dream", "done"]);
    assert!(out.contains(&format!("Applied dream #{id}'s ideas (2 applied, 0 skipped, 0 rejected of 2 ideas) and committed it")), "{out}");
    let d = g.env.store().get_dream(id).unwrap().unwrap();
    assert_eq!(d.status, DREAM_COMMITTED);
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&machine).join("build.md")).unwrap(),
        FACT.replace("CITES", "friction:local-1"),
        "what the critic judged is what was written"
    );
    let f = g.env.store().get_friction(g.friction.id).unwrap().unwrap();
    assert_eq!(f.status, FRICTION_ACCEPTED);
    assert!(d.diary.contains("idea #1 memory put"), "{}", d.diary);
    assert!(
        d.diary.contains(&format!(
            "approved by {critic} on {}: the entry says so; applied: wrote build.md",
            crit_opts.model
        )),
        "{}",
        d.diary
    );
    assert!(
        d.diary.contains("applied: accepted friction #1"),
        "{}",
        d.diary
    );
    let ws = g.env.store().dream_workers(id).unwrap();
    assert!(ws.iter().all(|w| w.status == WORKER_DONE), "{ws:?}");
    let out = run(&mut g.env, &["dream", "show", &id.to_string()]);
    assert!(
        out.contains(&format!("Worker:   {critic} (critic, ")),
        "{out}"
    );
    assert!(out.contains("Ideas:    2 (2 applied"), "{out}");
    let v: serde_json::Value = serde_json::from_str(&run(
        &mut g.env,
        &["dream", "ideas", &id.to_string(), "--json"],
    ))
    .unwrap();
    assert_eq!(v[0]["status"], "applied");
    assert_eq!(v[0]["scores"]["evidence"], 4);
    assert_eq!(v[0]["proposal"]["kind"], "memory");

    // The next run cleans up both workers.
    g.env.set_env("AGENT_NAME", "");
    g.env.ctx.set_caller_chain(|| Ok(Vec::new()));
    let out = run(&mut g.env, &["dream", "run", "--global", "--dry-run"]);
    assert!(
        out.contains(&format!("Would clean up {synth}"))
            && out.contains(&format!("Would clean up {critic}")),
        "{out}"
    );
}

#[test]
fn verbs_check_the_callers_role() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    let synth = format!("dream-{id}-synth");
    let machine = g.machine.clone();
    be(&mut g.env, &synth);
    let draft = g.draft("d.md", &FACT.replace("CITES", "friction:local-1"));
    for (words, want) in [
        (
            vec![
                "dream",
                "memory",
                "put",
                "a.md",
                "--dir",
                machine.as_str(),
                "--expect",
                "new",
                "--from",
                draft.as_str(),
            ],
            "agent dream memory put is for a dream's dreamer",
        ),
        (
            vec!["dream", "friction", "accept", "1"],
            "agent dream friction accept is for a dream's dreamer",
        ),
        (
            vec!["dream", "commit"],
            "agent dream commit is for a dream's dreamer",
        ),
        (
            vec![
                "dream",
                "roster",
                "note",
                "claude/x",
                "what",
                "--task",
                "edit",
                "--verdict",
                "good",
                "--session",
                "claude:x",
            ],
            "agent dream roster note is for a dream's dreamer",
        ),
        (
            vec!["dream", "judge", "1", "reject", "--why", "x"],
            "agent dream judge is for a dream's critic",
        ),
    ] {
        let err = run_err(&mut g.env, &words);
        assert!(err.contains(want), "{words:?}: {err}");
        assert!(
            err.contains(&format!(
                "{synth} is dream #{id}'s generator (it proposes changes with agent dream idea"
            )),
            "{err}"
        );
    }
    // What every worker may do.
    assert_eq!(
        run(&mut g.env, &["dream", "note", "read it all"]),
        format!("Noted in dream #{id}'s diary.\n")
    );
    assert_eq!(
        run(&mut g.env, &["dream", "ideas"]),
        format!("Dream #{id} has no ideas.\n")
    );

    // A critic proposes nothing; a project dream's dreamer neither proposes,
    // judges nor says done.
    g.env
        .store()
        .set_dream_worker_status(id, &synth, WORKER_DONE)
        .unwrap();
    let critic = DreamWorker::new(&format!("dream-{id}-critic"), ROLE_CRITIC, "", "");
    g.env.store().add_dream_worker(id, &critic).unwrap();
    be(&mut g.env, &critic.agent);
    let err = run_err(&mut g.env, &["dream", "idea", "--from", &draft]);
    assert!(
        err.contains("agent dream idea is for a dream's generator; dream-")
            && err.contains("'s critic (it judges ideas"),
        "{err}"
    );
    g.env.store().abandon_dream(id, "testing").unwrap();
    let err = run_err(&mut g.env, &["dream", "note", "late"]);
    assert!(
        err.contains(&format!("{} has no open dream", critic.agent)),
        "{err}"
    );
    // The generator's state names its dream, which is closed now.
    be(&mut g.env, &synth);
    let err = run_err(&mut g.env, &["dream", "note", "late"]);
    assert!(err.contains(&format!("dream #{id} is abandoned")), "{err}");
    let p = g
        .env
        .store()
        .open_dream("local", &[], |id| format!("dream-{id}"))
        .unwrap();
    g.env
        .store()
        .create_agent(&agent_store::Agent {
            name: p.agent.clone(),
            session_name: format!("rpc-cli-{}", p.agent),
            backend: "shell".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        g.env.store().dream_workers(p.id).unwrap()[0].role,
        ROLE_DREAMER
    );
    be(&mut g.env, &p.agent);
    for (words, want) in [
        (
            vec!["dream", "idea", "--from", draft.as_str()],
            "agent dream idea is for a dream's generator",
        ),
        (
            vec!["dream", "judge", "1", "reject", "--why", "x"],
            "agent dream judge is for a dream's critic",
        ),
        (
            vec!["dream", "done"],
            "agent dream done is for a dream's generator or critic",
        ),
    ] {
        let err = run_err(&mut g.env, &words);
        assert!(err.contains(want), "{words:?}: {err}");
    }
    // ... and still writes and commits as before.
    assert_eq!(
        run(&mut g.env, &["dream", "commit"]),
        format!(
            "Committed dream #{}: its 0 items count as reviewed.\n",
            p.id
        )
    );
}

#[test]
fn ideas_are_checked_before_they_are_recorded() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    be(&mut g.env, &format!("dream-{id}-synth"));
    let machine = g.machine.clone();
    let good = g.draft("good.md", &FACT.replace("CITES", "friction:local-1"));
    let uncited = g.draft(
        "uncited.md",
        &FACT.replace("CITES", "friction:local-1, claude:nobody"),
    );
    let outside = g.env.dir.path().join("outside.md");
    std::fs::write(&outside, FACT.replace("CITES", "friction:local-1")).unwrap();
    let outside = outside.to_string_lossy().into_owned();
    let put = |dir: &str, file: &str, draft: &str, cites: &str| {
        format!(
            r#"{{"op":"put","dir":"{dir}","file":"{file}","expect_version":"new","draft":"{draft}","why":"w","citations":[{cites}]}}"#
        )
    };
    for (json, want) in [
        ("not json".to_string(), "isn't an idea"),
        (
            r#"{"op":"accept","id":1,"note":"n","citations":["friction:local-1"],"extra":1}"#
                .to_string(),
            "unknown field `extra`",
        ),
        (
            put(
                "/srv/elsewhere/memory",
                "a.md",
                &good,
                r#""friction:local-1""#,
            ),
            "/srv/elsewhere/memory is no memory directory of dream",
        ),
        (
            put(&machine, "a.md", &outside, r#""friction:local-1""#),
            "isn't a file in your dream's directory",
        ),
        (
            put(&machine, "a.md", &good, r#""friction:local-9""#),
            "not recorded: friction:local-9 cites nothing",
        ),
        (
            put(&machine, "a.md", &good, r#""report:r1""#),
            "not recorded: report:r1 cites nothing",
        ),
        (
            put(&machine, "a.md", &uncited, r#""friction:local-1""#),
            "not recorded: claude:nobody cites nothing",
        ),
        (
            r#"{"op":"accept","id":7,"note":"n","citations":["friction:local-1"]}"#.to_string(),
            "there is no friction #7",
        ),
        (
            r#"{"op":"dup","id":1,"other":8,"note":"n","citations":["friction:local-1"]}"#
                .to_string(),
            "there is no friction #8",
        ),
    ] {
        let (_, res) = g.idea("bad", &json);
        let err = format!("{:#}", res.expect_err(&json));
        assert!(err.contains(want), "{json}: {err}");
    }
    assert!(
        g.env.store().dream_ideas(id).unwrap().is_empty(),
        "nothing refused was recorded"
    );
    // With a trailing slash on the dir, it is the same memory directory.
    let (out, res) = g.idea(
        "ok",
        &put(
            &format!("{machine}/"),
            "a.md",
            &good,
            r#""friction:local-1""#,
        ),
    );
    res.unwrap();
    assert_eq!(
        out,
        format!("Recorded idea #1: memory put {machine}/a.md.\n")
    );
    let (_, res) = g.idea(
        "again",
        &put(&machine, "a.md", &good, r#""friction:local-1""#),
    );
    assert!(format!("{:#}", res.unwrap_err()).contains("idea #1 already changes a.md"));
    let votes = g.env.store().idea_votes(1).unwrap();
    assert_eq!(votes.len(), 1, "its generator's own vote");
}

#[test]
fn a_version_conflict_skips_the_idea_and_the_rest_applies() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    let machine = g.machine.clone();
    std::fs::write(
        std::path::Path::new(&machine).join("build.md"),
        FACT.replace("CITES", "friction:local-1"),
    )
    .unwrap();
    let v = agent_dream::memory::version(FACT.replace("CITES", "friction:local-1").as_bytes());
    be(&mut g.env, &format!("dream-{id}-synth"));
    let draft = g.draft(
        "build.md",
        &FACT
            .replace("Each checkout", "Every checkout")
            .replace("CITES", "friction:local-1"),
    );
    g.idea(
        "put",
        &format!(r#"{{"op":"put","dir":"{machine}","file":"build.md","expect_version":"{v}","draft":"{draft}","why":"w","citations":["friction:local-1"]}}"#),
    )
    .1
    .unwrap();
    g.idea(
        "accept",
        r#"{"op":"accept","id":1,"note":"real","citations":["friction:local-1"]}"#,
    )
    .1
    .unwrap();
    g.idea(
        "dup",
        r#"{"op":"dup","id":1,"other":1,"note":"n","citations":["friction:local-1"]}"#,
    )
    .1
    .unwrap_err();
    run(&mut g.env, &["dream", "done"]);
    be(&mut g.env, &format!("dream-{id}-critic"));
    run(
        &mut g.env,
        &[
            "dream",
            "judge",
            "1",
            "approve",
            "--why",
            "ok",
            "--checked",
            "friction:local-1",
        ],
    );
    run(
        &mut g.env,
        &["dream", "judge", "2", "reject", "--why", "one session only"],
    );
    // Someone changes the file before the dream applies.
    let changed = FACT
        .replace("CITES", "friction:local-1")
        .replace("own", "very own");
    std::fs::write(std::path::Path::new(&machine).join("build.md"), &changed).unwrap();
    let out = run(&mut g.env, &["dream", "done"]);
    assert!(
        out.contains("(0 applied, 1 skipped, 1 rejected of 2 ideas) and committed it"),
        "{out}"
    );
    let d = g.env.store().get_dream(id).unwrap().unwrap();
    assert!(
        d.diary.contains("skipped: build.md is at version"),
        "{}",
        d.diary
    );
    assert!(
        d.diary.contains("idea #2 friction accept #1") && d.diary.contains("rejected by"),
        "{}",
        d.diary
    );
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&machine).join("build.md")).unwrap(),
        changed,
        "left as someone left it"
    );
    assert_eq!(
        g.env.store().get_friction(1).unwrap().unwrap().status,
        FRICTION_OPEN
    );
}

#[test]
fn applied_ideas_that_break_the_guards_are_put_back() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    let machine = g.machine.clone();
    // Two files carrying the same citation: removing one keeps it in the
    // other, but loses half the dir's files, which commit's guard refuses.
    // (The snapshot is the dream's, taken when it opened: so write them
    // under a project memory the dream will snapshot on first write.)
    let dir = std::path::Path::new(&machine);
    let a = FACT.replace("CITES", "friction:local-1");
    let b = a.replace("name: build", "name: build2");
    std::fs::write(dir.join("a.md"), &a).unwrap();
    std::fs::write(dir.join("b.md"), &b).unwrap();
    // As if they were there when the dream opened.
    let snap =
        crate::cli::dream_memory::snapshot_dir_for(&g.env.ctx.data_dir(), &id.to_string(), dir);
    std::fs::remove_dir_all(&snap).unwrap();
    agent_dream::memory::take_snapshot(dir, &snap).unwrap();
    be(&mut g.env, &format!("dream-{id}-synth"));
    let v = agent_dream::memory::version(a.as_bytes());
    g.idea(
        "rm",
        &format!(r#"{{"op":"remove","dir":"{machine}","file":"a.md","expect_version":"{v}","why":"b keeps it","citations":["friction:local-1"]}}"#),
    )
    .1
    .unwrap();
    // A friction triage proposed before it: it applies only after the
    // memory guards pass, so here it never does.
    g.idea(
        "accept",
        r#"{"op":"accept","id":1,"note":"real","citations":["friction:local-1"]}"#,
    )
    .1
    .unwrap();
    run(&mut g.env, &["dream", "done"]);
    be(&mut g.env, &format!("dream-{id}-critic"));
    for idea in ["1", "2"] {
        run(
            &mut g.env,
            &[
                "dream",
                "judge",
                idea,
                "approve",
                "--why",
                "ok",
                "--checked",
                "friction:local-1",
            ],
        );
    }
    let out = run(&mut g.env, &["dream", "done"]);
    assert!(out.contains("Put back"), "{out}");
    assert!(out.contains("abandoned: the applied memory changes (1 applied, 1 skipped, 0 rejected of 2 ideas) break 1 commit rule"), "{out}");
    assert!(
        out.contains("memory is put back and no friction triage was applied"),
        "{out}"
    );
    let d = g.env.store().get_dream(id).unwrap().unwrap();
    assert_eq!(d.status, DREAM_ABANDONED);
    assert!(
        d.diary.contains(
            "idea #2 friction accept #1: not applied: the memory changes broke commit's guards"
        ),
        "{}",
        d.diary
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.md")).unwrap(),
        a,
        "put back"
    );
    assert_eq!(
        g.env.store().get_friction(1).unwrap().unwrap().status,
        FRICTION_OPEN,
        "nothing applied"
    );
    let ideas = g.env.store().dream_ideas(id).unwrap();
    assert_eq!(ideas[0].status, agent_store::IDEA_SKIPPED);
    assert!(
        ideas[0]
            .outcome
            .ends_with("then put back: commit's guards failed"),
        "{}",
        ideas[0].outcome
    );
}

#[test]
fn friction_applies_after_the_memory_changes_pass() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    let machine = g.machine.clone();
    be(&mut g.env, &format!("dream-{id}-synth"));
    // The friction idea comes first by id, but applies after the memory
    // change and the guards.
    g.idea(
        "accept",
        r#"{"op":"accept","id":1,"note":"real","citations":["friction:local-1"]}"#,
    )
    .1
    .unwrap();
    let draft = g.draft("build.md", &FACT.replace("CITES", "friction:local-1"));
    g.idea(
        "put",
        &format!(r#"{{"op":"put","dir":"{machine}","file":"build.md","expect_version":"new","draft":"{draft}","why":"w","citations":["friction:local-1"]}}"#),
    )
    .1
    .unwrap();
    run(&mut g.env, &["dream", "done"]);
    be(&mut g.env, &format!("dream-{id}-critic"));
    for idea in ["1", "2"] {
        run(
            &mut g.env,
            &[
                "dream",
                "judge",
                idea,
                "approve",
                "--why",
                "ok",
                "--checked",
                "friction:local-1",
            ],
        );
    }
    let out = run(&mut g.env, &["dream", "done"]);
    assert!(
        out.contains("(2 applied, 0 skipped, 0 rejected of 2 ideas) and committed it"),
        "{out}"
    );
    assert!(std::path::Path::new(&machine).join("build.md").exists());
    assert_eq!(
        g.env.store().get_friction(1).unwrap().unwrap().status,
        FRICTION_ACCEPTED
    );
    let d = g.env.store().get_dream(id).unwrap().unwrap();
    // The diary keeps id order whatever order they applied in.
    let one = d.diary.find("idea #1 friction accept").unwrap();
    let two = d.diary.find("idea #2 memory put").unwrap();
    assert!(one < two, "{}", d.diary);
}

#[test]
fn run_picks_up_a_step_left_undone() {
    crate::require_termd!();
    let mut g = start_global();
    let id = g.id;
    let synth = format!("dream-{id}-synth");
    be(&mut g.env, &synth);
    g.idea(
        "accept",
        r#"{"op":"accept","id":1,"note":"real","citations":["friction:local-1"]}"#,
    )
    .1
    .unwrap();
    // The generator finished but the critic never started (a crash between).
    g.env
        .store()
        .set_dream_worker_status(id, &synth, WORKER_DONE)
        .unwrap();
    g.env.set_env("AGENT_NAME", "");
    g.env.ctx.set_caller_chain(|| Ok(Vec::new()));
    let out = run(&mut g.env, &["dream", "run", "--dry-run"]);
    assert!(
        out.starts_with(&format!(
            "Would start dream #{id}'s critic dream-{id}-critic (critic, "
        )),
        "{out}"
    );
    let out = run(&mut g.env, &["dream", "run"]);
    assert!(
        out.starts_with(&format!("Started dream #{id}'s critic dream-{id}-critic")),
        "{out}"
    );
    assert_eq!(g.spawns.lock().unwrap().len(), 2);
    assert_eq!(
        g.env.store().get_dream(id).unwrap().unwrap().status,
        DREAM_OPEN,
        "nothing else starts"
    );

    // Its critic goes away (its recorded session isn't there): the next run
    // abandons the dream.
    let out = run(&mut g.env, &["dream", "run"]);
    assert!(
        out.contains(&format!(
            "Closed dream #{id} as abandoned: its critic dream-{id}-critic is gone."
        )),
        "{out}"
    );
    let d = g.env.store().get_dream(id).unwrap().unwrap();
    assert_eq!(d.status, DREAM_ABANDONED);
    let ws = g.env.store().dream_workers(id).unwrap();
    assert_eq!(ws[1].status, WORKER_FAILED);
    assert_eq!(
        g.env.store().get_friction(1).unwrap().unwrap().status,
        FRICTION_OPEN,
        "nothing applied"
    );
}

#[test]
fn a_project_dream_is_still_one_dreamer() {
    crate::require_termd!();
    let (mut env, home) = crate::cli::sessions::sessions_test::setup_sessions();
    let spawns = Arc::new(Mutex::new(Vec::new()));
    env.ctx.dream_spawns = Some(Arc::clone(&spawns));
    let shop = env.dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let shop = shop.to_string_lossy().into_owned();
    crate::cli::sessions::sessions_test::claude_transcript(
        &home,
        "old",
        &shop,
        "Old work",
        std::time::Duration::from_secs(86400),
    );
    run(&mut env, &["dream", "scope", "add", &shop]);
    let out = run(&mut env, &["dream", "run"]);
    let d = env.store().list_dreams(1).unwrap().remove(0);
    assert!(
        out.starts_with(&format!("Started dream #{} over {shop} with ", d.id)),
        "{out}"
    );
    assert!(
        out.contains(&format!(": agent dream-{} (its digest is", d.id)),
        "{out}"
    );
    let spawned = spawns.lock().unwrap().clone();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].name, format!("dream-{}", d.id));
    assert_eq!(
        (spawned[0].model.as_str(), spawned[0].effort.as_str()),
        ("", "")
    );
    assert!(spawned[0].task_file.ends_with("/brief.md"));
    let brief = std::fs::read_to_string(&spawned[0].task_file).unwrap();
    assert!(
        brief.starts_with(&format!(
            "You are dream-{}, dreaming dream #{}, a project dream over {shop}",
            d.id, d.id
        )),
        "{brief}"
    );
    let ws = env.store().dream_workers(d.id).unwrap();
    assert_eq!(ws.len(), 1);
    assert_eq!(
        (ws[0].role.as_str(), ws[0].status.as_str()),
        (ROLE_DREAMER, WORKER_RUNNING)
    );
    assert!(
        !env.store()
            .get_dream(d.id)
            .unwrap()
            .unwrap()
            .diary
            .contains("started"),
        "a dreamer's start isn't noted"
    );
}

use crate::tree_test::new_test_store;
use crate::*;

#[test]
fn a_dream_records_its_workers() {
    let (s, _, _dir) = new_test_store();
    // A project dream's one agent is its dreamer.
    let p = s.open_dream("h", &[], |id| format!("dream-{id}")).unwrap();
    let ws = s.dream_workers(p.id).unwrap();
    assert_eq!(ws.len(), 1);
    assert_eq!(
        (
            ws[0].agent.as_str(),
            ws[0].role.as_str(),
            ws[0].status.as_str()
        ),
        (p.agent.as_str(), ROLE_DREAMER, WORKER_RUNNING)
    );
    s.commit_dream(p.id).unwrap();
    assert_eq!(s.dream_workers(p.id).unwrap()[0].status, WORKER_DONE);

    // A global dream's generator, then its critic.
    let g = s
        .open_dream_with("h", &DreamOf::global(), &[], |id| {
            DreamWorker::new(&format!("dream-{id}-synth"), ROLE_GENERATOR, "m1", "xhigh")
        })
        .unwrap();
    assert_eq!(g.agent, format!("dream-{}-synth", g.id));
    let (d, w) = s.open_dream_worker(&g.agent).unwrap().unwrap();
    assert_eq!(
        (d.id, w.role.as_str(), w.model.as_str(), w.effort.as_str()),
        (g.id, ROLE_GENERATOR, "m1", "xhigh")
    );
    s.set_dream_worker_status(g.id, &g.agent, WORKER_DONE)
        .unwrap();
    let critic = format!("dream-{}-critic", g.id);
    s.add_dream_worker(g.id, &DreamWorker::new(&critic, ROLE_CRITIC, "m2", ""))
        .unwrap();
    assert_eq!(s.open_dream_of(&critic).unwrap().map(|d| d.id), Some(g.id));
    let ws = s.dream_workers(g.id).unwrap();
    assert_eq!(
        ws.iter().map(|w| w.role.as_str()).collect::<Vec<_>>(),
        [ROLE_GENERATOR, ROLE_CRITIC]
    );
    assert!(!ws[0].finished_at.is_zero() && ws[1].finished_at.is_zero());
    assert!(
        s.set_dream_worker_status(g.id, "nobody", WORKER_DONE)
            .is_err()
    );

    // Every worker's sessions are the dreams' own.
    let agents = s.dream_agents().unwrap();
    for a in [p.agent.as_str(), g.agent.as_str(), critic.as_str()] {
        assert!(agents.contains(a), "{a}: {agents:?}");
    }

    // Abandoning fails the workers still running.
    s.abandon_dream(g.id, "testing").unwrap();
    let ws = s.dream_workers(g.id).unwrap();
    assert_eq!(
        (ws[0].status.as_str(), ws[1].status.as_str()),
        (WORKER_DONE, WORKER_FAILED)
    );
    assert!(s.open_dream_of(&critic).unwrap().is_none());
    assert!(
        s.add_dream_worker(g.id, &DreamWorker::new("late", ROLE_CRITIC, "", ""))
            .is_err()
    );
}

#[test]
fn dreams_from_before_workers_get_their_dreamer() {
    let (s, path, _dir) = new_test_store();
    // Rows as a store from before workers held them.
    {
        let conn = s.conn();
        conn.execute_batch(
            "INSERT INTO dreams (host, agent, started_at, status) VALUES ('h', 'dream-1', '2026-11-01T00:00:00Z', 'committed');
			 INSERT INTO dreams (host, agent, started_at, status) VALUES ('h', 'dream-2', '2026-11-02T00:00:00Z', 'abandoned');
			 INSERT INTO dreams (host, agent, started_at, status) VALUES ('h', 'dream-3', '2026-11-03T00:00:00Z', 'open');
			 INSERT INTO dreams (host, agent, started_at, status) VALUES ('h', '', '2026-11-04T00:00:00Z', 'quiet');",
        )
        .unwrap();
    }
    drop(s);
    let s = Store::open(&path).unwrap();
    let got: Vec<(String, String)> = (1..=4)
        .flat_map(|id| s.dream_workers(id).unwrap())
        .map(|w| (w.agent, w.status))
        .collect();
    assert_eq!(
        got,
        [
            ("dream-1".to_string(), WORKER_DONE.to_string()),
            ("dream-2".to_string(), WORKER_FAILED.to_string()),
            ("dream-3".to_string(), WORKER_RUNNING.to_string()),
        ]
    );
    assert_eq!(s.open_dream_of("dream-3").unwrap().map(|d| d.id), Some(3));
    // Migrating again adds nothing.
    drop(s);
    let s = Store::open(&path).unwrap();
    assert_eq!(s.dream_workers(3).unwrap().len(), 1);
}

#[test]
fn ideas_are_recorded_voted_and_judged() {
    let (s, _, _dir) = new_test_store();
    let g = s
        .open_dream_with("h", &DreamOf::global(), &[], |id| {
            DreamWorker::new(&format!("dream-{id}-synth"), ROLE_GENERATOR, "m1", "")
        })
        .unwrap();
    let (_, w) = s.open_dream_worker(&g.agent).unwrap().unwrap();
    let cites = vec!["dream:1".to_string(), "friction:h-1".to_string()];
    let id = s
        .add_dream_idea(
            g.id,
            CALLER_SYNTHESIS,
            &w,
            r#"{"op":"accept","id":1}"#,
            &cites,
        )
        .unwrap();
    let i = s.get_dream_idea(id).unwrap().unwrap();
    assert_eq!(
        (i.status.as_str(), i.caller.as_str(), i.worker.as_str()),
        (IDEA_PROPOSED, CALLER_SYNTHESIS, g.agent.as_str())
    );
    assert_eq!(i.citations, cites);
    assert_eq!(i.rank, None);
    assert_eq!(i.scores, Scores::default());
    let votes = s.idea_votes(id).unwrap();
    assert_eq!(votes.len(), 1);
    assert_eq!(
        (votes[0].model.as_str(), votes[0].evidence.clone()),
        ("m1", cites.clone())
    );
    assert_eq!(s.ideas_of_caller(CALLER_SYNTHESIS).unwrap().len(), 1);

    let mut v = Verdict {
        status: IDEA_APPROVED.into(),
        judged_by: "critic".into(),
        why: "the sessions show it".into(),
        checked: vec!["friction:h-1".into()],
        scores: Scores {
            evidence: Some(4),
            ..Default::default()
        },
    };
    s.judge_dream_idea(id, &v).unwrap();
    let i = s.get_dream_idea(id).unwrap().unwrap();
    assert_eq!(
        (i.status.as_str(), i.judged_by.as_str(), i.why.as_str()),
        (IDEA_APPROVED, "critic", "the sessions show it")
    );
    assert_eq!(i.checked, ["friction:h-1"]);
    assert_eq!(i.scores.evidence, Some(4));
    assert_eq!(i.scores.leverage, None);

    v.scores.novelty = Some(6);
    let err = s.judge_dream_idea(id, &v).unwrap_err();
    assert!(format!("{err:#}").contains("--novelty is 6"), "{err:#}");
    v.scores.novelty = None;
    v.status = "maybe".into();
    assert!(s.judge_dream_idea(id, &v).is_err());
    assert!(
        s.judge_dream_idea(
            99,
            &Verdict {
                status: IDEA_REJECTED.into(),
                ..Default::default()
            }
        )
        .is_err()
    );

    s.set_idea_outcome(id, IDEA_APPLIED, "accepted #1").unwrap();
    v.status = IDEA_REJECTED.into();
    let err = s.judge_dream_idea(id, &v).unwrap_err();
    assert!(format!("{err:#}").contains("is applied"), "{err:#}");
    assert_eq!(s.dream_ideas(g.id).unwrap()[0].outcome, "accepted #1");

    s.commit_dream(g.id).unwrap();
    assert!(
        s.add_dream_idea(g.id, CALLER_SYNTHESIS, &w, "{}", &[])
            .is_err(),
        "a closed dream takes no ideas"
    );
}

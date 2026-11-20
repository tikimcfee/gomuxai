mod common;

use agent_store::*;
use chrono::TimeDelta;
use common::{setup_test_db, temp_dir};

fn job(script: &str) -> Job {
    Job {
        script: script.into(),
        path: format!("/w/{script}"),
        sha256: "ab".repeat(32),
        content: "#!/bin/sh\necho 'hi' && exit 3\n".into(),
        by: "lead".into(),
        host: "hub".into(),
        cwd: "/w".into(),
        args: vec!["a b".into(), "\"c\"".into()],
        started_at: Time::date_utc(2026, 11, 1, 9, 30, 15, 123_456_789),
        ..Default::default()
    }
}

#[test]
fn job_lifecycle() {
    let s = setup_test_db();
    let mut j = job("build.sh");
    s.create_job(&mut j).unwrap();
    assert_eq!(j.id, 1);
    let mut j2 = job("test.sh");
    s.create_job(&mut j2).unwrap();
    assert_eq!(j2.id, 2);

    let got = s.get_job(1).unwrap().unwrap();
    assert_eq!(got, j);
    assert!(got.running());
    assert_eq!(got.exit_code, None);

    j.unit = "agent-job-0a1b2c3d-1.scope".into();
    j.memory_high = "24G".into();
    j.memory_max = "28G".into();
    j.pid = 4242;
    j.pid_start = "123456".into();
    s.update_job_launch(&j).unwrap();
    assert_eq!(s.get_job(1).unwrap().unwrap(), j);

    let end = j.started_at + TimeDelta::seconds(90);
    s.finish_job(1, &end, Some(137)).unwrap();
    let got = s.get_job(1).unwrap().unwrap();
    assert!(!got.running());
    assert_eq!(got.exit_code, Some(137));
    assert!(got.ended_at.unwrap().equal(&end));

    // Running only, then all.
    let running: Vec<i64> = s.list_jobs(false).unwrap().iter().map(|j| j.id).collect();
    assert_eq!(running, vec![2]);
    let all: Vec<i64> = s.list_jobs(true).unwrap().iter().map(|j| j.id).collect();
    assert_eq!(all, vec![1, 2]);

    // Ended with no code known.
    s.finish_job(2, &end, None).unwrap();
    let got = s.get_job(2).unwrap().unwrap();
    assert!(!got.running());
    assert_eq!(got.exit_code, None);
    assert!(s.list_jobs(false).unwrap().is_empty());

    assert_eq!(s.get_job(99).unwrap(), None);
}

/// review-3 1.5: a job found gone has no end time (it isn't known), only
/// when it was found so; a real end its runner records afterwards wins, and
/// one recorded before is never overwritten by a finding.
#[test]
fn job_found_gone_has_no_end_time() {
    let s = setup_test_db();
    let mut j = job("gone.sh");
    s.create_job(&mut j).unwrap();
    let found = j.started_at + TimeDelta::hours(3);
    assert!(s.found_job_gone(j.id, &found, "found gone").unwrap());
    let got = s.get_job(j.id).unwrap().unwrap();
    assert!(!got.running());
    assert_eq!(got.ended_at, None);
    assert!(got.found_gone_at.unwrap().equal(&found));
    assert_eq!((got.exit_code, got.note.as_str()), (None, "found gone"));
    assert!(s.list_jobs(false).unwrap().is_empty());
    // Found again: nothing changes.
    assert!(
        !s.found_job_gone(j.id, &(found + TimeDelta::hours(1)), "again")
            .unwrap()
    );
    assert_eq!(s.get_job(j.id).unwrap().unwrap().note, "found gone");
    // Its runner records the real end after all: that wins.
    let end = j.started_at + TimeDelta::seconds(4);
    let mut ended = got.clone();
    ended.ended_at = Some(end);
    ended.exit_code = Some(143);
    ended.note = "stopped".into();
    s.record_job_end(&ended).unwrap();
    let got = s.get_job(j.id).unwrap().unwrap();
    assert_eq!(
        (got.found_gone_at, got.exit_code, got.note.as_str()),
        (None, Some(143), "stopped")
    );
    assert!(got.ended_at.unwrap().equal(&end));
    assert!(!s.found_job_gone(j.id, &found, "late").unwrap());
    assert_eq!(s.get_job(j.id).unwrap().unwrap().exit_code, Some(143));

    // A job that never ran, as its runner records it.
    let mut n = job("refused.sh");
    s.create_job(&mut n).unwrap();
    n.not_run = true;
    n.note = "not run: --memory-max 1G can't be applied".into();
    n.ran = "/d/jobs/2/script".into();
    s.record_job_end(&n).unwrap();
    let got = s.get_job(n.id).unwrap().unwrap();
    assert!(got.not_run && !got.running() && got.ended_at.is_some());
    assert_eq!(got.exit_code, None);
    // update_job_launch records the copy that runs.
    let mut l = job("launch.sh");
    s.create_job(&mut l).unwrap();
    l.ran = "/d/jobs/3/script".into();
    s.update_job_launch(&l).unwrap();
    assert_eq!(s.get_job(l.id).unwrap().unwrap().ran, "/d/jobs/3/script");
}

/// A jobs table from before ran, found_gone_at and not_run: opening the
/// store adds them, and the old rows read as before (a read-only open of
/// such a store reads them too, with the columns' defaults).
#[test]
fn jobs_table_from_before_the_added_columns() {
    let dir = temp_dir();
    let path = dir.path().join("old-jobs.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE agents (name TEXT PRIMARY KEY, session_name TEXT NOT NULL, backend TEXT NOT NULL, task_file TEXT, log_file TEXT, created_at TEXT NOT NULL);
         CREATE TABLE jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, script TEXT NOT NULL, path TEXT NOT NULL, sha256 TEXT NOT NULL, content TEXT NOT NULL, started_by TEXT NOT NULL, host TEXT NOT NULL, cwd TEXT NOT NULL, args TEXT NOT NULL DEFAULT '[]', unit TEXT NOT NULL DEFAULT '', memory_high TEXT NOT NULL DEFAULT '', memory_max TEXT NOT NULL DEFAULT '', memory_swap_max TEXT NOT NULL DEFAULT '', note TEXT NOT NULL DEFAULT '', started_at TEXT NOT NULL, ended_at TEXT NOT NULL DEFAULT '', exit_code INTEGER, pid INTEGER NOT NULL DEFAULT 0, pid_start TEXT NOT NULL DEFAULT '');
         INSERT INTO jobs (script, path, sha256, content, started_by, host, cwd, started_at, ended_at, exit_code) VALUES ('a.sh', '/w/a.sh', 'ab', 'exit 3', 'lead', 'hub', '/w', '2026-11-01T09:30:15Z', '2026-11-01T09:30:20Z', 3);
         INSERT INTO jobs (script, path, sha256, content, started_by, host, cwd, started_at) VALUES ('b.sh', '/w/b.sh', 'cd', 'sleep 9', 'lead', 'hub', '/w', '2026-11-01T09:31:00Z');",
    )
    .unwrap();
    drop(conn);
    let check = |s: &Store| {
        let all = s.list_jobs(true).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            (all[0].exit_code, all[0].ran.as_str(), all[0].not_run),
            (Some(3), "", false)
        );
        assert!(all[0].ended_at.is_some() && all[0].found_gone_at.is_none());
        let running: Vec<i64> = s.list_jobs(false).unwrap().iter().map(|j| j.id).collect();
        assert_eq!(running, vec![2]);
        assert!(s.get_job(2).unwrap().unwrap().running());
    };
    let ro = open_read_only(&path).unwrap();
    check(&ro);
    drop(ro);
    let s = Store::open(&path).unwrap();
    check(&s);
    assert!(s.found_job_gone(2, &Time::now(), "found gone").unwrap());
    assert!(s.list_jobs(false).unwrap().is_empty());
}

#[test]
fn job_stored_text() {
    let s = setup_test_db();
    let mut j = job("build.sh");
    s.create_job(&mut j).unwrap();
    let conn = rusqlite::Connection::open(&s.path).unwrap();
    let (args, started, ended, code): (String, String, String, Option<i64>) = conn
        .query_row(
            "SELECT args, started_at, ended_at, exit_code FROM jobs WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(args, r#"["a b","\"c\""]"#);
    assert_eq!(started, "2026-11-01T09:30:15.123456789Z");
    assert_eq!(ended, "");
    assert_eq!(code, None);
}

#[test]
fn promoted_scripts() {
    let s = setup_test_db();
    assert_eq!(s.get_script("nightly").unwrap(), None);
    let mut p = PromotedScript {
        name: "nightly".into(),
        sha256: "cd".repeat(32),
        source: "/w/build.sh".into(),
        promoted_by: "user".into(),
        promoted_at: Time::date_utc(2026, 11, 2, 1, 0, 0, 0),
    };
    s.set_script(&p).unwrap();
    assert_eq!(s.get_script("nightly").unwrap(), Some(p.clone()));
    p.sha256 = "ef".repeat(32);
    s.set_script(&p).unwrap();
    s.set_script(&PromotedScript {
        name: "a".into(),
        ..p.clone()
    })
    .unwrap();
    let names: Vec<String> = s
        .list_scripts()
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec!["a", "nightly"]);
    assert_eq!(s.get_script("nightly").unwrap().unwrap().sha256, p.sha256);
}

#[test]
fn host_state_crud() {
    let s = setup_test_db();
    assert_eq!(s.get_host_state("focus").unwrap(), None);
    s.set_host_state("focus", "helper").unwrap();
    assert_eq!(
        s.get_host_state("focus").unwrap().as_deref(),
        Some("helper")
    );
    s.set_host_state("focus", "").unwrap();
    assert_eq!(s.get_host_state("focus").unwrap().as_deref(), Some(""));
    s.set_host_state("other", "x").unwrap();
    s.delete_host_state("focus").unwrap();
    s.delete_host_state("focus").unwrap();
    assert_eq!(s.get_host_state("focus").unwrap(), None);
    assert_eq!(s.get_host_state("other").unwrap().as_deref(), Some("x"));
}

/// A store opened read-only that predates these tables reads as empty, and
/// one that has them reads them.
#[test]
fn read_only_store_with_and_without_the_tables() {
    let dir = temp_dir();
    let path = dir.path().join("old.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE agents (name TEXT PRIMARY KEY, session_name TEXT NOT NULL, backend TEXT NOT NULL, task_file TEXT, log_file TEXT, created_at TEXT NOT NULL);")
        .unwrap();
    drop(conn);
    let ro = open_read_only(&path).unwrap();
    assert!(ro.list_jobs(true).unwrap().is_empty());
    assert_eq!(ro.get_job(1).unwrap(), None);
    assert_eq!(ro.get_host_state("focus").unwrap(), None);
    assert!(ro.list_scripts().unwrap().is_empty());
    drop(ro);

    let s = Store::open(&path).unwrap();
    let mut j = job("x.sh");
    s.create_job(&mut j).unwrap();
    s.set_host_state("focus", "lead").unwrap();
    drop(s);
    let ro = open_read_only(&path).unwrap();
    assert_eq!(ro.list_jobs(false).unwrap(), vec![j]);
    assert_eq!(ro.get_host_state("focus").unwrap().as_deref(), Some("lead"));
}

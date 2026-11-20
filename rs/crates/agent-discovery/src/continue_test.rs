use agent_base::gostd::filepath;
use chrono::{TimeZone, Utc};

use crate::{
    HARNESS_AGY, HARNESS_CLAUDE, HARNESS_KIMI, ParentSession, STATUS_ENDED, Session, r#continue,
};

#[test]
fn test_continue() {
    let tmp = tempfile::Builder::new()
        .tempdir_in("/var/tmp")
        .expect("tempdir in /var/tmp");
    let root = tmp.path().to_str().unwrap();
    let dir = filepath::join(&[root, "shop"]);
    let child = filepath::join(&[&dir, "web"]);
    for d in [
        dir.clone(),
        filepath::join(&[&child, "src"]),
        filepath::join(&[root, "other"]),
    ] {
        std::fs::create_dir_all(&d).unwrap();
    }
    let at = |min: u32| Some(Utc.with_ymd_and_hms(2026, 11, 8, 12, min, 0).unwrap());
    let sess = |id: &str, harness: &str, cwd: &str, min: u32| Session {
        session_id: id.to_string(),
        harness: harness.to_string(),
        path: format!("/t/{id}.jsonl"),
        cwd: cwd.to_string(),
        mtime: at(min),
        status: STATUS_ENDED.to_string(),
        ..Session::default()
    };
    let mut sub = sess("c-sub", HARNESS_CLAUDE, &dir, 50);
    sub.parent_session = Some(ParentSession {
        session_id: "c-new".to_string(),
        tool_id: String::new(),
    });
    let sessions = vec![
        sess("c-old", HARNESS_CLAUDE, &dir, 10),
        sess("c-new", HARNESS_CLAUDE, &dir, 20),
        sub, // newest in dir, but a subagent's
        sess(
            "c-other",
            HARNESS_CLAUDE,
            &filepath::join(&[root, "other"]),
            40,
        ),
        sess("c-child", HARNESS_CLAUDE, &child, 45),
        sess("a-here", HARNESS_AGY, &dir, 5),
        sess("a-child", HARNESS_AGY, &child, 30),
        sess("k-here", HARNESS_KIMI, &dir, 59),
        Session {
            session_id: "c-nocwd".to_string(),
            harness: HARNESS_CLAUDE.to_string(),
            path: "/t/x.jsonl".to_string(),
            mtime: at(55),
            ..Session::default()
        },
    ];

    for (harness, target_dir, want) in [
        (HARNESS_CLAUDE, dir.clone(), "c-new"),
        (HARNESS_CLAUDE, format!("{dir}/"), "c-new"),
        (HARNESS_CLAUDE, child.clone(), "c-child"),
        (HARNESS_CLAUDE, filepath::join(&[root, "none"]), ""),
        (HARNESS_AGY, dir.clone(), "a-here"),
        (HARNESS_AGY, root.to_string(), "a-child"),
        (HARNESS_AGY, filepath::join(&[&child, "src"]), "a-child"),
        (HARNESS_AGY, filepath::join(&[root, "other"]), ""),
    ] {
        let got = r#continue(&sessions, harness, &target_dir);
        let got_id = got.as_ref().map(|s| s.session_id.as_str()).unwrap_or("");
        assert_eq!(got_id, want, "continue({harness}, {target_dir})");
        assert_eq!(got.is_some(), !want.is_empty());
    }

    let link = filepath::join(&[root, "link"]);
    std::os::unix::fs::symlink(&dir, &link).unwrap();
    let got = r#continue(&sessions, HARNESS_CLAUDE, &link).expect("via symlink");
    assert_eq!(got.session_id, "c-new");
}

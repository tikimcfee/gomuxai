use std::path::Path;

use agent_base::config::Config;
use agent_discovery::{Dir, HARNESS_CLAUDE, ORIGIN_DEFAULT};
use agent_store::Store;

use crate::*;

struct Env {
    store: Store,
    dirs: Vec<Dir>,
    projects: String,
    _tmp: tempfile::TempDir,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("gomuxai.db")).unwrap();
    let projects = tmp.path().join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    let projects = projects.to_string_lossy().into_owned();
    Env {
        store,
        dirs: vec![Dir {
            harness: HARNESS_CLAUDE.into(),
            path: projects.clone(),
            origin: ORIGIN_DEFAULT.into(),
        }],
        projects,
        _tmp: tmp,
    }
}

/// session writes a Claude session's transcript under projects.
fn session(projects: &str, id: &str) {
    let dir = Path::new(projects).join("-work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"/work\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"
        ),
    )
    .unwrap();
}

fn note(key: &str, task: &str, verdict: &str, source: &str) -> NoteInput {
    let k = Key::parse(key).unwrap();
    NoteInput {
        harness: k.harness,
        model: k.model,
        effort: k.effort,
        task: task.into(),
        verdict: verdict.into(),
        text: "it went".into(),
        source: source.into(),
        by: "lead".into(),
        ..Default::default()
    }
}

fn set(store: &Store, key: &str, tasks: &str, speed: &str, cost: &str) {
    set_entry(
        store,
        &Key::parse(key).unwrap(),
        &EntryChanges {
            tasks: Some(tasks.into()),
            speed: Some(speed.into()),
            cost: Some(cost.into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap();
}

#[test]
fn keys() {
    for (s, want) in [
        ("claude/claude-opus-5-5", ("claude", "claude-opus-5-5", "")),
        (
            "claude/claude-haiku-5-5@low",
            ("claude", "claude-haiku-5-5", "low"),
        ),
        ("agy", ("agy", "", "")),
        ("agy/", ("agy", "", "")),
        ("claude@max", ("claude", "", "max")),
        (
            "vertex/claude-opus@2025",
            ("vertex", "claude-opus@2025", ""),
        ),
    ] {
        let k = Key::parse(s).unwrap();
        assert_eq!(
            (k.harness.as_str(), k.model.as_str(), k.effort.as_str()),
            want,
            "{s}"
        );
    }
    assert_eq!(
        Key::parse("claude/m@low").unwrap().to_string(),
        "claude/m@low"
    );
    assert_eq!(Key::parse("agy").unwrap().to_string(), "agy/");
    for bad in ["", "/m", "a b/m"] {
        let e = Key::parse(bad).unwrap_err().to_string();
        assert!(e.contains("HARNESS/MODEL[@EFFORT]"), "{bad:?}: {e}");
    }
}

#[test]
fn task_kinds_are_checked() {
    assert_eq!(parse_tasks("edit, test,edit").unwrap(), ["edit", "test"]);
    let e = parse_tasks("edit,refactor").unwrap_err().to_string();
    assert!(e.contains("\"refactor\" isn't a task kind"), "{e}");
    assert!(e.contains(&TASK_KINDS.join(", ")), "{e}");
}

#[test]
fn set_enable_remove() {
    let v = env();
    let k = Key::parse("claude/claude-haiku-5-5").unwrap();
    let (e, created) = set_entry(
        &v.store,
        &k,
        &EntryChanges {
            good_at: Some("small precise edits".into()),
            tasks: Some("edit,test".into()),
            ..Default::default()
        },
        "user",
    )
    .unwrap();
    assert!(created && e.enabled);
    // A second set changes only what it is given.
    let (e, created) = set_entry(
        &v.store,
        &k,
        &EntryChanges {
            speed: Some("fast".into()),
            ..Default::default()
        },
        "lead",
    )
    .unwrap();
    assert!(!created);
    assert_eq!(
        (
            e.good_at.as_str(),
            e.tasks.len(),
            e.speed.as_str(),
            e.set_by.as_str()
        ),
        ("small precise edits", 2, "fast", "lead")
    );
    let err = set_entry(
        &v.store,
        &k,
        &EntryChanges {
            speed: Some("quick".into()),
            ..Default::default()
        },
        "lead",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("use fast, medium, slow"), "{err}");

    assert!(!set_enabled(&v.store, &k, false, "user").unwrap().enabled);
    let other = Key::parse("agy/x").unwrap();
    let err = set_enabled(&v.store, &other, true, "user")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no roster entry agy/x") && err.contains("agent roster set agy/x"),
        "{err}"
    );
    remove_entry(&v.store, &k).unwrap();
    assert!(remove_entry(&v.store, &k).is_err());
}

#[test]
fn notes_cite_a_session_on_this_host() {
    let v = env();
    session(&v.projects, "3f3a9c00-1111");
    session(&v.projects, "3f3a9c00-2222");
    session(&v.projects, "77aa0000-3333");

    // A unique prefix resolves to the whole id.
    let n = add_note_in(
        &v.store,
        &v.dirs,
        note("claude/haiku@low", "edit", "good", "claude:77aa"),
    )
    .unwrap();
    assert_eq!(n.source, "claude:77aa0000-3333");
    assert_eq!((n.id, n.effort.as_str(), n.by.as_str()), (1, "low", "lead"));

    for (source, want) in [
        ("claude:3f3a9c", "is the start of 2 claude sessions"),
        (
            "claude:deadbeef",
            "agent sessions --harness claude lists them",
        ),
        ("agy:77aa", "no agy sessions at all"),
        ("77aa", "isn't HARNESS:ID"),
        ("", "cites no session"),
    ] {
        let e = add_note_in(
            &v.store,
            &v.dirs,
            note("claude/haiku", "edit", "good", source),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains(want), "{source:?}: {e}");
    }
    for (n, want) in [
        (
            note("claude/haiku", "refactor", "good", "claude:77aa"),
            "isn't a task kind",
        ),
        (
            note("claude/haiku", "edit", "great", "claude:77aa"),
            "isn't a verdict",
        ),
        (
            NoteInput {
                text: " ".into(),
                ..note("claude/haiku", "edit", "good", "claude:77aa")
            },
            "says nothing",
        ),
        (
            NoteInput {
                supersedes: 9,
                ..note("claude/haiku", "edit", "good", "claude:77aa")
            },
            "no note 9",
        ),
        (
            NoteInput {
                supersedes: 1,
                ..note("claude/haiku", "edit", "good", "claude:77aa")
            },
            "is on claude/haiku@low, not claude/haiku",
        ),
    ] {
        let e = add_note_in(&v.store, &v.dirs, n).unwrap_err().to_string();
        assert!(e.contains(want), "{e}");
    }
    assert_eq!(v.store.list_roster_notes().unwrap().len(), 1);

    // add_note finds the sessions where the config says.
    let cfg = Config {
        claude_projects_dir: Some(v.projects.clone()),
        agy_brain_dir: Some(String::new()),
        kimi_sessions_dir: Some(String::new()),
        ..Default::default()
    };
    let n = add_note(
        &v.store,
        &cfg,
        NoteInput {
            supersedes: 1,
            ..note("claude/haiku@low", "edit", "bad", "claude:3f3a9c00-2222")
        },
    )
    .unwrap();
    assert_eq!((n.id, n.supersedes), (2, 1));
}

#[test]
fn view_counts_and_availability() {
    let v = env();
    for id in ["s1", "s2", "s3"] {
        session(&v.projects, id);
    }
    set(&v.store, "claude/haiku", "edit,test", "fast", "low");
    set(&v.store, "agy/gem", "edit", "medium", "medium");
    set(&v.store, "claude/opus", "", "slow", "high");
    set_enabled(&v.store, &Key::parse("claude/opus").unwrap(), false, "user").unwrap();
    let add = |key: &str, task: &str, verdict: &str, s: &str| {
        add_note_in(&v.store, &v.dirs, note(key, task, verdict, s)).unwrap()
    };
    add("claude/haiku", "edit", "good", "claude:s1");
    add("claude/haiku", "edit", "good", "claude:s1");
    let old = add("claude/haiku", "edit", "bad", "claude:s2");
    add_note_in(
        &v.store,
        &v.dirs,
        NoteInput {
            supersedes: old.id,
            ..note("claude/haiku", "edit", "good", "claude:s3")
        },
    )
    .unwrap();
    add("claude/haiku", "review", "bad", "claude:s2");
    add("kimi/k2", "chat", "mixed", "claude:s1");

    let only_claude = |cmd: &str| cmd == "claude";
    let h = host_view_with(&v.store, "hub", &only_claude).unwrap();
    assert_eq!(h.host, "hub");
    let keys: Vec<(&str, bool, &str)> = h
        .entries
        .iter()
        .map(|e| (e.key.as_str(), e.available, e.unavailable.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            ("agy/gem", false, "agy not on PATH"),
            ("claude/haiku", true, ""),
            ("claude/opus", false, "disabled"),
        ]
    );
    let haiku = &h.entries[1];
    // The superseded bad note doesn't count; two notes from s1 are one session.
    assert_eq!(
        haiku.notes.good,
        Tally {
            notes: 3,
            sessions: 2
        }
    );
    assert_eq!(
        haiku.notes.bad,
        Tally {
            notes: 1,
            sessions: 1
        }
    );
    assert_eq!(haiku.notes.sessions, 3);
    assert_eq!(haiku.notes.describe(), "3 good, 1 bad from 3 sessions");
    assert_eq!(h.entries[0].notes.describe(), "no notes");
    assert_eq!(haiku.by_task["edit"].good.sessions, 2);
    assert_eq!(haiku.by_task["edit"].bad.notes, 0);
    assert_eq!(h.unlisted["kimi/k2"].mixed.notes, 1);

    // It serializes flat, as the bus will carry it.
    let j = serde_json::to_value(&h).unwrap();
    assert_eq!(j["entries"][1]["harness"], "claude");
    assert_eq!(j["entries"][1]["key"], "claude/haiku");
    let back: HostRoster = serde_json::from_value(j).unwrap();
    assert_eq!(back, h);

    let s = show_with(
        &v.store,
        "hub",
        &Key::parse("claude/haiku").unwrap(),
        &only_claude,
    )
    .unwrap();
    assert_eq!(s.notes.len(), 5);
    assert_eq!(s.notes[2].superseded_by, s.notes[3].note.id);
    let s = show_with(
        &v.store,
        "hub",
        &Key::parse("kimi/k2").unwrap(),
        &only_claude,
    )
    .unwrap();
    assert!(s.entry.is_none() && s.notes.len() == 1);
    let e = show_with(&v.store, "hub", &Key::parse("x/y").unwrap(), &only_claude)
        .unwrap_err()
        .to_string();
    assert!(e.contains("no roster entry or note for x/y"), "{e}");
}

#[test]
fn pick_ranks() {
    let v = env();
    for id in ["s1", "s2", "s3"] {
        session(&v.projects, id);
    }
    set(&v.store, "claude/haiku", "edit,test", "fast", "low");
    set(
        &v.store,
        "claude/sonnet",
        "edit,feature",
        "medium",
        "medium",
    );
    set(
        &v.store,
        "claude/opus",
        "feature,long-horizon",
        "slow",
        "high",
    );
    set(&v.store, "agy/gem", "edit", "fast", "low");
    let all = |_: &str| true;
    let local = host_view_with(&v.store, "here", &all).unwrap();

    let p = |task: &str, prefer: &str, hosts: &[HostRoster]| {
        pick(
            hosts,
            "here",
            &PickOptions {
                task: task.into(),
                prefer: prefer.into(),
            },
        )
        .unwrap()
    };
    let keys = |ps: &[Pick]| ps.iter().map(|p| p.key.clone()).collect::<Vec<_>>();

    // Three suit edit equally; fast breaks the tie between them by speed,
    // then by host and key.
    let ps = p("edit", "fast", std::slice::from_ref(&local));
    assert_eq!(keys(&ps)[..3], ["agy/gem", "claude/haiku", "claude/sonnet"]);
    assert_eq!(ps[0].command, "agent spawn NAME --backend agy --model gem");
    assert!(
        ps[0]
            .why
            .starts_with("tasks include edit; no notes on edit; fast, low cost"),
        "{}",
        ps[0].why
    );
    let ps = p("edit", "strong", std::slice::from_ref(&local));
    assert_eq!(ps[0].key, "claude/sonnet");

    // Notes move entries: two good sessions on edit count as much as
    // suiting it, and win the tie on evidence; a bad one sinks agy.
    for s in ["claude:s1", "claude:s2"] {
        add_note_in(&v.store, &v.dirs, note("claude/opus", "edit", "good", s)).unwrap();
    }
    add_note_in(
        &v.store,
        &v.dirs,
        note("agy/gem", "edit", "bad", "claude:s3"),
    )
    .unwrap();
    let local = host_view_with(&v.store, "here", &all).unwrap();
    let ps = p("edit", "", std::slice::from_ref(&local));
    assert_eq!(
        keys(&ps),
        ["claude/opus", "claude/haiku", "claude/sonnet", "agy/gem"]
    );
    assert_eq!(ps[0].score, 2);
    assert!(
        ps[0].why.contains(
            "tasks are feature,long-horizon, not edit; notes on edit: 2 good from 2 sessions"
        ),
        "{}",
        ps[0].why
    );
    assert_eq!(ps[3].score, 1);

    // Another host's entries spawn there; on a tie this host's come first
    // (the hub sorts before "zz" by name, so that isn't why).
    let mut zz = local.clone();
    zz.host = "zz".into();
    let mut hub = local.clone();
    hub.host = "hub".into();
    let ps = pick(
        &[hub, zz],
        "zz",
        &PickOptions {
            task: "long-horizon".into(),
            prefer: String::new(),
        },
    )
    .unwrap();
    assert_eq!(
        ps[0].command,
        "agent spawn NAME --backend claude --model opus"
    );
    assert_eq!(
        ps[1].command,
        "agent spawn NAME@hub --backend claude --model opus"
    );

    let bad = |task: &str, prefer: &str, hosts: &[HostRoster]| {
        pick(
            hosts,
            "here",
            &PickOptions {
                task: task.into(),
                prefer: prefer.into(),
            },
        )
        .unwrap_err()
        .to_string()
    };
    assert!(bad("refactor", "", &[]).contains("isn't a task kind"));
    assert!(bad("edit", "cheapest", &[]).contains("isn't a preference"));
    assert!(bad("edit", "", &[]).contains("has no entries"));
    let none = host_view_with(&v.store, "here", &|_: &str| false).unwrap();
    assert!(bad("edit", "", &[none]).contains("no roster entry is available"));
}

#[test]
fn saved_picks() {
    let v = env();
    set(&v.store, "claude/haiku", "edit", "fast", "low");
    set(&v.store, "claude/sonnet", "edit", "medium", "medium");
    let all = |_: &str| true;
    let view = host_view_with(&v.store, "here", &all).unwrap();
    let opts = |prefer: &str| PickOptions {
        task: "edit".into(),
        prefer: prefer.into(),
    };
    let fast = pick(std::slice::from_ref(&view), "here", &opts("fast")).unwrap();
    assert_eq!(
        fast[0].argv,
        [
            "agent",
            "spawn",
            "NAME",
            "--backend",
            "claude",
            "--model",
            "haiku"
        ]
    );

    let r = save_pick(&v.store, "quick-edit", &fast[0], "edit", "lead").unwrap();
    assert_eq!((r.outcome.clone(), r.pick.version), (SaveOutcome::New, 1));
    let r = save_pick(&v.store, "quick-edit", &fast[0], "edit", "lead").unwrap();
    assert_eq!((r.outcome, r.pick.version), (SaveOutcome::Unchanged, 1));
    let strong = pick(std::slice::from_ref(&view), "here", &opts("strong")).unwrap();
    let r = save_pick(&v.store, "quick-edit", &strong[0], "edit", "lead").unwrap();
    assert_eq!((r.outcome, r.pick.version), (SaveOutcome::NewVersion, 2));
    assert_eq!(r.pick.model, "sonnet");

    assert_eq!(saved(&v.store, "quick-edit", 1).unwrap().model, "haiku");
    assert_eq!(saved(&v.store, "quick-edit", 0).unwrap().version, 2);
    let e = saved(&v.store, "quick-edit", 5).unwrap_err().to_string();
    assert!(e.contains("has no version 5: it has 1 to 2"), "{e}");
    let e = saved(&v.store, "nope", 0).unwrap_err().to_string();
    assert!(
        e.contains("no saved pick nope") && e.contains("agent roster picks"),
        "{e}"
    );

    assert_eq!(
        parse_saved_ref("quick-edit@2").unwrap(),
        ("quick-edit".into(), 2)
    );
    assert_eq!(
        parse_saved_ref("quick-edit").unwrap(),
        ("quick-edit".into(), 0)
    );
    assert!(parse_saved_ref("q@x").is_err());
    assert!(check_pick_name("a b").is_err());
    assert!(check_pick_name("-a").is_err());

    // Whether its entry is still there and on.
    let p1 = saved(&v.store, "quick-edit", 1).unwrap();
    assert_eq!(
        check_saved(p1.clone(), std::slice::from_ref(&view)).status,
        "available"
    );
    set_enabled(
        &v.store,
        &Key::parse("claude/haiku").unwrap(),
        false,
        "user",
    )
    .unwrap();
    let view = host_view_with(&v.store, "here", &all).unwrap();
    let c = check_saved(p1.clone(), std::slice::from_ref(&view));
    assert_eq!(
        (c.available, c.status.as_str()),
        (Some(false), "off: disabled")
    );
    remove_entry(&v.store, &Key::parse("claude/haiku").unwrap()).unwrap();
    let view = host_view_with(&v.store, "here", &all).unwrap();
    let c = check_saved(p1.clone(), std::slice::from_ref(&view));
    assert_eq!(c.status, "gone: here has no entry claude/haiku now");
    let c = check_saved(p1, &[]);
    assert_eq!(c.available, None);
}

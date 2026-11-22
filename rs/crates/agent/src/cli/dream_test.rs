//! Tests of `agent dream` (docs/DREAMING.md; no Go file).
use std::time::Duration;

use agent_store::{DREAM_ABANDONED, DREAM_COMMITTED, DreamItem, FRICTION_OPEN, Friction, StateEntry, Time};

use crate::cli::logs::logs_test::write_test_file;
use crate::cli::sessions::sessions_test::{claude_transcript, setup_sessions};
use crate::cli::testenv::TestEnv;

fn run(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    res.unwrap_or_else(|e| panic!("{words:?}: {e:#}"));
    out
}

fn run_err(env: &mut TestEnv, words: &[&str]) -> String {
    let (_, res) = env.run(words);
    format!("{:#}", res.expect_err(&format!("{words:?} succeeded")))
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

#[test]
fn dream_gather_run_list_and_show() {
    crate::require_termd!();
    let (mut env, home) = setup_sessions();
    // A finished session with a memory directory beside it, in a project
    // directory; another out of scope; another from before the floor.
    let shop = env.dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let shop = shop.to_string_lossy().into_owned();
    let old = claude_transcript(&home, "old", &shop, "Old work", Duration::from_secs(2 * 86400));
    claude_transcript(&home, "elsewhere", "/srv/personal", "Mine", Duration::from_secs(86400));
    claude_transcript(&home, "ancient", &format!("{shop}/sub"), "Long ago", Duration::from_secs(30 * 86400));
    let memory = format!("{}/memory", std::path::Path::new(&old).parent().unwrap().display());
    std::fs::create_dir_all(&memory).unwrap();
    write_test_file(&format!("{memory}/MEMORY.md"), "- [a](a.md) — a\n");
    file_friction(&env, "local-1", "wait misses a turn");

    // Nothing is in scope yet: gather finds nothing, run starts nothing.
    let none = "Nothing is in scope for dreaming. agent dream scope add <dir> adds a project directory: sessions run in it or below it are reviewed, with their memory and commits.\n";
    assert_eq!(run(&mut env, &["dream", "gather"]), none);
    assert_eq!(run(&mut env, &["dream", "run"]), format!("{none}No dream started.\n"));
    assert_eq!(run(&mut env, &["dream", "scope", "list"]), none);
    assert!(env.store().list_dreams(0).unwrap().is_empty());
    assert!(env.store().get_host_state("dream.floor").unwrap().is_none());

    assert_eq!(
        run(&mut env, &["dream", "scope", "add", &format!("{shop}/")]),
        format!("Added {shop} to the dream scope.\n")
    );
    assert_eq!(
        run(&mut env, &["dream", "scope", "add", &shop]),
        format!("{shop} is already in the dream scope.\n")
    );
    let err = run_err(&mut env, &["dream", "scope", "add", "/no/such/dir"]);
    assert!(err.contains("/no/such/dir isn't a directory"), "{err}");
    assert_eq!(run(&mut env, &["dream", "scope", "list"]), format!("{shop}\n"));
    assert_eq!(run(&mut env, &["dream", "scope", "list", "--json"]), format!("[\"{shop}\"]\n"));

    let out = run(&mut env, &["dream", "gather"]);
    assert!(!out.contains("elsewhere") && !out.contains("ancient"), "out of scope, before the floor: {out}");
    assert!(
        out.starts_with("The next dream would review 1 session, 1 friction entry, 1 memory directory (3 new).\n"),
        "{out}"
    );
    assert!(out.contains("  claude:old  claude session in shop: Old work\n"), "{out}");
    assert!(!out.contains("sess-l") && !out.contains("sess-w"), "running sessions: {out}");
    assert!(out.contains(&format!("  {memory}  memory of shop (1 file)\n")), "{out}");
    assert!(out.contains("#1 bug +0: wait misses a turn"), "{out}");

    let out = run(&mut env, &["dream", "gather", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["quiet"], false);
    let items: Vec<DreamItem> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| serde_json::from_value(i.clone()).unwrap())
        .collect();
    assert_eq!(items.len(), 3);

    let out = run(&mut env, &["dream", "run", "--dry-run"]);
    assert!(out.starts_with("Would start a dream: The next dream would review"), "{out}");
    assert!(env.store().list_dreams(0).unwrap().is_empty(), "dry run recorded a dream");

    // As if a dream reviewed them and committed.
    let d = env.store().open_dream("local", &items, |id| format!("dream-{id}")).unwrap();
    env.store().commit_dream(d.id).unwrap();
    let out = run(&mut env, &["dream", "gather"]);
    assert_eq!(out, "Nothing for a dream: nothing new since the last committed dream.\n");

    // An open dream whose agent is gone is abandoned; then nothing is new.
    let gone = env.store().open_dream("local", &[], |id| format!("dream-{id}")).unwrap();
    let out = run(&mut env, &["dream", "run", "--dry-run"]);
    assert!(out.starts_with(&format!("Would close dream #{} as abandoned", gone.id)), "{out}");
    let out = run(&mut env, &["dream", "run"]);
    assert_eq!(
        out,
        format!(
            "Closed dream #{} as abandoned: its agent {} is gone.\nDream #3 is quiet: nothing new since the last committed dream. No agent started.\n",
            gone.id, gone.agent
        )
    );
    assert_eq!(env.store().get_dream(gone.id).unwrap().unwrap().status, DREAM_ABANDONED);
    // That first run fixed the floor, 7 days back.
    let floor = env.store().get_host_state("dream.floor").unwrap().expect("the floor");
    let floor = chrono::DateTime::parse_from_rfc3339(&floor).unwrap();
    let back = chrono::Utc::now().signed_duration_since(floor).num_hours();
    assert!((7 * 24 - 1..=7 * 24).contains(&back), "floor {floor}, {back}h back");

    let out = run(&mut env, &["dream", "list"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[1].starts_with("#3    quiet      -"), "{out}");
    assert!(lines[2].starts_with("#2    abandoned  dream-2"), "{out}");
    assert!(lines[3].starts_with("#1    committed  dream-1"), "{out}");
    assert!(lines[3].contains("    3  "), "{out}");

    let out = run(&mut env, &["dream", "show", "1"]);
    assert!(out.starts_with("Dream #1 (committed) on local\nAgent:    dream-1\n"), "{out}");
    assert!(out.contains("  session  claude:old  claude session in shop: Old work\n           read: agent transcript old\n"), "{out}");
    let out = run(&mut env, &["dream", "show"]);
    assert!(out.starts_with("Dream #3 (quiet)"), "the newest: {out}");
    let err = run_err(&mut env, &["dream", "show", "9"]);
    assert!(err.contains("there is no dream #9"), "{err}");

    // A vote brings the entry back.
    let mut f = env.store().get_friction(1).unwrap().unwrap();
    f.updated_at = Time::now();
    env.store().update_friction(&f).unwrap();
    let out = run(&mut env, &["dream", "gather"]);
    assert!(out.starts_with("The next dream would review 1 friction entry (1 new).\n"), "{out}");

    assert_eq!(
        run(&mut env, &["dream", "scope", "remove", &shop]),
        format!("Removed {shop} from the dream scope.\n")
    );
    let err = run_err(&mut env, &["dream", "scope", "remove", &shop]);
    assert!(err.contains("isn't in the dream scope"), "{err}");
    assert_eq!(run(&mut env, &["dream", "gather"]), none, "friction alone starts nothing");
}

#[test]
fn dream_verbs_act_on_the_callers_open_dream() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    env.ctx.set_caller_chain(|| Ok(Vec::new()));

    let err = run_err(&mut env, &["dream", "note", "hello"]);
    assert!(err.contains("agent dream note is for a dream's own agent, on its open dream: you aren't an agent"), "{err}");

    let a = file_friction(&env, "local-1", "wait --next misses a turn that ended before it began");
    let b = file_friction(&env, "local-2", "the next wait misses an early turn");
    let d = env.store().open_dream("local", &[], |id| format!("dream-{id}")).unwrap();
    env.store()
        .create_agent(&agent_store::Agent {
            name: d.agent.clone(),
            session_name: format!("rpc-cli-{}", d.agent),
            backend: "shell".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.store()
        .set_state(&StateEntry {
            agent_name: d.agent.clone(),
            key: "dream".into(),
            value: d.id.to_string(),
            updated_at: Time::now(),
        })
        .unwrap();

    env.set_env("AGENT_NAME", "worker");
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    let err = run_err(&mut env, &["dream", "note", "hello"]);
    assert!(err.contains("worker has no open dream"), "{err}");

    env.set_env("AGENT_NAME", &d.agent);
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    assert_eq!(run(&mut env, &["dream", "note", "read 3 sessions"]), "Noted in dream #1's diary.\n");

    let out = run(&mut env, &["dream", "friction", "accept", &a.id.to_string(), "--note", "real"]);
    assert!(out.starts_with("Accepted friction #1"), "{out}");
    assert_eq!(env.store().get_friction(a.id).unwrap().unwrap().accepted_by, "dream-1");
    let out = run(&mut env, &["dream", "friction", "dup", &b.id.to_string(), &a.id.to_string()]);
    assert!(out.starts_with("Closed friction #2 as a duplicate of #1"), "{out}");

    let out = run(&mut env, &["dream", "limitation", "wait --next misses a turn that ended before it began"]);
    assert!(out.starts_with("Not filed: it looks like #1"), "{out}");
    let out = run(&mut env, &["dream", "limitation", "the digest can't show subagent transcripts"]);
    assert_eq!(out, "Filed friction #3 (other).\n");
    let filed = env.store().get_friction(3).unwrap().unwrap();
    assert_eq!((filed.kind.as_str(), filed.filed_by.as_str()), ("other", "dream-1"));
    assert!(filed.detail.contains("Filed by dream #1 (dream-1)."), "{}", filed.detail);

    // What the dream triaged is recorded as it left it.
    let items = env.store().dream_items(d.id).unwrap();
    let mut keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    keys.sort();
    let mut want = vec!["local-1", "local-2", filed.uid.as_str()];
    want.sort();
    assert_eq!(keys, want);

    assert_eq!(
        run(&mut env, &["dream", "commit"]),
        "Committed dream #1: its 3 items count as reviewed.\n"
    );
    let got = env.store().get_dream(d.id).unwrap().unwrap();
    assert_eq!(got.status, DREAM_COMMITTED);
    assert!(got.diary.ends_with(" read 3 sessions"), "{}", got.diary);
    let a_now = env.store().get_friction(a.id).unwrap().unwrap();
    assert!(
        env.store()
            .is_reviewed("friction", &a_now.uid, &format!("{}+{}", a_now.updated_at.utc().format_rfc3339_nano(), a_now.votes))
            .unwrap(),
        "the accepted entry is reviewed as the dream left it"
    );

    let err = run_err(&mut env, &["dream", "note", "late"]);
    assert!(err.contains("dream #1 is committed"), "{err}");
    let err = run_err(&mut env, &["dream", "abandon"]);
    assert!(err.contains("dream #1 is committed"), "{err}");
}

/// Opens a dream with items as `dream run` would (snapshots included), as
/// agent dream-<id>, and makes the env's caller that agent.
fn open_dream_as_agent(env: &mut TestEnv, items: &[DreamItem]) -> agent_store::Dream {
    let d = env.store().open_dream("local", items, |id| format!("dream-{id}")).unwrap();
    for i in items.iter().filter(|i| i.kind == "memory") {
        let dir = std::path::Path::new(&i.key);
        let snap = crate::cli::dream_memory::snapshot_dir_for(&env.ctx.data_dir(), &d.id.to_string(), dir);
        agent_dream::memory::take_snapshot(dir, &snap).unwrap();
    }
    env.store()
        .create_agent(&agent_store::Agent {
            name: d.agent.clone(),
            session_name: format!("rpc-cli-{}", d.agent),
            backend: "shell".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.set_env("AGENT_NAME", &d.agent);
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    d
}

const FACT: &str = "---\nname: tests\ndescription: run the tests first\nmetadata:\n  type: feedback\n---\nRun the tests before committing.\n**Why:** a broken build stalls every agent.\n[source: CITES; added: 2026-11-20]\n";

#[test]
fn dream_commit_guards_memory_and_its_citations() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    let mem = env.dir.path().join("proj").join("memory");
    std::fs::create_dir_all(&mem).unwrap();
    std::fs::write(mem.join("old.md"), FACT.replace("CITES", "claude:s0")).unwrap();
    let mem_s = mem.to_string_lossy().into_owned();
    let item = |kind: &str, key: &str| DreamItem {
        kind: kind.into(),
        key: key.into(),
        fingerprint: "f".into(),
        ..Default::default()
    };
    let d = open_dream_as_agent(&mut env, &[item("session", "claude:s1"), item("memory", &mem_s)]);

    let draft = env.dir.path().join("draft.md");
    let put = |env: &mut TestEnv, file: &str, cites: &str, expect: &str| {
        std::fs::write(&draft, FACT.replace("CITES", cites)).unwrap();
        let draft = draft.to_string_lossy().into_owned();
        env.run(&["dream", "memory", "put", file, "--dir", &mem_s, "--expect", expect, "--from", &draft])
    };
    let (out, res) = put(&mut env, "new.md", "claude:s1, claude:s9", "new");
    res.unwrap();
    assert!(out.starts_with("Wrote new.md"), "{out}");
    // Its own write is recorded at the fingerprint gather sees.
    let fp = agent_dream::gather::memory_fingerprint(&mem).unwrap().1;
    let items = env.store().dream_items(d.id).unwrap();
    assert!(items.iter().any(|i| i.kind == "memory" && i.key == mem_s && i.fingerprint == fp), "{items:?}");

    // claude:s9 is nothing the store knows.
    let (out, res) = env.run(&["dream", "commit"]);
    assert!(res.is_err());
    assert_eq!(
        out,
        format!(
            "Not committed: dream #1's memory changes break 1 rule:\n  {mem_s}/new.md: cites claude:s9, which is no item of this dream and no reviewed one\n"
        )
    );
    // Losing old.md's citation breaks the guard too.
    std::fs::remove_file(mem.join("old.md")).unwrap();
    let (out, _) = env.run(&["dream", "commit"]);
    assert!(out.contains("source citation claude:s0 (was in old.md) is gone"), "{out}");
    std::fs::write(mem.join("old.md"), FACT.replace("CITES", "claude:s0")).unwrap();

    let v = agent_dream::memory::version(FACT.replace("CITES", "claude:s1, claude:s9").as_bytes());
    let (_, res) = put(&mut env, "new.md", "claude:s1", &v);
    res.unwrap();
    assert_eq!(
        run(&mut env, &["dream", "commit"]),
        "Committed dream #1: its 2 items count as reviewed.\n"
    );
    assert_eq!(env.store().get_dream(d.id).unwrap().unwrap().status, DREAM_COMMITTED);
}

#[test]
fn dream_gather_carries_evidence() {
    crate::require_termd!();
    let (mut env, home) = setup_sessions();
    let shop = env.dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let shop = shop.to_string_lossy().into_owned();
    // A finished session that failed a shell call twice and was corrected.
    let path = claude_transcript(&home, "work", &shop, "Work", Duration::from_secs(3600));
    let ts = (chrono::Utc::now() - chrono::TimeDelta::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut body = std::fs::read_to_string(&path).unwrap();
    for (n, cmd) in ["pkill -f srv", "grep -q x f"].iter().enumerate() {
        body += &format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{ts}\",\"message\":{{\"id\":\"m{n}\",\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"tu{n}\",\"name\":\"Bash\",\"input\":{{\"command\":\"{cmd}\"}}}}]}}}}\n\
             {{\"type\":\"user\",\"timestamp\":\"{ts}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"tu{n}\",\"content\":\"Exit code {}\",\"is_error\":true}}]}}}}\n",
            if n == 0 { 144 } else { 1 }
        );
    }
    body += &format!(
        "{{\"type\":\"user\",\"timestamp\":\"{ts}\",\"cwd\":\"{shop}\",\"message\":{{\"role\":\"user\",\"content\":\"no, don't kill the server\"}}}}\n"
    );
    write_test_file(&path, &body);
    sessions_set_mtime(&path);
    // A failed command in the scope, and one outside it.
    for cwd in [shop.as_str(), "/srv/elsewhere"] {
        let mut rec = agent_store::CommandRecord {
            started_at: Time::now(),
            agent: "helper".into(),
            cwd: cwd.into(),
            args: vec!["history".into(), "--limit".into(), "5".into()],
            exit_code: 1,
            error: "unknown flag: --limit (see agent history --help for its flags)".into(),
            ..Default::default()
        };
        env.store().log_command(&mut rec).unwrap();
    }
    run(&mut env, &["dream", "scope", "add", &shop]);

    let out = run(&mut env, &["dream", "gather"]);
    assert!(
        out.contains("\nEvidence: 1 failed agent command (1 group), 1 failed tool call in 1 session (1 group), 1 correction.\n"),
        "{out}"
    );
    let v: serde_json::Value = serde_json::from_str(&run(&mut env, &["dream", "gather", "--json"])).unwrap();
    let e = &v["evidence"];
    assert_eq!(e["commands"][0]["what"], "agent history --limit");
    assert_eq!(e["commands"][0]["sources"], serde_json::json!(["helper"]));
    assert_eq!(e["tools"][0]["what"], "Bash pkill");
    assert_eq!(e["tools"][0]["error"], "exit 144");
    assert_eq!(e["tools"][0]["sources"], serde_json::json!(["claude:work"]));
    assert_eq!(e["corrections"][0]["excerpt"], "no, don't kill the server");
}

/// Sets a transcript's mtime an hour back, so it reads as finished.
fn sessions_set_mtime(path: &str) {
    let t = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

#[test]
fn digest_lists_the_scoped_projects() {
    assert_eq!(super::format_scope(&[]), "");
    let s = super::format_scope(&["/srv/app".to_string()]);
    assert!(s.contains("## projects"), "{s}");
    assert!(s.contains("git -C /srv/app log"), "{s}");
}

#[test]
fn dream_abandon_puts_memory_back() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    let mem = env.dir.path().join("proj").join("memory");
    std::fs::create_dir_all(&mem).unwrap();
    let old = FACT.replace("CITES", "claude:s0");
    std::fs::write(mem.join("old.md"), &old).unwrap();
    let mem_s = mem.to_string_lossy().into_owned();
    let item = |kind: &str, key: &str| DreamItem {
        kind: kind.into(),
        key: key.into(),
        fingerprint: "f".into(),
        ..Default::default()
    };
    open_dream_as_agent(&mut env, &[item("session", "claude:s1"), item("memory", &mem_s)]);
    let draft = env.dir.path().join("draft.md");
    std::fs::write(&draft, FACT.replace("CITES", "claude:s0, claude:s1")).unwrap();
    let v = agent_dream::memory::version(old.as_bytes());
    let draft_s = draft.to_string_lossy().into_owned();
    let (_, res) = env.run(&["dream", "memory", "put", "old.md", "--dir", &mem_s, "--expect", &v, "--from", &draft_s]);
    res.unwrap();
    assert_ne!(std::fs::read_to_string(mem.join("old.md")).unwrap(), old);

    let out = run(&mut env, &["dream", "abandon", "testing"]);
    assert!(out.contains("Put back"), "{out}");
    assert_eq!(std::fs::read_to_string(mem.join("old.md")).unwrap(), old);
}

#[test]
fn a_commit_in_a_scoped_repo_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().to_string_lossy().into_owned();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(["-C", &repo, "-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(ok.status.success(), "{ok:?}");
        String::from_utf8(ok.stdout).unwrap()
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "x"]);
    let hash = git(&["rev-parse", "HEAD"]).trim().to_string();
    assert!(super::commit_in_repo(&repo, &hash[..8]));
    assert!(!super::commit_in_repo(&repo, "deadbee1"));
    assert!(!super::commit_in_repo(&repo, &hash[..5]), "too short");
    assert!(!super::commit_in_repo(&repo, "HEAD"), "not a hash");
}

#[test]
fn dream_roster_note_cites_only_the_dreams_sessions() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    let item = |kind: &str, key: &str| DreamItem {
        kind: kind.into(),
        key: key.into(),
        fingerprint: "f".into(),
        ..Default::default()
    };
    open_dream_as_agent(&mut env, &[item("session", "claude:s1")]);
    let note = |env: &mut TestEnv, extra: &[&str]| {
        let mut words = vec!["dream", "roster", "note", "claude/m", "--task", "edit", "--verdict", "good"];
        words.extend_from_slice(extra);
        words.push("did it");
        run_err(env, &words)
    };
    let err = note(&mut env, &["--session", "claude:other"]);
    assert!(err.contains("is no session dream #1 was given"), "{err}");
    let err = note(&mut env, &["--session", "claude:s1", "--by", "someone"]);
    assert!(err.contains("--by is the dream's own agent"), "{err}");
}

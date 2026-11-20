use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent_base::gostd::filepath;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::{Agy, Claude, Kimi, Reader, detect, file_options};

fn corpus_dir() -> String {
    filepath::join(&[env!("CARGO_MANIFEST_DIR"), "testdata", "corpus"])
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            walk_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[test]
fn test_corpus_claude() {
    run_corpus(&Claude);
}

#[test]
fn test_corpus_agy() {
    run_corpus(&Agy);
}

#[test]
fn test_corpus_kimi() {
    run_corpus(&Kimi);
}

fn run_corpus(r: &dyn Reader) {
    let root = corpus_dir();
    let harness_dir = Path::new(&root).join(r.harness());
    let mut all = Vec::new();
    walk_files(&harness_dir, &mut all);
    let cases: Vec<PathBuf> = all
        .into_iter()
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".events.jsonl")))
        .collect();
    assert!(!cases.is_empty(), "no {} cases", r.harness());

    for want_path in cases {
        let want_str = want_path.to_str().unwrap();
        let case_name = want_str
            .strip_prefix(&format!("{root}/"))
            .unwrap_or(want_str);
        let expected = read_json_lines(want_str);
        #[derive(Deserialize)]
        struct SessionPath {
            path: String,
        }
        let session_line: SessionPath =
            serde_json::from_slice(&expected[0]).expect("parse session line");
        let transcript_path = filepath::join(&[&root, &session_line.path]);
        let data = std::fs::read(&transcript_path).expect("read corpus transcript");

        let mut opts = file_options(r.harness(), &transcript_path);
        opts.requests = want_str.ends_with(".requests.events.jsonl");
        let mut p = r.new_parser(opts);
        let events = p.feed(&data);
        let mut s = p.session();
        s.file(&transcript_path, data.len() as i64);
        s.path = session_line.path;

        let mut got = vec![marshal_line("session", &s)];
        for e in &events {
            got.push(marshal_line("event", e));
        }

        let max_len = got.len().max(expected.len());
        for i in 0..max_len {
            if i >= got.len() {
                panic!(
                    "{case_name} line {i}: missing; want\n{}",
                    String::from_utf8_lossy(&expected[i])
                );
            } else if i >= expected.len() {
                panic!(
                    "{case_name} line {i}: unexpected\n{}",
                    String::from_utf8_lossy(&got[i])
                );
            } else {
                let g = comparable(&got[i]);
                let w = comparable(&expected[i]);
                assert_eq!(
                    g,
                    w,
                    "{case_name} line {i}:\n got {}\nwant {}",
                    String::from_utf8_lossy(&got[i]),
                    String::from_utf8_lossy(&expected[i])
                );
            }
        }
    }
}

fn read_json_lines(path: &str) -> Vec<Vec<u8>> {
    let data = std::fs::read_to_string(path).expect("read jsonl");
    data.trim()
        .split('\n')
        .map(|l| l.as_bytes().to_vec())
        .collect()
}

pub(crate) fn marshal_line<T: serde::Serialize>(typ: &str, v: &T) -> Vec<u8> {
    let body = serde_json::to_string(v).expect("marshal line");
    let mut out = format!("{{\"type\":\"{typ}\",");
    out.push_str(&body[1..]);
    out.into_bytes()
}

/// Decodes a stream line for comparison: fields with zero values dropped, and on
/// a session line, the fields the corpus doesn't compare. A request's payload
/// is compared as it is.
fn comparable(line: &[u8]) -> Value {
    let mut v: Value = serde_json::from_slice(line).expect("valid json line");
    let map = v.as_object_mut().expect("object line");
    if map.get("type").and_then(Value::as_str) == Some("session") {
        map.remove("mtime");
        map.remove("status");
        map.remove("status_at");
    }
    let request = map.remove("request");
    let mut cleaned = drop_zero(v);
    if let Some(req) = request {
        cleaned
            .as_object_mut()
            .unwrap()
            .insert("request".to_string(), sort_keys(req));
    }
    sort_keys(cleaned)
}

/// Normalizes object key order recursively so value equality ignores key order
/// (remember `preserve_order` is enabled on `serde_json::Map`).
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
            for (k, val) in map {
                sorted.insert(k, sort_keys(val));
            }
            let mut out = serde_json::Map::new();
            for (k, val) in sorted {
                out.insert(k, val);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

fn drop_zero(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, x) in map {
                let x = drop_zero(x);
                if !is_zero(&x) {
                    out.insert(k, x);
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(drop_zero).collect()),
        other => other,
    }
}

fn is_zero(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
    }
}

#[test]
fn test_corpus_well_formed() {
    let root = corpus_dir();
    let mut all = Vec::new();
    walk_files(Path::new(&root), &mut all);
    let files: Vec<PathBuf> = all
        .into_iter()
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".events.jsonl")))
        .collect();
    assert!(!files.is_empty(), "no .events.jsonl files under {root}");
    for f in files {
        check_events_file(f.to_str().unwrap(), &root);
    }
}

fn check_events_file(path: &str, root: &str) {
    let data = std::fs::read_to_string(path).expect("read events file");
    let mut transcript: Option<Vec<u8>> = None;
    let mut line_starts: Vec<i64> = Vec::new();
    let mut seq_conv = 0usize;
    let mut seq_req = 0usize;

    for (n, raw_line) in data.lines().enumerate() {
        let rec: serde_json::Map<String, Value> =
            serde_json::from_str(raw_line).unwrap_or_else(|e| panic!("{path} line {n}: {e}"));
        let typ = rec.get("type").and_then(Value::as_str).unwrap_or("");
        if n == 0 {
            assert_eq!(typ, "session", "{path} line 0: type {typ:?}, want session");
            #[derive(Deserialize)]
            struct Sess {
                session_id: String,
                harness: String,
                path: String,
                size: usize,
            }
            let s: Sess = serde_json::from_str(raw_line).expect("session line");
            assert!(
                !s.session_id.is_empty() && ["claude", "agy", "kimi"].contains(&s.harness.as_str()),
                "{path}: session_id {:?}, harness {:?}",
                s.session_id,
                s.harness
            );
            let t_bytes = std::fs::read(filepath::join(&[root, &s.path])).expect("session path");
            assert!(!t_bytes.is_empty(), "transcript {} is empty", s.path);
            assert_eq!(
                t_bytes.len(),
                s.size,
                "session size {}, transcript {} has {} bytes",
                s.size,
                s.path,
                t_bytes.len()
            );
            line_starts.push(0);
            for (i, &b) in t_bytes[..t_bytes.len() - 1].iter().enumerate() {
                if b == b'\n' {
                    line_starts.push((i + 1) as i64);
                }
            }
            transcript = Some(t_bytes);
            continue;
        }
        assert_eq!(typ, "event", "{path} line {n}: type {typ:?}, want event");
        for k in ["id", "op", "seq", "at", "line", "ts", "turn", "kind"] {
            assert!(rec.contains_key(k), "{path} line {n}: no {k:?}");
        }
        #[derive(Deserialize)]
        struct TurnId {
            id: String,
        }
        #[derive(Deserialize)]
        struct Ev {
            id: String,
            op: String,
            ts: String,
            kind: String,
            seq: usize,
            at: i64,
            line: usize,
            turn: TurnId,
        }
        let e: Ev = serde_json::from_str(raw_line).expect("event line");
        assert!(
            e.op == "add" || e.op == "update",
            "{path} line {n}: op {:?}",
            e.op
        );
        let want_seq = if e.kind == "request" {
            let s = seq_req;
            seq_req += 1;
            s
        } else {
            let s = seq_conv;
            seq_conv += 1;
            s
        };
        assert_eq!(
            e.seq, want_seq,
            "{path} line {n}: seq {}, want {want_seq}",
            e.seq
        );
        assert!(
            ["prompt", "think", "say", "tool", "system", "request"].contains(&e.kind.as_str()),
            "{path} line {n}: kind {:?}",
            e.kind
        );
        assert!(
            rec.contains_key(&e.kind),
            "{path} line {n}: kind {} but no {:?} object",
            e.kind,
            e.kind
        );
        assert!(
            !e.id.is_empty() && !e.ts.is_empty() && !e.turn.id.is_empty(),
            "{path} line {n}: empty id, ts or turn.id"
        );
        assert!(
            e.line < line_starts.len() && line_starts[e.line] == e.at,
            "{path} line {n}: at {} is not the start of transcript line {}",
            e.at,
            e.line
        );
    }
    assert!(transcript.is_some(), "{path}: empty file");
}

#[test]
fn test_corpus_readme() {
    let root = corpus_dir();
    let readme = std::fs::read_to_string(filepath::join(&[&root, "README.md"])).expect("README.md");
    let readme_case = Regex::new(r"(?m)^\| `([a-z]+/[a-z0-9-]+)` ").unwrap();
    let mut listed = Vec::new();
    for caps in readme_case.captures_iter(&readme) {
        let c = caps[1].to_string();
        let ev_path = filepath::join(&[&root, &format!("{c}.events.jsonl")]);
        assert!(
            std::fs::metadata(&ev_path).is_ok(),
            "README lists {c}: missing {ev_path}"
        );
        listed.push(c);
    }
    assert!(!listed.is_empty(), "README lists no cases");

    let mut top_streams = Vec::new();
    if let Ok(harnesses) = std::fs::read_dir(&root) {
        for h in harnesses.flatten() {
            if !h.path().is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(h.path()) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_file()
                        && let Some(s) = p.to_str()
                        && s.ends_with(".events.jsonl")
                    {
                        top_streams.push(s.to_string());
                    }
                }
            }
        }
    }
    for s in top_streams {
        let rel = s.strip_prefix(&format!("{root}/")).unwrap_or(&s);
        let without_ext = rel.strip_suffix(".events.jsonl").unwrap_or(rel);
        let c = without_ext.strip_suffix(".requests").unwrap_or(without_ext);
        assert!(
            listed.iter().any(|x| x == c),
            "{s} is not a case in README.md"
        );
    }
}

#[test]
fn test_byte_for_byte_with_go_if_present() {
    let Ok(go_dump_path) = std::env::var("PARITY_GO_DUMP") else {
        return;
    };
    if go_dump_path.is_empty() {
        return;
    }
    let want = std::fs::read_to_string(&go_dump_path).expect("read go_dump.txt");
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut rel_paths = vec![
        "testdata/claude.jsonl".to_string(),
        "testdata/agy.jsonl".to_string(),
    ];
    let corpus_root = Path::new(manifest).join("testdata/corpus");
    let mut all = Vec::new();
    walk_files(&corpus_root, &mut all);
    for p in all {
        if let Some(s) = p.to_str()
            && s.ends_with(".jsonl")
            && !s.ends_with(".events.jsonl")
            && let Some(rel) = s.strip_prefix(&format!("{manifest}/"))
        {
            rel_paths.push(rel.to_string());
        }
    }
    rel_paths.sort();

    let mut out = String::new();
    for rel in rel_paths {
        let full = filepath::join(&[manifest, &rel]);
        let data = std::fs::read(&full).unwrap();
        let r = detect(&data);
        for req in [false, true] {
            let mut opts = file_options(r.harness(), &full);
            opts.requests = req;
            let mut parser = r.new_parser(opts);
            let events = parser.feed(&data);
            let mut sess = parser.session();
            sess.file(&full, data.len() as i64);
            sess.path = rel.clone();
            out.push_str(&format!("=== {rel} req={req} ===\n"));
            out.push_str(&serde_json::to_string(&sess).unwrap());
            out.push('\n');
            for e in &events {
                out.push_str(&serde_json::to_string(e).unwrap());
                out.push('\n');
            }
        }
    }
    if out != want {
        for (i, (g, w)) in out.lines().zip(want.lines()).enumerate() {
            assert_eq!(g, w, "byte-for-byte mismatch at line {}", i + 1);
        }
        assert_eq!(
            out.lines().count(),
            want.lines().count(),
            "line count mismatch"
        );
    }
}

//! Tests of the summarizer's pure parts and its client, against a fake
//! OpenAI-compatible endpoint (no model, no GPU).

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::config::Summaries as Cfg;

/// What a fake endpoint answers: (method, path, body) -> (status, body).
pub type Handler = Arc<dyn Fn(&str, &str, &Value) -> (u16, String) + Send + Sync>;

/// A fake HTTP/1.1 server on 127.0.0.1 answering with `h`; its base URL.
pub fn fake_server(h: Handler) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let h = h.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut w = s;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let (method, path) = (
                        parts.next().unwrap_or("").to_string(),
                        parts.next().unwrap_or("").to_string(),
                    );
                    let mut len = 0usize;
                    loop {
                        let mut hd = String::new();
                        if r.read_line(&mut hd).unwrap_or(0) == 0 {
                            return;
                        }
                        if hd.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = hd.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; len];
                    if r.read_exact(&mut body).is_err() {
                        return;
                    }
                    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let (status, out) = h(&method, &path, &v);
                    if status == 0 {
                        // A server that died mid-request: the connection drops.
                        let _ = w.shutdown(std::net::Shutdown::Both);
                        return;
                    }
                    let _ = write!(
                        w,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{out}",
                        out.len()
                    );
                }
            });
        }
    });
    format!("http://{addr}")
}

/// The answers of a well-behaved vLLM: one model, a tokenizer of 4 chars
/// a token, and `chat` for completions.
pub fn vllm(chat: impl Fn(&Value) -> (u16, String) + Send + Sync + 'static) -> Handler {
    Arc::new(move |method, path, v| match (method, path) {
        ("GET", "/v1/models") => (
            200,
            json!({"data": [{"id": "summ", "max_model_len": 10000,
                "root": "/hf/hub/models--Org--Small-4B/snapshots/0123456789abcdef"}]})
            .to_string(),
        ),
        ("GET", "/version") => (200, json!({"version": "0.28.0"}).to_string()),
        ("GET", "/health") => (200, String::new()),
        ("POST", "/tokenize") => {
            let n = v["prompt"].as_str().unwrap_or("").len().div_ceil(4);
            (200, json!({"count": n}).to_string())
        }
        ("POST", "/v1/chat/completions") => chat(v),
        _ => (404, "{}".into()),
    })
}

/// A chat answer whose content is `content`.
pub fn completion(content: &str) -> String {
    json!({"choices": [{"message": {"content": content}, "finish_reason": "stop"}],
           "usage": {"prompt_tokens": 1000, "completion_tokens": 80}})
    .to_string()
}

fn cfg(endpoint: &str) -> Cfg {
    Cfg {
        endpoint: endpoint.into(),
        backoff_ms: 1,
        ..Cfg::default()
    }
}

fn cand(rel: &str, kind: &str, size: u64) -> Candidate {
    Candidate {
        id: format!("p:{rel}"),
        project: "p".into(),
        rel: rel.into(),
        path: format!("/nowhere/{rel}"),
        file_kind: kind.into(),
        size,
        content_key: "k".into(),
        ..Default::default()
    }
}

#[test]
fn name_filters() {
    let c = Cfg::default();
    let skip = |rel: &str, kind: &str, size: u64| skip_by_name(&cand(rel, kind, size), &c);
    assert_eq!(skip("src/main.rs", "source", 900), None);
    assert_eq!(skip("package.json", "build", 900), None);
    assert_eq!(skip("tsconfig.base.json", "config", 900), None);
    assert_eq!(skip(".env.local", "config", 90), Some("sensitive name"));
    assert_eq!(
        skip("certs/server.pem", "config", 90),
        Some("sensitive name")
    );
    assert_eq!(
        skip("exp/outputs/a/0297__fn__in0.json", "config", 900),
        Some("program output dir")
    );
    assert_eq!(
        skip("results/run.md", "doc", 900),
        Some("program output dir")
    );
    assert_eq!(
        skip("shaping/vendor/hb.js", "source", 900),
        Some("vendored")
    );
    assert_eq!(skip("third_party/x.c", "source", 900), Some("vendored"));
    assert_eq!(skip("Cargo.lock", "build", 900), Some("lock or checksum"));
    assert_eq!(skip("go.sum", "build", 900), Some("lock or checksum"));
    assert_eq!(skip("LICENSE", "doc", 9000), Some("boilerplate"));
    assert_eq!(skip("LICENSE-MIT.md", "doc", 9000), Some("boilerplate"));
    assert_eq!(skip(".gitignore", "config", 90), Some("boilerplate"));
    assert_eq!(skip("data/x.csv", "data", 90), Some("kind data"));
    assert_eq!(
        skip("tests/fixtures/a.rs", "fixture", 90),
        Some("kind fixture")
    );
    assert_eq!(
        skip("big.json", "config", 100_000),
        Some("json not a manifest")
    );
    assert_eq!(skip("small.json", "config", 900), None);
    assert_eq!(
        skip("rec.jsonl", "config", 900),
        Some("json not a manifest")
    );
    let mut nc = cand("a.bin", "source", 9);
    nc.no_content = "binary".into();
    assert!(skip_by_name(&nc, &c).is_some());
}

#[test]
fn numbered_siblings_and_content_filters() {
    let mut cs: Vec<Candidate> = (0..60)
        .map(|i| cand(&format!("rec/{i:04}__f__in0.json"), "config", 90))
        .collect();
    cs.push(cand("src/v2.rs", "source", 9));
    let n = numbered_dirs(cs.iter(), 50);
    assert!(n.contains("p:rec"));
    assert!(!n.contains("p:src"));

    let c = Cfg::default();
    let rs = cand("pkg/__init__.py", "source", 0);
    assert_eq!(
        skip_by_content(&rs, "", &c),
        Some("trivial (under the floor)")
    );
    assert_eq!(
        skip_by_content(&rs, "x = 1\n", &c),
        Some("trivial (under the floor)")
    );
    let long = "fn a() {}\n".repeat(30);
    assert_eq!(skip_by_content(&rs, &long, &c), None);
    let generated = format!("// Code generated by protoc. DO NOT EDIT.\n{long}");
    assert_eq!(
        skip_by_content(&rs, &generated, &c),
        Some("generated header")
    );
    // JSON on one long line is a machine's.
    let j = cand("cfg/settings2.json", "config", 9000);
    let one_line = format!("{{\"a\": [{}]}}", "1, ".repeat(1500));
    assert_eq!(
        skip_by_content(&j, &one_line, &c),
        Some("json not a manifest")
    );
    let by_hand = format!("{{\n{}\"z\": 1\n}}\n", "  \"key\": \"value\",\n".repeat(20));
    assert_eq!(skip_by_content(&j, &by_hand, &c), None);
}

#[test]
fn scrub_redacts_keys_urls_and_words() {
    let words = vec!["bighost7".to_string()];
    let t = "key = \"sk-ant-REDACTEDREDACTEDREDACTED00\"\nmail a.b@example.org\n\
             url https://x.io/cb?code=abc123&state=9\nssh bighost7\nplain words stay";
    let (s, n) = scrub(t, &words);
    assert_eq!(n, 4, "{s}");
    for gone in ["sk-ant", "a.b@example.org", "code=abc123", "bighost7"] {
        assert!(!s.contains(gone), "{gone} in {s}");
    }
    assert!(s.contains("plain words stay"));
    let key = "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\nafter";
    let (s, _) = scrub(key, &[]);
    assert_eq!(s, "[redacted]\nafter");
    assert!(!scrub_hit("Parses the token stream into a tree.", &[]));
}

fn facts<'a>(rel: &'a str, text: &'a str, symbols: &'a [String]) -> Facts<'a> {
    Facts {
        rel,
        project: "p",
        text,
        symbols,
        scrub_words: &[],
    }
}

#[test]
fn checks_strip_leads_cap_and_ask_rule() {
    let syms = vec![
        "deliver_held".to_string(),
        "WallLayout".to_string(),
        "render".to_string(),
    ];
    let text = "pub fn deliver_held() {}\npub struct WallLayout;\nfn render() {}\n";
    let f = facts("src/rank_utils.rs", text, &syms);
    let v = json!({
        "summary": "This file is a ranking helper. It calls deliver_held. One. Two. Three.",
        "why": "Keeps ranking in one place.",
        "asks": [
            "where is rank_utils.rs",          // file name
            "rank utils helpers",              // stem with a separator? no: words, kept
            "rank_utils module",               // shaped stem
            "how is WallLayout built",         // symbol
            "the --fast flag",                 // identifier shape
            "how do I render ranked results",  // a plain lowercase symbol word: kept
            "order search results by score",
        ],
    });
    let e = check(&v, &f).unwrap();
    assert!(e.summary.starts_with("A ranking helper."), "{}", e.summary);
    assert_eq!(e.summary.matches(". ").count() + 1, 3, "{}", e.summary);
    assert!(e.trimmed);
    assert_eq!(e.stripped.len(), 4, "{:?}", e.stripped);
    assert_eq!(
        e.asks,
        vec![
            "rank utils helpers",
            "how do I render ranked results",
            "order search results by score"
        ]
    );
    assert_eq!(
        strip_lead("This module defines the CLI."),
        "Defines the CLI."
    );
    assert_eq!(strip_lead("Thisfile stays"), "Thisfile stays");

    // An identifier the file doesn't have is made up.
    let v = json!({"summary": "Calls fetch_remote_cache to warm up.", "why": "w",
                   "asks": ["a b", "c d", "e f"]});
    assert_eq!(check(&v, &f), Err("unknown identifier"));
    let v = json!({"summary": "Uses `NoSuchThing`.", "why": "w", "asks": ["a b", "c d"]});
    assert_eq!(check(&v, &f), Err("unknown identifier"));
    // Under two asks left.
    let v =
        json!({"summary": "Ranks.", "why": "w", "asks": ["rank_utils.rs", "x_y z", "plain one"]});
    assert_eq!(check(&v, &f), Err("asks (under 2 left)"));
    // The output scrub.
    let v = json!({"summary": "Mails a.b@example.org.", "why": "w", "asks": ["a b", "c d"]});
    assert_eq!(check(&v, &f), Err("scrub"));
    // The scrub's placeholder, echoed back.
    let v =
        json!({"summary": "Holds the [redacted] key setup.", "why": "w", "asks": ["a b", "c d"]});
    assert_eq!(check(&v, &f), Err("placeholder echo"));
    let v = json!({"summary": "You are a helpful assistant.", "why": "w", "asks": ["a b", "c d"]});
    assert_eq!(check(&v, &f), Err("instruction echo"));
    // A plain stem is a plain word: kept.
    let f = facts("src/rank.js", "x", &[]);
    assert_eq!(ask_leak("rank search results", &f), None);
    assert_eq!(ask_leak("open rank.js", &f), Some("file name"));
    assert_eq!(ask_norm("Where's  the Word-wall?"), "where s the word wall");
}

#[test]
fn salvage_and_reasoning() {
    let (v, s) =
        salvage(r#"{"summary": "Parses configs.", "why": "One place", "asks": ["a b", "c"#);
    let v = v.unwrap();
    assert!(s);
    assert_eq!(v["summary"], "Parses configs.");
    assert_eq!(v["asks"][0], "a b");
    let (v, s) = salvage("{\"summary\": \"x\", \"why\": \"y\", \"asks\": []}");
    assert!(v.is_some() && !s);
    assert_eq!(
        strip_reasoning("<think>hmm</think>\n```json\n{}\n```"),
        "{}"
    );
    assert_eq!(strip_reasoning("a</think>{\"x\":1}"), "{\"x\":1}");
    assert_eq!(salvage("no json").0, None);
}

#[test]
fn lists_fit_and_outline() {
    let syms: Vec<String> = (0..1000).map(|i| format!("sym{i:04}")).collect();
    let (s, sampled) = symbol_list(&syms, 200, true);
    assert!(sampled && s.len() <= 200);
    // Spread over the file: the late symbols are in the sample.
    assert!(
        s.contains("sym0000") && s.split(", ").any(|x| x >= "sym0900"),
        "{s}"
    );
    let (s, sampled) = symbol_list(&syms[..3], 200, true);
    assert_eq!((s.as_str(), sampled), ("sym0000, sym0001, sym0002", false));
    assert_eq!(fit("a  b\nc", 10), "a b c");
    assert!(fit(&"word ".repeat(100), 50).ends_with('…'));
    assert_eq!(sentences("One. Two. Three. Four.", 3), "One. Two. Three.");
    assert_eq!(outline("# A\ntext\n## B\n#nope\n", 100), "# A\n## B\n");
    assert_eq!(
        hub_snapshot("/x/hub/models--Qwen--Qwen3-4B/snapshots/abc/"),
        ("Qwen/Qwen3-4B".to_string(), "abc".to_string())
    );
    assert_eq!(
        hub_snapshot("Qwen/Qwen3-4B"),
        (String::new(), String::new())
    );
    assert_eq!(
        identifiers("uses a_b, fooBar, FooBar, `x y`, a::b, --flag and run() but not Word"),
        vec!["a_b", "fooBar", "FooBar", "x y", "a::b", "--flag", "run"]
    );
}

#[test]
fn clips_code_head_and_doc_ends() {
    let c = Cfg {
        clip_tokens: 100,
        doc_head_tokens: 40,
        doc_tail_tokens: 20,
        ..Cfg::default()
    };
    let mut count = |s: &str| -> Result<usize, CallError> { Ok(s.len().div_ceil(4)) };
    let small = "fn a() {}\n";
    let k = clip_file(small, "source", &c, &mut count).unwrap();
    assert!(!k.clipped && k.content == small);
    let code: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
    let k = clip_file(&code, "source", &c, &mut count).unwrap();
    assert!(k.clipped && k.content.starts_with("line 000") && k.content.len() <= 400);
    let doc = format!(
        "# Title\n{}## Not found\nthe end conclusion\n",
        "body text\n".repeat(300)
    );
    let k = clip_file(&doc, "doc", &c, &mut count).unwrap();
    assert!(k.clipped);
    assert!(k.content.contains("# Title\n## Not found"), "{}", k.content);
    assert!(k.content.contains("the end conclusion"));
    assert!(k.note.contains("outline"));
}

#[test]
fn client_against_a_fake_server() {
    // Two 503s, then an answer: retried with back-off.
    let fails = Arc::new(AtomicUsize::new(0));
    let f2 = fails.clone();
    let url = fake_server(vllm(move |_| {
        if f2.fetch_add(1, Ordering::SeqCst) < 2 {
            (503, "{\"error\": \"busy\"}".into())
        } else {
            (
                200,
                completion(
                    "{\"summary\": \"S.\", \"why\": \"W.\", \"asks\": [\"a b\", \"c d\", \"e f\"]}",
                ),
            )
        }
    }));
    let c = cfg(&url);
    let cl = Client::new(&c);
    let info = cl.info("").unwrap();
    assert_eq!(info.served, "summ");
    assert_eq!(info.model_id, "Org/Small-4B");
    assert_eq!(info.snapshot, "0123456789abcdef");
    assert_eq!(info.server_version, "0.28.0");
    assert_eq!(cl.tokens("summ", "abcdefgh").unwrap(), 2);
    let mut retried = 0;
    let a = with_retries(
        &c,
        &|| false,
        |_| retried += 1,
        || cl.chat(&body(&c, "summ", "u", 1)),
    )
    .unwrap();
    assert_eq!(retried, 2);
    assert_eq!(a.prompt_tokens, 1000);
    assert!(a.content.contains("\"summary\""));

    // A context-length 400 is told from any other 400.
    let url = fake_server(vllm(|v| {
        if v["messages"][1]["content"].as_str() == Some("long") {
            (
                400,
                "{\"message\": \"This model's maximum context length is 10000 tokens\"}".into(),
            )
        } else {
            (
                400,
                "{\"message\": \"unknown field response_format\"}".into(),
            )
        }
    }));
    let cl = Client::new(&cfg(&url));
    assert!(matches!(
        cl.chat(&body(&c, "summ", "long", 1)),
        Err(CallError::TooLong(_))
    ));
    assert!(matches!(
        cl.chat(&body(&c, "summ", "x", 1)),
        Err(CallError::Bad(_))
    ));

    // Nothing listening: a connection error.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let cl = Client::new(&cfg(&dead));
    assert!(matches!(
        cl.tokens("summ", "x"),
        Err(CallError::Connection(_))
    ));
}

#[test]
fn fingerprint_follows_every_setting() {
    let info = ServerInfo {
        served: "summ".into(),
        root: "/h/models--A--B/snapshots/s1".into(),
        model_id: "A/B".into(),
        snapshot: "s1".into(),
        max_model_len: 10000,
        server_version: "0.28.0".into(),
    };
    let c = Cfg::default();
    let a = Fingerprint::new(&c, &info, "abc");
    assert_eq!(a.key(), Fingerprint::new(&c, &info, "abc").key());
    assert_eq!(a.writer(), "model:A/B@s1");
    let t = Cfg {
        temperature: 0.5,
        ..c.clone()
    };
    assert_ne!(Fingerprint::new(&t, &info, "abc").key(), a.key());
    // The build and operational settings are recorded, not keyed.
    assert_eq!(Fingerprint::new(&c, &info, "abd").key(), a.key());
    let ops = Cfg {
        concurrency: 7,
        pause_file: "/p".into(),
        timeout_ms: 1,
        retries: 9,
        backoff_ms: 9,
        endpoint: "http://elsewhere".into(),
        ..c.clone()
    };
    assert_eq!(Fingerprint::new(&ops, &info, "abc").key(), a.key());
    let other = ServerInfo {
        snapshot: "s2".into(),
        ..info.clone()
    };
    assert_ne!(Fingerprint::new(&c, &other, "abc").key(), a.key());
    // The served alias is not part of it.
    let alias = ServerInfo {
        served: "other".into(),
        ..info
    };
    assert_eq!(Fingerprint::new(&c, &alias, "abc").key(), a.key());
}

#[test]
fn caps_stems_are_words_and_case_is_forgiven() {
    let f = facts("docs/PROTOCOL.md", "the protocol", &[]);
    assert_eq!(ask_leak("terminal protocol for input and output", &f), None);
    let f = facts("src/WallLayout.js", "x", &[]);
    assert_eq!(ask_leak("how the walllayout works", &f), Some("file stem"));
    let text = "import opentelemetry\nfrom opentelemetry import trace\n";
    let f = facts("otel/demo.py", text, &[]);
    let v =
        json!({"summary": "Sends spans with OpenTelemetry.", "why": "w", "asks": ["a b", "c d"]});
    assert!(check(&v, &f).is_ok());
}

#[test]
fn generated_marks_only_on_their_own_comment_line() {
    let c = Cfg::default();
    let k = cand("src/x.rs", "source", 9);
    let body = "fn a() {}\n".repeat(30);
    for head in [
        "// Code generated by protoc. DO NOT EDIT.\n",
        "# This file is automatically @generated by Cargo.\n",
        "# THIS FILE IS AUTOMATICALLY GENERATED BY CARGO\n",
        "// Don't edit by hand; rerun the script.\n",
        "/* auto-generated from schema.json */\n",
    ] {
        assert_eq!(
            skip_by_content(&k, &format!("{head}{body}"), &c),
            Some("generated header"),
            "{head}"
        );
    }
    for head in [
        "lines = [\n    \"// Generated by tools/gen_vocab.py\",\n]\n",
        "//! The code generated here is owned by the caller.\n",
        "# Prebaked asset, generated by `make bake`\n",
    ] {
        assert_eq!(
            skip_by_content(&k, &format!("{head}{body}"), &c),
            None,
            "{head}"
        );
    }
}

#[test]
fn leads_that_say_what_kind_of_file_it_is() {
    for (a, b) in [
        ("A Python script that merges claims.", "Merges claims."),
        ("A test file that verifies colors.", "Verifies colors."),
        (
            "A document that outlines six patterns.",
            "Outlines six patterns.",
        ),
        ("This JavaScript file exports hands.", "Exports hands."),
        ("This file is a parser.", "A parser."),
        (
            "The file system watcher wakes on change.",
            "The file system watcher wakes on change.",
        ),
        (
            "A Makefile that defines targets.",
            "A Makefile that defines targets.",
        ),
        ("Defines the protocol.", "Defines the protocol."),
    ] {
        assert_eq!(strip_lead(a), b, "{a}");
    }
}

#[test]
fn dir_lead_keeps_the_verb() {
    assert_eq!(
        strip_dir_lead("This directory holds the parser."),
        "Holds the parser."
    );
    assert_eq!(
        strip_dir_lead("The project is a tool for maps."),
        "A tool for maps."
    );
    assert_eq!(
        strip_dir_lead("This Rust crate contains the parser."),
        "Contains the parser."
    );
    assert_eq!(strip_dir_lead("Parses the maps."), "Parses the maps.");
}

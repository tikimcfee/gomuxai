//! The repros of the cross-model review (`rs/reports/review-transcript-discovery.md`),
//! each held to what the Go gives for it.
//!
//! `testdata/review/<case>.go.txt` is the Go's own output for `<case>.jsonl`:
//! `ReadFile(path, nil, 100000)`'s counts (or its error), its session, its
//! events and `Latest` of them, each encoded as `agent transcript` encodes a
//! line (a `json.Encoder` with `SetEscapeHTML(false)`). The cases are named
//! for the review's findings (T1 invalid UTF-8 and lone surrogates, T2 U+2028,
//! T3 `toolUseResult` decoded field by field, T4 error texts, T5 mistyped
//! structs, T6 duplicate and differently cased keys, T7 RE2's ASCII classes,
//! T10 Kimi's `"time": null`).

use std::collections::BTreeMap;

use agent_base::gostd::json;
use serde_json::json as value;

use crate::meta::{agy_annotation_title, file_uri_path};
use crate::{
    latest, read_agy_history, read_agy_subagent, read_claude_subagent_meta, read_file, trim_space,
};

/// What the Go's review driver prints for `read <path> 100000`.
fn render(path: &str) -> String {
    let mut out = String::new();
    let mut line = |v: &dyn erased::Encode| {
        out.push_str(&v.encode());
        out.push('\n');
    };
    match read_file(path, None, 100000) {
        Err(e) => line(&value!({"error": format!("{e:#}")})),
        Ok(f) => {
            line(&value!({"lines": f.lines, "partial": f.partial, "skipped": f.skipped}));
            line(&f.session);
            for e in &f.events {
                line(e);
            }
            for e in latest(&f.events) {
                line(&e);
            }
        }
    }
    out
}

mod erased {
    pub(super) trait Encode {
        fn encode(&self) -> String;
    }

    impl<T: serde::Serialize> Encode for T {
        fn encode(&self) -> String {
            super::json::to_string(self, false).expect("encode")
        }
    }
}

#[test]
fn test_review_repros_read_as_the_go_does() {
    let cases = [
        "t1-lone-surrogates",
        "t1-surrogate-in-tool-output",
        "t1-invalid-utf8",
        "t1-agy-surrogates",
        "t2-line-separators",
        "t2-line-separators-tools",
        "t3-agentid-null",
        "t3-agentid-num",
        "t3-agentid-obj",
        "t3-agentid-bool",
        "t3-agentid-arr",
        "t4-missing",
        "t4-unparseable",
        "t5-turnposition-array",
        "t5-queued-prompt-null",
        "t6-duplicate-keys",
        "t6-key-case",
        "t6-agy-duplicate-keys",
        "t7-unicode-spaces",
    ];
    let mut inputs: Vec<(String, String)> = cases
        .iter()
        .map(|c| {
            (
                format!("testdata/review/{c}.jsonl"),
                format!("testdata/review/{c}.go.txt"),
            )
        })
        .collect();
    inputs.push((
        "testdata/review/t10-kimi-time-null/session_f0aa1fb8-41b1-5485-a887-ecd00a0bf053/agents/main/wire.jsonl"
            .to_string(),
        "testdata/review/t10-kimi-time-null.go.txt".to_string(),
    ));
    for (input, golden) in inputs {
        let want = std::fs::read_to_string(&golden).expect("golden");
        let got = render(&input);
        for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
            assert_eq!(g, w, "{input}: line {} differs from the Go's", i + 1);
        }
        assert_eq!(
            got.lines().count(),
            want.lines().count(),
            "{input}: line count"
        );
    }
}

/// The review's T1 repro, spelled out: every string with a lone surrogate
/// escape is read, the escape a U+FFFD.
#[test]
fn test_lone_surrogates_read_as_replacement_characters() {
    let f = read_file("testdata/review/t1-lone-surrogates.jsonl", None, 100).unwrap();
    let r = char::REPLACEMENT_CHARACTER;
    assert_eq!(f.skipped, 0);
    let texts: Vec<String> = f
        .events
        .iter()
        .filter_map(|e| {
            e.prompt
                .as_ref()
                .map(|p| p.text.clone())
                .or_else(|| e.think.as_ref().map(|t| t.text.clone()))
                .or_else(|| e.say.as_ref().map(|s| s.text.clone()))
        })
        .collect();
    assert_eq!(
        texts,
        [
            format!("look at this {r}"),
            format!("hmm {r}"),
            format!("Done {r}")
        ]
    );
    assert_eq!(f.session.title, format!("t {r}"));
    assert!(f.session.title_is_custom());
}

/// The review's T2 repro: U+2028 and U+2029 are escaped in a string and in a
/// raw input alike; `<&>` stays as it is.
#[test]
fn test_line_separators_escaped_as_go() {
    let f = read_file("testdata/review/t2-line-separators.jsonl", None, 100).unwrap();
    let prompt = json::to_string(&f.events[0], false).unwrap();
    assert!(prompt.contains(r#""text":"a\u2028b""#), "{prompt}");
    let tool = json::to_string(&f.events[1], false).unwrap();
    assert!(
        tool.contains(r#""input":{"command":"x\u2028y \u2029 <&>"}"#),
        "{tool}"
    );
    assert!(tool.contains(r#""detail":"x\u2028y \u2029 <&>""#), "{tool}");
}

/// The review's T9 table and more, with what Go's `url.Parse` gives.
#[test]
fn test_file_uri_path_matches_go_net_url() {
    let r = char::REPLACEMENT_CHARACTER;
    for (uri, want) in [
        ("file:relative/x", String::new()),
        ("file://[::1/x", String::new()),
        ("file://ho st/x", String::new()),
        ("file:///a%ff", format!("/a{r}")),
        ("file:///ok", "/ok".into()),
        ("file:/x", "/x".into()),
        ("file://host/x", "/x".into()),
        ("FILE:///X", "/X".into()),
        ("file:///a%zz", String::new()),
        ("file:///a#%zz", String::new()),
        ("file:///a#ok", "/a".into()),
        ("file:///a?q#f", "/a".into()),
        ("file://h%41/x", String::new()),
        ("file://h%c3%a9/x", "/x".into()),
        ("file://u@h/x", "/x".into()),
        ("file://u:p@h:80/x", "/x".into()),
        ("file://h:8x/x", String::new()),
        ("file://[::1]/x", "/x".into()),
        ("file://[::1]:80/x", "/x".into()),
        ("file://[1.2.3.4]/x", String::new()),
        ("file://[fe80::1%25en0]/x", "/x".into()),
        ("file://a[b/x", String::new()),
        ("file://a:b:c/x", String::new()),
        ("file://u%zz@h/x", String::new()),
        ("file://u^@h/x", String::new()),
        ("file://h^/x", String::new()),
        ("file://h\"/x", "/x".into()),
        ("file://h</x", "/x".into()),
        ("file:///a/../b/./c/", "/b/c".into()),
        ("file:///a%2fb", "/a/b".into()),
        ("file://", String::new()),
        ("file:///", "/".into()),
        ("file:", String::new()),
        (":x", String::new()),
        ("file:///\x7f", String::new()),
        ("file:///a b", "/a b".into()),
        ("file://\u{e9}/x", "/x".into()),
        ("file:///a?", "/a".into()),
        ("file:///a?b?", "/a".into()),
        ("file://h/x%", String::new()),
        ("file:x?/y", String::new()),
    ] {
        assert_eq!(file_uri_path(uri), want, "{uri:?}");
    }
}

/// C1: a title's escapes may make bytes that aren't UTF-8; Go keeps them,
/// and writes each as U+FFFD. An unescaped bad byte is U+FFFD already.
#[test]
fn test_agy_annotation_title_bytes_as_go() {
    let r = char::REPLACEMENT_CHARACTER;
    for (data, want) in [
        (&b"title:\"\\xff\\xfe\""[..], format!("{r}{r}")),
        (b"title:\"a\xffb\"", format!("a{r}b")),
        (b"title:\"a\xff\\nb\"", format!("a{r}\nb")),
        (b"title:\" \xe2\x82 \"", format!("{r}{r}")),
        (b"title:\"\xc3\\xa9\"", format!("{r}{r}")),
        (b"title:\"\\303\\251\"", "\u{e9}".to_string()),
        (b"title:\"\\ud800\"", String::new()),
        (b"title:\"\\u00e9\\t\"", "\u{e9}".to_string()),
        (b"x\ntitle:\"\\U0001F600\"\n", "\u{1F600}".to_string()),
    ] {
        assert_eq!(
            agy_annotation_title(data),
            want,
            "{}",
            String::from_utf8_lossy(data)
        );
    }
}

fn temp_file(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> String {
    let path = dir.path().join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, data).unwrap();
    path.to_str().unwrap().to_string()
}

/// T8 and C2: Go reads `null` as a field's zero value, and a struct from an
/// object or `null` only.
#[test]
fn test_descriptors_null_as_go() {
    let dir = tempfile::Builder::new()
        .prefix("review")
        .tempdir_in("/var/tmp")
        .unwrap();
    let subs = "brain/par/.system_generated/subagents";
    for (name, data, want) in [
        ("n1.json", &b"{\"conversationId\":\"c\",\"workspaceUris\":null}"[..], Some(("c", ""))),
        (
            "n2.json",
            b"{\"conversationId\":\"c\",\"state\":null,\"subagentDescriptor\":null}",
            Some(("c", "")),
        ),
        ("n3.json", b"{\"conversationId\":\"c\",\"subagentDescriptor\":[]}", None),
        (
            "n4.json",
            b"{\"ConversationId\":\"c\",\"conversationId\":\"d\",\"Workspaceuris\":[\"file:///w\"]}",
            Some(("d", "/w")),
        ),
    ] {
        let path = temp_file(&dir, &format!("{subs}/{name}"), data);
        let got = read_agy_subagent(&path);
        assert_eq!(
            got.as_ref().map(|s| (s.conversation_id.as_str(), s.workspace.as_str())),
            want,
            "{name}"
        );
        if let Some(s) = got {
            assert_eq!((s.parent.as_str(), s.state.as_str(), s.role.as_str()), ("par", "", ""));
        }
    }

    for (name, data, want) in [
        ("mnull", &b"null"[..], Some(("", None))),
        ("marr", b"[]", None),
        (
            "mdup",
            b"{\"description\":\"a\",\"Description\":\"b\",\"finished\":true,\"finished\":null}",
            Some(("b", None)),
        ),
        ("mbad", b"{\"description\":5}", None),
    ] {
        temp_file(&dir, &format!("{name}.meta.json"), data);
        let path = dir.path().join(format!("{name}.jsonl"));
        let got = read_claude_subagent_meta(path.to_str().unwrap());
        assert_eq!(
            got.as_ref().map(|m| (m.description.as_str(), m.finished)),
            want,
            "{name}"
        );
    }
}

/// T6 and C3: the last of a duplicated key wins, a key's case doesn't matter,
/// and a form feed or vertical tab around a line isn't JSON whitespace.
#[test]
fn test_agy_history_lines_as_go() {
    let dir = tempfile::Builder::new()
        .prefix("review")
        .tempdir_in("/var/tmp")
        .unwrap();
    temp_file(
        &dir,
        "history.jsonl",
        b"{\"conversationId\":\"a\",\"workspace\":\"/a\"}\n\
          {\"conversationId\":\"d\",\"workspace\":\"/d\",\"conversationId\":\"e\"}\n\
          {\"ConversationId\":\"f\",\"Workspace\":\"/f\"}\n\
          {\"conversationId\":\"g\",\"workspace\":\"/g\"}\x0c\n\
          \x0b{\"conversationId\":\"h\",\"workspace\":\"/h\"}\n \
          {\"conversationId\":\"i\",\"workspace\":\"/i\\ud800\"} \n\
          {\"conversationId\":\"j\",\"workspace\":null}\n",
    );
    let got: BTreeMap<String, String> = read_agy_history(dir.path().to_str().unwrap())
        .into_iter()
        .collect();
    let r = char::REPLACEMENT_CHARACTER;
    let want = BTreeMap::from([
        ("a".to_string(), "/a".to_string()),
        ("e".to_string(), "/d".to_string()),
        ("f".to_string(), "/f".to_string()),
        ("i".to_string(), format!("/i{r}")),
    ]);
    assert_eq!(got, want);
}

/// Go's `bytes.TrimSpace`: a vertical tab and Unicode spaces are trimmed, an
/// invalid byte stops the trim.
#[test]
fn test_trim_space_as_go() {
    assert_eq!(trim_space(b"\x0b\x0c x \t\r\n"), b"x");
    assert_eq!(trim_space("\u{a0}\u{2028}x\u{3000}".as_bytes()), b"x");
    assert_eq!(trim_space(b"\xc2\xa0x\xff"), b"x\xff");
    assert_eq!(trim_space(b"\xff \xc2\xa0"), b"\xff");
    assert_eq!(trim_space(b" \xa0x"), b"\xa0x");
    assert_eq!(trim_space(b"   "), b"");
}

/// The performance note: following a long line that arrives in small appends
/// reads the same events as reading it whole (and, scanning each byte once
/// for a newline, takes linear time).
#[test]
fn test_feed_long_line_in_small_appends() {
    let text = "y".repeat(1 << 20);
    let data = format!(
        "{{\"type\":\"user\",\"uuid\":\"u1\",\"promptId\":\"p\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n{{\"type\":\"user\",\"uuid\":\"u2\",\"promptId\":\"q\",\"message\":{{\"role\":\"user\",\"content\":\"next\"}}}}\n"
    );
    let data = data.as_bytes();
    let whole = crate::Claude.new_parser(Default::default()).feed(data);
    let mut p = crate::Claude.new_parser(Default::default());
    let mut pending: Vec<u8> = Vec::new();
    let mut got = Vec::new();
    for chunk in data.chunks(1024) {
        pending.extend_from_slice(chunk);
        let start = p.pos().at;
        got.extend(p.feed(&pending));
        let used = (p.pos().at - start) as usize;
        pending.drain(..used);
    }
    assert_eq!(got, whole);
    assert_eq!(got.len(), 2);
    // A caller that feeds less than it did (it may hold less) loses nothing.
    let mut p = crate::Claude.new_parser(Default::default());
    assert!(p.feed(&data[..100]).is_empty());
    assert!(p.feed(&data[..50]).is_empty());
    assert_eq!(p.feed(data).len(), 2);
}

use crate::Reader as _;

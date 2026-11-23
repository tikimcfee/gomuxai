use super::*;

fn parse(json: &str) -> Result<SynthesisIn> {
    Ok(serde_json::from_str(json)?)
}

const FACT: &str = "---\nname: tests\ndescription: run the tests\nmetadata:\n  type: feedback\n---\nRun them.\n[source: dream:1; added: 2026-11-20]\n";

fn err(json: &str, content: Option<&str>) -> String {
    match parse(json) {
        Err(e) => format!("{e:#}"),
        Ok(raw) => format!(
            "{:#}",
            check_synthesis(raw, content.map(str::to_string)).expect_err(json)
        ),
    }
}

#[test]
fn a_memory_put_keeps_its_drafts_content() {
    let raw = parse(
        r#"{"op":"put","dir":"/m","file":"tests.md","expect_version":"new","draft":"/d/tests.md",
            "why":"three sessions","citations":["dream:1"," dream:1 ","claude:abc"]}"#,
    )
    .unwrap();
    let s = check_synthesis(raw, Some(FACT.to_string())).unwrap();
    assert_eq!(s.citations(), ["dream:1", "claude:abc"]);
    assert_eq!(s.describe(), "memory put /m/tests.md");
    let Synthesis::Memory { content, .. } = &s else {
        panic!("{s:?}")
    };
    assert_eq!(content, FACT);
    // It round-trips as recorded.
    let back: Synthesis = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back, s);

    let r = parse(r#"{"op":"remove","dir":"/m","file":"old.md","expect_version":"0a1b","why":"merged","citations":["dream:1"]}"#).unwrap();
    assert_eq!(
        check_synthesis(r, None).unwrap().describe(),
        "memory remove /m/old.md"
    );
}

#[test]
fn shapes_are_checked() {
    for (json, content, want) in [
        (r#"{"op":"put"}"#, None, "\"citations\" is empty"),
        (
            r#"{"op":"rewrite","citations":["dream:1"]}"#,
            None,
            "\"rewrite\" is no idea op",
        ),
        (
            r#"{"op":"put","shout":1,"citations":["dream:1"]}"#,
            None,
            "unknown field `shout`",
        ),
        (
            r#"{"op":"put","file":"a.md","expect_version":"new","why":"w","citations":["dream:1"]}"#,
            None,
            "needs \"dir\"",
        ),
        (
            r#"{"op":"put","dir":"/m","file":"../a.md","expect_version":"new","why":"w","citations":["dream:1"]}"#,
            None,
            "",
        ),
        (
            r#"{"op":"put","dir":"/m","file":"a.md","expect_version":"new","why":"w","draft":"/d/a.md","citations":["dream:1"]}"#,
            None,
            "couldn't be read",
        ),
        (
            r#"{"op":"put","dir":"/m","file":"a.md","expect_version":"new","why":"w","draft":"/d/a.md","citations":["dream:1"]}"#,
            Some("no frontmatter"),
            "the draft /d/a.md",
        ),
        (
            r#"{"op":"put","dir":"/m","file":"a.md","why":"w","draft":"/d/a.md","citations":["dream:1"]}"#,
            Some(FACT),
            "needs \"expect_version\"",
        ),
        (
            r#"{"op":"put","dir":"/m","file":"a.md","expect_version":"new","draft":"/d/a.md","citations":["dream:1"]}"#,
            Some(FACT),
            "needs \"why\"",
        ),
        (
            r#"{"op":"put","dir":"/m","file":"a.md","expect_version":"new","why":"w","id":3,"citations":["dream:1"]}"#,
            None,
            "takes no \"id\"",
        ),
        (
            r#"{"op":"remove","dir":"/m","file":"MEMORY.md","expect_version":"v","why":"w","citations":["dream:1"]}"#,
            None,
            "can't be removed",
        ),
        (
            r#"{"op":"remove","dir":"/m","file":"a.md","expect_version":"v","why":"w","draft":"/d","citations":["dream:1"]}"#,
            None,
            "takes no \"draft\"",
        ),
        (
            r#"{"op":"accept","note":"n","citations":["dream:1"]}"#,
            None,
            "needs \"id\"",
        ),
        (
            r#"{"op":"accept","id":2,"citations":["dream:1"]}"#,
            None,
            "needs \"note\"",
        ),
        (
            r#"{"op":"accept","id":2,"other":3,"note":"n","citations":["dream:1"]}"#,
            None,
            "takes no \"other\"",
        ),
        (
            r#"{"op":"accept","id":2,"why":"w","note":"n","citations":["dream:1"]}"#,
            None,
            "takes no \"why\"",
        ),
        (
            r#"{"op":"dup","id":2,"note":"n","citations":["dream:1"]}"#,
            None,
            "the original entry's number",
        ),
        (
            r#"{"op":"dup","id":2,"other":2,"note":"n","citations":["dream:1"]}"#,
            None,
            "isn't a duplicate of itself",
        ),
        (
            r#"{"op":"link","id":2,"other":"34","note":"n","citations":["dream:1"]}"#,
            None,
            "as id@host",
        ),
        (
            r#"{"op":"link","id":2,"other":[1],"note":"n","citations":["dream:1"]}"#,
            None,
            "\"other\" is [1]",
        ),
    ] {
        let got = err(json, content);
        assert!(got.contains(want), "{json}: {got}");
    }
    for json in [
        r#"{"op":"accept","id":2,"note":"real","citations":["friction:h-2"]}"#,
        r#"{"op":"dup","id":3,"other":2,"note":"same","citations":["friction:h-2"]}"#,
        r#"{"op":"dup","id":3,"other":"2","note":"same","citations":["friction:h-2"]}"#,
        r#"{"op":"link","id":3,"other":"34@hub","note":"same","citations":["friction:h-2"]}"#,
    ] {
        check_synthesis(parse(json).unwrap(), None).unwrap_or_else(|e| panic!("{json}: {e:#}"));
    }
}

#[test]
fn citations_resolve_or_are_refused() {
    let known = |kind: &str, key: &str| {
        kind == "dream" && key == "1" || kind == "session" && key == "claude:abc"
    };
    let cites: Vec<String> = ["dream:1", "dream:2", "claude:abc", "report:r1", "docs/x.md"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        unresolved_citations(&cites, &known),
        ["dream:2", "report:r1", "docs/x.md"],
        "unknown prefixes are refused, not waved through"
    );
}

#[test]
fn synthesis_never_converges() {
    let earlier = vec!["an idea".to_string()];
    assert!(converges_with("synthesis", "an idea", &earlier).is_none());
}

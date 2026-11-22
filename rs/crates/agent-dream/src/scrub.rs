//! Excerpts of transcripts made safe to show a dream (docs/DREAMING.md, "Evidence
//! rules"): what a person pasted, what a harness inserted, code, quotes, key blocks
//! and lines that look like they hold a credential are dropped, and what is left is
//! clipped.

use std::sync::LazyLock;

use regex::Regex;

/// Lowercased markers of a line that may hold a credential or personal contact.
const CREDENTIAL_MARKERS: &[&str] = &[
    "token",
    "sk-",
    "ghp_",
    "gho_",
    "github_pat_",
    "xox",
    "akia",
    "password",
    "passwd",
    "secret",
    "api_key",
    "apikey",
    "bearer ",
    "authorization:",
];

static OPEN_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<([A-Za-z_][A-Za-z0-9_.:-]*)(?:\s[^<>]*)?>").unwrap());
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});

/// What is safe to show of `text`, at most `max_chars` characters: tagged blocks
/// (`<pasted_content ...>...</pasted_content>`, `<system-reminder>`, ...), code
/// fences, `>` quote lines, lines starting with `<`, private key blocks, and lines
/// with a credential marker or an email address dropped; blank runs collapsed; then
/// clipped on a char boundary, ending in "…" when clipped.
pub fn excerpt(text: &str, max_chars: usize) -> String {
    let text = drop_tagged_blocks(text);
    let mut out: Vec<&str> = Vec::new();
    let mut fence: Option<&str> = None;
    let mut in_key = false;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
            continue;
        }
        if in_key {
            if t.contains("-----END") {
                in_key = false;
            }
            continue;
        }
        if t.contains("-----BEGIN") && t.contains("PRIVATE KEY") {
            in_key = !t.contains("-----END");
            continue;
        }
        if t.starts_with('>') || t.starts_with('<') || has_credential(t) {
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    clip(&out.join("\n"), max_chars)
}

/// Whether a line carries a credential marker or an email address.
pub fn has_credential(line: &str) -> bool {
    let l = line.to_lowercase();
    CREDENTIAL_MARKERS.iter().any(|m| l.contains(m)) || EMAIL_RE.is_match(line)
}

/// Removes each `<tag ...>...</tag>` block, from its opening tag to the first
/// matching close. An opening tag with no close drops the rest of the text when its
/// name has `_` or `-` (a harness's block, cut short); otherwise the tag stays
/// (`Vec<String>` in prose), and a line it starts is dropped later.
fn drop_tagged_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = OPEN_TAG_RE.captures(rest) {
        let m = c.get(0).unwrap();
        let name = &c[1];
        out.push_str(&rest[..m.start()]);
        let after = &rest[m.end()..];
        let close = format!("</{name}>");
        if let Some(i) = after.find(&close) {
            rest = &after[i + close.len()..];
        } else if name.contains(['_', '-']) {
            rest = "";
        } else {
            out.push_str(m.as_str());
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// At most `max` chars of `s`, cut on a char boundary; "…" ends a cut one (and
/// counts toward `max`).
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_pasted_and_harness_blocks() {
        let t = "keep 1\nsee <pasted_content id=\"x\">\nsk-live secret\nmore\n</pasted_content> after\n<system-reminder>\nbe nice\n</system-reminder>\nkeep 2";
        assert_eq!(excerpt(t, 1000), "keep 1\nsee  after\n\nkeep 2");
    }

    #[test]
    fn an_unclosed_harness_block_drops_the_rest() {
        assert_eq!(
            excerpt("keep\n<pasted_content id=1>\nleak\nleak", 100),
            "keep"
        );
        assert_eq!(excerpt("a Vec<String> b", 100), "a Vec<String> b");
        assert_eq!(excerpt("x <br/> y\n<div>html\nz", 100), "x <br/> y\nz");
    }

    #[test]
    fn drops_fences_quotes_and_tag_lines() {
        let t = "before\n```rust\nlet k = 1;\n```\nmid\n~~~\nraw\n~~~\n> quoted\n  > indented quote\n<div>html\nafter";
        assert_eq!(excerpt(t, 1000), "before\nmid\nafter");
        // An unclosed fence drops the rest.
        assert_eq!(excerpt("a\n```\nb\nc", 100), "a");
    }

    #[test]
    fn drops_private_key_blocks() {
        let t = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3Nza\nzzz\n-----END OPENSSH PRIVATE KEY-----\nb";
        assert_eq!(excerpt(t, 100), "a\nb");
    }

    #[test]
    fn drops_credential_lines() {
        for bad in [
            "export GITHUB_TOKEN=abc",
            "key sk-ant-123",
            "ghp_abcdef",
            "slack xoxb-1",
            "AKIAIOSFODNN7EXAMPLE",
            "the Password is hunter2",
            "client_secret: x",
            "mail someone@example.com about it",
        ] {
            assert!(has_credential(bad), "{bad}");
            assert_eq!(
                excerpt(&format!("ok\n{bad}\nfine"), 100),
                "ok\nfine",
                "{bad}"
            );
        }
        assert!(!has_credential("an ordinary line @ noon"));
    }

    #[test]
    fn collapses_blanks_and_clips_on_char_boundaries() {
        assert_eq!(excerpt("\n\na\n\n\n\nb\n\n", 100), "a\n\nb");
        assert_eq!(excerpt("héllo wörld", 5), "héll…");
        assert_eq!(excerpt("ééé", 3), "ééé");
        assert_eq!(excerpt("abc", 0), "");
        assert_eq!(clip("日本語テキスト", 4), "日本語…");
    }
}

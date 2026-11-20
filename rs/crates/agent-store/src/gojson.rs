//! Go's `json.Marshal` text, for the JSON the store keeps in columns
//! (`command_log.args`, `friction.command`, `friction_votes.command`).
//!
//! serde_json and Go agree on everything the store writes but one thing: Go
//! escapes `<`, `>` and `&` (and U+2028, U+2029) inside strings as
//! backslash-u escapes (`<` is stored as backslash, `u003c`), so an argument
//! like `a && b` is not stored as typed. Those characters can only occur
//! inside strings in JSON text, so replacing them in serde_json's output
//! gives Go's.

use serde::Serialize;

/// `json.Marshal(v)`, as text.
pub fn marshal<T: Serialize + ?Sized>(v: &T) -> serde_json::Result<String> {
    Ok(escape_html(&serde_json::to_string(v)?))
}

const LINE_SEPARATOR: char = '\u{2028}';
const PARAGRAPH_SEPARATOR: char = '\u{2029}';

/// Go's HTML-safe escaping of JSON text.
pub fn escape_html(s: &str) -> String {
    if !s.contains(['<', '>', '&', LINE_SEPARATOR, PARAGRAPH_SEPARATOR]) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            LINE_SEPARATOR => out.push_str("\\u2028"),
            PARAGRAPH_SEPARATOR => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_go() {
        let arg = format!("a && <b> {LINE_SEPARATOR}\"q\"\n{}", char::from(1u8));
        let args = vec!["send".to_string(), "x".to_string(), arg];
        let want = [
            "[\"send\",\"x\",\"a ",
            "\\u0026\\u0026 \\u003cb\\u003e \\u2028",
            "\\\"q\\\"\\n\\u0001\"]",
        ]
        .concat();
        assert_eq!(marshal(&args).unwrap(), want);
        let back: Vec<String> = serde_json::from_str(&marshal(&args).unwrap()).unwrap();
        assert_eq!(back, args);
    }
}

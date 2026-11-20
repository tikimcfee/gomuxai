//! Go's `strconv.Quote`, which is also what `fmt`'s `%q` prints for a string:
//! error messages and hints quote names this way ("agent \"helper\" not
//! found"), and a Rust `{:?}` differs (it escapes `'`, writes `\u{1b}`).

/// Go's `strconv.Quote`: `s` in double quotes, with `"` and `\` escaped,
/// `\a \b \f \n \r \t \v` for those controls, `\xNN` for the other ASCII
/// controls and DEL, and `\uNNNN` / `\UNNNNNNNN` for other characters that
/// aren't printable.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        push_escaped(&mut out, c, '"');
    }
    out.push('"');
    out
}

/// Go's `strconv.QuoteRune`, `%q` of a rune: `'x'`.
pub fn quote_rune(c: char) -> String {
    let mut out = String::from("'");
    push_escaped(&mut out, c, '\'');
    out.push('\'');
    out
}

fn push_escaped(out: &mut String, c: char, quote: char) {
    if c == quote || c == '\\' {
        out.push('\\');
        out.push(c);
        return;
    }
    if is_print(c) {
        out.push(c);
        return;
    }
    match c {
        '\x07' => out.push_str("\\a"),
        '\x08' => out.push_str("\\b"),
        '\x0c' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\x0b' => out.push_str("\\v"),
        c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
        c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
        c => out.push_str(&format!("\\U{:08x}", c as u32)),
    }
}

/// Go's `strconv.IsPrint`: letters, marks, numbers, punctuation, symbols and
/// the ASCII space. Rust has no Unicode category tables in std, so this
/// rejects the controls, the separators other than ' ', the format
/// characters, private use and noncharacters, and accepts the rest, which
/// differs from Go only for unassigned code points (Go escapes those).
pub fn is_print(c: char) -> bool {
    let u = c as u32;
    if u < 0x20 || (0x7f..=0x9f).contains(&u) {
        return false;
    }
    if u < 0x7f {
        return true;
    }
    !matches!(u,
        // Space separators other than ' ' (Zs), line and paragraph separators.
        0xa0 | 0x1680 | 0x2000..=0x200a | 0x202f | 0x205f | 0x3000 | 0x2028 | 0x2029
        // Format characters (Cf).
        | 0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x890 | 0x891 | 0x8e2 | 0x180e
        | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x206f | 0xfeff
        | 0xfff9..=0xfffb | 0x110bd | 0x110cd | 0x13430..=0x1343f | 0x1bca0..=0x1bca3
        | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f
        // Private use (Co).
        | 0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd
        // Noncharacters.
        | 0xfdd0..=0xfdef)
        && (u & 0xfffe) != 0xfffe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_matches_go() {
        // The escapes Go writes are spelled with concat! so no editor folds
        // them into the characters they stand for.
        for (in_, want) in [
            ("", r#""""#),
            ("a", r#""a""#),
            ("it's", r#""it's""#),
            ("say \"hi\"", r#""say \"hi\"""#),
            ("back\\slash", r#""back\\slash""#),
            ("\x07\x08\x0c\n\r\t\x0b", r#""\a\b\f\n\r\t\v""#),
            ("\x1b[0m", r#""\x1b[0m""#),
            ("\x7f", r#""\x7f""#),
            ("\u{9b}", concat!("\"\\", "u009b\"")),
            ("\u{276f} h\u{e9}llo", "\"\u{276f} h\u{e9}llo\""),
            ("\u{a0}", concat!("\"\\", "u00a0\"")),
            ("\u{feff}", concat!("\"\\", "ufeff\"")),
            ("\u{10ffff}", concat!("\"\\", "U0010ffff\"")),
            ("\u{65e5}\u{672c}", "\"\u{65e5}\u{672c}\""),
        ] {
            assert_eq!(quote(in_), want, "quote({in_:?})");
        }
        assert_eq!(quote_rune('\''), r"'\''");
        assert_eq!(quote_rune('"'), r#"'"'"#);
    }
}

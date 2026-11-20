//! Go's `encoding/json` where its exact text matters: the hook outputs
//! Claude Code and agy read, the hooks bundle agy loads, and Claude Code's
//! settings file, which gomuxai edits and must write back as Claude Code
//! would (keys sorted, numbers as written). Not a Go package of ours; the
//! backends' Go code calls `json.Marshal`, `json.MarshalIndent`,
//! `json.Compact` and a `json.Decoder` with `UseNumber`, and these are those.
//!
//! serde_json and Go agree on every byte of compact and indented output but
//! one: Go escapes `<`, `>` and `&` (and U+2028, U+2029) inside strings as
//! backslash-u escapes (`<` is written as a backslash then `u003c`). Those
//! characters can only occur inside strings in JSON text, so replacing them
//! in serde_json's output gives Go's. Map keys are sorted by Go; callers
//! here marshal structs (fields in Go's order) or `BTreeMap`s.
//!
//! The settings file is read into [`Value`], whose numbers keep their text
//! (Go's `json.Number`) and whose objects are sorted maps, and decoded with
//! Go's syntax errors, which the "isn't a JSON object" message quotes.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// Go's `json.Marshal`: compact, HTML characters escaped.
pub(crate) fn marshal<T: Serialize + ?Sized>(v: &T) -> Vec<u8> {
    match serde_json::to_string(v) {
        Ok(s) => escape_html(&s).into_bytes(),
        Err(_) => Vec::new(),
    }
}

/// Go's `json.MarshalIndent(v, "", indent)`.
pub(crate) fn marshal_indent<T: Serialize + ?Sized>(
    v: &T,
    indent: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    v.serialize(&mut ser)?;
    let s = String::from_utf8(buf)?;
    Ok(escape_html(&s).into_bytes())
}

/// The escape Go writes for `c` inside a JSON string: a backslash, `u`, and
/// four lowercase hex digits.
fn u_escape(c: char) -> String {
    format!("\\u{:04x}", c as u32)
}

/// Replaces `<`, `>`, `&`, U+2028 and U+2029 in JSON text with the escapes
/// Go's encoder writes for them.
pub(crate) fn escape_html(s: &str) -> String {
    if !s.contains(['<', '>', '&', '\u{2028}', '\u{2029}']) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => out.push_str(&u_escape(c)),
            c => out.push(c),
        }
    }
    out
}

/// Go's `json.Compact`: `src` without the whitespace between tokens, or
/// `None` when it isn't one valid JSON value (Go's error).
pub(crate) fn compact(src: &[u8]) -> Option<Vec<u8>> {
    let mut d = Decoder::new(src, Mode::Unmarshal);
    d.top().ok()?;
    let mut out = Vec::with_capacity(src.len());
    let mut in_string = false;
    let mut escaped = false;
    for &c in src {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'"' => {
                in_string = true;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Some(out)
}

/// Go's `json.Unmarshal` into a struct: the syntax errors are Go's, as is a
/// top-level value of another kind ("json: cannot unmarshal array into Go
/// value of type <go_type>"); `null` leaves the struct as it was (its
/// default). Object members decode with Go's lenient UTF-8, surrogate, and
/// null rules (`agent_base::gostd::json`).
pub(crate) fn unmarshal_struct<T: DeserializeOwned + Default>(
    data: &[u8],
    go_type: &str,
) -> anyhow::Result<T> {
    let mut d = Decoder::new(data, Mode::Unmarshal);
    let v = d.top().map_err(|e| anyhow::anyhow!(e))?;
    match v {
        Value::Null => Ok(T::default()),
        Value::Object(_) => Ok(agent_base::gostd::json::from_slice(data)?),
        other => anyhow::bail!(
            "json: cannot unmarshal {} into Go value of type {go_type}",
            other.kind()
        ),
    }
}

/// For a `json.RawMessage` field: the value's text as it was, `null`
/// included (Go hands `null` to RawMessage's UnmarshalJSON); absent is
/// `None` (`#[serde(default)]`).
pub(crate) fn raw_value<'de, D>(d: D) -> Result<Option<Box<RawValue>>, D::Error>
where
    D: Deserializer<'de>,
{
    Box::<RawValue>::deserialize(d).map(Some)
}

/// A JSON value as Go decodes one into `any` with `UseNumber`: numbers keep
/// their text, objects are maps (the last of duplicate keys wins, and
/// they're written back sorted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Drop for Value {
    /// Takes a deeply nested tree apart without recursing on the call stack.
    fn drop(&mut self) {
        let mut stack: Vec<Value> = match self {
            Value::Array(a) if !a.is_empty() => std::mem::take(a),
            Value::Object(m) if !m.is_empty() => std::mem::take(m).into_values().collect(),
            _ => return,
        };
        while let Some(mut v) = stack.pop() {
            match &mut v {
                Value::Array(a) if !a.is_empty() => stack.append(a),
                Value::Object(m) if !m.is_empty() => stack.extend(std::mem::take(m).into_values()),
                _ => {}
            }
        }
    }
}

impl Value {
    /// What Go's UnmarshalTypeError calls a value of this kind.
    fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
}

/// Reads `data` as a `json.Decoder` with `UseNumber` decodes its first value
/// into a `map[string]any`: `Ok(None)` for `null` (a nil map), an error with
/// Go's text for a syntax error or a value that isn't an object. What follows
/// the first value is not read, as the Decoder doesn't.
pub(crate) fn decode_object(data: &[u8]) -> Result<Option<BTreeMap<String, Value>>, String> {
    let mut d = Decoder::new(data, Mode::Stream);
    let mut v = d.top()?;
    match &mut v {
        Value::Null => Ok(None),
        Value::Object(m) => Ok(Some(std::mem::take(m))),
        other => Err(format!(
            "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
            other.kind()
        )),
    }
}

/// Writes `v` as a `json.Encoder` with `SetEscapeHTML(escape_html)` and
/// `SetIndent("", indent)` does, newline included.
pub(crate) fn encode_indent(v: &Value, indent: &str, escape_html: bool) -> String {
    let mut out = String::new();
    write_value(&mut out, v, indent, 0, escape_html);
    out.push('\n');
    out
}

fn newline(out: &mut String, indent: &str, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str(indent);
    }
}

fn write_value(out: &mut String, v: &Value, indent: &str, depth: usize, html: bool) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(n),
        Value::String(s) => write_string(out, s, html),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, e, indent, depth + 1, html);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, e)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_string(out, k, html);
                out.push_str(": ");
                write_value(out, e, indent, depth + 1, html);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

/// Go's string encoding: a backslash before `"` and `\`, the short escapes
/// for backspace, form feed, newline, carriage return and tab, a backslash-u
/// escape for the other controls and for U+2028 and U+2029 always, and with
/// `html` for `<`, `>` and `&` too.
fn write_string(out: &mut String, s: &str, html: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' if html => out.push_str(&u_escape(c)),
            '\u{2028}' | '\u{2029}' => out.push_str(&u_escape(c)),
            c if (c as u32) < 0x20 => out.push_str(&u_escape(c)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// How the input ends: `json.Unmarshal` reads all of it (an early end is
/// "unexpected end of JSON input", and anything after the value an error);
/// a `json.Decoder` reads one value (an early end is "unexpected EOF").
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unmarshal,
    Stream,
}

/// Go's `maxNestingDepth`.
const MAX_NESTING: usize = 10000;

enum Frame {
    Array(Vec<Value>),
    Object {
        map: BTreeMap<String, Value>,
        key: String,
    },
}

/// An iterative JSON reader with the words and nesting limit of Go's scanner.
struct Decoder<'a> {
    b: &'a [u8],
    i: usize,
    mode: Mode,
}

type DResult<T> = Result<T, String>;

/// Go's `quoteChar`: a byte in single quotes, as an error names it.
fn quote_char(c: u8) -> String {
    match c {
        b'\'' => r"'\''".to_string(),
        b'"' => "'\"'".to_string(),
        _ => {
            let s = agent_base::gostd::strconv::quote(&char::from(c).to_string());
            format!("'{}'", &s[1..s.len() - 1])
        }
    }
}

fn invalid(c: u8, context: &str) -> String {
    format!("invalid character {} {context}", quote_char(c))
}

impl<'a> Decoder<'a> {
    fn new(b: &'a [u8], mode: Mode) -> Self {
        Decoder { b, i: 0, mode }
    }

    fn eof(&self) -> String {
        match self.mode {
            Mode::Unmarshal => "unexpected end of JSON input".to_string(),
            Mode::Stream => "unexpected EOF".to_string(),
        }
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> DResult<u8> {
        self.b.get(self.i).copied().ok_or_else(|| self.eof())
    }

    fn top(&mut self) -> DResult<Value> {
        let v = self.value()?;
        if self.mode == Mode::Unmarshal {
            self.skip_ws();
            if let Some(&c) = self.b.get(self.i) {
                return Err(invalid(c, "after top-level value"));
            }
        }
        Ok(v)
    }

    fn value(&mut self) -> DResult<Value> {
        let mut stack: Vec<Frame> = Vec::new();
        'value: loop {
            self.skip_ws();
            let c = self.peek()?;
            let mut v = match c {
                b'{' => {
                    if stack.len() >= MAX_NESTING {
                        return Err("exceeded max depth".to_string());
                    }
                    self.i += 1;
                    self.skip_ws();
                    if self.peek()? == b'}' {
                        self.i += 1;
                        Value::Object(BTreeMap::new())
                    } else {
                        let key = self.object_key()?;
                        stack.push(Frame::Object {
                            map: BTreeMap::new(),
                            key,
                        });
                        continue 'value;
                    }
                }
                b'[' => {
                    if stack.len() >= MAX_NESTING {
                        return Err("exceeded max depth".to_string());
                    }
                    self.i += 1;
                    self.skip_ws();
                    if self.peek()? == b']' {
                        self.i += 1;
                        Value::Array(Vec::new())
                    } else {
                        stack.push(Frame::Array(Vec::new()));
                        continue 'value;
                    }
                }
                b'"' => Value::String(self.string()?),
                b'-' | b'0'..=b'9' => self.number()?,
                b't' => self.literal("true", Value::Bool(true))?,
                b'f' => self.literal("false", Value::Bool(false))?,
                b'n' => self.literal("null", Value::Null)?,
                _ => return Err(invalid(c, "looking for beginning of value")),
            };

            loop {
                match stack.last_mut() {
                    None => return Ok(v),
                    Some(Frame::Array(a)) => {
                        a.push(v);
                        self.skip_ws();
                        match self.peek()? {
                            b',' => {
                                self.i += 1;
                                continue 'value;
                            }
                            b']' => {
                                self.i += 1;
                                let Some(Frame::Array(a)) = stack.pop() else {
                                    unreachable!("top frame is an array");
                                };
                                v = Value::Array(a);
                            }
                            c => return Err(invalid(c, "after array element")),
                        }
                    }
                    Some(Frame::Object { map, key }) => {
                        map.insert(std::mem::take(key), v);
                        self.skip_ws();
                        match self.peek()? {
                            b',' => {
                                self.i += 1;
                                let next_key = self.object_key()?;
                                if let Some(Frame::Object { key, .. }) = stack.last_mut() {
                                    *key = next_key;
                                }
                                continue 'value;
                            }
                            b'}' => {
                                self.i += 1;
                                let Some(Frame::Object { map, .. }) = stack.pop() else {
                                    unreachable!("top frame is an object");
                                };
                                v = Value::Object(map);
                            }
                            c => return Err(invalid(c, "after object key:value pair")),
                        }
                    }
                }
            }
        }
    }

    fn object_key(&mut self) -> DResult<String> {
        self.skip_ws();
        let c = self.peek()?;
        if c != b'"' {
            return Err(invalid(c, "looking for beginning of object key string"));
        }
        let key = self.string()?;
        self.skip_ws();
        let c = self.peek()?;
        if c != b':' {
            return Err(invalid(c, "after object key"));
        }
        self.i += 1;
        Ok(key)
    }

    fn literal(&mut self, word: &str, v: Value) -> DResult<Value> {
        let w = word.as_bytes();
        self.i += 1;
        for &want in &w[1..] {
            let c = self.peek()?;
            if c != want {
                return Err(invalid(
                    c,
                    &format!("in literal {word} (expecting {})", quote_char(want)),
                ));
            }
            self.i += 1;
        }
        Ok(v)
    }

    fn digits(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
    }

    /// Reads a number, kept as written. At the top level of a stream a
    /// number may end with the input.
    fn number(&mut self) -> DResult<Value> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
            let c = self.peek()?;
            if !c.is_ascii_digit() {
                return Err(invalid(c, "in numeric literal"));
            }
        }
        if self.b[self.i] == b'0' {
            self.i += 1;
        } else {
            self.digits();
        }
        if self.i < self.b.len() && self.b[self.i] == b'.' {
            self.i += 1;
            let c = self.peek()?;
            if !c.is_ascii_digit() {
                return Err(invalid(c, "after decimal point in numeric literal"));
            }
            self.digits();
        }
        if self.i < self.b.len() && matches!(self.b[self.i], b'e' | b'E') {
            self.i += 1;
            if matches!(self.peek()?, b'+' | b'-') {
                self.i += 1;
            }
            let c = self.peek()?;
            if !c.is_ascii_digit() {
                return Err(invalid(c, "in exponent of numeric literal"));
            }
            self.digits();
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).unwrap_or_default();
        Ok(Value::Number(text.to_string()))
    }

    /// Reads a string literal (at its opening quote), checked as Go's
    /// scanner checks it, and returns its value as Go unquotes it.
    fn string(&mut self) -> DResult<String> {
        self.i += 1;
        let start = self.i;
        loop {
            let c = self.peek()?;
            match c {
                b'"' => break,
                b'\\' => {
                    self.i += 1;
                    let e = self.peek()?;
                    match e {
                        b'b' | b'f' | b'n' | b'r' | b't' | b'\\' | b'/' | b'"' => self.i += 1,
                        b'u' => {
                            self.i += 1;
                            for _ in 0..4 {
                                let h = self.peek()?;
                                if !h.is_ascii_hexdigit() {
                                    return Err(invalid(h, "in \\u hexadecimal character escape"));
                                }
                                self.i += 1;
                            }
                        }
                        _ => return Err(invalid(e, "in string escape code")),
                    }
                }
                c if c < 0x20 => return Err(invalid(c, "in string literal")),
                _ => self.i += 1,
            }
        }
        let raw = &self.b[start..self.i];
        self.i += 1;
        Ok(unquote(raw))
    }
}

fn hex4(b: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(b.get(..4)?).ok()?;
    u32::from_str_radix(s, 16).ok()
}

/// Go's `unquoteBytes` for a checked literal: escapes decoded (a surrogate
/// pair joined, a lone surrogate U+FFFD), and each byte of invalid UTF-8
/// replaced by U+FFFD.
fn unquote(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if c == b'\\' {
            let e = raw[i + 1];
            i += 2;
            match e {
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let r = hex4(&raw[i..]).unwrap_or(0xFFFD);
                    i += 4;
                    if (0xD800..0xE000).contains(&r) {
                        // A surrogate: joined with a low one escaped right after.
                        if r < 0xDC00
                            && raw.get(i) == Some(&b'\\')
                            && raw.get(i + 1) == Some(&b'u')
                            && let Some(lo) = hex4(&raw[i + 2..])
                            && (0xDC00..0xE000).contains(&lo)
                        {
                            let joined = 0x10000 + ((r - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(joined).unwrap_or('\u{FFFD}'));
                            i += 6;
                        } else {
                            out.push('\u{FFFD}');
                        }
                    } else {
                        out.push(char::from_u32(r).unwrap_or('\u{FFFD}'));
                    }
                }
                other => out.push(char::from(other)),
            }
            continue;
        }
        if c < 0x80 {
            out.push(char::from(c));
            i += 1;
            continue;
        }
        let width = match c {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        match raw
            .get(i..i + width)
            .and_then(|s| std::str::from_utf8(s).ok())
        {
            Some(s) if width > 0 => {
                out.push_str(s);
                i += width;
            }
            _ => {
                out.push('\u{FFFD}');
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_err(s: &str) -> String {
        decode_object(s.as_bytes()).unwrap_err()
    }

    #[test]
    fn go_syntax_errors() {
        assert_eq!(stream_err(r#"{"model": "opus","#), "unexpected EOF");
        assert_eq!(
            stream_err("not json"),
            "invalid character 'o' in literal null (expecting 'u')"
        );
        assert_eq!(
            stream_err("x"),
            "invalid character 'x' looking for beginning of value"
        );
        assert_eq!(
            stream_err(r#"{"a" 1}"#),
            "invalid character '1' after object key"
        );
        assert_eq!(
            stream_err(r#"{"a":1 "b"}"#),
            "invalid character '\"' after object key:value pair"
        );
        assert_eq!(
            stream_err(r#"{"a":[1 2]}"#),
            "invalid character '2' after array element"
        );
        assert_eq!(
            stream_err(r#"{a}"#),
            "invalid character 'a' looking for beginning of object key string"
        );
        assert_eq!(
            stream_err(r#"{"a":-x}"#),
            "invalid character 'x' in numeric literal"
        );
        assert_eq!(
            stream_err(r#"{"a":"\q"}"#),
            "invalid character 'q' in string escape code"
        );
        assert_eq!(
            stream_err("{\"a\":\"\t\"}"),
            "invalid character '\\t' in string literal"
        );
        assert_eq!(
            stream_err("[]"),
            "json: cannot unmarshal array into Go value of type map[string]interface {}"
        );
        assert_eq!(
            stream_err("\"s\""),
            "json: cannot unmarshal string into Go value of type map[string]interface {}"
        );
        assert_eq!(decode_object(b"null"), Ok(None));
        // A Decoder reads the first value only.
        assert!(decode_object(br#"{"a":1} trailing"#).unwrap().is_some());
        let err = unmarshal_struct::<crate::generic::GenericConfig>(b"{} x", "T").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid character 'x' after top-level value"
        );
        let err = unmarshal_struct::<crate::generic::GenericConfig>(b"{", "T").unwrap_err();
        assert_eq!(err.to_string(), "unexpected end of JSON input");
        let err =
            unmarshal_struct::<crate::generic::GenericConfig>(b"[1]", "backend.GenericConfig")
                .unwrap_err();
        assert_eq!(
            err.to_string(),
            "json: cannot unmarshal array into Go value of type backend.GenericConfig"
        );
    }

    #[test]
    fn numbers_and_strings_as_written() {
        let m = decode_object(b"{\"big\":12345678901234567890123,\"f\":1.50e+3,\"s\":\"a\\u00e9\\ud83d\\ude42\\ud800x<\xff\"}")
            .unwrap()
            .unwrap();
        assert_eq!(m["big"], Value::Number("12345678901234567890123".into()));
        assert_eq!(m["f"], Value::Number("1.50e+3".into()));
        let s = "a\u{e9}\u{1f642}\u{FFFD}x<\u{FFFD}";
        assert_eq!(m["s"], Value::String(s.into()));
        let out = encode_indent(&Value::Object(m), "  ", false);
        assert_eq!(
            out,
            format!(
                "{{\n  \"big\": 12345678901234567890123,\n  \"f\": 1.50e+3,\n  \"s\": \"{s}\"\n}}\n"
            )
        );
    }

    #[test]
    fn indent_like_go() {
        let m = decode_object(
            b"{\"b\":[],\"a\":{},\"c\":[1,{\"x\":null,\"y\":true}],\"d\":\"\\u2028\\u0001\"}",
        )
        .unwrap()
        .unwrap();
        assert_eq!(m["d"], Value::String("\u{2028}\u{1}".into()));
        assert_eq!(
            encode_indent(&Value::Object(m), "  ", false),
            "{\n  \"a\": {},\n  \"b\": [],\n  \"c\": [\n    1,\n    {\n      \"x\": null,\n      \"y\": true\n    }\n  ],\n  \"d\": \"\\u2028\\u0001\"\n}\n"
        );
    }

    #[test]
    fn compact_like_go() {
        assert_eq!(
            compact(br#"{ "command": "a  b" , "x" : [1, 2] }"#).unwrap(),
            br#"{"command":"a  b","x":[1,2]}"#
        );
        assert_eq!(compact(br#"{"a":"\"  \\"}"#).unwrap(), br#"{"a":"\"  \\"}"#);
        assert_eq!(compact(b""), None);
        assert_eq!(compact(b"{"), None);
    }

    #[test]
    fn marshal_escapes_html() {
        let mut m = BTreeMap::new();
        m.insert("k", "<a & b>\u{2028}");
        assert_eq!(
            marshal(&m),
            b"{\"k\":\"\\u003ca \\u0026 b\\u003e\\u2028\"}".to_vec()
        );
    }

    #[test]
    fn max_depth_without_recursing() {
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(|| {
                let ok_obj = format!("{}1{}", "{\"a\":".repeat(10000), "}".repeat(10000));
                assert!(decode_object(ok_obj.as_bytes()).unwrap().is_some());

                let deep_obj = "{\"a\":".repeat(50_000);
                assert_eq!(
                    decode_object(deep_obj.as_bytes()),
                    Err("exceeded max depth".to_string())
                );
                assert_eq!(compact(deep_obj.as_bytes()), None);

                let deep_arr = format!("{}{}", "[".repeat(10001), "]".repeat(10001));
                assert_eq!(
                    decode_object(deep_arr.as_bytes()),
                    Err("exceeded max depth".to_string())
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

//! Go's `encoding/json`, where it behaves differently from serde_json and the
//! difference shows in what we print. The Go is 1.27, whose `encoding/json`
//! is the v1 API over the v2 implementation (the default `jsonv2`
//! experiment); every rule below was checked against it.
//!
//! **Decoding.** [`from_slice`] and [`from_str`] decode as `json.Unmarshal`
//! does; [`from_slice_partial`] also keeps what a `json.Unmarshal` that
//! failed on a mistyped field still filled in; [`parse`] reads a value once
//! so it can be decoded into several types ([`Document::decode`]), as Go
//! fills a struct and the structs embedded in it from one object. Where they
//! differ from serde_json:
//!
//! - A string's invalid UTF-8 bytes become U+FFFD, one per byte, and so does
//!   a backslash-u escape of half a surrogate pair. serde_json fails the
//!   whole value.
//! - `null` leaves a field at its zero value: `""`, `0`, `false`, an empty
//!   list or map, a struct's defaults; an `Option` is `None`.
//! - An object's member names match a struct's fields exactly first, then
//!   ignoring case as `strings.EqualFold` does (`"Type"` fills `type`).
//! - A name that comes twice is decoded twice into the same field: the last
//!   string, number or bool that isn't `null` wins, nested structs and maps
//!   merge, and a list is replaced (a later `null` empties a list, a map or
//!   an `Option`). serde's derived structs reject the record.
//! - A struct decodes from an object or `null` only; serde also takes an
//!   array.
//! - An integer field takes an integer literal only (`1.0` and `1e2` are
//!   errors); a float field, a finite number.
//! - Decoded into [`serde_json::Value`] (Go's `any`), an integer beyond
//!   2^53 is the float64 Go would hold.
//! - Only space, tab, CR and LF are whitespace, and values nest up to 10000
//!   deep.
//!
//! Unlike Go, decoding into a type recurses at most 500 levels deep (deeper
//! would risk a thread's stack, in a debug build most); a
//! [`serde_json::value::RawValue`] of any depth is fine, and parsing and
//! dropping a value never recurse. (serde_json stops at 128.)
//!
//! **Encoding.** [`GoFormatter`] makes serde_json write the bytes Go's
//! encoder writes; [`to_writer`], [`to_vec`], [`to_string`], [`marshal`] and
//! [`Encoder`] use it. U+2028 and U+2029 are always escaped, in strings and
//! in raw values alike; `<`, `>` and `&` only with HTML escaping on, which is
//! `json.Marshal`'s and a new `json.Encoder`'s default
//! (`SetEscapeHTML(false)` turns it off). A float is written as Go writes a
//! float64 (`1e+21`, `1e-7`, `5`).
//!
//! **Field attributes.** For structs decoded with serde_json itself,
//! [`null_as_zero`], [`object_or_null`] and [`optional_object`] are
//! `deserialize_with` helpers that give one field Go's rules.

use std::borrow::Cow;
use std::cell::RefCell;
use std::io;

use serde::de::value::{BorrowedStrDeserializer, StringDeserializer};
use serde::de::{
    self, DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Unexpected,
    Visitor,
};
use serde::{Deserialize, Serialize};

/// Decoding and encoding errors are serde_json's.
pub type Error = serde_json::Error;
pub type Result<T> = std::result::Result<T, Error>;

/// Go's `maxNestingDepth`.
const MAX_NESTING: usize = 10000;
/// How deep a typed decode goes (see the module docs).
const MAX_DECODE_DEPTH: usize = 500;
/// serde_json's name for a [`serde_json::value::RawValue`] in the serde data
/// model: a deserializer hands one over as a map of this one key to the
/// value's text.
const RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";
/// The largest integer a float64 holds exactly.
const FLOAT64_EXACT: u64 = 1 << 53;

fn error(msg: impl std::fmt::Display) -> Error {
    <Error as de::Error>::custom(msg)
}

// --- strings ----------------------------------------------------------------

/// Go's `string(b)` as Go's JSON encoder writes it (and as its decoder reads
/// a string's raw bytes): each byte of an invalid UTF-8 sequence is one
/// U+FFFD. `String::from_utf8_lossy` writes one U+FFFD per sequence instead
/// ("\xe2\x82" is one there, two in Go).
pub fn lossy(b: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(b) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => {
            let mut out = String::with_capacity(b.len() + 8);
            push_lossy(&mut out, b);
            Cow::Owned(out)
        }
    }
}

fn push_lossy(out: &mut String, mut b: &[u8]) {
    loop {
        match std::str::from_utf8(b) {
            Ok(s) => {
                out.push_str(s);
                return;
            }
            Err(e) => {
                let (good, rest) = b.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(good).unwrap_or_default());
                let bad = e.error_len().unwrap_or(rest.len());
                for _ in 0..bad {
                    out.push(char::REPLACEMENT_CHARACTER);
                }
                b = &rest[bad..];
            }
        }
    }
}

/// Decodes a JSON string's contents (between its quotes, already checked by
/// the parser) as Go does: a backslash-u escape of a high surrogate followed
/// by one of a low surrogate is their character; either half alone is
/// U+FFFD, and the escape after an unpaired high half is read on its own.
fn unescape(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' {
            let end = raw[i..]
                .iter()
                .position(|&c| c == b'\\')
                .map_or(raw.len(), |p| i + p);
            push_lossy(&mut out, &raw[i..end]);
            i = end;
            continue;
        }
        let c = raw.get(i + 1).copied().unwrap_or(b'\\');
        i += 2;
        match c {
            b'b' => out.push('\x08'),
            b'f' => out.push('\x0c'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let r = hex4(&raw[i..]).unwrap_or(0xFFFD);
                i += 4;
                if (0xD800..0xE000).contains(&r) {
                    if r < 0xDC00
                        && raw[i..].starts_with(b"\\u")
                        && let Some(r2) = hex4(&raw[i + 2..])
                        && (0xDC00..0xE000).contains(&r2)
                    {
                        let cp = 0x10000 + ((r - 0xD800) << 10) + (r2 - 0xDC00);
                        out.push(char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER));
                        i += 6;
                        continue;
                    }
                    out.push(char::REPLACEMENT_CHARACTER);
                    continue;
                }
                out.push(char::from_u32(r).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            // '"', '\\' and '/' stand for themselves.
            c => out.push(c as char),
        }
    }
    out
}

fn hex4(b: &[u8]) -> Option<u32> {
    let h = b.get(..4)?;
    let mut r = 0u32;
    for &c in h {
        r = r * 16 + (c as char).to_digit(16)?;
    }
    Some(r)
}

/// Reports whether `a` and `b` are equal under Unicode simple case folding,
/// as Go's `strings.EqualFold` (which is how Go matches a member name to a
/// field that doesn't match it exactly).
fn equal_fold(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    let (mut x, mut y) = (a.chars(), b.chars());
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(c), Some(d)) => {
                let same = c == d
                    || one_char(c.to_lowercase(), c) == one_char(d.to_lowercase(), d)
                    || one_char(c.to_uppercase(), c) == one_char(d.to_uppercase(), d);
                if !same {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// A case mapping's result if it is one character, else `c` itself (a simple
/// fold maps one character to one).
fn one_char(mut it: impl Iterator<Item = char>, c: char) -> char {
    match (it.next(), it.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

// --- the parsed value -------------------------------------------------------

/// A string's contents: the bytes between its quotes, and whether it has
/// escapes to decode.
#[derive(Debug, Clone, Copy)]
struct Text {
    start: usize,
    end: usize,
    escaped: bool,
}

/// One JSON value, by where it is in the input.
#[derive(Debug)]
struct Node {
    start: usize,
    end: usize,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Null,
    Bool(bool),
    Number,
    String(Text),
    Array(Vec<Node>),
    Object(Vec<(Text, Node)>),
}

impl Drop for Node {
    /// Takes a tree apart without recursing, however deep it nests.
    fn drop(&mut self) {
        let mut stack: Vec<Node> = match &mut self.kind {
            Kind::Array(items) if !items.is_empty() => std::mem::take(items),
            Kind::Object(members) if !members.is_empty() => {
                members.drain(..).map(|(_, n)| n).collect()
            }
            _ => return,
        };
        while let Some(mut n) = stack.pop() {
            match &mut n.kind {
                Kind::Array(items) => stack.append(items),
                Kind::Object(members) => stack.extend(members.drain(..).map(|(_, n)| n)),
                _ => {}
            }
        }
    }
}

fn unexpected(n: &Node) -> Unexpected<'static> {
    match n.kind {
        Kind::Null => Unexpected::Unit,
        Kind::Bool(b) => Unexpected::Bool(b),
        Kind::Number => Unexpected::Other("number"),
        Kind::String(_) => Unexpected::Other("string"),
        Kind::Array(_) => Unexpected::Seq,
        Kind::Object(_) => Unexpected::Map,
    }
}

// --- parsing ----------------------------------------------------------------

/// Finds the first `"`, `\` or control byte, eight bytes at a time. The
/// word test may report a byte that isn't one (a borrow between lanes); the
/// byte scan after it settles that.
fn find_special(s: &[u8]) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let has_zero = |v: u64| v.wrapping_sub(ONES) & !v & HIGH;
    let mut i = 0;
    while i + 8 <= s.len() {
        let mut w = [0u8; 8];
        w.copy_from_slice(&s[i..i + 8]);
        let w = u64::from_le_bytes(w);
        let control = w.wrapping_sub(0x20 * ONES) & !w & HIGH;
        if control | has_zero(w ^ (b'"' as u64 * ONES)) | has_zero(w ^ (b'\\' as u64 * ONES)) != 0 {
            break;
        }
        i += 8;
    }
    s[i..]
        .iter()
        .position(|&b| b == b'"' || b == b'\\' || b < 0x20)
        .map(|p| i + p)
}

fn describe(b: u8) -> String {
    match b {
        b'\'' => "'\\''".to_string(),
        b'"' => "'\"'".to_string(),
        0x20..=0x7e => format!("'{}'", b as char),
        _ => format!("{:?}", b as char),
    }
}

enum Frame {
    Array {
        start: usize,
        items: Vec<Node>,
    },
    Object {
        start: usize,
        members: Vec<(Text, Node)>,
        name: Option<Text>,
    },
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while let Some(&b) = self.src.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn fail(&self, context: &str) -> Error {
        match self.src.get(self.pos) {
            None => error("unexpected end of JSON input"),
            Some(&b) => error(format!("invalid character {} {context}", describe(b))),
        }
    }

    /// Reads one value, without recursing.
    fn value(&mut self) -> Result<Node> {
        let mut stack: Vec<Frame> = Vec::new();
        'value: loop {
            self.ws();
            let start = self.pos;
            let mut node = match self.src.get(self.pos) {
                None => return Err(error("unexpected end of JSON input")),
                Some(b'[') | Some(b'{') => {
                    if stack.len() >= MAX_NESTING {
                        return Err(error("exceeded max depth"));
                    }
                    let open = self.src[self.pos];
                    self.pos += 1;
                    self.ws();
                    if open == b'[' {
                        if self.src.get(self.pos) == Some(&b']') {
                            self.pos += 1;
                            Node {
                                start,
                                end: self.pos,
                                kind: Kind::Array(Vec::new()),
                            }
                        } else {
                            stack.push(Frame::Array {
                                start,
                                items: Vec::new(),
                            });
                            continue 'value;
                        }
                    } else if self.src.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        Node {
                            start,
                            end: self.pos,
                            kind: Kind::Object(Vec::new()),
                        }
                    } else {
                        let name = self.name()?;
                        stack.push(Frame::Object {
                            start,
                            members: Vec::new(),
                            name: Some(name),
                        });
                        continue 'value;
                    }
                }
                Some(b'"') => {
                    let t = self.string()?;
                    Node {
                        start,
                        end: self.pos,
                        kind: Kind::String(t),
                    }
                }
                Some(b't') => {
                    self.literal(b"true")?;
                    Node {
                        start,
                        end: self.pos,
                        kind: Kind::Bool(true),
                    }
                }
                Some(b'f') => {
                    self.literal(b"false")?;
                    Node {
                        start,
                        end: self.pos,
                        kind: Kind::Bool(false),
                    }
                }
                Some(b'n') => {
                    self.literal(b"null")?;
                    Node {
                        start,
                        end: self.pos,
                        kind: Kind::Null,
                    }
                }
                Some(b'-' | b'0'..=b'9') => {
                    self.number()?;
                    Node {
                        start,
                        end: self.pos,
                        kind: Kind::Number,
                    }
                }
                Some(_) => return Err(self.fail("looking for beginning of value")),
            };
            // Hand the value to the containers it closes.
            loop {
                match stack.last_mut() {
                    None => return Ok(node),
                    Some(Frame::Array { items, .. }) => {
                        items.push(node);
                        self.ws();
                        match self.src.get(self.pos) {
                            Some(b',') => {
                                self.pos += 1;
                                continue 'value;
                            }
                            Some(b']') => {
                                self.pos += 1;
                                let Some(Frame::Array { start, items }) = stack.pop() else {
                                    unreachable!("the top frame is an array");
                                };
                                node = Node {
                                    start,
                                    end: self.pos,
                                    kind: Kind::Array(items),
                                };
                            }
                            _ => return Err(self.fail("after array element")),
                        }
                    }
                    Some(Frame::Object { members, name, .. }) => {
                        let Some(n) = name.take() else {
                            unreachable!("an object frame waits on a member's value");
                        };
                        members.push((n, node));
                        self.ws();
                        match self.src.get(self.pos) {
                            Some(b',') => {
                                self.pos += 1;
                                *name = Some(self.name()?);
                                continue 'value;
                            }
                            Some(b'}') => {
                                self.pos += 1;
                                let Some(Frame::Object { start, members, .. }) = stack.pop() else {
                                    unreachable!("the top frame is an object");
                                };
                                node = Node {
                                    start,
                                    end: self.pos,
                                    kind: Kind::Object(members),
                                };
                            }
                            _ => return Err(self.fail("after object key:value pair")),
                        }
                    }
                }
            }
        }
    }

    /// Reads an object member's name and the colon after it.
    fn name(&mut self) -> Result<Text> {
        self.ws();
        if self.src.get(self.pos) != Some(&b'"') {
            return Err(self.fail("looking for beginning of object key string"));
        }
        let t = self.string()?;
        self.ws();
        if self.src.get(self.pos) != Some(&b':') {
            return Err(self.fail("after object key"));
        }
        self.pos += 1;
        Ok(t)
    }

    /// Reads a string, at its opening quote.
    fn string(&mut self) -> Result<Text> {
        self.pos += 1;
        let start = self.pos;
        let mut escaped = false;
        loop {
            let Some(i) = find_special(&self.src[self.pos..]) else {
                self.pos = self.src.len();
                return Err(error("unexpected end of JSON input"));
            };
            self.pos += i;
            match self.src[self.pos] {
                b'"' => {
                    let end = self.pos;
                    self.pos += 1;
                    return Ok(Text {
                        start,
                        end,
                        escaped,
                    });
                }
                b'\\' => {
                    escaped = true;
                    match self.src.get(self.pos + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.pos += 2
                        }
                        Some(b'u') => {
                            if hex4(&self.src[self.pos + 2..]).is_none() {
                                return Err(error("invalid escape sequence in string"));
                            }
                            self.pos += 6;
                        }
                        None => return Err(error("unexpected end of JSON input")),
                        Some(_) => return Err(error("invalid escape sequence in string")),
                    }
                }
                _ => return Err(self.fail("in string literal")),
            }
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<()> {
        for &c in word {
            if self.src.get(self.pos) != Some(&c) {
                return Err(self.fail("in literal"));
            }
            self.pos += 1;
        }
        Ok(())
    }

    fn number(&mut self) -> Result<()> {
        let digit = |p: &Self| matches!(p.src.get(p.pos), Some(b'0'..=b'9'));
        if self.src.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        match self.src.get(self.pos) {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while digit(self) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.fail("in numeric literal")),
        }
        if self.src.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if !digit(self) {
                return Err(self.fail("after decimal point in numeric literal"));
            }
            while digit(self) {
                self.pos += 1;
            }
        }
        if matches!(self.src.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.src.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digit(self) {
                return Err(self.fail("in exponent of numeric literal"));
            }
            while digit(self) {
                self.pos += 1;
            }
        }
        Ok(())
    }
}

/// A JSON value read once, to decode into one type or several.
pub struct Document<'a> {
    src: &'a [u8],
    /// `src` as text, when it is valid UTF-8 (strings then borrow from it).
    text: Option<&'a str>,
    root: Node,
}

impl std::fmt::Debug for Document<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("len", &self.src.len())
            .finish()
    }
}

/// Reads `src` as one JSON value, with what may surround it being
/// whitespace only: the syntax `json.Unmarshal` (and `json.Valid`) accepts.
pub fn parse(src: &[u8]) -> Result<Document<'_>> {
    let mut p = Parser { src, pos: 0 };
    let root = p.value()?;
    p.ws();
    if p.pos < src.len() {
        return Err(p.fail("after top-level value"));
    }
    Ok(Document {
        src,
        text: std::str::from_utf8(src).ok(),
        root,
    })
}

/// Go's `json.Valid`.
pub fn valid(src: &[u8]) -> bool {
    parse(src).is_ok()
}

/// Go's `json.Unmarshal(src, &v)` into a fresh `v`, as the module docs
/// describe: `Err` wherever Go returns an error.
pub fn from_slice<'a, T: Deserialize<'a>>(src: &'a [u8]) -> Result<T> {
    parse(src)?.decode()
}

/// [`from_slice`] for text.
pub fn from_str<'a, T: Deserialize<'a>>(src: &'a str) -> Result<T> {
    from_slice(src.as_bytes())
}

/// Go's `json.Unmarshal(src, &v)` into a fresh `v` when the caller keeps `v`
/// whatever the error: a member of the wrong type leaves its field at its
/// zero value and the rest are still decoded (an array element likewise),
/// and the error is the first such mismatch. A syntax error leaves the value
/// at its default.
pub fn from_slice_partial<'a, T: Deserialize<'a> + Default>(src: &'a [u8]) -> (T, Option<Error>) {
    match parse(src) {
        Ok(doc) => doc.decode_partial(),
        Err(e) => (T::default(), Some(e)),
    }
}

impl<'a> Document<'a> {
    fn ctx(&self, lenient: bool) -> Ctx<'a> {
        Ctx {
            src: self.src,
            text: self.text,
            lenient,
            first: RefCell::new(None),
        }
    }

    /// Decodes the value into a `T`, as [`from_slice`].
    pub fn decode<T: Deserialize<'a>>(&self) -> Result<T> {
        let ctx = self.ctx(false);
        T::deserialize(De::one(&self.root, &ctx, 0))
    }

    /// Decodes the value into a `T`, as [`from_slice_partial`].
    pub fn decode_partial<T: Deserialize<'a> + Default>(&self) -> (T, Option<Error>) {
        let ctx = self.ctx(true);
        match T::deserialize(De::one(&self.root, &ctx, 0)) {
            Ok(v) => (v, ctx.first.into_inner()),
            Err(e) => (T::default(), Some(e)),
        }
    }

    /// An object's members in order, duplicates included, as a
    /// `json.Decoder` reading tokens sees them: each name decoded, each value
    /// as its text (invalid UTF-8 in it made U+FFFD, as [`lossy`]). `None` if
    /// the value isn't an object.
    pub fn members(&self) -> Option<Vec<(Cow<'a, str>, Cow<'a, str>)>> {
        let ctx = self.ctx(false);
        let Kind::Object(members) = &self.root.kind else {
            return None;
        };
        Some(
            members
                .iter()
                .map(|(name, n)| (ctx.text_of(*name), ctx.raw_of(n)))
                .collect(),
        )
    }
}

// --- decoding ---------------------------------------------------------------

struct Ctx<'a> {
    src: &'a [u8],
    text: Option<&'a str>,
    /// Set for [`from_slice_partial`]: a mismatch is recorded, not returned.
    lenient: bool,
    first: RefCell<Option<Error>>,
}

impl<'a> Ctx<'a> {
    fn text_of(&self, t: Text) -> Cow<'a, str> {
        if t.escaped {
            return Cow::Owned(unescape(&self.src[t.start..t.end]));
        }
        match self.text {
            Some(s) => Cow::Borrowed(&s[t.start..t.end]),
            None => lossy(&self.src[t.start..t.end]),
        }
    }

    fn raw_of(&self, n: &Node) -> Cow<'a, str> {
        match self.text {
            Some(s) => Cow::Borrowed(&s[n.start..n.end]),
            None => lossy(&self.src[n.start..n.end]),
        }
    }

    fn number_of(&self, n: &Node) -> &'a str {
        // A number's bytes are ASCII.
        std::str::from_utf8(&self.src[n.start..n.end]).unwrap_or_default()
    }

    /// A value that isn't the type the field wants: Go's
    /// `UnmarshalTypeError`, returned, or recorded and passed over.
    fn mismatch(&self, n: &Node, exp: &dyn de::Expected) -> Result<()> {
        let e = <Error as de::Error>::invalid_type(unexpected(n), exp);
        if !self.lenient {
            return Err(e);
        }
        let mut first = self.first.borrow_mut();
        if first.is_none() {
            *first = Some(e);
        }
        Ok(())
    }
}

/// The values a field is decoded from: one, or one per time its name came.
enum Vals<'d> {
    One(&'d Node),
    Many(Vec<&'d Node>),
}

impl<'d> Vals<'d> {
    fn all(&self) -> &[&'d Node] {
        match self {
            Vals::One(n) => std::slice::from_ref(n),
            Vals::Many(v) => v,
        }
    }

    fn last(&self) -> &'d Node {
        match self {
            Vals::One(n) => n,
            Vals::Many(v) => v[v.len() - 1],
        }
    }

    fn push(&mut self, n: &'d Node) {
        match self {
            Vals::One(first) => *self = Vals::Many(vec![*first, n]),
            Vals::Many(v) => v.push(n),
        }
    }
}

struct De<'d, 'a> {
    vals: Vals<'d>,
    ctx: &'d Ctx<'a>,
    depth: usize,
}

impl<'d, 'a> De<'d, 'a> {
    fn one(n: &'d Node, ctx: &'d Ctx<'a>, depth: usize) -> Self {
        De {
            vals: Vals::One(n),
            ctx,
            depth,
        }
    }

    /// The value a string, number or bool field ends up with: the last one
    /// `ok` accepts, `null`s passed over; `None` leaves the zero value.
    fn pick(&self, ok: impl Fn(&Node) -> bool, exp: &dyn de::Expected) -> Result<Option<&'d Node>> {
        let mut won = None;
        for &n in self.vals.all() {
            if matches!(n.kind, Kind::Null) {
                continue;
            }
            if ok(n) {
                won = Some(n);
            } else {
                self.ctx.mismatch(n, exp)?;
            }
        }
        Ok(won)
    }

    /// Like `pick`, for a list: a `null` counts, and empties it.
    fn pick_or_null(
        &self,
        ok: impl Fn(&Node) -> bool,
        exp: &dyn de::Expected,
    ) -> Result<Option<&'d Node>> {
        let mut won = None;
        for &n in self.vals.all() {
            if matches!(n.kind, Kind::Null) || ok(n) {
                won = Some(n);
            } else {
                self.ctx.mismatch(n, exp)?;
            }
        }
        Ok(won)
    }

    fn deeper(&self) -> Result<usize> {
        if self.depth >= MAX_DECODE_DEPTH {
            return Err(error("exceeded max depth"));
        }
        Ok(self.depth + 1)
    }

    fn text_node(&self, n: &Node) -> Cow<'a, str> {
        match n.kind {
            Kind::String(t) => self.ctx.text_of(t),
            _ => Cow::Borrowed(""),
        }
    }

    fn int<T: std::str::FromStr>(&self, exp: &dyn de::Expected) -> Result<Option<T>> {
        let ctx = self.ctx;
        let n = self.pick(
            |n| matches!(n.kind, Kind::Number) && ctx.number_of(n).parse::<T>().is_ok(),
            exp,
        )?;
        Ok(n.and_then(|n| ctx.number_of(n).parse::<T>().ok()))
    }
}

fn visit_text<'a, V: Visitor<'a>>(s: Cow<'a, str>, v: V) -> Result<V::Value> {
    match s {
        Cow::Borrowed(b) => v.visit_borrowed_str(b),
        Cow::Owned(o) => v.visit_string(o),
    }
}

impl<'d, 'a> Deserializer<'a> for De<'d, 'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.vals.last();
        match &n.kind {
            Kind::Null => v.visit_unit(),
            Kind::Bool(b) => v.visit_bool(*b),
            Kind::Number => {
                let lit = self.ctx.number_of(n);
                if !lit.contains(['.', 'e', 'E']) {
                    match lit.strip_prefix('-') {
                        Some(abs) => {
                            if let Ok(u) = abs.parse::<u64>()
                                && u <= FLOAT64_EXACT
                                && u != 0
                            {
                                return v.visit_i64(-(u as i64));
                            }
                        }
                        None => {
                            if let Ok(u) = lit.parse::<u64>()
                                && u <= FLOAT64_EXACT
                            {
                                return v.visit_u64(u);
                            }
                        }
                    }
                }
                match lit.parse::<f64>() {
                    Ok(f) if f.is_finite() => v.visit_f64(f),
                    _ => {
                        self.ctx.mismatch(n, &v)?;
                        v.visit_unit()
                    }
                }
            }
            Kind::String(t) => visit_text(self.ctx.text_of(*t), v),
            Kind::Array(items) => v.visit_seq(SeqDe {
                it: items.iter(),
                ctx: self.ctx,
                depth: self.deeper()?,
            }),
            Kind::Object(members) => v.visit_map(MapDe {
                it: members
                    .iter()
                    .map(|(t, n)| (*t, n))
                    .collect::<Vec<_>>()
                    .into_iter(),
                ctx: self.ctx,
                depth: self.deeper()?,
                value: None,
            }),
        }
    }

    fn deserialize_bool<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.pick(|n| matches!(n.kind, Kind::Bool(_)), &v)?;
        match n.map(|n| &n.kind) {
            Some(Kind::Bool(b)) => v.visit_bool(*b),
            _ => v.visit_bool(false),
        }
    }

    fn deserialize_i8<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_i64(v)
    }

    fn deserialize_i16<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_i64(v)
    }

    fn deserialize_i32<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_i64(v)
    }

    fn deserialize_i64<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.int::<i64>(&v)?;
        v.visit_i64(n.unwrap_or(0))
    }

    fn deserialize_u8<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_u64(v)
    }

    fn deserialize_u16<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_u64(v)
    }

    fn deserialize_u32<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_u64(v)
    }

    fn deserialize_u64<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.int::<u64>(&v)?;
        v.visit_u64(n.unwrap_or(0))
    }

    fn deserialize_f32<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_f64(v)
    }

    fn deserialize_f64<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let ctx = self.ctx;
        let finite = |n: &Node| {
            matches!(n.kind, Kind::Number)
                && ctx.number_of(n).parse::<f64>().is_ok_and(f64::is_finite)
        };
        let n = self.pick(finite, &v)?;
        v.visit_f64(n.and_then(|n| ctx.number_of(n).parse().ok()).unwrap_or(0.0))
    }

    fn deserialize_char<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let ctx = self.ctx;
        let one = |n: &Node| match n.kind {
            Kind::String(t) => ctx.text_of(t).chars().count() == 1,
            _ => false,
        };
        let n = self.pick(one, &v)?;
        match n {
            Some(n) => v.visit_char(self.text_node(n).chars().next().unwrap_or_default()),
            None => v.visit_char('\0'),
        }
    }

    fn deserialize_str<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.pick(|n| matches!(n.kind, Kind::String(_)), &v)?;
        match n {
            Some(n) => visit_text(self.text_node(n), v),
            None => v.visit_borrowed_str(""),
        }
    }

    fn deserialize_string<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_str(v)
    }

    fn deserialize_bytes<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.pick(|n| matches!(n.kind, Kind::String(_)), &v)?;
        match n {
            Some(n) => v.visit_bytes(self.text_node(n).as_bytes()),
            None => v.visit_bytes(b""),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_bytes(v)
    }

    /// A Go pointer: `null` makes it nil, and the values after the last
    /// `null` fill one.
    fn deserialize_option<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let all = self.vals.all();
        match all.iter().rposition(|n| matches!(n.kind, Kind::Null)) {
            None => v.visit_some(self),
            Some(i) if i + 1 == all.len() => v.visit_none(),
            Some(i) => {
                let rest = all[i + 1..].to_vec();
                v.visit_some(De {
                    vals: Vals::Many(rest),
                    ctx: self.ctx,
                    depth: self.depth,
                })
            }
        }
    }

    fn deserialize_unit<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.vals.last();
        if !matches!(n.kind, Kind::Null) {
            self.ctx.mismatch(n, &v)?;
        }
        v.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value> {
        self.deserialize_unit(v)
    }

    fn deserialize_newtype_struct<V: Visitor<'a>>(
        self,
        name: &'static str,
        v: V,
    ) -> Result<V::Value> {
        if name == RAW_VALUE_TOKEN {
            // json.RawMessage: the last value's text, null included.
            return v.visit_map(RawDe {
                raw: Some(self.ctx.raw_of(self.vals.last())),
            });
        }
        v.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let n = self.pick_or_null(|n| matches!(n.kind, Kind::Array(_)), &v)?;
        let items: &'d [Node] = match n.map(|n| &n.kind) {
            Some(Kind::Array(items)) => items,
            _ => &[],
        };
        v.visit_seq(SeqDe {
            it: items.iter(),
            ctx: self.ctx,
            depth: self.deeper()?,
        })
    }

    fn deserialize_tuple<V: Visitor<'a>>(self, _len: usize, v: V) -> Result<V::Value> {
        self.deserialize_seq(v)
    }

    fn deserialize_tuple_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        _len: usize,
        v: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(v)
    }

    /// A Go map: objects that come twice merge, and a `null` empties it.
    fn deserialize_map<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        let mut members: Vec<(Text, &'d Node)> = Vec::new();
        for &n in self.vals.all() {
            match &n.kind {
                Kind::Null => members.clear(),
                Kind::Object(ms) => members.extend(ms.iter().map(|(t, n)| (*t, n))),
                _ => self.ctx.mismatch(n, &v)?,
            }
        }
        v.visit_map(MapDe {
            it: members.into_iter(),
            ctx: self.ctx,
            depth: self.deeper()?,
            value: None,
        })
    }

    /// A Go struct: each member goes to the field its name matches (exactly,
    /// else ignoring case), and a field named more than once gets every value.
    fn deserialize_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        v: V,
    ) -> Result<V::Value> {
        let mut slots: Vec<(usize, Vals<'d>)> = Vec::new();
        for &n in self.vals.all() {
            match &n.kind {
                Kind::Null => {}
                Kind::Object(members) => {
                    for (t, val) in members {
                        let name = self.ctx.text_of(*t);
                        let field = fields
                            .iter()
                            .position(|f| *f == name)
                            .or_else(|| fields.iter().position(|f| equal_fold(f, &name)));
                        let Some(i) = field else {
                            continue;
                        };
                        match slots.iter_mut().find(|(j, _)| *j == i) {
                            Some((_, vals)) => vals.push(val),
                            None => slots.push((i, Vals::One(val))),
                        }
                    }
                }
                _ => self.ctx.mismatch(n, &v)?,
            }
        }
        v.visit_map(StructDe {
            it: slots.into_iter(),
            fields,
            ctx: self.ctx,
            depth: self.deeper()?,
            value: None,
        })
    }

    fn deserialize_enum<V: Visitor<'a>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        v: V,
    ) -> Result<V::Value> {
        let n = self.vals.last();
        match n.kind {
            Kind::String(t) => match self.ctx.text_of(t) {
                Cow::Borrowed(s) => v.visit_enum(s.into_deserializer()),
                Cow::Owned(s) => v.visit_enum(s.into_deserializer()),
            },
            _ => Err(<Error as de::Error>::invalid_type(unexpected(n), &v)),
        }
    }

    fn deserialize_identifier<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        self.deserialize_str(v)
    }

    fn deserialize_ignored_any<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        v.visit_unit()
    }
}

struct SeqDe<'d, 'a> {
    it: std::slice::Iter<'d, Node>,
    ctx: &'d Ctx<'a>,
    depth: usize,
}

impl<'d, 'a> SeqAccess<'a> for SeqDe<'d, 'a> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'a>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        match self.it.next() {
            None => Ok(None),
            Some(n) => seed.deserialize(De::one(n, self.ctx, self.depth)).map(Some),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.it.len())
    }
}

struct MapDe<'d, 'a> {
    it: std::vec::IntoIter<(Text, &'d Node)>,
    ctx: &'d Ctx<'a>,
    depth: usize,
    value: Option<&'d Node>,
}

impl<'d, 'a> MapAccess<'a> for MapDe<'d, 'a> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'a>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        let Some((t, n)) = self.it.next() else {
            return Ok(None);
        };
        self.value = Some(n);
        seed.deserialize(NameDe(self.ctx.text_of(t))).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'a>>(&mut self, seed: V) -> Result<V::Value> {
        let n = self
            .value
            .take()
            .ok_or_else(|| error("a map value was read twice"))?;
        seed.deserialize(De::one(n, self.ctx, self.depth))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.it.len())
    }
}

struct StructDe<'d, 'a> {
    it: std::vec::IntoIter<(usize, Vals<'d>)>,
    fields: &'static [&'static str],
    ctx: &'d Ctx<'a>,
    depth: usize,
    value: Option<Vals<'d>>,
}

impl<'d, 'a> MapAccess<'a> for StructDe<'d, 'a> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'a>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        let Some((i, vals)) = self.it.next() else {
            return Ok(None);
        };
        self.value = Some(vals);
        seed.deserialize(BorrowedStrDeserializer::<Error>::new(self.fields[i]))
            .map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'a>>(&mut self, seed: V) -> Result<V::Value> {
        let vals = self
            .value
            .take()
            .ok_or_else(|| error("a field was read twice"))?;
        seed.deserialize(De {
            vals,
            ctx: self.ctx,
            depth: self.depth,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.it.len())
    }
}

/// A map key: a member's name.
struct NameDe<'a>(Cow<'a, str>);

impl<'a> Deserializer<'a> for NameDe<'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'a>>(self, v: V) -> Result<V::Value> {
        visit_text(self.0, v)
    }

    serde::forward_to_deserialize_any! {
        <W: Visitor<'a>>
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// A raw value as serde_json's `RawValue` takes one.
struct RawDe<'a> {
    raw: Option<Cow<'a, str>>,
}

impl<'a> MapAccess<'a> for RawDe<'a> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'a>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.raw.is_none() {
            return Ok(None);
        }
        seed.deserialize(BorrowedStrDeserializer::<Error>::new(RAW_VALUE_TOKEN))
            .map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'a>>(&mut self, seed: V) -> Result<V::Value> {
        match self.raw.take() {
            Some(Cow::Borrowed(s)) => seed.deserialize(BorrowedStrDeserializer::<Error>::new(s)),
            Some(Cow::Owned(s)) => seed.deserialize(StringDeserializer::<Error>::new(s)),
            None => Err(error("the raw value was read twice")),
        }
    }
}

// --- field attributes for serde_json ----------------------------------------

/// `#[serde(default, deserialize_with = "null_as_zero")]`: `null` leaves the
/// field at its zero value (`T::default()`), as Go leaves a string, number,
/// bool, slice, map or struct field a `null` is decoded into.
pub fn null_as_zero<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// `#[serde(default, deserialize_with = "object_or_null")]` on a struct
/// field: an object fills it and `null` leaves its default, as in Go; any
/// other value, an array included, is an error (serde would take an array
/// for a struct).
pub fn object_or_null<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(optional_object(d)?.unwrap_or_default())
}

/// `#[serde(default, deserialize_with = "optional_object")]` on an
/// `Option` of a struct (a Go pointer to one): `null` is `None`, an object
/// `Some`, anything else an error.
pub fn optional_object<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<ObjectOnly<T>>::deserialize(d)?.map(|o| o.0))
}

struct ObjectOnly<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObjectOnly<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        T::deserialize(StructAsMap(d)).map(ObjectOnly)
    }
}

/// Hands a struct's visitor a map only.
struct StructAsMap<D>(D);

macro_rules! forward {
    ($($method:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            fn $method<V: Visitor<'de>>(self, $($arg: $ty,)* v: V) -> std::result::Result<V::Value, D::Error> {
                self.0.$method($($arg,)* v)
            }
        )*
    };
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for StructAsMap<D> {
    type Error = D::Error;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        v: V,
    ) -> std::result::Result<V::Value, D::Error> {
        self.0.deserialize_map(v)
    }

    forward! {
        deserialize_any();
        deserialize_bool();
        deserialize_i8();
        deserialize_i16();
        deserialize_i32();
        deserialize_i64();
        deserialize_u8();
        deserialize_u16();
        deserialize_u32();
        deserialize_u64();
        deserialize_f32();
        deserialize_f64();
        deserialize_char();
        deserialize_str();
        deserialize_string();
        deserialize_bytes();
        deserialize_byte_buf();
        deserialize_option();
        deserialize_unit();
        deserialize_unit_struct(name: &'static str);
        deserialize_newtype_struct(name: &'static str);
        deserialize_seq();
        deserialize_tuple(len: usize);
        deserialize_tuple_struct(name: &'static str, len: usize);
        deserialize_map();
        deserialize_enum(name: &'static str, variants: &'static [&'static str]);
        deserialize_identifier();
        deserialize_ignored_any();
    }
}

// --- encoding ---------------------------------------------------------------

/// A serde_json formatter that writes what Go's encoder writes: U+2028 and
/// U+2029 escaped in every string and raw value, `<`, `>` and `&` too when
/// `escape_html` is set, and floats as Go formats a float64. Everything else
/// serde_json already writes as Go does (`\b`, `\f`, `\n`, `\r`, `\t`, the
/// other controls as lowercase backslash-u escapes, `/` and DEL as they are).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoFormatter {
    pub escape_html: bool,
}

impl GoFormatter {
    pub fn new(escape_html: bool) -> Self {
        GoFormatter { escape_html }
    }
}

impl serde_json::ser::Formatter for GoFormatter {
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        write_escaped(w, fragment, self.escape_html, false)
    }

    /// Go's `appendCompact` on a `json.RawMessage`: the whitespace between
    /// tokens dropped, and the same characters escaped wherever they are
    /// (they can only be inside strings).
    fn write_raw_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        write_escaped(w, fragment, self.escape_html, true)
    }

    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(format_float(value).as_bytes())
    }

    fn write_f32<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f32) -> io::Result<()> {
        w.write_all(format_float32(value).as_bytes())
    }
}

/// Writes `s` with Go's escapes; `compact` (for JSON text, not a string's
/// contents) also drops the whitespace outside strings.
fn write_escaped<W: ?Sized + io::Write>(
    w: &mut W,
    s: &str,
    html: bool,
    compact: bool,
) -> io::Result<()> {
    let b = s.as_bytes();
    let mut start = 0;
    let (mut in_string, mut escaped) = (false, false);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if compact {
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_string = false;
                }
            } else if c == b'"' {
                in_string = true;
            } else if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                w.write_all(&b[start..i])?;
                i += 1;
                start = i;
                continue;
            }
        }
        let (esc, len): (&[u8], usize) = match c {
            b'<' if html => (b"\\u003c", 1),
            b'>' if html => (b"\\u003e", 1),
            b'&' if html => (b"\\u0026", 1),
            0xE2 if b.get(i + 1) == Some(&0x80) && b.get(i + 2) == Some(&0xA8) => (b"\\u2028", 3),
            0xE2 if b.get(i + 1) == Some(&0x80) && b.get(i + 2) == Some(&0xA9) => (b"\\u2029", 3),
            _ => {
                i += 1;
                continue;
            }
        };
        w.write_all(&b[start..i])?;
        w.write_all(esc)?;
        i += len;
        start = i;
    }
    w.write_all(&b[start..])
}

/// Go's escaping of JSON text already encoded: `json.HTMLEscape` when
/// `escape_html` is set (`<`, `>`, `&`, U+2028, U+2029), U+2028 and U+2029
/// only when it isn't. They can only occur inside strings, so this is what
/// Go's encoder would have written.
pub fn escape(json: &str, escape_html: bool) -> Cow<'_, str> {
    let needs =
        json.contains(['\u{2028}', '\u{2029}']) || (escape_html && json.contains(['<', '>', '&']));
    if !needs {
        return Cow::Borrowed(json);
    }
    let mut out = Vec::with_capacity(json.len() + 16);
    // Writing to a Vec doesn't fail.
    let _ = write_escaped(&mut out, json, escape_html, false);
    Cow::Owned(String::from_utf8(out).unwrap_or_default())
}

/// Go's encoding of a float64: the shortest text that reads back as the same
/// number, in `%f` form, or `%e` form (`1e+21`, `1e-7`) below 1e-6 or from
/// 1e21 up.
pub fn format_float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        go_exponent(format!("{f:e}"))
    } else {
        format!("{f}")
    }
}

/// [`format_float`] for a float32.
pub fn format_float32(f: f32) -> String {
    let abs = f.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        go_exponent(format!("{f:e}"))
    } else {
        format!("{f}")
    }
}

/// Rust's `1e21` is Go's `1e+21`; a negative exponent reads the same (Go
/// trims `e-07` to `e-7`).
fn go_exponent(s: String) -> String {
    match s.split_once('e') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
        _ => s,
    }
}

/// Writes `v` as Go's encoder does, HTML escaping on or off.
pub fn to_writer<W: io::Write, T: ?Sized + Serialize>(
    w: W,
    v: &T,
    escape_html: bool,
) -> Result<()> {
    let mut ser = serde_json::Serializer::with_formatter(w, GoFormatter::new(escape_html));
    v.serialize(&mut ser)
}

/// [`to_writer`] into bytes.
pub fn to_vec<T: ?Sized + Serialize>(v: &T, escape_html: bool) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(128);
    to_writer(&mut out, v, escape_html)?;
    Ok(out)
}

/// [`to_writer`] into text.
pub fn to_string<T: ?Sized + Serialize>(v: &T, escape_html: bool) -> Result<String> {
    String::from_utf8(to_vec(v, escape_html)?).map_err(error)
}

/// Go's `json.Marshal(v)`: compact, HTML escaping on.
pub fn marshal<T: ?Sized + Serialize>(v: &T) -> Result<Vec<u8>> {
    to_vec(v, true)
}

/// Go's `json.Encoder`: each value compact, then a newline. HTML escaping
/// starts on, as in Go.
#[derive(Debug)]
pub struct Encoder<W> {
    w: W,
    escape_html: bool,
}

impl<W: io::Write> Encoder<W> {
    pub fn new(w: W) -> Self {
        Encoder {
            w,
            escape_html: true,
        }
    }

    /// Go's `SetEscapeHTML`.
    pub fn set_escape_html(&mut self, on: bool) {
        self.escape_html = on;
    }

    /// Go's `Encode`: the whole line is written at once, or nothing if `v`
    /// can't be encoded.
    pub fn encode<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<()> {
        let mut line = to_vec(v, self.escape_html)?;
        line.push(b'\n');
        self.w.write_all(&line).map_err(Error::io)
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.w
    }

    pub fn into_inner(self) -> W {
        self.w
    }
}

#[cfg(test)]
mod tests {
    //! Every expectation here is what Go 1.27's `encoding/json` gave for the
    //! same input.

    use std::collections::BTreeMap;

    use serde::Deserialize;
    use serde_json::value::RawValue;

    use super::*;

    /// `~` stands for U+FFFD.
    fn r(s: &str) -> String {
        s.replace('~', &char::REPLACEMENT_CHARACTER.to_string())
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default)]
    struct Pair {
        #[serde(rename = "A")]
        a: String,
        #[serde(rename = "B")]
        b: String,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(default)]
    struct S {
        s: String,
        stop_reason: String,
        n: i64,
        f: f64,
        b: bool,
        l: Vec<String>,
        m: BTreeMap<String, i64>,
        o: Pair,
        p: Option<Pair>,
        #[serde(deserialize_with = "raw")]
        r: Option<Box<RawValue>>,
        i: Option<serde_json::Value>,
    }

    fn raw<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Option<Box<RawValue>>, D::Error> {
        Box::<RawValue>::deserialize(d).map(Some)
    }

    fn s(src: &[u8]) -> S {
        from_slice(src).unwrap_or_else(|e| panic!("{}: {e}", String::from_utf8_lossy(src)))
    }

    fn fails(src: &[u8]) {
        assert!(
            from_slice::<S>(src).is_err(),
            "{} decoded",
            String::from_utf8_lossy(src)
        );
    }

    #[test]
    fn invalid_utf8_is_one_replacement_per_byte() {
        let v = s(b"{\"s\":\"a\xff\xfeb\xe2\x82c\xed\xa0\x80d\xf0\x9f\x98\"}");
        assert_eq!(v.s, r("a~~b~~c~~~d~~~"));
        assert_eq!(lossy(b"a\xe2\x82b"), r("a~~b"));
        assert_eq!(lossy(b"\xf0\x9f\x98"), r("~~~"));
        assert_eq!(lossy(b"ok"), "ok");
    }

    #[test]
    fn lone_surrogates_are_replacements() {
        for (src, want) in [
            (&b"{\"s\":\"\\ud83d\"}"[..], r("~")),
            (b"{\"s\":\"\\ud83dx\"}", r("~x")),
            (b"{\"s\":\"\\udc00\"}", r("~")),
            (b"{\"s\":\"\\ud800\\ud800\\udc00\"}", r("~\u{10000}")),
            (b"{\"s\":\"\\ud83d\\u0041\"}", r("~A")),
            (b"{\"s\":\"\\ud83d\\ude00\"}", "\u{1F600}".to_string()),
            (
                b"{\"s\":\"\\\"\\\\\\/\\b\\f\\n\\r\\t\"}",
                "\"\\/\x08\x0c\n\r\t".to_string(),
            ),
        ] {
            assert_eq!(s(src).s, want, "{}", String::from_utf8_lossy(src));
        }
    }

    #[test]
    fn names_match_exactly_then_ignoring_case_and_the_last_wins() {
        for (src, want) in [
            (&b"{\"s\":\"a\",\"s\":\"b\"}"[..], "b"),
            (b"{\"s\":\"a\",\"s\":null}", "a"),
            (b"{\"S\":\"x\"}", "x"),
            (b"{\"s\":\"a\",\"S\":\"b\"}", "b"),
            (b"{\"S\":\"b\",\"s\":\"a\"}", "a"),
            (b"{\"S\":\"b\",\"s\":\"a\",\"S\":\"c\"}", "c"),
            (b"{\"s\":\"a\",\"S\":null}", "a"),
            ("{\"\u{17f}\":\"long s\"}".as_bytes(), "long s"),
        ] {
            assert_eq!(s(src).s, want, "{}", String::from_utf8_lossy(src));
        }
        assert_eq!(s(b"{\"STOP_REASON\":\"x\"}").stop_reason, "x");
        assert_eq!(s(b"{\"Stop_Reason\":\"x\"}").stop_reason, "x");
        assert_eq!(s(b"{\"stop-reason\":\"x\"}").stop_reason, "");
        assert_eq!(s(b"{\"stopreason\":\"x\"}").stop_reason, "");
    }

    #[test]
    fn duplicates_merge_structs_and_maps_and_replace_lists() {
        let v = s(b"{\"o\":{\"A\":\"1\"},\"o\":{\"B\":\"2\"}}");
        assert_eq!(
            v.o,
            Pair {
                a: "1".into(),
                b: "2".into()
            }
        );
        let v = s(b"{\"m\":{\"a\":1},\"m\":{\"b\":2}}");
        assert_eq!(
            v.m,
            BTreeMap::from([("a".to_string(), 1), ("b".to_string(), 2)])
        );
        let v = s(b"{\"m\":{\"a\":1},\"m\":null,\"m\":{\"b\":2}}");
        assert_eq!(v.m, BTreeMap::from([("b".to_string(), 2)]));
        assert_eq!(s(b"{\"l\":[\"a\"],\"l\":[\"b\",\"c\"]}").l, ["b", "c"]);
        assert!(s(b"{\"l\":[\"a\"],\"l\":null}").l.is_empty());
        let v = s(b"{\"p\":{\"A\":\"1\"},\"p\":{\"B\":\"2\"}}");
        assert_eq!(
            v.p,
            Some(Pair {
                a: "1".into(),
                b: "2".into()
            })
        );
        assert_eq!(s(b"{\"p\":{\"A\":\"1\"},\"p\":null}").p, None);
        let v = s(b"{\"i\":{\"a\":[1,2.5,1e21,\"x\",null,true]},\"i\":7}");
        assert_eq!(v.i, Some(serde_json::json!(7)));
    }

    #[test]
    fn null_is_the_zero_value() {
        let v = s(b"{\"n\":null,\"f\":null,\"b\":null,\"l\":null,\"m\":null,\"o\":null,\"r\":null,\"i\":null,\"s\":null,\"p\":null}");
        assert_eq!((v.n, v.f, v.b, v.s.as_str()), (0, 0.0, false, ""));
        assert!(v.l.is_empty() && v.m.is_empty() && v.p.is_none() && v.i.is_none());
        assert_eq!(v.o, Pair::default());
        assert_eq!(v.r.map(|r| r.get().to_string()).as_deref(), Some("null"));
        let top: S = from_slice(b"null").unwrap();
        assert_eq!(top.s, "");
    }

    #[test]
    fn mismatched_types_fail_and_partial_keeps_the_rest() {
        for src in [
            &b"{\"p\":[]}"[..],
            b"{\"o\":[]}",
            b"{\"o\":5,\"s\":\"x\"}",
            b"{\"n\":1e2}",
            b"{\"n\":1.0}",
            b"{\"f\":1e400}",
            b"{\"n\":18446744073709551615}",
            b"{\"n\":7,\"s\":5,\"b\":true}",
            b"{\"l\":[\"a\",5,\"b\"]}",
            b"[]",
            b"\"x\"",
        ] {
            fails(src);
        }
        assert_eq!(s(b"{\"n\":-0}").n, 0);

        let (v, err) = from_slice_partial::<S>(b"{\"p\":7,\"s\":\"after\"}");
        assert!(err.is_some());
        assert_eq!((v.s.as_str(), v.p), ("after", Some(Pair::default())));
        let (v, err) = from_slice_partial::<S>(b"{\"n\":7,\"s\":5,\"b\":true}");
        assert!(err.is_some());
        assert_eq!((v.n, v.s.as_str(), v.b), (7, "", true));
        let (v, _) = from_slice_partial::<S>(b"{\"l\":[\"a\",5,\"b\"]}");
        assert_eq!(v.l, ["a", "", "b"]);
        let (v, err) = from_slice_partial::<S>(b"{\"s\":\"x\",\"s\":5}");
        assert!(err.is_some());
        assert_eq!(v.s, "x");
        let (v, err) = from_slice_partial::<S>(b"{\"s\":\"x\"");
        assert!(err.is_some());
        assert_eq!(v.s, "");
    }

    #[test]
    fn syntax_is_go_s() {
        for src in [
            &b"{\"s\":\"a\"} x"[..],
            b"{\"s\":\"\\x\"}",
            b"{\"s\":\"a\tb\"}",
            b"{\"s\":\"a\",}",
            b" {\"s\":\"a\"}\x0c",
            b"{\"s\":\"a\"}\x0b",
            b"{\"s\":\"\\u12\"}",
            b"",
            b"01",
            b"-",
            b"1.",
            b"1e",
            b"tru",
            b"{\"s\" \"a\"}",
        ] {
            assert!(!valid(src), "{} is valid", String::from_utf8_lossy(src));
        }
        for src in [
            &b" {\"s\":\"a\"}\r\n\t "[..],
            b"-0.5e+10",
            b"[]",
            b"{}",
            b"\"\xff\"",
        ] {
            assert!(valid(src), "{} is invalid", String::from_utf8_lossy(src));
        }
    }

    #[test]
    fn nesting_goes_10000_deep_without_recursing() {
        let deep = |n: usize| {
            let mut v = "[".repeat(n).into_bytes();
            v.extend("]".repeat(n).into_bytes());
            v
        };
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                assert!(valid(&deep(10000)));
                assert!(!valid(&deep(10001)));
                // A typed decode stops at its own limit instead of overflowing.
                assert!(from_slice::<serde_json::Value>(&deep(10000)).is_err());
                assert!(from_slice::<serde_json::Value>(&deep(499)).is_ok());
                assert!(from_slice::<serde_json::Value>(&deep(501)).is_err());
                let raw: Box<RawValue> = from_slice(&deep(10000)).unwrap();
                assert_eq!(raw.get().len(), 20000);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn raw_values_keep_their_text() {
        let v = s(b"{\"r\":\"\\ud83d\"}");
        assert_eq!(v.r.unwrap().get(), "\"\\ud83d\"");
        let v = s(b"{\"r\": [1, {\"a\" : 2}] }");
        assert_eq!(v.r.unwrap().get(), "[1, {\"a\" : 2}]");
        let v = s(b"{\"r\":\"\xff\"}");
        assert_eq!(v.r.unwrap().get(), r("\"~\""));
    }

    #[test]
    fn numbers_decoded_into_any_are_go_s_float64() {
        let v: serde_json::Value = from_slice(b"[1,-2,9007199254740993,1.5,-0]").unwrap();
        assert_eq!(
            to_string(&v, false).unwrap(),
            "[1,-2,9007199254740992,1.5,-0]"
        );
    }

    #[test]
    fn members_lists_every_member_in_order() {
        let doc = parse(b"{\"a\":1,\"b\" : {\"x\": 2},\"a\":\"\\u003c\"}").unwrap();
        let got: Vec<(String, String)> = doc
            .members()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            got,
            [
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "{\"x\": 2}".to_string()),
                ("a".to_string(), "\"\\u003c\"".to_string()),
            ]
        );
        assert!(parse(b"[1]").unwrap().members().is_none());
    }

    #[test]
    fn a_document_decodes_into_several_types() {
        #[derive(Deserialize)]
        struct One {
            #[serde(default)]
            s: String,
        }
        #[derive(Deserialize)]
        struct Two {
            #[serde(default)]
            n: i64,
        }
        let doc = parse(b"{\"s\":\"x\",\"n\":3}").unwrap();
        assert_eq!(doc.decode::<One>().unwrap().s, "x");
        assert_eq!(doc.decode::<Two>().unwrap().n, 3);
    }

    #[test]
    fn encoding_escapes_as_go_does() {
        let s = "x\u{2028}<&>\u{2029}\x08\x0c\x7f\x01\x1f";
        let mut enc = Encoder::new(Vec::new());
        enc.set_escape_html(false);
        enc.encode(s).unwrap();
        assert_eq!(
            String::from_utf8(enc.into_inner()).unwrap(),
            "\"x\\u2028<&>\\u2029\\b\\f\x7f\\u0001\\u001f\"\n"
        );
        assert_eq!(
            String::from_utf8(marshal(s).unwrap()).unwrap(),
            "\"x\\u2028\\u003c\\u0026\\u003e\\u2029\\b\\f\x7f\\u0001\\u001f\""
        );
        let raw =
            RawValue::from_string("{\"a\": \"x\u{2028}<&>\u{2029} y\" }".to_string()).unwrap();
        assert_eq!(
            to_string(&raw, false).unwrap(),
            "{\"a\":\"x\\u2028<&>\\u2029 y\"}"
        );
        assert_eq!(
            to_string(&raw, true).unwrap(),
            "{\"a\":\"x\\u2028\\u003c\\u0026\\u003e\\u2029 y\"}"
        );
        let keyed = BTreeMap::from([("<k>", 1)]);
        assert_eq!(to_string(&keyed, true).unwrap(), "{\"\\u003ck\\u003e\":1}");
        assert_eq!(escape("\"a<\u{2028}\"", false), "\"a<\\u2028\"");
        assert_eq!(escape("\"a<\u{2028}\"", true), "\"a\\u003c\\u2028\"");
        assert_eq!(escape("plain", true), "plain");
    }

    #[test]
    fn floats_are_go_s() {
        let fs = [
            1e21,
            1e20,
            1e-7,
            1e-6,
            0.1,
            123456789.0,
            1.5e-10,
            2.5e300,
            100.0,
            -0.0,
            -1e-7,
        ];
        assert_eq!(
            to_string(&fs, false).unwrap(),
            "[1e+21,100000000000000000000,1e-7,0.000001,0.1,123456789,1.5e-10,2.5e+300,100,-0,-1e-7]"
        );
        assert_eq!(format_float32(1e21), "1e+21");
        assert_eq!(format_float32(0.5), "0.5");
    }

    #[test]
    fn field_attributes_give_serde_json_go_s_rules() {
        #[derive(Debug, Default, Deserialize)]
        struct Inner {
            #[serde(default)]
            k: String,
        }
        #[derive(Debug, Deserialize)]
        struct Outer {
            #[serde(default, deserialize_with = "null_as_zero")]
            s: String,
            #[serde(default, deserialize_with = "null_as_zero")]
            l: Vec<i64>,
            #[serde(default, deserialize_with = "object_or_null")]
            o: Inner,
            #[serde(default, deserialize_with = "optional_object")]
            p: Option<Inner>,
        }
        let v: Outer =
            serde_json::from_str("{\"s\":null,\"l\":null,\"o\":null,\"p\":null}").unwrap();
        assert_eq!(
            (v.s.as_str(), v.l.len(), v.o.k.as_str(), v.p.is_none()),
            ("", 0, "", true)
        );
        let v: Outer = serde_json::from_str("{\"o\":{\"k\":\"a\"},\"p\":{\"k\":\"b\"}}").unwrap();
        assert_eq!((v.o.k.as_str(), v.p.unwrap().k.as_str()), ("a", "b"));
        assert!(serde_json::from_str::<Outer>("{\"o\":[]}").is_err());
        assert!(serde_json::from_str::<Outer>("{\"p\":[\"x\"]}").is_err());
        // serde on its own takes an array for a struct; Go doesn't.
        assert!(serde_json::from_str::<Inner>("[]").is_ok());
        assert!(from_slice::<Inner>(b"[]").is_err());
    }
}

//! What a crawl reads out of a file, with no model (docs/INDEX.md, "Seeds" and
//! "The crawl"): its seed summary (a module doc, a package comment, a
//! docstring, a doc's first paragraph), symbols by regular expression with
//! their doc comments, Markdown sections, and a doc's backticked references.

use std::sync::LazyLock;

use regex::Regex;

use crate::names;

/// What one file holds.
#[derive(Debug, Default, Clone)]
pub struct Extracted {
    /// A doc's first heading.
    pub title: Option<String>,
    /// The file's seed summary.
    pub seed: Option<String>,
    pub symbols: Vec<Symbol>,
    pub sections: Vec<Section>,
    /// A doc's backticked references, with their lines.
    pub refs: Vec<(String, usize)>,
    pub lines: usize,
}

/// One definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    pub kind: String,
    pub name: String,
    pub line: usize,
    pub signature: String,
    /// Its doc comment's first sentence ("" for none).
    pub doc: String,
}

/// One Markdown heading.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub level: usize,
    pub heading: String,
    pub slug: String,
    pub line: usize,
    /// The first paragraph under it.
    pub seed: String,
}

/// The longest seed kept, in characters.
pub const SEED_MAX: usize = 400;

/// At most `max` chars, cut at a word and ended with "…" when cut. Leading
/// space (an indent) is kept.
pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim_end();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    let cut = match cut.rfind(' ') {
        Some(i) if i > max / 2 => cut[..i].to_string(),
        _ => cut,
    };
    format!("{}…", cut.trim_end_matches([',', ';', ':', ' ']))
}

/// The first sentence of a text: up to the first `.`, `!` or `?` followed by a
/// space or the end.
pub fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'!' | b'?') {
            let next = bytes.get(i + 1);
            if next.is_none() || next.is_some_and(|n| n.is_ascii_whitespace()) {
                // Not an abbreviation like "e.g." or a version "v1.2".
                let word_start = s[..i].rfind(' ').map(|j| j + 1).unwrap_or(0);
                let word = &s[word_start..i];
                if matches!(word, "e.g" | "i.e" | "etc" | "vs" | "cf") {
                    continue;
                }
                return s[..=i].to_string();
            }
        }
    }
    s.to_string()
}

/// Joins comment lines into prose: the first paragraph, whitespace collapsed.
fn paragraph(lines: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for l in lines {
        let t = l.trim();
        if t.is_empty() {
            if out.is_empty() {
                continue;
            }
            break;
        }
        // A heading inside a doc comment (`# Examples`) ends the summary part.
        if t.starts_with("# ") && !out.is_empty() {
            break;
        }
        out.push(t.trim_start_matches("# "));
    }
    out.join(" ")
}

fn strip_comment(l: &str) -> String {
    let t = l.trim();
    for p in ["///", "//!", "//", "/**", "/*", "*/", "*", "#!", "##", "#"] {
        if let Some(rest) = t.strip_prefix(p) {
            return rest.trim_end_matches("*/").trim().to_string();
        }
    }
    t.trim_end_matches("*/").trim().to_string()
}

/// Reads a file's text: `rel` gives its name and so its language.
pub fn extract(rel: &str, lang: &str, text: &str, symbols: bool) -> Extracted {
    let lines: Vec<&str> = text.lines().collect();
    let mut x = Extracted {
        lines: lines.len(),
        ..Default::default()
    };
    match lang {
        "markdown" => markdown(&lines, &mut x),
        "rust" => x.seed = rust_module_doc(&lines),
        "go" => x.seed = go_package_doc(&lines),
        "python" => x.seed = python_docstring(&lines),
        "javascript" | "typescript" => x.seed = leading_block(&lines),
        "shell" => x.seed = leading_hash(&lines),
        _ => {}
    }
    if symbols {
        x.symbols = find_symbols(lang, &lines);
    }
    let _ = rel;
    x.seed = x.seed.filter(|s| !s.is_empty()).map(|s| clip(&s, SEED_MAX));
    x
}

fn rust_module_doc(lines: &[&str]) -> Option<String> {
    let mut doc = Vec::new();
    for l in lines {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("//!") {
            doc.push(rest.trim().to_string());
        } else if t.is_empty() && doc.is_empty() || t.starts_with("#![") {
            continue;
        } else if !doc.is_empty() || !t.is_empty() {
            break;
        }
    }
    Some(paragraph(&doc))
}

fn go_package_doc(lines: &[&str]) -> Option<String> {
    let pkg = lines.iter().position(|l| l.starts_with("package "))?;
    let mut doc = Vec::new();
    let mut i = pkg;
    while i > 0 && lines[i - 1].trim_start().starts_with("//") {
        i -= 1;
    }
    for l in &lines[i..pkg] {
        if l.trim_start().starts_with("//go:") {
            continue;
        }
        doc.push(strip_comment(l));
    }
    Some(paragraph(&doc))
}

fn python_docstring(lines: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        break;
    }
    docstring_at(lines, i)
}

/// A Python docstring starting on line `i`, if one does.
fn docstring_at(lines: &[&str], i: usize) -> Option<String> {
    let t = lines.get(i)?.trim();
    let t = t.trim_start_matches(['r', 'u', 'b', 'R', 'U', 'B']);
    let q = if t.starts_with("\"\"\"") {
        "\"\"\""
    } else if t.starts_with("'''") {
        "'''"
    } else {
        return None;
    };
    let first = &t[3..];
    if let Some(end) = first.find(q) {
        return Some(first[..end].trim().to_string());
    }
    let mut doc = vec![first.to_string()];
    for l in &lines[i + 1..] {
        if let Some(end) = l.find(q) {
            doc.push(l[..end].to_string());
            break;
        }
        doc.push(l.to_string());
    }
    Some(paragraph(&doc))
}

fn leading_block(lines: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < lines.len() && (lines[i].trim().is_empty() || lines[i].starts_with("#!")) {
        i += 1;
    }
    let t = lines.get(i)?.trim();
    let mut doc = Vec::new();
    if t.starts_with("/*") {
        for l in &lines[i..] {
            let end = l.contains("*/");
            doc.push(strip_comment(l));
            if end {
                break;
            }
        }
    } else if t.starts_with("//") {
        for l in &lines[i..] {
            if !l.trim_start().starts_with("//") {
                break;
            }
            doc.push(strip_comment(l));
        }
    }
    let p = paragraph(&doc);
    // A license header isn't what a file is.
    if p.to_ascii_lowercase().contains("copyright") || p.to_ascii_lowercase().contains("license") {
        return None;
    }
    Some(p)
}

fn leading_hash(lines: &[&str]) -> Option<String> {
    let mut doc = Vec::new();
    for l in lines {
        let t = l.trim();
        if t.starts_with("#!") || (t.is_empty() && doc.is_empty()) {
            continue;
        }
        if let Some(rest) = t.strip_prefix('#') {
            doc.push(rest.trim().to_string());
        } else {
            break;
        }
    }
    let p = paragraph(&doc);
    if p.starts_with("shellcheck") || p.starts_with("-*-") {
        return None;
    }
    Some(p)
}

static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(```|~~~)").unwrap());
static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+?)\s*#*\s*$").unwrap());

/// A heading's text as people read it: links and code marks dropped.
fn heading_text(h: &str) -> String {
    static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap());
    LINK.replace_all(h, "$1")
        .replace(['`', '*'], "")
        .trim()
        .to_string()
}

fn markdown(lines: &[&str], x: &mut Extracted) {
    let mut in_fence = false;
    let mut slugs: std::collections::HashMap<String, usize> = Default::default();
    // (index into x.sections, collecting its paragraph?)
    let mut para: Vec<String> = Vec::new();
    let mut collecting: Option<usize> = None; // None: the doc's own first paragraph
    let mut doc_para_done = false;
    let mut doc_para: Vec<String> = Vec::new();

    let flush = |para: &mut Vec<String>,
                 collecting: Option<usize>,
                 x: &mut Extracted,
                 doc_para: &mut Vec<String>,
                 doc_para_done: &mut bool| {
        if para.is_empty() {
            return;
        }
        let text = para.join(" ");
        if let Some(i) = collecting
            && x.sections[i].seed.is_empty()
        {
            x.sections[i].seed = clip(&text, SEED_MAX);
        }
        if !*doc_para_done {
            *doc_para = para.clone();
            *doc_para_done = true;
        }
        para.clear();
    };

    for (n, raw) in lines.iter().enumerate() {
        if FENCE.is_match(raw) {
            in_fence = !in_fence;
            flush(&mut para, collecting, x, &mut doc_para, &mut doc_para_done);
            continue;
        }
        if in_fence {
            continue;
        }
        for (r, col) in refs_in(raw) {
            let _ = col;
            if !x.refs.iter().any(|(e, _)| e == &r) {
                x.refs.push((r, n + 1));
            }
        }
        if let Some(c) = HEADING.captures(raw) {
            flush(&mut para, collecting, x, &mut doc_para, &mut doc_para_done);
            let level = c[1].len();
            let heading = heading_text(&c[2]);
            if x.title.is_none() {
                x.title = Some(heading.clone());
            }
            let base = names::slug(&heading);
            let k = slugs.entry(base.clone()).or_insert(0);
            let slug = if *k == 0 {
                base.clone()
            } else {
                format!("{base}-{k}")
            };
            *k += 1;
            x.sections.push(Section {
                level,
                heading,
                slug,
                line: n + 1,
                seed: String::new(),
            });
            collecting = Some(x.sections.len() - 1);
            continue;
        }
        let t = raw.trim();
        // Badges, HTML, tables and rules aren't prose.
        if t.is_empty()
            || t.starts_with("[![")
            || t.starts_with("![")
            || t.starts_with('<')
            || t.starts_with('|')
            || t.starts_with("---")
            || t.starts_with("===")
        {
            flush(&mut para, collecting, x, &mut doc_para, &mut doc_para_done);
            continue;
        }
        para.push(t.to_string());
    }
    flush(&mut para, collecting, x, &mut doc_para, &mut doc_para_done);
    if !doc_para.is_empty() {
        x.seed = Some(doc_para.join(" "));
    }
}

/// The backticked spans of a line that look like references to a path or a
/// project id (`rs/crates/agent-dream`, `stale.rs`, `gomuxai:docs/X.md`),
/// mirroring `agent_dream::stale`'s path test so a doc's references and the
/// memory check agree.
pub fn refs_in(line: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut rest = line;
    let mut off = 0;
    while let Some(i) = rest.find('`') {
        let after = &rest[i + 1..];
        let Some(j) = after.find('`') else { break };
        let s = after[..j].trim();
        let p = trim_ref(s);
        if looks_like_ref(p) {
            out.push((p.to_string(), off + i));
        }
        off += i + j + 2;
        rest = &after[j + 1..];
    }
    out
}

/// Extensions that make a token without a `/` a file name (agent_dream::stale's).
const EXTENSIONS: &[&str] = &[
    "rs", "go", "md", "toml", "json", "jsonl", "yaml", "yml", "sh", "py", "js", "ts", "tsx", "c",
    "h", "cpp", "swift", "plist", "lock", "db", "log", "txt", "html", "css", "service", "timer",
    "mod", "sum", "conf", "ini", "sql", "nix",
];

fn has_extension(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && EXTENSIONS.contains(&ext),
        None => false,
    }
}

/// `x.rs:12` → `x.rs`; trailing punctuation and `./` dropped.
pub fn trim_ref(tok: &str) -> &str {
    let mut t = tok.trim_end_matches([',', '.', ';', ')']);
    if let Some((head, tail)) = t.rsplit_once(':')
        && has_extension(head)
        && !tail.contains('/')
    {
        t = head;
    }
    t.strip_prefix("./").unwrap_or(t)
}

fn looks_like_ref(tok: &str) -> bool {
    if tok.is_empty() || tok.len() > 300 || tok.chars().any(char::is_whitespace) {
        return false;
    }
    if tok.contains("://") || tok.starts_with("www.") || tok.contains('@') {
        return false;
    }
    const PLACEHOLDER: &[char] = &[
        '<', '>', '*', '?', '[', ']', '{', '}', '$', '|', '=', '%', '(', '"', '\'',
    ];
    if tok.contains(PLACEHOLDER) || tok.contains("...") || tok.contains('…') {
        return false;
    }
    if tok.contains("::") || tok.starts_with('-') || tok.starts_with('~') {
        return false;
    }
    tok.contains('/') || has_extension(tok)
}

struct Lang {
    pats: Vec<(Regex, &'static str)>,
}

fn re(s: &str) -> Regex {
    Regex::new(s).unwrap()
}

static RUST: LazyLock<Lang> = LazyLock::new(|| Lang {
    pats: vec![
        (
            re(
                r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)"#,
            ),
            "fn",
        ),
        (
            re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_]\w*)"),
            "struct",
        ),
        (
            re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_]\w*)"),
            "enum",
        ),
        (
            re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?union\s+([A-Za-z_]\w*)"),
            "union",
        ),
        (
            re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?trait\s+([A-Za-z_]\w*)"),
            "trait",
        ),
        (
            re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?type\s+([A-Za-z_]\w*)"),
            "type",
        ),
        (
            re(
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+(?:mut\s+)?([A-Z_][A-Z0-9_]*)\s*:",
            ),
            "const",
        ),
        (
            re(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*\{"),
            "mod",
        ),
        (re(r"^\s*macro_rules!\s+([A-Za-z_]\w*)"), "macro"),
        (
            re(
                r"^\s*(?:unsafe\s+)?impl(?:<[^>]*>)?\s+([A-Za-z_][\w:]*(?:<[^>]*>)?(?:\s+for\s+[A-Za-z_][\w:]*)?)",
            ),
            "impl",
        ),
    ],
});
static GO: LazyLock<Lang> = LazyLock::new(|| Lang {
    pats: vec![
        (re(r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"), "fn"),
        (re(r"^type\s+([A-Za-z_]\w*)\s+struct\b"), "struct"),
        (re(r"^type\s+([A-Za-z_]\w*)\s+interface\b"), "interface"),
        (re(r"^type\s+([A-Za-z_]\w*)\b"), "type"),
        (re(r"^(?:const|var)\s+([A-Za-z_]\w*)\b"), "const"),
    ],
});
static TS: LazyLock<Lang> = LazyLock::new(|| Lang {
    pats: vec![
        (
            re(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\*?\s+([A-Za-z_$][\w$]*)"),
            "fn",
        ),
        (
            re(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)"),
            "class",
        ),
        (
            re(r"^\s*(?:export\s+)?(?:declare\s+)?interface\s+([A-Za-z_$][\w$]*)"),
            "interface",
        ),
        (
            re(r"^\s*(?:export\s+)?(?:declare\s+)?type\s+([A-Za-z_$][\w$]*)\s*(?:<[^=]*>)?\s*="),
            "type",
        ),
        (
            re(r"^\s*(?:export\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)"),
            "enum",
        ),
        (re(r"^export\s+(?:const|let)\s+([A-Za-z_$][\w$]*)"), "const"),
    ],
});
static PY: LazyLock<Lang> = LazyLock::new(|| Lang {
    pats: vec![
        (re(r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)"), "fn"),
        (re(r"^\s*class\s+([A-Za-z_]\w*)"), "class"),
    ],
});
static SWIFT: LazyLock<Lang> = LazyLock::new(|| Lang {
    pats: vec![
        (
            re(
                r"^\s*(?:@\w+\s+)*(?:(?:public|private|internal|open|fileprivate|static|final|override|mutating|nonisolated)\s+)*func\s+([A-Za-z_]\w*)",
            ),
            "fn",
        ),
        (
            re(
                r"^\s*(?:@\w+\s+)*(?:(?:public|private|internal|open|fileprivate|final)\s+)*class\s+([A-Za-z_]\w*)",
            ),
            "class",
        ),
        (
            re(
                r"^\s*(?:@\w+\s+)*(?:(?:public|private|internal|open|fileprivate)\s+)*struct\s+([A-Za-z_]\w*)",
            ),
            "struct",
        ),
        (
            re(
                r"^\s*(?:@\w+\s+)*(?:(?:public|private|internal|open|fileprivate|indirect)\s+)*enum\s+([A-Za-z_]\w*)",
            ),
            "enum",
        ),
        (
            re(
                r"^\s*(?:@\w+\s+)*(?:(?:public|private|internal|open|fileprivate)\s+)*protocol\s+([A-Za-z_]\w*)",
            ),
            "interface",
        ),
        (
            re(
                r"^\s*(?:(?:public|private|internal|open|fileprivate)\s+)*extension\s+([A-Za-z_][\w.]*)",
            ),
            "impl",
        ),
    ],
});
static SHELL: LazyLock<Lang> = LazyLock::new(|| Lang {
    pats: vec![
        (re(r"^\s*function\s+([A-Za-z_][\w:-]*)"), "fn"),
        (re(r"^\s*([A-Za-z_][\w:-]*)\s*\(\)\s*\{?"), "fn"),
    ],
});
static MAKE: LazyLock<Lang> = LazyLock::new(|| Lang {
    pats: vec![(
        re(r"^([A-Za-z0-9_][A-Za-z0-9_./%-]*)\s*:(?:[^=]|$)"),
        "target",
    )],
});

/// A doc comment that ends just above `line`, for languages whose doc
/// comments go above (`///`, `//`, `/** */`, `#`), skipping attributes and
/// decorators; or the Python docstring just below.
fn doc_above(lang: &str, lines: &[&str], line: usize) -> String {
    if lang == "python" {
        // The docstring is the first statement in the body.
        let mut i = line + 1;
        // A signature can wrap: find the line ending in ':'.
        let mut j = line;
        while j < lines.len() && !lines[j].trim_end().ends_with(':') && j < line + 10 {
            j += 1;
        }
        if j > line {
            i = j + 1;
        }
        return docstring_at(lines, i)
            .map(|d| first_sentence(&d))
            .unwrap_or_default();
    }
    let mut doc: Vec<String> = Vec::new();
    let mut i = line;
    while i > 0 {
        let t = lines[i - 1].trim();
        if t.starts_with("#[") || t.starts_with('@') && lang != "make" && lang != "shell" {
            i -= 1;
            continue;
        }
        let is_comment = match lang {
            "rust" => t.starts_with("///") || t.starts_with("//") && !t.starts_with("//!"),
            "make" | "shell" => t.starts_with('#') && !t.starts_with("#!"),
            _ => {
                t.starts_with("//")
                    || t.starts_with('*')
                    || t.starts_with("/*")
                    || t.ends_with("*/")
            }
        };
        if !is_comment {
            break;
        }
        doc.push(strip_comment(t));
        i -= 1;
    }
    doc.reverse();
    first_sentence(&paragraph(&doc))
}

/// The definitions in a file, by language.
pub fn find_symbols(lang: &str, lines: &[&str]) -> Vec<Symbol> {
    let l: &Lang = match lang {
        "rust" => &RUST,
        "go" => &GO,
        "typescript" | "javascript" => &TS,
        "python" => &PY,
        "swift" => &SWIFT,
        "shell" => &SHELL,
        "make" => &MAKE,
        _ => return Vec::new(),
    };
    let mut out: Vec<Symbol> = Vec::new();
    let mut in_block_string = false;
    for (n, raw) in lines.iter().enumerate() {
        if lang == "make" && raw.starts_with('\t') {
            continue;
        }
        // Skip Rust raw strings and Python long strings crudely: a line with an odd
        // count of `"""` toggles.
        if lang == "python" && raw.matches("\"\"\"").count() % 2 == 1 {
            in_block_string = !in_block_string;
            continue;
        }
        if in_block_string {
            continue;
        }
        for (pat, kind) in &l.pats {
            let Some(c) = pat.captures(raw) else { continue };
            let mut name = c[1].trim().to_string();
            if *kind == "impl" {
                name = format!("impl {}", name.split('<').next().unwrap_or(&name).trim());
                if let Some((t, ty)) = c[1].split_once(" for ") {
                    name = format!(
                        "impl {} for {}",
                        t.split('<').next().unwrap_or(t).trim(),
                        ty.trim()
                    );
                }
            }
            if lang == "make" && (name.starts_with('.') || name.contains('%')) {
                break;
            }
            if lang == "shell" && matches!(name.as_str(), "if" | "then" | "while" | "for") {
                break;
            }
            let mut doc = doc_above(lang, lines, n);
            if lang == "make"
                && doc.is_empty()
                && let Some((_, after)) = raw.split_once("##")
            {
                doc = first_sentence(after.trim());
            }
            let sig = raw.trim().trim_end_matches('{').trim_end();
            out.push(Symbol {
                kind: kind.to_string(),
                name,
                line: n + 1,
                signature: clip(sig, 160),
                doc: clip(&doc, 300),
            });
            break;
        }
    }
    out
}

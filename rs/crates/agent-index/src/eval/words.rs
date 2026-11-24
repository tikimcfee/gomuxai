//! The words a query gives either system, as the Python harness made them:
//! a plain sentence's content words, and an agent's search pattern as the
//! words a person would give the index.

use std::sync::LazyLock;

use regex::Regex;

const STOP: &[&str] = &[
    "a", "about", "all", "also", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by",
    "can", "could", "did", "do", "does", "each", "for", "from", "get", "had", "has", "have", "how",
    "i", "if", "in", "into", "is", "it", "its", "just", "let", "like", "make", "me", "more", "my",
    "need", "not", "now", "of", "on", "one", "only", "or", "our", "out", "please", "see", "should",
    "so", "some", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this",
    "to", "up", "us", "use", "vs", "want", "was", "we", "what", "when", "where", "which", "who",
    "why", "will", "with", "would", "you", "your",
];

/// Words of a pattern that are the language's, not the agent's.
const KEYWORDS: &[&str] = &[
    "fn", "pub", "struct", "impl", "let", "mut", "use", "mod", "enum", "trait", "const", "static",
    "func", "type", "var", "def", "class", "import", "export", "function", "return", "async",
    "await", "self", "crate", "super",
];

const EXTENSIONS: &str =
    "rs|py|ts|js|go|md|mjs|cjs|jsx|tsx|toml|json|yaml|yml|txt|sh|wgsl|html|css|swift|c|h";

/// The kinds of search whose pattern is a glob (fd), not a regex.
pub const GLOB_KINDS: &[&str] = &["find", "fd", "glob"];

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9_]+").unwrap());
static CLASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]*)\]").unwrap());
static ESCAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\[A-Za-z]").unwrap());
static REPEAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\d*,?\d*\}").unwrap());
static EXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"\\?\.(?:{EXTENSIONS})\b")).unwrap());

fn is_digits(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| c.is_ascii_digit())
}

/// A plain sentence as the words worth searching for: three letters or
/// more, not a stop word or a number (all its tokens when none is left),
/// each once whatever its case; case kept.
pub fn content_words(phrase: &str) -> Vec<String> {
    let toks: Vec<&str> = TOKEN.find_iter(phrase).map(|m| m.as_str()).collect();
    let words: Vec<&str> = toks
        .iter()
        .copied()
        .filter(|t| t.len() >= 3 && !STOP.contains(&t.to_lowercase().as_str()) && !is_digits(t))
        .collect();
    let pool = if words.is_empty() { toks } else { words };
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for w in pool {
        let l = w.to_lowercase();
        if !seen.contains(&l) {
            seen.push(l);
            out.push(w.to_string());
        }
    }
    out
}

/// A search pattern as the words a person would give the index: regex or
/// glob syntax, file extensions, language keywords and words under three
/// letters dropped; underscores kept; `[Dd]eliver` read as `Deliver`.
pub fn pattern_words(kind: &str, pat: &str) -> Vec<String> {
    let mut s = pat.to_string();
    if !GLOB_KINDS.contains(&kind) {
        s = CLASS
            .replace_all(&s, |c: &regex::Captures| {
                let inner = &c[1];
                let alpha = !inner.is_empty() && inner.chars().all(char::is_alphabetic);
                let one = {
                    let mut l: Vec<String> =
                        inner.chars().map(|c| c.to_lowercase().collect()).collect();
                    l.sort();
                    l.dedup();
                    l.len() == 1
                };
                if alpha && one {
                    inner.chars().next().map(String::from).unwrap_or_default()
                } else {
                    " ".to_string()
                }
            })
            .into_owned();
        s = ESCAPE.replace_all(&s, " ").into_owned();
        s = REPEAT.replace_all(&s, " ").into_owned();
    }
    s = EXT.replace_all(&s, " ").into_owned();
    let mut words: Vec<String> = Vec::new();
    for w in TOKEN.find_iter(&s).map(|m| m.as_str()) {
        if w.trim_matches('_').len() >= 3
            && !is_digits(w)
            && !KEYWORDS.contains(&w.to_lowercase().as_str())
            && !words.iter().any(|x| x == w)
        {
            words.push(w.to_string());
        }
    }
    words
}

/// grep's basic syntax as rg reads it: `\( \) \{ \} \? \+ \|` swap meaning
/// with their bare forms.
pub fn bre_to_ere(pat: &str) -> String {
    let c: Vec<char> = pat.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\\' && i + 1 < c.len() {
            let n = c[i + 1];
            if "(){}?+|".contains(n) {
                out.push(n);
            } else {
                out.push('\\');
                out.push(n);
            }
            i += 2;
        } else if "(){}?+|".contains(c[i]) {
            out.push('\\');
            out.push(c[i]);
            i += 1;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What run_eval.py gives for the same input.
    #[test]
    fn as_the_python_harness() {
        assert_eq!(
            content_words("Where does the watcher debounce a git checkout, and why 2026?"),
            ["watcher", "debounce", "git", "checkout"]
        );
        assert_eq!(content_words("is it on"), ["is", "it", "on"]);
        assert_eq!(content_words("Deliver deliver DELIVER"), ["Deliver"]);
        assert_eq!(
            pattern_words("grep", r"fn [Dd]eliver_held\(\b.{2,5}"),
            ["Deliver_held"]
        );
        assert_eq!(pattern_words("glob", "**/*.rs"), Vec::<String>::new());
        assert_eq!(
            pattern_words("rg", r"struct WallLayout\s*\{"),
            ["WallLayout"]
        );
        assert_eq!(
            pattern_words("grep", "__init__ main.py x"),
            ["__init__", "main"]
        );
        assert_eq!(bre_to_ere(r"a\(b\)c+"), r"a(b)c\+");
    }
}

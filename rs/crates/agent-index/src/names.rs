//! Words from names: identifier splitting (kept whole as well as split, so
//! `gather_global` matches both `gather_global` and `gather global`, and
//! `glyph3d` stays whole), label variants (`word-wall`, `word wall`,
//! `wordwall`, `WordWall`), label normalization, and GitHub-style heading slugs.

/// The words of an identifier or file name: split at `_`, `-`, `.`, `/`,
/// spaces and camelCase humps (`HTTPServer` → `http`, `server`), lowercased.
/// Letters and digits stay together (`glyph3d`, `v2`), so a split never makes
/// a bare `3d` out of a name.
pub fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in s.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0 && c.is_uppercase() {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
                // aB → a|B; ABc → A|Bc (the last capital of a run starts a word).
                if (prev.is_lowercase()
                    || prev.is_ascii_digit()
                    || (prev.is_uppercase() && next_lower))
                    && !cur.is_empty()
                {
                    words.push(std::mem::take(&mut cur));
                }
            }
            cur.extend(c.to_lowercase());
        }
        if !cur.is_empty() {
            words.push(cur);
        }
    }
    words
}

/// An identifier both whole and split, for an FTS column: `gather_global
/// gather global`. A name that doesn't split is itself once.
pub fn whole_and_split(name: &str) -> String {
    let words = split_words(name);
    let whole = name.to_lowercase();
    let joined = words.join(" ");
    if words.len() <= 1 || joined == whole {
        return whole;
    }
    // Keep the whole form as one token where FTS would keep it (letters, digits, `_`),
    // plus the run-together form (`wordwall`), plus the words.
    let mut out = vec![whole.clone()];
    let compact: String = words.concat();
    if compact != whole {
        out.push(compact);
    }
    out.push(joined);
    out.join(" ")
}

/// A label's normalized form: its words, lowercased, one space apart.
pub fn norm(label: &str) -> String {
    split_words(label).join(" ")
}

/// A normalized label without spaces (`word wall` → `wordwall`).
pub fn compact(label: &str) -> String {
    split_words(label).concat()
}

/// The spellings a name answers to: itself, kebab, spaced, run together, and
/// CamelCase (`word-wall`, `word wall`, `wordwall`, `WordWall`). A one-word name
/// answers only to itself and its lowercase form.
pub fn variants(name: &str) -> Vec<String> {
    let words = split_words(name);
    let mut out: Vec<String> = vec![name.to_string()];
    if words.len() > 1 {
        out.push(words.join("-"));
        out.push(words.join(" "));
        out.push(words.join("_"));
        out.push(words.concat());
        out.push(
            words
                .iter()
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                })
                .collect(),
        );
    } else if let Some(w) = words.first()
        && w != name
    {
        out.push(w.clone());
    }
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|v| !v.is_empty() && seen.insert(v.clone()));
    out
}

/// GitHub's heading anchor: lowercased, punctuation dropped (but `-` and `_`),
/// spaces to `-`. Duplicates get `-1`, `-2` from the caller.
pub fn slug(heading: &str) -> String {
    let mut out = String::new();
    for c in heading.trim().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.extend(c.to_lowercase());
        } else if c == ' ' {
            out.push('-');
        }
    }
    out
}

/// The query's terms: words of letters, digits and `_` (an identifier stays
/// one term), lowercased.
pub fn query_terms(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in q.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let t = t.trim_matches('_').to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Words too common to name anything: left out of the "no entity mentions"
/// report and of label n-grams on their own.
pub const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "does", "for", "from", "how", "i", "in",
    "is", "it", "me", "my", "of", "on", "or", "s", "that", "the", "this", "to", "want", "what",
    "where", "which", "who", "why", "with", "work", "stuff", "thing", "find", "where's",
];

pub fn is_stop(w: &str) -> bool {
    STOP_WORDS.contains(&w)
}

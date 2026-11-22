//! What `agent dream commit` checks of the memory a dream changed
//! (docs/DREAMING.md, "Evidence rules"): every session, friction entry or
//! commit a changed memory file cites must be one the store knows, an item
//! of this dream or one a committed dream reviewed. The three-session rule
//! for a preference stays in the brief: a preference can't be told from a
//! fact mechanically.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::gather::{KIND_COMMIT, KIND_DREAM, KIND_FRICTION, KIND_SESSION};
use crate::memory;

/// The session harnesses a citation can name (`claude:<session-id>`).
pub const SESSION_KINDS: &[&str] = &["claude", "agy", "kimi"];

/// A citation in a changed memory file that names nothing the store knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Uncited {
    pub file: String,
    pub citation: String,
}

impl std::fmt::Display for Uncited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: cites {}, which is no item of this dream and no reviewed one",
            self.file, self.citation
        )
    }
}

/// What a citation must resolve to: a dream item's kind, and its key (for
/// a commit, the hash or its start, matched against `repo@hash` keys).
/// None for a kind no item has (a doc, say): it isn't checked.
pub fn citation_item(citation: &str) -> Option<(&'static str, String)> {
    let (kind, id) = citation.split_once(':')?;
    if SESSION_KINDS.contains(&kind) {
        return Some((KIND_SESSION, citation.to_string()));
    }
    match kind {
        "friction" => Some((KIND_FRICTION, id.to_string())),
        "commit" => Some((KIND_COMMIT, id.to_string())),
        // A committed dream (the global dream cites the project dreams it
        // learned from).
        "dream" => Some((KIND_DREAM, id.to_string())),
        _ => None,
    }
}

/// Whether a commit citation (a hash, or its first 7 or more characters)
/// names the commit item key `repo@hash`.
pub fn commit_matches(cited: &str, key: &str) -> bool {
    let Some((_, hash)) = key.rsplit_once('@') else {
        return false;
    };
    cited.len() >= 7 && hash.starts_with(cited)
}

/// The top-level `.md` files of dir that differ from the snapshot taken when
/// the dream opened (`snapshot_dir/files/`), or that it didn't have.
pub fn changed_files(dir: &Path, snapshot_dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !e.file_type()?.is_file() || memory::check_file_name(&name).is_err() {
            continue;
        }
        let now = fs::read(e.path()).with_context(|| format!("reading {name}"))?;
        let before = fs::read(snapshot_dir.join("files").join(&name)).ok();
        if before.as_deref() != Some(now.as_slice()) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// The citations in dir's changed files that `known` doesn't resolve.
/// `known(kind, key)` says whether the store knows the item.
pub fn unknown_citations(
    dir: &Path,
    snapshot_dir: &Path,
    known: &dyn Fn(&str, &str) -> bool,
) -> Result<Vec<Uncited>> {
    let mut out = Vec::new();
    for name in changed_files(dir, snapshot_dir)? {
        let content =
            fs::read_to_string(dir.join(&name)).with_context(|| format!("reading {name}"))?;
        for c in memory::parse_file(&name, &content).citations() {
            if let Some((kind, key)) = citation_item(&c)
                && !known(kind, &key)
            {
                out.push(Uncited {
                    file: name.clone(),
                    citation: c,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACT: &str = "---\nname: a\ndescription: d\nmetadata:\n  type: feedback\n---\nRun tests first.\n[source: claude:s1, friction:h-1, commit:abcdef12, doc:README; added: 2026-11-01]\n";

    #[test]
    fn citation_items() {
        assert_eq!(
            citation_item("claude:s1"),
            Some(("session", "claude:s1".into()))
        );
        assert_eq!(
            citation_item("friction:h-1"),
            Some(("friction", "h-1".into()))
        );
        assert_eq!(citation_item("commit:abc"), Some(("commit", "abc".into())));
        assert_eq!(citation_item("dream:7"), Some(("dream", "7".into())));
        assert_eq!(citation_item("doc:README"), None);
        assert!(commit_matches("abcdef1", "/srv/app@abcdef1234"));
        assert!(!commit_matches("abc", "/srv/app@abcdef1234"), "too short");
        assert!(!commit_matches("abcdef9", "/srv/app@abcdef1234"));
    }

    #[test]
    fn only_changed_files_are_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("memory");
        let snap = tmp.path().join("snap");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("old.md"), FACT.replace("claude:s1", "claude:gone")).unwrap();
        memory::take_snapshot(&dir, &snap).unwrap();
        fs::write(dir.join("new.md"), FACT).unwrap();
        assert_eq!(changed_files(&dir, &snap).unwrap(), ["new.md"]);

        let known = |kind: &str, key: &str| match kind {
            "session" => key == "claude:s1",
            "friction" => key == "h-1",
            "commit" => commit_matches(key, "/srv/app@abcdef1234"),
            _ => false,
        };
        assert!(unknown_citations(&dir, &snap, &known).unwrap().is_empty());

        fs::write(
            dir.join("new.md"),
            FACT.replace("friction:h-1", "friction:h-9"),
        )
        .unwrap();
        let got = unknown_citations(&dir, &snap, &known).unwrap();
        assert_eq!(
            got,
            [Uncited {
                file: "new.md".into(),
                citation: "friction:h-9".into()
            }]
        );
        assert_eq!(
            got[0].to_string(),
            "new.md: cites friction:h-9, which is no item of this dream and no reviewed one"
        );
    }
}

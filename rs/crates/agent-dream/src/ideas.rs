//! The idea engine's ideas (docs/DREAMING-VISION.md, "The idea engine"): a
//! generator proposes them (`agent dream idea`), a critic judges each
//! against its primary sources (`agent dream judge`), and the caller does
//! what it does with the approved ones. The one caller built is the global
//! dream's synthesis, whose idea is one proposed change: a memory file put
//! or removed, or a friction entry triaged. Code applies the approved ones.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::memory;
use crate::review;

/// A memory change's ops.
pub const OP_PUT: &str = "put";
pub const OP_REMOVE: &str = "remove";
/// A friction triage's ops, as `agent dream friction` names them.
pub const OP_DUP: &str = "dup";
pub const OP_LINK: &str = "link";
pub const OP_ACCEPT: &str = "accept";

/// A synthesis idea as the generator writes it (the `--from` file of
/// `agent dream idea`), before it is checked.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisIn {
    pub op: String,
    // A memory change.
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub expect_version: Option<String>,
    #[serde(default)]
    pub draft: Option<String>,
    #[serde(default)]
    pub why: Option<String>,
    // A friction triage.
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub other: Option<serde_json::Value>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub citations: Vec<String>,
}

/// A synthesis idea as it is recorded: checked, with a memory put's content
/// read from its draft when it was proposed, so what the critic judged is
/// what code applies, whatever happens to the draft file afterwards.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Synthesis {
    Memory {
        dir: String,
        file: String,
        op: String,
        expect_version: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        draft: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        content: String,
        why: String,
        citations: Vec<String>,
    },
    Friction {
        op: String,
        id: i64,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        other: String,
        note: String,
        citations: Vec<String>,
    },
}

impl Synthesis {
    pub fn citations(&self) -> &[String] {
        match self {
            Synthesis::Memory { citations, .. } | Synthesis::Friction { citations, .. } => {
                citations
            }
        }
    }

    /// One line saying what it would change.
    pub fn describe(&self) -> String {
        match self {
            Synthesis::Memory { dir, file, op, .. } => {
                format!("memory {op} {}/{file}", dir.trim_end_matches('/'))
            }
            Synthesis::Friction { op, id, other, .. } if other.is_empty() => {
                format!("friction {op} #{id}")
            }
            Synthesis::Friction { op, id, other, .. } => format!("friction {op} #{id} {other}"),
        }
    }
}

fn need(v: &Option<String>, field: &str, op: &str) -> Result<String> {
    match v.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => bail!("a {op} idea needs {field:?}"),
    }
}

fn none_of(present: &[(&str, bool)], op: &str) -> Result<()> {
    let extra: Vec<&str> = present
        .iter()
        .filter(|(_, p)| *p)
        .map(|(n, _)| *n)
        .collect();
    if !extra.is_empty() {
        bail!("a {op} idea takes no {}", extra.join(", "));
    }
    Ok(())
}

/// Checks a synthesis idea's shape and returns it as recorded, with
/// `content` the draft's content for a put (read by the caller, which
/// checks where the draft is). Its citations are checked by
/// [`unresolved_citations`].
pub fn check_synthesis(raw: SynthesisIn, content: Option<String>) -> Result<Synthesis> {
    let op = raw.op.trim().to_string();
    let mut citations: Vec<String> = raw
        .citations
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    citations.dedup();
    if citations.is_empty() {
        bail!("an idea cites its sources: \"citations\" is empty");
    }
    match op.as_str() {
        OP_PUT | OP_REMOVE => {
            none_of(
                &[
                    ("\"id\"", raw.id.is_some()),
                    ("\"other\"", raw.other.is_some()),
                    ("\"note\"", raw.note.is_some()),
                ],
                &op,
            )?;
            let dir = need(&raw.dir, "dir", &op)?;
            let file = need(&raw.file, "file", &op)?;
            memory::check_file_name(&file)?;
            let expect_version = need(&raw.expect_version, "expect_version", &op)?;
            let why = need(&raw.why, "why", &op)?;
            let (draft, content) = if op == OP_PUT {
                let draft = need(&raw.draft, "draft", &op)?;
                let Some(content) = content else {
                    bail!("a put idea's draft {draft} couldn't be read");
                };
                if file != memory::INDEX {
                    memory::parse_frontmatter(&content)
                        .map_err(|e| anyhow::anyhow!("the draft {draft}: {e:#}"))?;
                }
                (draft, content)
            } else {
                if file == memory::INDEX {
                    bail!(
                        "{} can't be removed: put it again without the line",
                        memory::INDEX
                    );
                }
                none_of(&[("\"draft\"", raw.draft.is_some())], &op)?;
                (String::new(), String::new())
            };
            Ok(Synthesis::Memory {
                dir,
                file,
                op,
                expect_version,
                draft,
                content,
                why,
                citations,
            })
        }
        OP_DUP | OP_LINK | OP_ACCEPT => {
            none_of(
                &[
                    ("\"dir\"", raw.dir.is_some()),
                    ("\"file\"", raw.file.is_some()),
                    ("\"expect_version\"", raw.expect_version.is_some()),
                    ("\"draft\"", raw.draft.is_some()),
                    ("\"why\" (friction says why in \"note\")", raw.why.is_some()),
                ],
                &op,
            )?;
            let Some(id) = raw.id.filter(|id| *id > 0) else {
                bail!("a {op} idea needs \"id\", a friction entry's number");
            };
            let other = match &raw.other {
                None => String::new(),
                Some(serde_json::Value::String(s)) => s.trim().to_string(),
                Some(serde_json::Value::Number(n)) => n.to_string(),
                Some(v) => bail!("\"other\" is {v}: a friction entry's number, or id@host"),
            };
            match op.as_str() {
                OP_ACCEPT if !other.is_empty() => bail!("an accept idea takes no \"other\""),
                OP_DUP if !other.parse::<i64>().is_ok_and(|o| o > 0) => {
                    bail!("a dup idea needs \"other\", the original entry's number on this host")
                }
                OP_DUP if other == id.to_string() => bail!("an entry isn't a duplicate of itself"),
                OP_LINK
                    if other
                        .split_once('@')
                        .is_none_or(|(i, h)| i.is_empty() || h.is_empty()) =>
                {
                    bail!("a link idea needs \"other\", the entry on another host as id@host")
                }
                _ => {}
            }
            let note = need(&raw.note, "note", &op)?;
            Ok(Synthesis::Friction {
                op,
                id,
                other,
                note,
                citations,
            })
        }
        "" => bail!("an idea needs \"op\": put, remove, dup, link or accept"),
        _ => bail!("{op:?} is no idea op: put, remove, dup, link or accept"),
    }
}

/// The citations that don't resolve: a kind no dream item has (only
/// sessions, `friction:`, `commit:` and `dream:` are taken), or an item
/// `known(kind, key)` doesn't know, by the rules commit uses.
pub fn unresolved_citations(
    citations: &[String],
    known: &dyn Fn(&str, &str) -> bool,
) -> Vec<String> {
    citations
        .iter()
        .filter(|c| match review::citation_item(c) {
            None => true,
            Some((kind, key)) => !known(kind, &key),
        })
        .cloned()
        .collect()
}

/// The earlier idea of the same caller that a new one is the same as, for
/// a vote instead of a duplicate (docs/DREAMING-VISION.md, "Convergence
/// across runs"), or None. Synthesis ideas are one dream's change set and
/// never converge: each dream proposes its own. Later callers (vision, act)
/// match by token similarity here, then ask the critic to confirm.
pub fn converges_with<'a, T>(_caller: &str, _idea: &str, _earlier: &'a [T]) -> Option<&'a T> {
    None
}

#[cfg(test)]
#[path = "ideas_test.rs"]
mod ideas_test;

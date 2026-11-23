//! Labels a person or a glossary adds (docs/INDEX.md, "Query expansion"):
//! `alias add`, and `lexicon import` of a JSONL glossary whose lay phrasings
//! become `ask` labels, canonical names `alt` labels, and provenance the
//! entity they point at.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::crawl::{add_label, normalize, refresh_search};
use crate::model;
use crate::roots;

/// Adds a person's label to an entity (or one of its old ids).
pub fn alias_add(conn: &Connection, id: &str, label: &str) -> Result<String> {
    let label = label.trim();
    if label.is_empty() {
        bail!("an empty label names nothing: agent index alias add ID \"the word wall\"");
    }
    let id = resolve_id(conn, id)?;
    add_label(
        conn,
        &id,
        label,
        "alt",
        "person",
        2.0,
        "agent index alias add",
    )?;
    refresh_search(conn, &BTreeSet::from([id.clone()]))?;
    Ok(id)
}

/// An id as given, or what an old id is now; an error when neither is known.
pub fn resolve_id(conn: &Connection, id: &str) -> Result<String> {
    if model::get(conn, id)?.is_some() {
        return Ok(id.to_string());
    }
    let alias: Option<String> = {
        let mut cur = id.to_string();
        let mut found = None;
        for _ in 0..20 {
            let next: Option<String> = conn
                .query_row(
                    "SELECT new_id FROM aliases WHERE old_id = ?",
                    params![cur],
                    |r| r.get(0),
                )
                .ok();
            match next {
                Some(n) => {
                    found = Some(n.clone());
                    cur = n;
                }
                None => break,
            }
        }
        found
    };
    if let Some(a) = alias
        && model::get(conn, &a)?.is_some()
    {
        return Ok(a);
    }
    bail!(
        "no entity {id} in the index: agent index search finds ids, and agent index crawl brings it up to date"
    )
}

/// One glossary row.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct LexiconRow {
    #[serde(default)]
    pub term: String,
    #[serde(default)]
    pub canonical: String,
    #[serde(default)]
    pub definition: String,
    #[serde(default)]
    pub confusables: serde_json::Value,
    #[serde(default)]
    pub provenance: Vec<String>,
    #[serde(default)]
    pub lay_phrasings: Vec<String>,
}

/// What an import did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LexiconReport {
    pub file: String,
    pub rows: usize,
    pub imported: usize,
    /// Rows whose provenance resolved to no entity (and whose canonical name
    /// is no label either).
    pub unresolved: usize,
    pub unreadable: usize,
    pub labels: usize,
    /// Some of the unresolved terms, to see why.
    pub unresolved_terms: Vec<String>,
}

/// The entity a provenance entry (`path:line`, a path, or an id) points at.
fn resolve_provenance(conn: &Connection, prov: &str, base: &Path) -> Result<Option<String>> {
    let p = prov.trim();
    if p.is_empty() {
        return Ok(None);
    }
    // `path:12` or `path:12:3`: the path.
    let mut path = p;
    while let Some((head, tail)) = path.rsplit_once(':') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            path = head;
        } else {
            break;
        }
    }
    if let Ok(id) = resolve_id(conn, path) {
        return Ok(Some(id));
    }
    let by_path = |abs: &str| -> Result<Option<String>> {
        let mut st = conn.prepare(
            "SELECT id FROM entities WHERE path = ? AND kind IN ('file', 'dir', 'project') LIMIT 1",
        )?;
        let mut rows = st.query_map(params![abs], |r| r.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    };
    if path.starts_with('/') {
        return by_path(&normalize(Path::new(path)));
    }
    // Relative: to the glossary's dir, then each project's root.
    if let Some(id) = by_path(&normalize(&base.join(path)))? {
        return Ok(Some(id));
    }
    for pr in roots::list_projects(conn)? {
        if let Some(id) = by_path(&normalize(&Path::new(&pr.path).join(path)))? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Imports a JSONL glossary. Lay phrasings become `ask` labels, the term and
/// its canonical name `alt` labels, each on the entities its provenance points
/// at; a row with no provenance that resolves goes to the entities its
/// canonical name already labels; rows that resolve nowhere are skipped and
/// counted.
pub fn lexicon_import(conn: &Connection, file: &Path) -> Result<LexiconReport> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let base = file
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let mut rep = LexiconReport {
        file: file.display().to_string(),
        ..Default::default()
    };
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let evidence_base = format!(
        "lexicon {}",
        file.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rep.rows += 1;
        let row: LexiconRow = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => {
                rep.unreadable += 1;
                continue;
            }
        };
        let mut targets: BTreeSet<String> = BTreeSet::new();
        for p in &row.provenance {
            if let Some(id) = resolve_provenance(conn, p, &base)? {
                targets.insert(id);
            }
        }
        if targets.is_empty() {
            for name in [&row.canonical, &row.term] {
                let norm = crate::names::norm(name);
                if norm.is_empty() {
                    continue;
                }
                let mut st = conn.prepare(
                    "SELECT DISTINCT entity FROM labels WHERE norm = ? AND kind IN ('pref', 'alt') LIMIT 5",
                )?;
                for id in st.query_map(params![norm], |r| r.get::<_, String>(0))? {
                    targets.insert(id?);
                }
            }
        }
        if targets.is_empty() {
            rep.unresolved += 1;
            if rep.unresolved_terms.len() < 10 {
                rep.unresolved_terms.push(row.term.clone());
            }
            continue;
        }
        rep.imported += 1;
        let mut evidence = evidence_base.clone();
        if !row.definition.is_empty() {
            evidence = format!("{evidence}: {}", crate::extract::clip(&row.definition, 200));
        }
        if !row.confusables.is_null() {
            let c = row.confusables.to_string();
            if c != "[]" && c != "{}" {
                evidence = format!("{evidence}; confusables: {}", crate::extract::clip(&c, 300));
            }
        }
        for id in &targets {
            let mut seen = BTreeSet::new();
            for (label, kind) in [(&row.canonical, "alt"), (&row.term, "alt")]
                .into_iter()
                .chain(row.lay_phrasings.iter().map(|l| (l, "ask")))
            {
                let l = label.trim();
                if l.is_empty() || !seen.insert((l.to_string(), kind)) {
                    continue;
                }
                add_label(conn, id, l, kind, "lexicon", 1.5, &evidence)?;
                rep.labels += 1;
            }
            touched.insert(id.clone());
        }
    }
    refresh_search(conn, &touched)?;
    Ok(rep)
}

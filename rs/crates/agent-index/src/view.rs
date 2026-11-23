//! The read side's views and their text (docs/INDEX.md, "Query surface"):
//! resolving an ID or PATH argument, `show`, `tree`, `related`, `stale`, and
//! fitting a list to a token budget (four characters a token): summaries cut
//! to their first sentence, then the tail dropped, saying how many were left
//! out.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::crawl::normalize;
use crate::extract::first_sentence;
use crate::model::{self, ENTITY_COLS, Entity, Summary};
use crate::query::SearchResult;

/// Tokens a text costs, estimated at four characters a token.
pub fn tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// What an ID or PATH argument named.
#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub id: String,
    /// The id asked for, when it was an old id that now answers to `id`.
    pub alias_of: Option<String>,
    /// The path asked for, when it isn't indexed itself and `id` is the nearest
    /// indexed directory above it.
    pub nearest_of: Option<String>,
}

fn entity_at_path(conn: &Connection, path: &str) -> Result<Option<String>> {
    let mut st = conn.prepare(
        "SELECT id FROM entities WHERE path = ? AND kind IN ('file', 'dir', 'project', 'root')
         ORDER BY CASE kind WHEN 'file' THEN 0 WHEN 'project' THEN 1 WHEN 'dir' THEN 2 ELSE 3 END LIMIT 1",
    )?;
    let mut rows = st.query_map(params![path], |r| r.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

/// Resolves an id (`alpha:src/x.rs`, `@dev`, an old id) or a path (against
/// `cwd`, canonicalized: a symlink that leads out of every indexed project
/// resolves to nothing).
pub fn resolve(conn: &Connection, arg: &str, cwd: &Path) -> Result<Resolved> {
    if model::get(conn, arg)?.is_some() {
        return Ok(Resolved {
            id: arg.to_string(),
            alias_of: None,
            nearest_of: None,
        });
    }
    if let Ok(id) = crate::labels::resolve_id(conn, arg) {
        return Ok(Resolved {
            id,
            alias_of: Some(arg.to_string()),
            nearest_of: None,
        });
    }
    let looks_like_id = arg.starts_with('@')
        || arg
            .split_once(':')
            .is_some_and(|(p, _)| !p.is_empty() && !p.contains('/'));
    let raw = Path::new(arg);
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let canon = match std::fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(_) if looks_like_id => {
            bail!("no entity {arg} in the index: agent index search finds ids")
        }
        Err(e) => bail!("{}: {e}", abs.display()),
    };
    let path = normalize(&canon);
    if let Some(id) = entity_at_path(conn, &path)? {
        return Ok(Resolved {
            id,
            alias_of: None,
            nearest_of: None,
        });
    }
    // The nearest indexed directory above, inside a project.
    let projects = crate::roots::list_projects(conn)?;
    let Some(proj) = projects
        .iter()
        .filter(|p| path == p.path || path.starts_with(&format!("{}/", p.path)))
        .max_by_key(|p| p.path.len())
    else {
        bail!(
            "{path} is outside every indexed project: agent index root list shows what is indexed (a symlink is never followed out of its project)"
        );
    };
    let mut cur = Path::new(&path).parent();
    while let Some(c) = cur {
        let s = normalize(c);
        if s.len() < proj.path.len() {
            break;
        }
        if let Some(id) = entity_at_path(conn, &s)? {
            return Ok(Resolved {
                id,
                alias_of: None,
                nearest_of: Some(path),
            });
        }
        cur = c.parent();
    }
    bail!(
        "{path} isn't indexed yet: agent index crawl --project {}",
        proj.name
    )
}

/// One entity in a list: id, kind, title and the first sentence of its summary.
#[derive(Debug, Clone, Serialize)]
pub struct Brief {
    pub id: String,
    pub kind: String,
    pub file_kind: String,
    pub name: String,
    pub title: String,
    pub summary: String,
    pub freshness: String,
    pub importance: f64,
    pub line: i64,
}

fn brief(conn: &Connection, e: &Entity) -> Result<Brief> {
    let s = model::summary(conn, &e.id)?;
    Ok(Brief {
        id: e.id.clone(),
        kind: e.kind.clone(),
        file_kind: e.file_kind.clone(),
        name: e.name.clone(),
        title: e.display_title().to_string(),
        summary: s
            .as_ref()
            .map(|s| first_sentence(&s.text))
            .unwrap_or_default(),
        freshness: model::freshness(&e.content_key, s.as_ref()).to_string(),
        importance: e.importance,
        line: e.line,
    })
}

fn children(conn: &Connection, id: &str) -> Result<Vec<Entity>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ENTITY_COLS} FROM entities WHERE parent = ?
         ORDER BY importance DESC, CASE kind WHEN 'project' THEN 0 WHEN 'dir' THEN 1 WHEN 'file' THEN 2 ELSE 3 END, line, name"
    ))?;
    let rows = st.query_map(params![id], Entity::from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One edge as seen from an entity.
#[derive(Debug, Clone, Serialize)]
pub struct EdgeView {
    /// How it reads from this entity: `depends on`, `used by`, `documented by`,
    /// `documents`, `tests`, `tested by`, `changes with`.
    pub relation: String,
    pub kind: String,
    pub other: String,
    pub weight: f64,
    pub origin: String,
    pub evidence: String,
}

fn relation(kind: &str, outgoing: bool) -> String {
    match (kind, outgoing) {
        ("depends-on", true) => "depends on",
        ("depends-on", false) => "used by",
        ("documented-by", true) => "documented by",
        ("documented-by", false) => "documents",
        ("tests", true) => "tests",
        ("tests", false) => "tested by",
        ("co-changes", _) => "changes with",
        ("noted-in", true) => "noted in",
        ("noted-in", false) => "notes",
        ("about", true) => "about",
        ("about", false) => "has member",
        ("same-as", _) => "same as",
        ("implements", true) => "implements",
        ("implements", false) => "implemented by",
        (k, _) => k,
    }
    .to_string()
}

/// An entity's edges both ways, grouped by how they read from it, each group
/// by weight.
pub fn edges(conn: &Connection, id: &str) -> Result<BTreeMap<String, Vec<EdgeView>>> {
    let mut out: BTreeMap<String, Vec<EdgeView>> = BTreeMap::new();
    for (col, other, outgoing) in [("source", "target", true), ("target", "source", false)] {
        let mut st = conn.prepare(&format!(
            "SELECT kind, {other}, weight, origin, evidence FROM edges WHERE {col} = ? ORDER BY weight DESC, {other}"
        ))?;
        let rows = st.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (kind, o, weight, origin, evidence) = row?;
            // co-changes is stored both ways: show it once.
            if kind == "co-changes" && !outgoing {
                continue;
            }
            let rel = relation(&kind, outgoing);
            out.entry(rel.clone()).or_default().push(EdgeView {
                relation: rel,
                kind,
                other: o,
                weight,
                origin,
                evidence,
            });
        }
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| {
            b.weight
                .total_cmp(&a.weight)
                .then_with(|| a.other.cmp(&b.other))
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct LabelView {
    pub label: String,
    pub kind: String,
    pub origin: String,
    pub evidence: String,
}

/// `show`: one entity in full.
#[derive(Debug, Clone, Serialize)]
pub struct Show {
    pub entity: Entity,
    pub resolved: Resolved,
    pub freshness: String,
    pub summary: Option<Summary>,
    /// From the root down to its parent.
    pub breadcrumb: Vec<Brief>,
    /// Dirs and files (or a section's subsections), by importance.
    pub children: Vec<Brief>,
    pub children_total: usize,
    /// A file's symbols and sections, in file order.
    pub symbols: Vec<Brief>,
    pub edges: BTreeMap<String, Vec<EdgeView>>,
    pub labels: Vec<LabelView>,
    /// A doc's references that resolve nowhere.
    pub dangling: Vec<(String, i64)>,
}

pub const SHOW_CHILDREN: usize = 25;

pub fn show(conn: &Connection, r: Resolved) -> Result<Show> {
    let Some(e) = model::get(conn, &r.id)? else {
        bail!("no entity {}", r.id);
    };
    let summary = model::summary(conn, &e.id)?;
    let freshness = model::freshness(&e.content_key, summary.as_ref()).to_string();
    let mut breadcrumb = Vec::new();
    let mut cur = e.parent.clone();
    let mut guard = 0;
    while !cur.is_empty() && guard < 100 {
        let Some(p) = model::get(conn, &cur)? else {
            break;
        };
        breadcrumb.push(brief(conn, &p)?);
        cur = p.parent.clone();
        guard += 1;
    }
    breadcrumb.reverse();
    let kids = children(conn, &e.id)?;
    let (syms, rest): (Vec<Entity>, Vec<Entity>) = kids
        .into_iter()
        .partition(|k| e.kind == "file" && (k.kind == "symbol" || k.kind == "section"));
    let mut symbols: Vec<Brief> = syms.iter().map(|k| brief(conn, k)).collect::<Result<_>>()?;
    // Sections nest: a file lists all of them, in order.
    if e.kind == "file" {
        let mut st = conn.prepare(&format!(
            "SELECT {ENTITY_COLS} FROM entities WHERE kind = 'section' AND substr(id, 1, length(?1)) = ?1 AND parent != ?2"
        ))?;
        let more: Vec<Entity> = st
            .query_map(params![format!("{}#", e.id), e.id], Entity::from_row)?
            .collect::<rusqlite::Result<_>>()?;
        for m in more {
            symbols.push(brief(conn, &m)?);
        }
        symbols.sort_by_key(|b| b.line);
    }
    let children_total = rest.len();
    let children: Vec<Brief> = rest
        .iter()
        .take(SHOW_CHILDREN)
        .map(|k| brief(conn, k))
        .collect::<Result<_>>()?;
    let labels: Vec<LabelView> = {
        let mut st = conn.prepare(
            "SELECT label, kind, origin, evidence FROM labels WHERE entity = ? ORDER BY origin != 'person', kind, label",
        )?;
        st.query_map(params![e.id], |r| {
            Ok(LabelView {
                label: r.get(0)?,
                kind: r.get(1)?,
                origin: r.get(2)?,
                evidence: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let dangling: Vec<(String, i64)> = {
        let mut st = conn.prepare("SELECT ref, line FROM dangling WHERE doc = ? ORDER BY line")?;
        st.query_map(params![e.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    Ok(Show {
        edges: edges(conn, &e.id)?,
        entity: e,
        resolved: r,
        freshness,
        summary,
        breadcrumb,
        children,
        children_total,
        symbols,
        labels,
        dangling,
    })
}

fn kind_line(e: &Entity) -> String {
    let mut k = e.kind.clone();
    let detail: Vec<&str> = [e.file_kind.as_str(), e.lang.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty() && *s != e.kind)
        .collect();
    if !detail.is_empty() {
        k = format!("{k} ({})", detail.join(", "));
    }
    k
}

fn path_line(e: &Entity) -> String {
    if e.line > 0 {
        format!("{}:{}", e.path, e.line)
    } else {
        e.path.clone()
    }
}

pub fn render_show(s: &Show) -> String {
    let mut o = String::new();
    let e = &s.entity;
    if let Some(old) = &s.resolved.alias_of {
        let _ = writeln!(o, "{old} was renamed: it is {} now.", e.id);
    }
    if let Some(p) = &s.resolved.nearest_of {
        let _ = writeln!(
            o,
            "{p} isn't indexed itself; the nearest indexed entity above it:"
        );
    }
    let _ = writeln!(o, "{}   {}  {}", e.id, kind_line(e), s.freshness);
    let _ = writeln!(o, "{}", path_line(e));
    if s.freshness == "stale" {
        let _ = writeln!(
            o,
            "Its summary was written against another version: a summary is a hint, the code is the truth."
        );
    }
    let mut extra: Vec<String> = Vec::new();
    if e.untracked {
        extra.push("untracked".into());
    }
    if !e.no_content.is_empty() {
        extra.push(format!("content not read: {}", e.no_content));
    }
    if e.lines > 0 {
        extra.push(format!("{} lines", e.lines));
    }
    if e.importance > 0.0 {
        extra.push(format!("importance {}", e.importance));
    }
    if !e.content_key.is_empty() {
        extra.push(format!(
            "key {}",
            &e.content_key[..e.content_key.len().min(12)]
        ));
    }
    if !extra.is_empty() {
        let _ = writeln!(o, "{}", extra.join(", "));
    }
    if !s.breadcrumb.is_empty() {
        let _ = writeln!(o, "In:");
        for b in &s.breadcrumb {
            let mut line = format!("  {}  {}", b.id, b.title);
            if !b.summary.is_empty() && b.summary != b.title {
                line = format!("{line}: {}", b.summary);
            }
            let _ = writeln!(o, "{}", crate::extract::clip(&line, 160));
        }
    }
    if !e.title.is_empty() && e.title != e.name && e.kind != "symbol" {
        let _ = writeln!(o, "Title: {}", e.title);
    }
    if e.kind == "symbol" && !e.signature.is_empty() {
        let _ = writeln!(o, "Signature: {}", e.signature);
    }
    match &s.summary {
        Some(sm) => {
            let who = if sm.writer == "seed" {
                "from the code itself"
            } else {
                sm.writer.as_str()
            };
            let _ = writeln!(o, "Summary ({who}): {}", sm.text);
            if !sm.why.is_empty() {
                let _ = writeln!(o, "Why: {}", sm.why);
            }
        }
        None => {
            let _ = writeln!(o, "Summary: none yet.");
        }
    }
    if !s.labels.is_empty() {
        let ls: Vec<String> = s
            .labels
            .iter()
            .take(15)
            .map(|l| format!("\"{}\" ({}, {})", l.label, l.kind, l.origin))
            .collect();
        let more = s.labels.len().saturating_sub(15);
        let _ = writeln!(
            o,
            "Labels: {}{}",
            ls.join(", "),
            if more > 0 {
                format!(", and {more} more")
            } else {
                String::new()
            }
        );
    }
    if !s.symbols.is_empty() {
        let _ = writeln!(o, "Symbols and sections ({}):", s.symbols.len());
        for b in s.symbols.iter().take(60) {
            let mut line = format!("  L{:<5} {:<8} {}", b.line, b.file_kind_or_kind(), b.name);
            if !b.summary.is_empty() {
                line = format!("{line}: {}", b.summary);
            }
            let _ = writeln!(o, "{}", crate::extract::clip(&line, 160));
        }
        if s.symbols.len() > 60 {
            let _ = writeln!(
                o,
                "  ... and {} more (--json lists them all)",
                s.symbols.len() - 60
            );
        }
    }
    if !s.children.is_empty() {
        let _ = writeln!(
            o,
            "Children ({} of {}, by importance):",
            s.children.len(),
            s.children_total
        );
        for b in &s.children {
            let mut line = format!(
                "  {}{}",
                b.name,
                if b.kind == "dir" || b.kind == "project" {
                    "/"
                } else {
                    ""
                }
            );
            if b.title != b.name {
                line = format!("{line}  {}", b.title);
            }
            if !b.summary.is_empty() {
                line = format!("{line}: {}", b.summary);
            }
            let _ = writeln!(o, "{}", crate::extract::clip(&line, 160));
        }
    }
    if !s.edges.is_empty() {
        let _ = writeln!(o, "Edges:");
        for (rel, es) in &s.edges {
            let shown: Vec<String> = es
                .iter()
                .take(10)
                .map(|x| {
                    if x.kind == "co-changes" {
                        format!("{} ({})", x.other, x.evidence)
                    } else if x.weight > 1.0 {
                        format!("{} (×{})", x.other, x.weight)
                    } else {
                        x.other.clone()
                    }
                })
                .collect();
            let more = es.len().saturating_sub(10);
            let _ = writeln!(
                o,
                "  {rel}: {}{}",
                shown.join(", "),
                if more > 0 {
                    format!(", and {more} more")
                } else {
                    String::new()
                }
            );
        }
    }
    if !s.dangling.is_empty() {
        let ds: Vec<String> = s
            .dangling
            .iter()
            .take(10)
            .map(|(r, l)| format!("`{r}` (line {l})"))
            .collect();
        let _ = writeln!(o, "Names that resolve nowhere: {}", ds.join(", "));
    }
    o
}

impl Brief {
    fn file_kind_or_kind(&self) -> &str {
        if self.kind == "section" {
            "section"
        } else if !self.file_kind.is_empty() {
            &self.file_kind
        } else {
            &self.kind
        }
    }
}

/// `tree`: a subtree to a depth, most important first.
#[derive(Debug, Clone, Serialize)]
pub struct TreeLine {
    pub depth: usize,
    pub id: String,
    pub kind: String,
    pub name: String,
    pub title: String,
    pub summary: String,
    pub children: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tree {
    pub root: String,
    pub lines: Vec<TreeLine>,
    /// Entries left out to fit the budget.
    pub left_out: usize,
}

/// The subtree under `id` (or every root and project when `id` is empty), to
/// `depth`, fitted to `budget` tokens (0: no limit).
pub fn tree(conn: &Connection, id: &str, depth: usize, budget: usize) -> Result<Tree> {
    let mut lines: Vec<TreeLine> = Vec::new();
    fn walk(
        conn: &Connection,
        id: &str,
        d: usize,
        max: usize,
        out: &mut Vec<TreeLine>,
    ) -> Result<()> {
        if d > max {
            return Ok(());
        }
        let kids: Vec<Entity> = children(conn, id)?
            .into_iter()
            .filter(|k| k.kind != "symbol" && k.kind != "section")
            .collect();
        for k in kids {
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM entities WHERE parent = ? AND kind NOT IN ('symbol', 'section')",
                params![k.id],
                |r| r.get(0),
            )?;
            let b = brief(conn, &k)?;
            out.push(TreeLine {
                depth: d,
                id: k.id.clone(),
                kind: k.kind.clone(),
                name: k.name.clone(),
                title: b.title,
                summary: b.summary,
                children: n as usize,
            });
            if matches!(k.kind.as_str(), "dir" | "project" | "root") {
                walk(conn, &k.id, d + 1, max, out)?;
            }
        }
        Ok(())
    }
    let root = if id.is_empty() {
        // Every root, and projects outside any root.
        let mut st = conn.prepare(&format!(
            "SELECT {ENTITY_COLS} FROM entities WHERE kind = 'root' OR (kind = 'project' AND parent = '') ORDER BY kind DESC, name"
        ))?;
        let tops: Vec<Entity> = st
            .query_map([], Entity::from_row)?
            .collect::<rusqlite::Result<_>>()?;
        for t in tops {
            let b = brief(conn, &t)?;
            lines.push(TreeLine {
                depth: 0,
                id: t.id.clone(),
                kind: t.kind.clone(),
                name: t.name.clone(),
                title: b.title,
                summary: b.summary,
                children: 0,
            });
            if depth >= 1 {
                walk(conn, &t.id, 1, depth, &mut lines)?;
            }
        }
        String::new()
    } else {
        walk(conn, id, 1, depth.max(1), &mut lines)?;
        id.to_string()
    };
    let mut left_out = 0;
    if budget > 0 {
        let mut used = 0;
        let mut keep = Vec::new();
        for l in lines {
            let t = tokens(&tree_line(&l));
            if used + t > budget {
                left_out += 1;
                continue;
            }
            used += t;
            keep.push(l);
        }
        lines = keep;
    }
    Ok(Tree {
        root,
        lines,
        left_out,
    })
}

fn tree_line(l: &TreeLine) -> String {
    let indent = "  ".repeat(l.depth.saturating_sub(1));
    let slash = if matches!(l.kind.as_str(), "dir" | "project" | "root") {
        "/"
    } else {
        ""
    };
    let mut s = format!("{indent}{}{slash}", l.name);
    if l.title != l.name {
        s = format!("{s}  {}", l.title);
    }
    if !l.summary.is_empty() && l.summary != l.title {
        s = format!("{s}: {}", l.summary);
    }
    if l.children > 0 && slash == "/" {
        s = format!("{s}  [{}]", l.children);
    }
    crate::extract::clip(&s, 150)
}

pub fn render_tree(t: &Tree) -> String {
    let mut o = String::new();
    for l in &t.lines {
        let _ = writeln!(o, "{}", tree_line(l));
    }
    o
}

/// `related`: an entity's neighbors by edge, with their titles.
#[derive(Debug, Clone, Serialize)]
pub struct Related {
    pub id: String,
    pub groups: BTreeMap<String, Vec<(EdgeView, Brief)>>,
}

pub fn related(conn: &Connection, id: &str) -> Result<Related> {
    let mut groups = BTreeMap::new();
    for (rel, es) in edges(conn, id)? {
        let mut v = Vec::new();
        for e in es {
            if let Some(o) = model::get(conn, &e.other)? {
                v.push((e, brief(conn, &o)?));
            }
        }
        if !v.is_empty() {
            groups.insert(rel, v);
        }
    }
    Ok(Related {
        id: id.to_string(),
        groups,
    })
}

pub fn render_related(r: &Related, budget: usize) -> (String, usize) {
    let mut o = String::new();
    let mut left_out = 0;
    let mut used = 0;
    for (rel, v) in &r.groups {
        let head = format!("{rel} ({}):\n", v.len());
        used += tokens(&head);
        o.push_str(&head);
        for (e, b) in v {
            let mut line = format!("  {}", b.id);
            if !e.evidence.is_empty() {
                line = format!("{line}  ({})", e.evidence);
            }
            if !b.summary.is_empty() {
                line = format!("{line}: {}", b.summary);
            }
            let line = format!("{}\n", crate::extract::clip(&line, 160));
            if budget > 0 && used + tokens(&line) > budget {
                left_out += 1;
                continue;
            }
            used += tokens(&line);
            o.push_str(&line);
        }
    }
    (o, left_out)
}

/// `stale`: what needs a summary, by importance, and doc references that
/// resolve nowhere.
#[derive(Debug, Clone, Serialize)]
pub struct Stale {
    /// Entities by freshness state.
    pub counts: BTreeMap<String, usize>,
    pub entities: Vec<Brief>,
    pub dangling: Vec<DanglingRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DanglingRef {
    pub doc: String,
    pub reference: String,
    pub line: i64,
}

/// The kinds a model would summarize: not symbols or sections, which keep
/// their seeds.
pub fn stale(conn: &Connection, project: &str, limit: usize) -> Result<Stale> {
    let mut st = conn.prepare(&format!(
        "SELECT {ENTITY_COLS} FROM entities WHERE kind IN ('project', 'dir', 'file') AND (?1 = '' OR project = ?1)
         ORDER BY importance DESC, id"
    ))?;
    let ents: Vec<Entity> = st
        .query_map(params![project], Entity::from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for e in &ents {
        let s = model::summary(conn, &e.id)?;
        let f = model::freshness(&e.content_key, s.as_ref());
        *counts.entry(f.to_string()).or_insert(0) += 1;
        if (f == "stale" || f == "none") && out.len() < limit && e.no_content.is_empty() {
            out.push(brief(conn, e)?);
        }
    }
    let mut st = conn.prepare(
        "SELECT d.doc, d.ref, d.line FROM dangling d JOIN entities e ON e.id = d.doc
         WHERE (?1 = '' OR e.project = ?1) ORDER BY e.importance DESC, d.doc, d.line LIMIT ?2",
    )?;
    let dangling: Vec<DanglingRef> = st
        .query_map(params![project, limit as i64], |r| {
            Ok(DanglingRef {
                doc: r.get(0)?,
                reference: r.get(1)?,
                line: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Stale {
        counts,
        entities: out,
        dangling,
    })
}

/// Search results as text, fitted to `budget` tokens (0: no limit): each
/// result's id, kind, freshness, path, summary and why it matched.
/// Returns the text and how many results were left out.
pub fn render_search(r: &SearchResult, budget: usize, explain: bool) -> (String, usize) {
    let mut head = String::new();
    if !r.mapping.is_empty() {
        let mut seen: HashMap<String, Vec<String>> = HashMap::new();
        let mut order = Vec::new();
        for m in &r.mapping {
            if !seen.contains_key(&m.phrase) {
                order.push(m.phrase.clone());
            }
            seen.entry(m.phrase.clone())
                .or_default()
                .push(format!("`{}` ({} label \"{}\")", m.entity, m.kind, m.label));
        }
        let parts: Vec<String> = order
            .iter()
            .map(|p| {
                let v = &seen[p];
                let more = v.len().saturating_sub(3);
                format!(
                    "\"{p}\" → {}{}",
                    v.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
                    if more > 0 {
                        format!(", and {more} more")
                    } else {
                        String::new()
                    }
                )
            })
            .collect();
        let _ = writeln!(head, "Labels: {}", parts.join("; "));
    }
    for t in &r.absent {
        let _ = writeln!(head, "No entity mentions `{t}`.");
    }
    let block = |i: usize, short: bool| -> String {
        let h = &r.hits[i];
        let mut b = String::new();
        let mut kind = h.kind.clone();
        if !h.file_kind.is_empty() && h.kind != "section" {
            kind = format!("{kind} ({})", h.file_kind);
        }
        let _ = writeln!(b, "{}. {}   {}  {}", h.rank, h.id, kind, h.freshness);
        let path = if h.line > 0 {
            format!("{}:{}", h.path, h.line)
        } else {
            h.path.clone()
        };
        let _ = writeln!(
            b,
            "   {path}{}",
            if h.untracked { "  (untracked)" } else { "" }
        );
        let text = if h.summary.is_empty() {
            if h.title != h.id && !h.id.ends_with(&h.title) {
                h.title.clone()
            } else {
                String::new()
            }
        } else if short {
            first_sentence(&h.summary)
        } else {
            h.summary.clone()
        };
        if !text.is_empty() {
            let _ = writeln!(b, "   {text}");
        }
        if !h.matched.is_empty() {
            let _ = writeln!(b, "   matched: {}", h.matched.join("; "));
        }
        if explain && !h.explain.is_empty() {
            let _ = writeln!(
                b,
                "   found by: {}; score {}",
                h.explain.join(", "),
                h.score
            );
        }
        b
    };
    let n = r.hits.len();
    let fit = |short: bool| -> (String, usize) {
        let mut body = String::new();
        let mut used = tokens(&head);
        let mut kept = 0;
        for i in 0..n {
            let b = block(i, short);
            if budget > 0 && used + tokens(&b) > budget && kept > 0 {
                break;
            }
            used += tokens(&b);
            body.push_str(&b);
            kept += 1;
        }
        (body, n - kept)
    };
    let (mut body, mut left) = fit(false);
    if left > 0 {
        (body, left) = fit(true);
    }
    let mut o = head;
    if n == 0 {
        let _ = writeln!(o, "Nothing in the index matches {:?}.", r.query);
    }
    o.push_str(&body);
    (o, left)
}

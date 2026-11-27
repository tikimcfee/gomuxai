//! The crawl (docs/INDEX.md, "The crawl"): code, always cheap. For each
//! project it lists files with their content keys (git blob ids, or content
//! hashes outside git), re-reads only what changed, carries renamed
//! entities' summaries, labels and edges over (keeping old ids as aliases),
//! then rebuilds the edges, importance, search rows and embed queue.
//!
//! Nothing outside a project is ever read or hashed: a file is read only when
//! its path, canonicalized, is still itself (no symlink anywhere on the way),
//! and a file over 1 MB is keyed by its size and mtime, never read.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::{Index, now};
use crate::edges;
use crate::extract::{self, Extracted};
use crate::files;
use crate::git;
use crate::manifest::{self, Manifest};
use crate::names;
use crate::roots::{self, Project};
use crate::walk;

#[derive(Debug, Clone, Default)]
pub struct CrawlOptions {
    /// Only this project.
    pub project: Option<String>,
    /// Re-read every file, not only what changed.
    pub rebuild: bool,
    /// Work it all out, then write nothing.
    pub dry_run: bool,
    /// The file count past which a root-found project is crawled shallow;
    /// 0 means [`LARGE_PROJECT`].
    pub large_project: usize,
    /// Only these projects (with `project`, that one too); empty is all.
    pub projects: Vec<String>,
    /// The watcher's flush: no new projects from the roots, and the
    /// cross-project steps (edges resolved across projects, importance)
    /// left for the next full crawl, which `force_edges` makes run them.
    pub quick: bool,
    /// Resolve edges and importance even when nothing changed (the full
    /// crawl after quick ones).
    pub force_edges: bool,
}

impl CrawlOptions {
    /// Whether the crawl covers this project.
    fn covers(&self, name: &str) -> bool {
        if self.project.is_none() && self.projects.is_empty() {
            return true;
        }
        self.project.as_deref() == Some(name) || self.projects.iter().any(|p| p == name)
    }
}

/// What one project's crawl did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectReport {
    pub name: String,
    pub path: String,
    pub depth: String,
    pub git: bool,
    pub head: String,
    /// Commits since the last crawl (0 when unknown or none).
    pub commits_since: usize,
    pub files: usize,
    pub dirs: usize,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub renamed: usize,
    pub unchanged: usize,
    pub symbols: usize,
    pub sections: usize,
    /// Files listed but not indexed: secrets by name.
    pub secret_skipped: usize,
    /// Files with an entity and no content: too large, binary, symlinks.
    pub no_content: usize,
    pub cochange: bool,
    /// Crawled shallow though deeper was asked: a root found it, and it lists
    /// more than [`LARGE_PROJECT`] files.
    pub capped: bool,
    /// Its walk (outside git) stopped at the file cap: its deepest levels are
    /// missing.
    pub truncated: bool,
    pub ms: u128,
}

/// What a crawl did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CrawlReport {
    pub projects: Vec<ProjectReport>,
    pub sync: roots::Sync,
    pub edges: BTreeMap<String, usize>,
    pub dangling: usize,
    pub search_rows: usize,
    pub queued: usize,
    pub dry_run: bool,
    pub ms: u128,
    /// Milliseconds per step: projects, edges, search, commit.
    pub phases: BTreeMap<String, u128>,
}

/// A project a root found with more files than this is crawled shallow
/// unless `project add` asks for more.
pub const LARGE_PROJECT: usize = 30_000;

/// At most this many symbols are kept from one file (generated headers
/// define tens of thousands).
pub const MAX_SYMBOLS_PER_FILE: usize = 500;

/// The id of a directory in a project ("" is the project itself).
pub fn dir_id(project: &str, rel: &str) -> String {
    if rel.is_empty() {
        project.to_string()
    } else {
        format!("{project}:{rel}")
    }
}

pub(crate) fn parent_rel(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

pub(crate) fn base_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// A short content hash (hex).
pub fn hash(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize()
        .iter()
        .take(10)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The ids starting with a prefix, as a range the primary key can seek:
/// `?1 <= id < ?2`, where ?2 is the prefix with its last character raised
/// (see [`prefix_range`]).
pub const CHILDREN_SQL: &str = "SELECT id FROM entities WHERE id >= ?1 AND id < ?2";

/// The half-open range of strings that start with `prefix` (ASCII `#` and
/// `:` end every prefix used here, so raising the last char is exact).
pub fn prefix_range(prefix: &str) -> (String, String) {
    let mut hi = prefix.to_string();
    let last = hi.pop().unwrap_or('\0');
    hi.push(char::from_u32(last as u32 + 1).unwrap_or(char::MAX));
    (prefix.to_string(), hi)
}

/// One file as the crawl lists it, before reading it.
#[derive(Debug, Clone)]
struct Listed {
    rel: String,
    key: String,
    untracked: bool,
    /// Changed against git's HEAD (always outside git): its dirs' tree ids
    /// don't hold.
    dirty: bool,
    /// Its working-tree content isn't what the index says: it needs a key.
    modified: bool,
    /// Not safe to read: a symlink, or under one.
    symlink: bool,
    submodule: bool,
    size: u64,
    /// Nanoseconds since the epoch.
    mtime: i64,
}

/// The file entity's stored key, mtime and size, to tell what changed.
struct Known {
    key: String,
    mtime: i64,
    size: i64,
    untracked: bool,
}

pub fn crawl(ix: &mut Index, opts: &CrawlOptions) -> Result<CrawlReport> {
    let start = Instant::now();
    let mut report = CrawlReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };
    let tx = ix.conn.transaction()?;
    if !opts.quick {
        report.sync = roots::sync_roots(&tx)?;
    }
    let all = roots::list_projects(&tx)?;
    for p in opts.project.iter().chain(&opts.projects) {
        if !all.iter().any(|x| &x.name == p) {
            anyhow::bail!(
                "no project {p}: agent index root list shows the roots, and agent index project add DIR adds one"
            );
        }
    }
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    let mut moved = false;
    for p in &all {
        if !opts.covers(&p.name) {
            continue;
        }
        let nested: Vec<String> = all
            .iter()
            .filter(|o| o.path.starts_with(&format!("{}/", p.path)))
            .map(|o| o.path[p.path.len() + 1..].to_string())
            .collect();
        let large = if opts.large_project == 0 {
            LARGE_PROJECT
        } else {
            opts.large_project
        };
        let r = crawl_project(&tx, p, &nested, opts.rebuild, large, &mut dirty)?;
        moved |= r.removed + r.renamed > 0 || r.cochange;
        report.projects.push(r);
    }
    let mut lap = start;
    let mut phase = |report: &mut CrawlReport, name: &str| {
        report
            .phases
            .insert(name.to_string(), lap.elapsed().as_millis());
        lap = Instant::now();
    };
    phase(&mut report, "projects");
    // Cross-project steps run over everything (a doc in one project names files
    // in another, a manifest depends on a sibling repo), but only when
    // something changed.
    let changed = opts.rebuild
        || opts.force_edges
        || moved
        || !dirty.is_empty()
        || !report.sync.added.is_empty()
        || !report.sync.removed.is_empty();
    if changed && !opts.quick {
        report.dangling = edges::resolve_edges(&tx)?;
        edges::importance(&tx)?;
    } else {
        report.dangling =
            tx.query_row("SELECT count(*) FROM dangling", [], |r| r.get::<_, i64>(0))? as usize;
    }
    report.edges = edges::edge_counts(&tx)?;
    phase(&mut report, "edges");
    // Search rows hold names, titles, summaries, symbols and labels, which only
    // the crawled entities changed (edges and importance aren't in them).
    let (rows, queued) = refresh_search(&tx, &dirty)?;
    report.search_rows = rows;
    report.queued = queued;
    phase(&mut report, "search");
    if opts.dry_run {
        tx.rollback()?;
    } else {
        tx.commit()?;
    }
    phase(&mut report, "commit");
    report.ms = start.elapsed().as_millis();
    Ok(report)
}

fn known_files(conn: &Connection, project: &str) -> Result<HashMap<String, Known>> {
    let mut st = conn.prepare(
        "SELECT rel, content_key, mtime, size, untracked FROM entities WHERE project = ? AND kind = 'file'",
    )?;
    let rows = st.query_map(params![project], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Known {
                key: r.get(1)?,
                mtime: r.get(2)?,
                size: r.get(3)?,
                untracked: r.get::<_, i64>(4)? != 0,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A file's size and mtime, and whether it is safe to read: a regular file
/// whose canonical path is itself, so no symlink on the way leads out.
struct Stat {
    safe: bool,
    size: u64,
    mtime: i64,
}

fn stat_file(dir: &Path, rel: &str) -> Stat {
    let p = dir.join(rel);
    let md = std::fs::symlink_metadata(&p).ok();
    let safe = md.as_ref().is_some_and(|m| m.file_type().is_file())
        && std::fs::canonicalize(&p).is_ok_and(|c| c == p);
    Stat {
        safe,
        size: md.as_ref().map(|m| m.len()).unwrap_or(0),
        mtime: md
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0),
    }
}

/// A file's bytes, when it is safe to read (see [`stat_file`]) and no larger
/// than 1 MB; never more than that is read.
pub(crate) fn read_inside(dir: &Path, rel: &str) -> Option<Vec<u8>> {
    let st = stat_file(dir, rel);
    if !st.safe || st.size > files::MAX_CONTENT {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(dir.join(rel))
        .ok()?
        .take(files::MAX_CONTENT + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > files::MAX_CONTENT {
        return None;
    }
    Some(buf)
}

/// A key from a file's size and mtime, for one that isn't read.
fn meta_key(what: &str, f: &Listed) -> String {
    hash(&[
        what.as_bytes(),
        &f.size.to_le_bytes(),
        &f.mtime.to_le_bytes(),
    ])
}

/// A project's files; whether it is a git repo; whether a plain walk stopped
/// at its file cap.
fn list(project: &Project, nested: &[String]) -> Result<(Vec<Listed>, bool, bool)> {
    let dir = Path::new(&project.path);
    let in_nested = |rel: &str| {
        nested
            .iter()
            .any(|n| rel == n || rel.starts_with(&format!("{n}/")))
    };
    if git::is_toplevel(dir) {
        let mut out = Vec::new();
        for f in git::list_files(dir)? {
            if in_nested(&f.rel) {
                continue;
            }
            // Build output git doesn't ignore (an old commit's .gitignore,
            // or none): never what a person wrote, as a plain walk skips it.
            if f.untracked && f.rel.split('/').any(agent_dream::skip::contains) {
                continue;
            }
            let submodule = f.mode == "160000";
            let st = stat_file(dir, &f.rel);
            out.push(Listed {
                symlink: !submodule && !st.safe,
                submodule,
                size: st.size,
                mtime: st.mtime,
                rel: f.rel,
                key: f.key,
                untracked: f.untracked,
                dirty: f.dirty,
                modified: f.modified,
            });
        }
        return Ok((out, true, false));
    }
    // A shallow project needs only its top directories' files (READMEs).
    let max_depth = if project.depth == "shallow" { 2 } else { 0 };
    let w = walk::walk(dir, nested, max_depth);
    let out = w
        .files
        .into_iter()
        .map(|f| Listed {
            rel: f.rel,
            key: String::new(),
            untracked: false,
            dirty: true,
            modified: true,
            symlink: f.symlink,
            submodule: false,
            size: f.size,
            mtime: f.mtime,
        })
        .collect();
    Ok((out, false, w.truncated))
}

/// Gives every listed file its key: the index blob for a clean tracked file;
/// the link text's hash for a symlink; size and mtime for a file over 1 MB
/// (and, outside git, for a shallow project's files); `git hash-object` for a
/// changed file in a repo; a content hash outside git, reused while size and
/// mtime hold.
fn key_files(
    dir: &Path,
    is_git: bool,
    shallow: bool,
    listed: &mut [Listed],
    known: &HashMap<String, Known>,
    reread: bool,
) {
    let mut to_hash: Vec<usize> = Vec::new();
    for (i, f) in listed.iter_mut().enumerate() {
        if f.submodule || (is_git && !f.modified && !f.symlink) {
            continue;
        }
        if f.symlink {
            // The link's own text, never what it points at.
            let target = std::fs::read_link(dir.join(&f.rel))
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            f.key = hash(&[b"symlink", target.as_bytes()]);
            continue;
        }
        if f.size > files::MAX_CONTENT {
            f.key = meta_key("large", f);
            continue;
        }
        if is_git {
            to_hash.push(i);
            continue;
        }
        if shallow {
            f.key = meta_key("meta", f);
            continue;
        }
        f.key = match known.get(&f.rel) {
            Some(k)
                if !reread
                    && k.mtime == f.mtime
                    && k.size == f.size as i64
                    && !k.key.is_empty() =>
            {
                k.key.clone()
            }
            _ => match read_inside(dir, &f.rel) {
                Some(b) => hash(&[&b]),
                None => meta_key("unreadable", f),
            },
        };
    }
    if !to_hash.is_empty() {
        let paths: Vec<String> = to_hash.iter().map(|&i| listed[i].rel.clone()).collect();
        let keys = git::hash_paths(dir, &paths);
        for (&i, k) in to_hash.iter().zip(keys) {
            listed[i].key = if k.is_empty() {
                meta_key("unreadable", &listed[i])
            } else {
                k
            };
        }
    }
}

/// The README of a directory among its files, by preference.
fn readme_of<'a>(files_in_dir: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for name in files_in_dir {
        let l = name.to_ascii_lowercase();
        let rank = match l.as_str() {
            "readme.md" => 0,
            "readme.markdown" | "readme.rst" => 1,
            "readme" | "readme.txt" => 2,
            _ => continue,
        };
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, name.to_string()));
        }
    }
    best.map(|(_, n)| n)
}

#[allow(clippy::too_many_arguments)]
fn upsert_entity(
    conn: &Connection,
    id: &str,
    kind: &str,
    parent: &str,
    project: &str,
    rel: &str,
    name: &str,
    path: &str,
    file_kind: &str,
    lang: &str,
    size: i64,
    lines: i64,
    line: i64,
    key: &str,
    mtime: i64,
    title: &str,
    signature: &str,
    untracked: bool,
    no_content: &str,
) -> Result<()> {
    let mut st = conn.prepare_cached(
        "INSERT INTO entities (id, kind, parent, project, rel, name, path, file_kind, lang, size, lines, line, content_key, mtime, title, signature, untracked, no_content, crawled_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(id) DO UPDATE SET kind = ?2, parent = ?3, project = ?4, rel = ?5, name = ?6, path = ?7,
           file_kind = ?8, lang = ?9, size = ?10, lines = ?11, line = ?12, content_key = ?13, mtime = ?14,
           title = ?15, signature = ?16, untracked = ?17, no_content = ?18, crawled_at = ?19",
    )?;
    st.execute(params![
        id,
        kind,
        parent,
        project,
        rel,
        name,
        path,
        file_kind,
        lang,
        size,
        lines,
        line,
        key,
        mtime,
        title,
        signature,
        untracked as i64,
        no_content,
        now()
    ])?;
    Ok(())
}

/// Records a seed summary for an entity when it differs from its last seed
/// (text or key); an empty seed drops the old ones. Whether anything changed.
pub fn set_seed(conn: &Connection, id: &str, key: &str, text: &str) -> Result<bool> {
    let last: Option<(String, String)> = conn
        .prepare_cached(
            "SELECT text, content_key FROM summaries WHERE entity = ? AND writer = 'seed' AND status = 'live' ORDER BY seq DESC LIMIT 1",
        )?
        .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    if text.is_empty() {
        if last.is_some() {
            conn.execute(
                "UPDATE summaries SET status = 'dropped' WHERE entity = ? AND writer = 'seed' AND status = 'live'",
                params![id],
            )?;
            return Ok(true);
        }
        return Ok(false);
    }
    if let Some((t, k)) = &last
        && t == text
    {
        if k != key {
            conn.execute(
                "UPDATE summaries SET content_key = ? WHERE entity = ? AND writer = 'seed' AND status = 'live'",
                params![key, id],
            )?;
        }
        return Ok(false);
    }
    conn.execute(
        "UPDATE summaries SET status = 'dropped' WHERE entity = ? AND writer = 'seed' AND status = 'live'",
        params![id],
    )?;
    conn.execute(
        "INSERT INTO summaries (entity, content_key, text, writer, status, written_at) VALUES (?, ?, ?, 'seed', 'live', ?)",
        params![id, key, text, now()],
    )?;
    Ok(true)
}

/// The ids of a file's symbols and sections.
fn child_ids(conn: &Connection, file_id: &str) -> Result<Vec<String>> {
    let (lo, hi) = prefix_range(&format!("{file_id}#"));
    let mut st = conn.prepare_cached(CHILDREN_SQL)?;
    let ids = st
        .query_map(params![lo, hi], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Deletes a file's symbol and section children, with their crawl-made rows;
/// their summaries stay.
fn delete_children(conn: &Connection, file_id: &str, dirty: &mut BTreeSet<String>) -> Result<()> {
    for id in child_ids(conn, file_id)? {
        delete_entity_rows(conn, &id)?;
        dirty.remove(&id);
    }
    Ok(())
}

/// Deletes an entity's search and search_tri rows, by the rowid they were
/// written at (a delete by the unindexed id column would scan the table).
pub fn delete_search(conn: &Connection, id: &str) -> Result<()> {
    let rid: Option<i64> = conn
        .prepare_cached("SELECT rid FROM search_rowid WHERE id = ?")?
        .query_row(params![id], |r| r.get(0))
        .optional()?;
    if let Some(rid) = rid {
        conn.execute("DELETE FROM search WHERE rowid = ?", params![rid])?;
        conn.execute("DELETE FROM search_tri WHERE rowid = ?", params![rid])?;
        conn.execute("DELETE FROM search_rowid WHERE id = ?", params![id])?;
    }
    Ok(())
}

/// Deletes a file's full-text row, if it has one.
pub fn delete_full_text(conn: &Connection, id: &str) -> Result<()> {
    let rid: Option<i64> = conn
        .prepare_cached("SELECT rid FROM search_full_ids WHERE id = ?")?
        .query_row(params![id], |r| r.get(0))
        .optional()?;
    if let Some(rid) = rid {
        conn.execute("DELETE FROM search_full WHERE rowid = ?", params![rid])?;
        conn.execute("DELETE FROM search_full_ids WHERE rid = ?", params![rid])?;
    }
    Ok(())
}

/// Indexes a file's text for the full-text list (replacing what it had).
fn set_full_text(conn: &Connection, id: &str, text: &str) -> Result<()> {
    delete_full_text(conn, id)?;
    conn.prepare_cached("INSERT INTO search_full_ids (id) VALUES (?)")?
        .execute(params![id])?;
    let rid = conn.last_insert_rowid();
    conn.prepare_cached("INSERT INTO search_full (rowid, text) VALUES (?, ?)")?
        .execute(params![rid, text])?;
    Ok(())
}

/// Removes one entity's crawl-made rows: the entity, its search rows, code
/// labels and edges, queue and vectors. Summaries and non-code edges and
/// labels stay, keyed by id, for a rename to carry or the file to come back.
fn delete_entity_rows(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM entities WHERE id = ?", params![id])?;
    delete_search(conn, id)?;
    delete_full_text(conn, id)?;
    conn.execute(
        "DELETE FROM labels WHERE entity = ? AND origin = 'code' AND kind != 'old'",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM edges WHERE origin = 'code' AND source = ?1",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM edges WHERE origin = 'code' AND target = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM refs WHERE doc = ?", params![id])?;
    conn.execute("DELETE FROM dangling WHERE doc = ?", params![id])?;
    conn.execute("DELETE FROM manifests WHERE file = ?", params![id])?;
    conn.execute("DELETE FROM embed_queue WHERE entity_id = ?", params![id])?;
    conn.execute("DELETE FROM vectors WHERE entity_id = ?", params![id])?;
    Ok(())
}

/// Moves what can't be rebuilt from an old id to a new one (summaries, model
/// and person edges, non-code labels), and keeps the old id as an alias and an
/// `old` label.
pub fn carry(conn: &Connection, old: &str, new: &str) -> Result<()> {
    conn.execute(
        "UPDATE summaries SET entity = ? WHERE entity = ? AND writer != 'seed'",
        params![new, old],
    )?;
    conn.execute(
        "UPDATE OR IGNORE edges SET source = ? WHERE source = ? AND origin != 'code'",
        params![new, old],
    )?;
    conn.execute(
        "UPDATE OR IGNORE edges SET target = ? WHERE target = ? AND origin != 'code'",
        params![new, old],
    )?;
    conn.execute(
        "UPDATE OR IGNORE labels SET entity = ? WHERE entity = ? AND (origin != 'code' OR kind = 'old')",
        params![new, old],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO aliases (old_id, new_id, at) VALUES (?, ?, ?)",
        params![old, new, now()],
    )?;
    // An alias that pointed at the old id now points at the new one.
    conn.execute(
        "UPDATE aliases SET new_id = ? WHERE new_id = ?",
        params![new, old],
    )?;
    conn.execute("DELETE FROM aliases WHERE old_id = new_id", [])?;
    // Normalized as every label is, so a search for the old id finds it.
    conn.execute(
        "INSERT OR IGNORE INTO labels (entity, label, norm, compact, kind, origin, weight, evidence)
         VALUES (?, ?, ?, ?, 'old', 'code', 0.5, 'renamed')",
        params![new, old, names::norm(old), names::compact(old)],
    )?;
    Ok(())
}

/// The ids under `old#` that something can't-be-rebuilt names: a non-seed
/// summary, a person's or glossary's label, an old-id label, a model or
/// person edge.
fn carried_children(conn: &Connection, old: &str) -> Result<BTreeSet<String>> {
    let (lo, hi) = prefix_range(&format!("{old}#"));
    let mut out = BTreeSet::new();
    for sql in [
        "SELECT DISTINCT entity FROM summaries WHERE entity >= ?1 AND entity < ?2 AND writer != 'seed'",
        "SELECT DISTINCT entity FROM labels WHERE entity >= ?1 AND entity < ?2 AND (origin != 'code' OR kind = 'old')",
        "SELECT DISTINCT source FROM edges WHERE source >= ?1 AND source < ?2 AND origin != 'code'",
        "SELECT DISTINCT target FROM edges WHERE target >= ?1 AND target < ?2 AND origin != 'code'",
    ] {
        let mut st = conn.prepare(sql)?;
        for id in st.query_map(params![lo, hi], |r| r.get::<_, String>(0))? {
            out.insert(id?);
        }
    }
    Ok(out)
}

/// Carries a file (or dir) and every child id something names.
fn carry_tree(conn: &Connection, old: &str, new: &str, dirty: &mut BTreeSet<String>) -> Result<()> {
    carry(conn, old, new)?;
    dirty.insert(new.to_string());
    for k in carried_children(conn, old)? {
        let nk = format!("{new}{}", &k[old.len()..]);
        carry(conn, &k, &nk)?;
        dirty.insert(nk);
    }
    Ok(())
}

/// Every ancestor dir of a path ("" last).
fn ancestors(rel: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut d = parent_rel(rel);
    loop {
        out.push(d);
        if d.is_empty() {
            break;
        }
        d = parent_rel(d);
    }
    out
}

fn crawl_project(
    conn: &Connection,
    p: &Project,
    nested: &[String],
    rebuild: bool,
    large: usize,
    dirty: &mut BTreeSet<String>,
) -> Result<ProjectReport> {
    let start = Instant::now();
    let dir = Path::new(&p.path);
    let mut rep = ProjectReport {
        name: p.name.clone(),
        path: p.path.clone(),
        depth: p.depth.clone(),
        ..Default::default()
    };
    let (mut listed, is_git, truncated) = list(p, nested)?;
    rep.truncated = truncated;
    // A project a root found (not one a person added) that turns out huge (a
    // kernel tree, a shelf of vendored code) is crawled shallow: the project
    // and its top directories. Its symbols, full text and file list are what
    // make an index of a dev tree gigabytes, and an embedder hours.
    let depth = if !p.explicit && p.depth != "shallow" && listed.len() > large {
        rep.capped = true;
        "shallow"
    } else {
        p.depth.as_str()
    };
    rep.depth = depth.to_string();
    // A depth changed since the last crawl reads every file again, as
    // --rebuild does, so the two agree.
    let reread = rebuild || (!p.crawled_depth.is_empty() && p.crawled_depth != depth);
    let shallow = depth == "shallow";
    let symbols = depth == "full";
    rep.git = is_git;
    let head = if is_git {
        git::head(dir)
    } else {
        String::new()
    };
    rep.head = head.clone();
    let known = known_files(conn, &p.name)?;
    listed.retain(|f| {
        let secret = files::is_secret(base_name(&f.rel));
        if secret {
            rep.secret_skipped += 1;
        }
        !secret
    });
    key_files(dir, is_git, shallow, &mut listed, &known, reread);

    // Renames: committed since the last crawl, and a `git mv` not committed yet.
    let mut renames: HashMap<String, String> = HashMap::new();
    if is_git {
        if !p.last_commit.is_empty() && !head.is_empty() && p.last_commit != head {
            rep.commits_since = git::commits_between(dir, &p.last_commit, &head);
            renames.extend(git::renames(dir, &p.last_commit, &head));
        }
        renames.extend(git::worktree_renames(dir));
    }

    // Dirs whose HEAD tree id no longer holds: above a changed, new or
    // deleted path.
    let mut dirty_dirs: HashSet<String> = HashSet::new();
    let mut all_dirs: BTreeSet<String> = BTreeSet::new();
    all_dirs.insert(String::new());
    if is_git {
        for path in git::changed_vs_head(dir) {
            dirty_dirs.extend(ancestors(&path).into_iter().map(str::to_string));
        }
    }
    let mut by_dir: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for f in &listed {
        for d in ancestors(&f.rel) {
            all_dirs.insert(d.to_string());
            if f.dirty {
                dirty_dirs.insert(d.to_string());
            }
        }
        by_dir
            .entry(parent_rel(&f.rel).to_string())
            .or_default()
            .push((base_name(&f.rel).to_string(), f.key.clone()));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut manifests_seen: HashSet<String> = HashSet::new();
    // Files new to the index, by key, to match a gone file's content.
    let mut new_by_key: HashMap<String, Vec<String>> = HashMap::new();
    if !shallow {
        for f in &listed {
            let name = base_name(&f.rel);
            rep.files += 1;
            let id = format!("{}:{}", p.name, f.rel);
            seen.insert(f.rel.clone());
            if manifest::is_manifest(name) {
                manifests_seen.insert(id.clone());
            }
            let prev = known.get(&f.rel);
            let unchanged = !reread
                && prev.is_some_and(|k| {
                    k.key == f.key && k.untracked == f.untracked && !f.key.is_empty()
                });
            if unchanged {
                rep.unchanged += 1;
                continue;
            }
            if prev.is_some() {
                rep.changed += 1;
            } else {
                rep.added += 1;
                new_by_key
                    .entry(f.key.clone())
                    .or_default()
                    .push(f.rel.clone());
            }
            read_file(conn, p, f, &id, symbols, &mut rep, dirty)?;
        }
    }

    // Files gone: carry renamed ones (by git, or else by identical content),
    // then remove the rest.
    let mut gone: Vec<&String> = known.keys().filter(|rel| !seen.contains(*rel)).collect();
    gone.sort();
    let mut gone_by_key: HashMap<&str, usize> = HashMap::new();
    for rel in &gone {
        *gone_by_key.entry(known[*rel].key.as_str()).or_insert(0) += 1;
    }
    let mut carried: Vec<(String, String)> = Vec::new();
    for rel in gone {
        let old_id = format!("{}:{rel}", p.name);
        let k = &known[rel].key;
        let by_content = || {
            let c = new_by_key.get(k)?;
            (c.len() == 1 && gone_by_key.get(k.as_str()) == Some(&1) && !k.is_empty())
                .then(|| c[0].clone())
        };
        let new_rel = renames
            .get(rel)
            .filter(|n| seen.contains(*n))
            .cloned()
            .or_else(by_content);
        delete_children(conn, &old_id, dirty)?;
        delete_entity_rows(conn, &old_id)?;
        dirty.remove(&old_id);
        match new_rel {
            Some(n) => {
                carry_tree(conn, &old_id, &format!("{}:{n}", p.name), dirty)?;
                carried.push((rel.clone(), n));
                rep.renamed += 1;
            }
            None => rep.removed += 1,
        }
    }
    // Manifests whose file is gone or no longer one.
    {
        let stale: Vec<String> = {
            let mut st = conn.prepare("SELECT file FROM manifests WHERE project = ?")?;
            st.query_map(params![p.name], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for f in stale {
            if !manifests_seen.contains(&f) {
                conn.execute("DELETE FROM manifests WHERE file = ?", params![f])?;
            }
        }
    }

    // Directories: tree ids where git's HEAD still describes them, else a hash
    // of their listing.
    let trees = if is_git {
        git::tree_ids(dir)
    } else {
        HashMap::new()
    };
    let dirs: Vec<String> = if shallow {
        all_dirs
            .iter()
            .filter(|d| d.is_empty() || !d.contains('/'))
            .cloned()
            .collect()
    } else {
        all_dirs.iter().cloned().collect()
    };
    let mut subdirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in &all_dirs {
        if !d.is_empty() {
            subdirs
                .entry(parent_rel(d).to_string())
                .or_default()
                .push(d.clone());
        }
    }
    // Keys bottom-up (deepest first), so a listing hash can use its subdirs' keys.
    let mut dir_keys: HashMap<String, String> = HashMap::new();
    let mut ordered: Vec<&String> = all_dirs.iter().collect();
    ordered.sort_by_key(|d| {
        std::cmp::Reverse(if d.is_empty() {
            0
        } else {
            d.matches('/').count() + 1
        })
    });
    for d in ordered {
        let key = match trees.get(d.as_str()) {
            Some(t) if !dirty_dirs.contains(d.as_str()) => t.clone(),
            _ => {
                let mut parts: Vec<String> = Vec::new();
                for (n, k) in by_dir.get(d.as_str()).into_iter().flatten() {
                    parts.push(format!("f {n} {k}"));
                }
                for s in subdirs.get(d.as_str()).into_iter().flatten() {
                    parts.push(format!(
                        "d {} {}",
                        base_name(s),
                        dir_keys.get(s).map(String::as_str).unwrap_or("")
                    ));
                }
                parts.sort();
                hash(&[parts.join("\n").as_bytes()])
            }
        };
        dir_keys.insert(d.clone(), key);
    }
    // A dir renamed whole: the files carried from under it all went to one
    // new dir with the same path below it.
    let mut dir_moves: HashMap<String, HashSet<String>> = HashMap::new();
    for (old, new) in &carried {
        let (mut o, mut n) = (parent_rel(old), parent_rel(new));
        while !o.is_empty() && !n.is_empty() && o != n {
            dir_moves
                .entry(o.to_string())
                .or_default()
                .insert(n.to_string());
            if base_name(o) != base_name(n) {
                break;
            }
            (o, n) = (parent_rel(o), parent_rel(n));
        }
    }
    // Remove dirs that are gone, carrying a renamed one.
    {
        let have: Vec<(String, String)> = {
            let mut st =
                conn.prepare("SELECT id, rel FROM entities WHERE project = ? AND kind = 'dir'")?;
            st.query_map(params![p.name], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let keep: HashSet<&String> = dirs.iter().collect();
        for (id, rel) in have {
            if keep.contains(&rel) {
                continue;
            }
            delete_entity_rows(conn, &id)?;
            dirty.remove(&id);
            if let Some(targets) = dir_moves.get(&rel)
                && targets.len() == 1
                && let Some(t) = targets.iter().next()
                && keep.contains(t)
            {
                carry_tree(conn, &id, &dir_id(&p.name, t), dirty)?;
            }
        }
    }
    let readme_seed = |conn: &Connection, d: &str| -> Result<(Option<String>, Option<String>)> {
        let names = by_dir
            .get(d)
            .map(|v| v.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>());
        let Some(readme) = names.and_then(|n| readme_of(n.into_iter())) else {
            return Ok((None, None));
        };
        let rel = if d.is_empty() {
            readme.clone()
        } else {
            format!("{d}/{readme}")
        };
        if shallow {
            // No file entities: read the README itself, if it is safe to.
            let Some(bytes) = read_inside(dir, &rel) else {
                return Ok((None, None));
            };
            let text = String::from_utf8_lossy(&bytes);
            let x = extract::extract(&rel, "markdown", &text, false);
            return Ok((x.title, x.seed));
        }
        let id = format!("{}:{rel}", p.name);
        let title: Option<String> = conn
            .query_row(
                "SELECT title FROM entities WHERE id = ?",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let seed: Option<String> = conn
            .query_row(
                "SELECT text FROM summaries WHERE entity = ? AND writer = 'seed' AND status = 'live' ORDER BY seq DESC LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok((
            title.filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case(&readme)),
            seed,
        ))
    };
    let remote = if is_git {
        git::remote(dir)
    } else {
        String::new()
    };
    for d in &dirs {
        let id = dir_id(&p.name, d);
        let key = dir_keys.get(d).cloned().unwrap_or_default();
        let (readme_title, readme_para) = readme_seed(conn, d)?;
        let man: Option<(String, String)> = conn
            .query_row(
                "SELECT name, data FROM manifests WHERE dir = ? ORDER BY (ecosystem = 'cargo') DESC LIMIT 1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let man_desc = man
            .as_ref()
            .and_then(|(_, data)| serde_json::from_str::<Manifest>(data).ok())
            .map(|m| m.description)
            .filter(|s| !s.is_empty());
        let name = if d.is_empty() {
            p.name.clone()
        } else {
            base_name(d).to_string()
        };
        let title = readme_title.clone().unwrap_or_else(|| name.clone());
        // A crate's description says what it is in one line; a README says more.
        let seed = match (&man_desc, &readme_para) {
            (Some(m), Some(r)) => format!("{m}. {r}").replace("..", "."),
            (Some(m), None) => m.clone(),
            (None, Some(r)) => r.clone(),
            (None, None) => String::new(),
        };
        let seed = extract::clip(&seed, extract::SEED_MAX);
        let (kind, parent, path) = if d.is_empty() {
            ("project", p.root.clone(), p.path.clone())
        } else {
            (
                "dir",
                dir_id(&p.name, parent_rel(d)),
                dir.join(d).to_string_lossy().into_owned(),
            )
        };
        let prev: Option<(String, String)> = conn
            .query_row(
                "SELECT content_key, title FROM entities WHERE id = ?",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        upsert_entity(
            conn, &id, kind, &parent, &p.name, d, &name, &path, "", "", 0, 0, 0, &key, 0, &title,
            "", false, "",
        )?;
        let seed_changed = set_seed(conn, &id, &key, &seed)?;
        // Labels from code: the name's spellings, the README title, the package
        // name, the remote's name.
        let mut labels: Vec<(String, &str, &str)> = Vec::new();
        for v in names::variants(&name) {
            labels.push((v, if d.is_empty() { "pref" } else { "alt" }, "name"));
        }
        if let Some(t) = &readme_title {
            labels.push((t.clone(), "alt", "readme title"));
        }
        if let Some((pkg, _)) = &man
            && !pkg.is_empty()
        {
            for v in names::variants(pkg) {
                labels.push((v, "alt", "package name"));
            }
        }
        if d.is_empty()
            && let Some(last) = remote.rsplit('/').next()
            && !last.is_empty()
        {
            labels.push((last.to_string(), "alt", "remote name"));
        }
        let labels_changed = sync_code_labels(conn, &id, &labels)?;
        let changed = prev.as_ref().is_none_or(|(k, t)| k != &key || t != &title);
        if reread || seed_changed || labels_changed || changed {
            dirty.insert(id);
        }
    }
    rep.dirs = dirs.len().saturating_sub(1);

    // Co-change: every 100 commits or a week, or when there is none yet.
    if is_git && !head.is_empty() && !shallow {
        let due = rebuild
            || p.cochange_commit.is_empty()
            || git::commits_between(dir, &p.cochange_commit, &head) >= 100
            || chrono::DateTime::parse_from_rfc3339(&p.cochange_at)
                .map(|t| chrono::Utc::now().signed_duration_since(t) > chrono::Duration::days(7))
                .unwrap_or(true);
        if due {
            edges::cochange(conn, p, dir)?;
            rep.cochange = true;
            conn.execute(
                "UPDATE projects SET cochange_commit = ?, cochange_at = ? WHERE name = ?",
                params![head, now(), p.name],
            )?;
        }
    }
    if reread || rep.added + rep.changed + rep.removed + rep.renamed > 0 {
        edges::tests_edges(conn, &p.name)?;
    }

    let root_commit = if is_git {
        git::root_commit(dir)
    } else {
        String::new()
    };
    conn.execute(
        "UPDATE projects SET is_git = ?, root_commit = ?, remote = ?, last_commit = ?, last_crawled_at = ?, crawled_depth = ? WHERE name = ?",
        params![is_git as i64, root_commit, remote, head, now(), depth, p.name],
    )?;
    rep.ms = start.elapsed().as_millis();
    Ok(rep)
}

/// Makes an entity's code labels (but old ids) exactly these; whether they
/// changed.
fn sync_code_labels(conn: &Connection, id: &str, labels: &[(String, &str, &str)]) -> Result<bool> {
    let have: BTreeSet<(String, String)> = {
        let mut st = conn.prepare_cached(
            "SELECT label, kind FROM labels WHERE entity = ? AND origin = 'code' AND kind != 'old'",
        )?;
        st.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let want: BTreeSet<(String, String)> = labels
        .iter()
        .filter(|(l, ..)| !names::norm(l).is_empty())
        .map(|(l, k, _)| (l.clone(), k.to_string()))
        .collect();
    if have == want {
        return Ok(false);
    }
    conn.execute(
        "DELETE FROM labels WHERE entity = ? AND origin = 'code' AND kind != 'old'",
        params![id],
    )?;
    for (label, kind, evidence) in labels {
        add_label(conn, id, label, kind, "code", 1.0, evidence)?;
    }
    Ok(true)
}

pub fn add_label(
    conn: &Connection,
    entity: &str,
    label: &str,
    kind: &str,
    origin: &str,
    weight: f64,
    evidence: &str,
) -> Result<()> {
    let norm = names::norm(label);
    if norm.is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO labels (entity, label, norm, compact, kind, origin, weight, evidence)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(entity, label, kind) DO UPDATE SET origin = excluded.origin, weight = excluded.weight, evidence = excluded.evidence",
        params![entity, label, norm, names::compact(label), kind, origin, weight, evidence],
    )?;
    Ok(())
}

/// Reads one changed file: its entity, seed, symbols and sections (kept by id,
/// so an unchanged symbol keeps its vector and embedding key), references and
/// manifest.
fn read_file(
    conn: &Connection,
    p: &Project,
    f: &Listed,
    id: &str,
    symbols: bool,
    rep: &mut ProjectReport,
    dirty: &mut BTreeSet<String>,
) -> Result<()> {
    let dir = Path::new(&p.path);
    let abs = dir.join(&f.rel);
    let name = base_name(&f.rel);
    let file_kind = if f.submodule {
        "submodule"
    } else {
        files::file_kind(&f.rel)
    };
    let lang = files::language(name);
    let parent = dir_id(&p.name, parent_rel(&f.rel));
    conn.execute("DELETE FROM refs WHERE doc = ?", params![id])?;
    conn.execute("DELETE FROM manifests WHERE file = ?", params![id])?;

    let mut no_content = String::new();
    let mut x = Extracted::default();
    let mut text = String::new();
    if f.submodule {
        no_content = "submodule".into();
    } else if f.symlink {
        // Never followed: an index that reads files must not reach outside what
        // it indexes.
        no_content = match std::fs::canonicalize(&abs) {
            Ok(t) if t.starts_with(dir) => "symlink".into(),
            _ => "symlink outside the project".into(),
        };
    } else if f.size > files::MAX_CONTENT {
        no_content = "over 1 MB".into();
    } else {
        match read_inside(dir, &f.rel) {
            Some(bytes) if files::is_binary(&bytes) => no_content = "binary".into(),
            Some(bytes) => {
                text = String::from_utf8_lossy(&bytes).into_owned();
                x = extract::extract(&f.rel, lang, &text, symbols);
            }
            None => no_content = "unreadable".into(),
        }
    }
    if !no_content.is_empty() {
        rep.no_content += 1;
    }
    // Full text, for full-depth projects and the files people read (not
    // fixtures, data or assets).
    if symbols
        && no_content.is_empty()
        && matches!(file_kind, "source" | "test" | "doc" | "config" | "build")
    {
        set_full_text(conn, id, &text)?;
    } else {
        delete_full_text(conn, id)?;
    }
    let title = x.title.clone().unwrap_or_default();
    let abs_s = abs.to_string_lossy().into_owned();
    upsert_entity(
        conn,
        id,
        "file",
        &parent,
        &p.name,
        &f.rel,
        name,
        &abs_s,
        file_kind,
        lang,
        f.size as i64,
        x.lines as i64,
        0,
        &f.key,
        f.mtime,
        &title,
        "",
        f.untracked,
        &no_content,
    )?;
    set_seed(conn, id, &f.key, x.seed.as_deref().unwrap_or(""))?;
    dirty.insert(id.to_string());

    let old_kids: HashSet<String> = child_ids(conn, id)?.into_iter().collect();
    let mut new_kids: HashSet<String> = HashSet::new();
    // Symbols: ids by name, made unique in the file with -2, -3.
    let mut used: HashMap<String, usize> = HashMap::new();
    for s in x.symbols.iter().take(MAX_SYMBOLS_PER_FILE) {
        let base = s.name.replace(' ', "-");
        let n = used.entry(base.clone()).or_insert(0);
        *n += 1;
        let frag = if *n == 1 { base } else { format!("{base}-{n}") };
        let sid = format!("{id}#{frag}");
        let skey = hash(&[s.signature.as_bytes(), s.doc.as_bytes()]);
        upsert_entity(
            conn,
            &sid,
            "symbol",
            id,
            &p.name,
            &f.rel,
            &s.name,
            &abs_s,
            &s.kind,
            lang,
            0,
            0,
            s.line as i64,
            &skey,
            0,
            // A symbol's title is its name (display_title): the signature
            // isn't stored twice.
            "",
            &s.signature,
            f.untracked,
            "",
        )?;
        set_seed(conn, &sid, &skey, &s.doc)?;
        dirty.insert(sid.clone());
        new_kids.insert(sid);
        rep.symbols += 1;
    }
    // Sections, nested under the nearest higher heading.
    if symbols || lang == "markdown" {
        let mut stack: Vec<(usize, String)> = Vec::new();
        for s in &x.sections {
            while stack.last().is_some_and(|(l, _)| *l >= s.level) {
                stack.pop();
            }
            let parent = stack
                .last()
                .map(|(_, i)| i.clone())
                .unwrap_or_else(|| id.to_string());
            let sid = format!("{id}#{}", s.slug);
            if new_kids.contains(&sid) {
                continue;
            }
            let skey = hash(&[s.heading.as_bytes(), s.seed.as_bytes()]);
            upsert_entity(
                conn,
                &sid,
                "section",
                &parent,
                &p.name,
                &f.rel,
                &s.heading,
                &abs_s,
                "",
                "markdown",
                0,
                0,
                s.line as i64,
                &skey,
                0,
                &s.heading,
                "",
                f.untracked,
                "",
            )?;
            set_seed(conn, &sid, &skey, &s.seed)?;
            dirty.insert(sid.clone());
            new_kids.insert(sid.clone());
            stack.push((s.level, sid));
            rep.sections += 1;
        }
    }
    for gone in old_kids.difference(&new_kids) {
        delete_entity_rows(conn, gone)?;
        dirty.remove(gone);
    }
    for (r, line) in &x.refs {
        conn.execute(
            "INSERT OR IGNORE INTO refs (doc, ref, line) VALUES (?, ?, ?)",
            params![id, r, *line as i64],
        )?;
    }
    if manifest::is_manifest(name)
        && let Some(m) = manifest::parse(name, &text)
    {
        conn.execute(
            "INSERT OR REPLACE INTO manifests (file, dir, project, ecosystem, name, data) VALUES (?, ?, ?, ?, ?, ?)",
            params![id, parent, p.name, m.ecosystem, m.name, serde_json::to_string(&m)?],
        )?;
        // The dir's labels and seed change with it.
        dirty.insert(parent);
    }
    Ok(())
}

/// The file name a test's name says it tests (see [`edges::tested_name`]).
pub fn tested_name(name: &str) -> Option<String> {
    edges::tested_name(name)
}

/// A path with `.` and `..` folded, as a string, without touching the disk.
pub fn normalize(p: &Path) -> String {
    let mut out: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            std::path::Component::RootDir => out.clear(),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
            std::path::Component::Prefix(_) => {}
        }
    }
    format!("/{}", out.join("/"))
}

/// The text an entity's search rows and embedding are made from.
pub struct SearchText {
    pub title: String,
    pub name: String,
    pub path: String,
    pub summary: String,
    pub symbols: String,
    pub why: String,
    pub labels: String,
}

/// A path's words for the path column: the path itself (which the tokenizer
/// splits at `/` and `.`), plus the camelCase and `_` splits it doesn't make,
/// each once.
fn path_words(rel: &str) -> String {
    let tokens: HashSet<String> = rel
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut extra: Vec<String> = Vec::new();
    for w in names::split_words(rel) {
        if !tokens.contains(&w) && !extra.contains(&w) {
            extra.push(w);
        }
    }
    if extra.is_empty() {
        rel.to_string()
    } else {
        format!("{rel} {}", extra.join(" "))
    }
}

pub fn search_text(conn: &Connection, id: &str) -> Result<Option<SearchText>> {
    let Some(e) = crate::model::get(conn, id)? else {
        return Ok(None);
    };
    let s = crate::model::summary(conn, id)?;
    let labels: Vec<String> = {
        let mut st =
            conn.prepare_cached("SELECT label FROM labels WHERE entity = ? ORDER BY label")?;
        st.query_map(params![id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    // A file's symbols; a dir's or project's children's names.
    let kids: Vec<String> = {
        let mut st = conn.prepare_cached(
            "SELECT name FROM entities WHERE parent = ? AND kind IN ('symbol', 'file', 'dir') ORDER BY importance DESC, name LIMIT 200",
        )?;
        st.query_map(params![id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let symbols = kids
        .iter()
        .map(|k| names::whole_and_split(k))
        .collect::<Vec<_>>()
        .join(" ");
    let name_text = match e.kind.as_str() {
        // Every word of `impl Display for Config`.
        "symbol" => e
            .name
            .split_whitespace()
            .map(names::whole_and_split)
            .collect::<Vec<_>>()
            .join(" "),
        // A file's stem whole and split, and its extension, each once
        // (`gather_global.rs` → `gather_global gather global rs`).
        "file" => match e.name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => {
                format!("{} {}", names::whole_and_split(stem), ext.to_lowercase())
            }
            _ => names::whole_and_split(&e.name),
        },
        _ => names::whole_and_split(&e.name),
    };
    let rel = if e.rel.is_empty() {
        e.project.clone()
    } else {
        e.rel.clone()
    };
    Ok(Some(SearchText {
        title: if e.kind == "symbol" {
            String::new()
        } else {
            e.title.clone()
        },
        name: name_text,
        path: path_words(&rel),
        summary: s.as_ref().map(|s| s.text.clone()).unwrap_or_default(),
        symbols,
        why: s.map(|s| s.why).unwrap_or_default(),
        labels: labels.join(" | "),
    }))
}

/// Whether the embedder embeds an entity, so the crawl queues it: the kinds
/// it embeds (not symbols, of which there are millions in a large tree),
/// and not test data. The same rule as `embed::wants_vector`, which is
/// built only with the embed feature.
pub fn wants_embedding(kind: &str, rel: &str, file_kind: &str) -> bool {
    matches!(kind, "project" | "dir" | "file" | "section" | "concept")
        && file_kind != "fixture"
        && !(kind == "file" && files::is_fixture(rel))
        && !(kind == "dir" && files::is_fixture(&format!("{rel}/x")))
}

/// Rewrites the search rows of the given entities, and queues those whose
/// embedding text changed. Returns (rows written, entities queued).
pub fn refresh_search(conn: &Connection, ids: &BTreeSet<String>) -> Result<(usize, usize)> {
    let mut rows = 0;
    let mut queued = 0;
    for id in ids {
        delete_search(conn, id)?;
        let Some(t) = search_text(conn, id)? else {
            continue;
        };
        conn.prepare_cached(
            "INSERT INTO search (title, name, path, summary, symbols, why, labels) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![t.title, t.name, t.path, t.summary, t.symbols, t.why, t.labels])?;
        let rid = conn.last_insert_rowid();
        conn.prepare_cached("INSERT OR REPLACE INTO search_rowid (id, rid) VALUES (?, ?)")?
            .execute(params![id, rid])?;
        let (rel, name, old, kind, file_kind): (String, String, String, String, String) = conn
            .prepare_cached(
                "SELECT rel, name, text_key, kind, file_kind FROM entities WHERE id = ?",
            )?
            .query_row(params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
        // Symbols are many and their names are found whole by the porter
        // table and the exact-name stage: no trigrams for them.
        if kind != "symbol" {
            conn.prepare_cached("INSERT INTO search_tri (rowid, text) VALUES (?, ?)")?
                .execute(params![rid, format!("{name} {rel} {}", t.labels)])?;
        }
        rows += 1;
        let text_key = hash(&[
            t.title.as_bytes(),
            t.name.as_bytes(),
            t.path.as_bytes(),
            t.summary.as_bytes(),
            t.why.as_bytes(),
            t.labels.as_bytes(),
            t.symbols.as_bytes(),
        ]);
        if old != text_key {
            conn.execute(
                "UPDATE entities SET text_key = ? WHERE id = ?",
                params![text_key, id],
            )?;
            if !wants_embedding(&kind, &rel, &file_kind) {
                continue;
            }
            conn.execute(
                "INSERT OR REPLACE INTO embed_queue (entity_id, text_key, reason, queued_at) VALUES (?, ?, ?, ?)",
                params![id, text_key, if old.is_empty() { "new" } else { "changed" }, now()],
            )?;
            queued += 1;
        }
    }
    Ok((rows, queued))
}

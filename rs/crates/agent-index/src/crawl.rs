//! The crawl (docs/INDEX.md, "The crawl"): code, always cheap. For each
//! project it lists files with their content keys (git blob ids, or content
//! hashes outside git), re-reads only what changed, carries renamed
//! entities' summaries, labels and edges over (keeping old ids as aliases),
//! then rebuilds the edges, importance, search rows and embed queue.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::{Index, now};
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
    /// Milliseconds per step: projects, edges, importance, search, commit.
    pub phases: BTreeMap<String, u128>,
}

/// The id of a directory in a project ("" is the project itself).
pub fn dir_id(project: &str, rel: &str) -> String {
    if rel.is_empty() {
        project.to_string()
    } else {
        format!("{project}:{rel}")
    }
}

fn parent_rel(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

fn base_name(rel: &str) -> &str {
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

/// One file as the crawl lists it, before reading it.
#[derive(Debug, Clone)]
struct Listed {
    rel: String,
    key: String,
    untracked: bool,
    dirty: bool,
    symlink: bool,
    submodule: bool,
    size: u64,
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
    let ix_tx = &tx;
    report.sync = roots::sync_roots(ix_tx)?;
    let all = roots::list_projects(ix_tx)?;
    if let Some(p) = &opts.project
        && !all.iter().any(|x| &x.name == p)
    {
        anyhow::bail!(
            "no project {p}: agent index root list shows the roots, and agent index project add DIR adds one"
        );
    }
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    for p in &all {
        if opts.project.as_ref().is_some_and(|x| x != &p.name) {
            continue;
        }
        let nested: Vec<String> = all
            .iter()
            .filter(|o| o.path.starts_with(&format!("{}/", p.path)))
            .map(|o| o.path[p.path.len() + 1..].to_string())
            .collect();
        let r = crawl_project(ix_tx, p, &nested, opts.rebuild, &mut dirty)?;
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
    // Cross-project steps run over everything: a doc in one project names files
    // in another, a manifest depends on a sibling repo.
    let (edges, dangling) = resolve_edges(ix_tx)?;
    report.edges = edges;
    report.dangling = dangling;
    phase(&mut report, "edges");
    importance(ix_tx)?;
    phase(&mut report, "importance");
    // Search rows hold names, titles, summaries, symbols and labels, which only
    // the crawled entities changed (edges and importance aren't in them).
    let (rows, queued) = refresh_search(ix_tx, &dirty)?;
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

fn list(project: &Project, nested: &[String]) -> Result<(Vec<Listed>, bool)> {
    let dir = Path::new(&project.path);
    let in_nested = |rel: &str| {
        nested
            .iter()
            .any(|n| rel == n || rel.starts_with(&format!("{n}/")))
    };
    if git::is_toplevel(dir) {
        let files = git::list_files(dir)?;
        let out = files
            .into_iter()
            .filter(|f| !in_nested(&f.rel))
            .map(|f| {
                let md = std::fs::symlink_metadata(dir.join(&f.rel)).ok();
                Listed {
                    // An untracked symlink is listed without its mode.
                    symlink: f.mode == "120000"
                        || md.as_ref().is_some_and(|m| m.file_type().is_symlink()),
                    submodule: f.mode == "160000",
                    size: md.as_ref().map(|m| m.len()).unwrap_or(0),
                    mtime: 0,
                    rel: f.rel,
                    key: f.key,
                    untracked: f.untracked,
                    dirty: f.dirty,
                }
            })
            .collect();
        return Ok((out, true));
    }
    let files = walk::walk(dir, nested);
    Ok((
        files
            .into_iter()
            .map(|f| Listed {
                rel: f.rel,
                key: String::new(),
                untracked: false,
                dirty: true,
                symlink: f.symlink,
                submodule: false,
                size: f.size,
                mtime: f.mtime,
            })
            .collect(),
        false,
    ))
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
    conn.execute(
        "INSERT INTO entities (id, kind, parent, project, rel, name, path, file_kind, lang, size, lines, line, content_key, mtime, title, signature, untracked, no_content, crawled_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(id) DO UPDATE SET kind = ?2, parent = ?3, project = ?4, rel = ?5, name = ?6, path = ?7,
           file_kind = ?8, lang = ?9, size = ?10, lines = ?11, line = ?12, content_key = ?13, mtime = ?14,
           title = ?15, signature = ?16, untracked = ?17, no_content = ?18, crawled_at = ?19",
        params![
            id, kind, parent, project, rel, name, path, file_kind, lang, size, lines, line, key, mtime,
            title, signature, untracked as i64, no_content, now()
        ],
    )?;
    Ok(())
}

/// Records a seed summary for an entity when it differs from its last seed
/// (text or key); an empty seed drops the old ones.
pub fn set_seed(conn: &Connection, id: &str, key: &str, text: &str) -> Result<bool> {
    let last: Option<(String, String)> = conn
        .query_row(
            "SELECT text, content_key FROM summaries WHERE entity = ? AND writer = 'seed' AND status = 'live' ORDER BY seq DESC LIMIT 1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
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

/// Deletes an entity's symbol and section children (and theirs), with their
/// search rows and code labels; their summaries stay.
fn delete_children(conn: &Connection, file_id: &str, dirty: &mut BTreeSet<String>) -> Result<()> {
    let prefix = format!("{file_id}#");
    let ids: Vec<String> = {
        let mut st =
            conn.prepare("SELECT id FROM entities WHERE substr(id, 1, length(?1)) = ?1")?;
        st.query_map(params![prefix], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for id in ids {
        delete_entity_rows(conn, &id)?;
        dirty.remove(&id);
    }
    Ok(())
}

/// Deletes an entity's search and search_tri rows, by the rowid they were
/// written at (a delete by the unindexed id column would scan the table).
pub fn delete_search(conn: &Connection, id: &str) -> Result<()> {
    let rid: Option<i64> = conn
        .query_row(
            "SELECT rid FROM search_rowid WHERE id = ?",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(rid) = rid {
        conn.execute("DELETE FROM search WHERE rowid = ?", params![rid])?;
        conn.execute("DELETE FROM search_tri WHERE rowid = ?", params![rid])?;
        conn.execute("DELETE FROM search_rowid WHERE id = ?", params![id])?;
    }
    Ok(())
}

/// Removes one entity's crawl-made rows: the entity, its search rows, code
/// labels and edges, queue and vectors. Summaries and non-code edges and
/// labels stay, keyed by id, for a rename to carry or the file to come back.
fn delete_entity_rows(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM entities WHERE id = ?", params![id])?;
    delete_search(conn, id)?;
    conn.execute(
        "DELETE FROM labels WHERE entity = ? AND origin = 'code' AND kind != 'old'",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM edges WHERE origin = 'code' AND (source = ?1 OR target = ?1)",
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
        "UPDATE OR IGNORE labels SET entity = ? WHERE entity = ? AND origin != 'code'",
        params![new, old],
    )?;
    conn.execute(
        "UPDATE OR IGNORE labels SET entity = ? WHERE entity = ? AND kind = 'old'",
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
    conn.execute(
        "INSERT OR IGNORE INTO labels (entity, label, norm, compact, kind, origin, weight, evidence)
         VALUES (?, ?, ?, ?, 'old', 'code', 0.5, 'renamed')",
        params![new, old, old.to_lowercase(), old.to_lowercase()],
    )?;
    Ok(())
}

fn crawl_project(
    conn: &Connection,
    p: &Project,
    nested: &[String],
    rebuild: bool,
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
    if rebuild {
        // Everything code made goes; what can't be rebuilt stays.
        let prefix = format!("{}:", p.name);
        let ids: Vec<String> = {
            let mut st = conn.prepare("SELECT id FROM entities WHERE project = ?")?;
            st.query_map(params![p.name], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in &ids {
            delete_entity_rows(conn, id)?;
        }
        let _ = prefix;
    }
    let (listed, is_git) = list(p, nested)?;
    rep.git = is_git;
    let head = if is_git {
        git::head(dir)
    } else {
        String::new()
    };
    rep.head = head.clone();
    let known = known_files(conn, &p.name)?;
    let shallow = p.depth == "shallow";
    let symbols = p.depth == "full";

    // Renames since the last crawl, to carry what was said about the old path.
    let mut renames: Vec<(String, String)> = Vec::new();
    if is_git && !p.last_commit.is_empty() && !head.is_empty() && p.last_commit != head {
        rep.commits_since = git::commits_between(dir, &p.last_commit, &head);
        renames = git::renames(dir, &p.last_commit, &head);
    }

    let mut seen: HashSet<String> = HashSet::new();
    // Every file kept (not secret), by dir, for dir keys and READMEs.
    let mut by_dir: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut dirty_dirs: HashSet<String> = HashSet::new();
    let mut all_dirs: BTreeSet<String> = BTreeSet::new();
    let mut manifests_seen: HashSet<String> = HashSet::new();
    for f in &listed {
        let name = base_name(&f.rel);
        if files::is_secret(name) {
            rep.secret_skipped += 1;
            continue;
        }
        // Dirs above it.
        let mut d = parent_rel(&f.rel);
        loop {
            all_dirs.insert(d.to_string());
            if f.dirty {
                dirty_dirs.insert(d.to_string());
            }
            if d.is_empty() {
                break;
            }
            d = parent_rel(d);
        }
        if shallow {
            // Only top-level dirs and the READMEs that describe them.
            by_dir
                .entry(parent_rel(&f.rel).to_string())
                .or_default()
                .push((name.to_string(), f.key.clone()));
            continue;
        }
        rep.files += 1;
        let id = format!("{}:{}", p.name, f.rel);
        seen.insert(id.clone());
        let abs = dir.join(&f.rel);
        // Outside git: a content hash, reused while size and mtime hold.
        let mut key = f.key.clone();
        let prev = known.get(&f.rel);
        if !is_git && !f.symlink {
            match prev {
                Some(k) if k.mtime == f.mtime && k.size == f.size as i64 && !rebuild => {
                    key = k.key.clone()
                }
                _ => {
                    key = match std::fs::read(&abs) {
                        Ok(b) => hash(&[&b]),
                        Err(_) => String::new(),
                    }
                }
            }
        }
        if f.symlink {
            // The link's target text is its content, never what it points at.
            let target = std::fs::read_link(&abs)
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            key = hash(&[b"symlink", target.as_bytes()]);
        }
        by_dir
            .entry(parent_rel(&f.rel).to_string())
            .or_default()
            .push((name.to_string(), key.clone()));
        let unchanged =
            prev.is_some_and(|k| k.key == key && k.untracked == f.untracked && !key.is_empty());
        if unchanged && !rebuild {
            rep.unchanged += 1;
            if manifest::is_manifest(name) {
                manifests_seen.insert(id.clone());
            }
            continue;
        }
        if prev.is_some() {
            rep.changed += 1;
        } else {
            rep.added += 1;
        }
        read_file(conn, p, f, &id, &key, symbols, &mut rep, dirty)?;
        if manifest::is_manifest(name) {
            manifests_seen.insert(id.clone());
        }
    }

    // Files gone: carry renamed ones, then remove the rest.
    let rename_map: HashMap<String, String> = renames
        .into_iter()
        .map(|(a, b)| (format!("{}:{a}", p.name), format!("{}:{b}", p.name)))
        .collect();
    if !shallow {
        let gone: Vec<String> = known
            .keys()
            .map(|rel| format!("{}:{rel}", p.name))
            .filter(|id| !seen.contains(id))
            .collect();
        for id in gone {
            if let Some(new) = rename_map.get(&id)
                && seen.contains(new)
            {
                carry(conn, &id, new)?;
                // The children too, by name: alpha:a.rs#f → alpha:b.rs#f.
                let kids: Vec<String> = {
                    let mut st = conn.prepare(
                        "SELECT DISTINCT entity FROM summaries WHERE substr(entity, 1, length(?1)) = ?1 AND writer != 'seed'",
                    )?;
                    st.query_map(params![format!("{id}#")], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?
                };
                for k in kids {
                    let nk = format!("{new}{}", &k[id.len()..]);
                    carry(conn, &k, &nk)?;
                }
                rep.renamed += 1;
                dirty.insert(new.clone());
            } else {
                rep.removed += 1;
            }
            delete_children(conn, &id, dirty)?;
            delete_entity_rows(conn, &id)?;
            dirty.remove(&id);
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
    // Children per dir: subdirs, for listing hashes.
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
    // Remove dirs that are gone.
    {
        let have: Vec<(String, String)> = {
            let mut st =
                conn.prepare("SELECT id, rel FROM entities WHERE project = ? AND kind = 'dir'")?;
            st.query_map(params![p.name], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let keep: HashSet<&String> = dirs.iter().collect();
        for (id, rel) in have {
            if !keep.contains(&rel) {
                delete_entity_rows(conn, &id)?;
                dirty.remove(&id);
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
            // No file entities: read the README itself.
            let abs = dir.join(&rel);
            let text = match std::fs::metadata(&abs) {
                Ok(m) if m.len() <= files::MAX_CONTENT => {
                    std::fs::read_to_string(&abs).unwrap_or_default()
                }
                _ => String::new(),
            };
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
    let manifest_of = |conn: &Connection, d: &str| -> Result<Option<(String, String)>> {
        Ok(conn
            .query_row(
                "SELECT name, data FROM manifests WHERE dir = ? ORDER BY (ecosystem = 'cargo') DESC LIMIT 1",
                params![dir_id(&p.name, d)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
    };
    for d in &dirs {
        let id = dir_id(&p.name, d);
        let key = dir_keys.get(d).cloned().unwrap_or_default();
        let (readme_title, readme_para) = readme_seed(conn, d)?;
        let man = manifest_of(conn, d)?;
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
        let prev_key: Option<String> = conn
            .query_row(
                "SELECT content_key FROM entities WHERE id = ?",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        upsert_entity(
            conn, &id, kind, &parent, &p.name, d, &name, &path, "", "", 0, 0, 0, &key, 0, &title,
            "", false, "",
        )?;
        let seed_changed = set_seed(conn, &id, &key, &seed)?;
        if rebuild || seed_changed || prev_key.as_deref() != Some(key.as_str()) {
            dirty.insert(id.clone());
        }
        // Labels from code: the name's spellings, the README title, the package name.
        conn.execute(
            "DELETE FROM labels WHERE entity = ? AND origin = 'code' AND kind != 'old'",
            params![id],
        )?;
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
        if d.is_empty() && is_git {
            let remote = git::remote(dir);
            if let Some(last) = remote.rsplit('/').next()
                && !last.is_empty()
            {
                labels.push((last.to_string(), "alt", "remote name"));
            }
        }
        for (label, kind, evidence) in labels {
            add_label(conn, &id, &label, kind, "code", 1.0, evidence)?;
        }
        dirty.insert(id);
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
            cochange(conn, p, dir)?;
            rep.cochange = true;
            conn.execute(
                "UPDATE projects SET cochange_commit = ?, cochange_at = ? WHERE name = ?",
                params![head, now(), p.name],
            )?;
        }
    }
    tests_edges(conn, &p.name)?;

    let (root_commit, remote) = if is_git {
        (git::root_commit(dir), git::remote(dir))
    } else {
        (String::new(), String::new())
    };
    conn.execute(
        "UPDATE projects SET is_git = ?, root_commit = ?, remote = ?, last_commit = ?, last_crawled_at = ? WHERE name = ?",
        params![is_git as i64, root_commit, remote, head, now(), p.name],
    )?;
    rep.ms = start.elapsed().as_millis();
    Ok(rep)
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

#[allow(clippy::too_many_arguments)]
fn read_file(
    conn: &Connection,
    p: &Project,
    f: &Listed,
    id: &str,
    key: &str,
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
    delete_children(conn, id, dirty)?;
    conn.execute("DELETE FROM refs WHERE doc = ?", params![id])?;
    conn.execute("DELETE FROM manifests WHERE file = ?", params![id])?;

    let mut no_content = String::new();
    let mut x = Extracted::default();
    let mut text = String::new();
    if f.submodule {
        no_content = "submodule".into();
    } else if f.symlink {
        // Never followed: an index that reads files must not reach outside what it indexes.
        no_content = match std::fs::canonicalize(&abs) {
            Ok(t) if t.starts_with(dir) => "symlink".into(),
            _ => "symlink outside the project".into(),
        };
    } else if f.size > files::MAX_CONTENT {
        no_content = "over 1 MB".into();
    } else {
        match std::fs::read(&abs) {
            Ok(bytes) if files::is_binary(&bytes) => no_content = "binary".into(),
            Ok(bytes) => {
                text = String::from_utf8_lossy(&bytes).into_owned();
                x = extract::extract(&f.rel, lang, &text, symbols);
            }
            Err(e) => no_content = format!("unreadable: {}", e.kind()),
        }
    }
    if !no_content.is_empty() {
        rep.no_content += 1;
    }
    let title = x.title.clone().unwrap_or_default();
    upsert_entity(
        conn,
        id,
        "file",
        &parent,
        &p.name,
        &f.rel,
        name,
        &abs.to_string_lossy(),
        file_kind,
        lang,
        f.size as i64,
        x.lines as i64,
        0,
        key,
        f.mtime,
        &title,
        "",
        f.untracked,
        &no_content,
    )?;
    set_seed(conn, id, key, x.seed.as_deref().unwrap_or(""))?;
    dirty.insert(id.to_string());

    // Symbols: ids by name, made unique in the file with -2, -3.
    let mut used: HashMap<String, usize> = HashMap::new();
    for s in &x.symbols {
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
            &abs.to_string_lossy(),
            &s.kind,
            lang,
            0,
            0,
            s.line as i64,
            &skey,
            0,
            &s.signature,
            &s.signature,
            f.untracked,
            "",
        )?;
        set_seed(conn, &sid, &skey, &s.doc)?;
        dirty.insert(sid);
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
            let skey = hash(&[s.heading.as_bytes(), s.seed.as_bytes()]);
            upsert_entity(
                conn,
                &sid,
                "section",
                &parent,
                &p.name,
                &f.rel,
                &s.heading,
                &abs.to_string_lossy(),
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
            stack.push((s.level, sid));
            rep.sections += 1;
        }
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

/// `tests` edges by naming: `x_test.rs`, `test_x.py`, `x.test.ts`, `x.spec.js`,
/// `XTests.swift` → the file they test, in the same dir if there is one, else
/// the one file of that name in the project.
fn tests_edges(conn: &Connection, project: &str) -> Result<()> {
    let prefix = format!("{project}:");
    conn.execute(
        "DELETE FROM edges WHERE kind = 'tests' AND origin = 'code' AND substr(source, 1, length(?1)) = ?1",
        params![prefix],
    )?;
    let files: Vec<(String, String, String)> = {
        let mut st = conn.prepare(
            "SELECT id, rel, content_key FROM entities WHERE project = ? AND kind = 'file'",
        )?;
        st.query_map(params![project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_rel: HashMap<&str, usize> = HashMap::new();
    for (i, (_, rel, _)) in files.iter().enumerate() {
        by_name
            .entry(base_name(rel).to_string())
            .or_default()
            .push(i);
        by_rel.insert(rel.as_str(), i);
    }
    for (id, rel, key) in &files {
        let name = base_name(rel);
        let Some(target) = tested_name(name) else {
            continue;
        };
        let dir = parent_rel(rel);
        let same_dir = if dir.is_empty() {
            target.clone()
        } else {
            format!("{dir}/{target}")
        };
        let hit = by_rel.get(same_dir.as_str()).copied().or_else(|| {
            let c = by_name.get(&target)?;
            if c.len() == 1 { Some(c[0]) } else { None }
        });
        if let Some(i) = hit
            && files[i].0 != *id
        {
            conn.execute(
                "INSERT OR IGNORE INTO edges (source, target, kind, weight, origin, evidence, source_key, target_key)
                 VALUES (?, ?, 'tests', 1, 'code', 'naming', ?, ?)",
                params![id, files[i].0, key, files[i].2],
            )?;
        }
    }
    Ok(())
}

/// The file name a test's name says it tests.
pub fn tested_name(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    for suf in ["_test", "_tests"] {
        if let Some(s) = stem.strip_suffix(suf)
            && !s.is_empty()
        {
            return Some(format!("{s}.{ext}"));
        }
    }
    if let Some(s) = stem.strip_prefix("test_")
        && !s.is_empty()
    {
        return Some(format!("{s}.{ext}"));
    }
    for mid in [".test", ".spec"] {
        if let Some(s) = stem.strip_suffix(mid)
            && !s.is_empty()
        {
            return Some(format!("{s}.{ext}"));
        }
    }
    if ext == "swift"
        && let Some(s) = stem.strip_suffix("Tests")
        && !s.is_empty()
    {
        return Some(format!("{s}.{ext}"));
    }
    None
}

/// `co-changes` from the last 2,000 commits: Jaccard over the commits that
/// touched each file, kept at 0.2 or more and with two shared commits, the top
/// ten per file. Commits touching over 50 files (mass renames, formatting)
/// are left out: they say nothing about which files belong together.
fn cochange(conn: &Connection, p: &Project, dir: &Path) -> Result<()> {
    let prefix = format!("{}:", p.name);
    conn.execute(
        "DELETE FROM edges WHERE kind = 'co-changes' AND origin = 'code' AND substr(source, 1, length(?1)) = ?1",
        params![prefix],
    )?;
    let commits = git::log_files(dir, 2000);
    let ids: HashSet<String> = {
        let mut st =
            conn.prepare("SELECT rel FROM entities WHERE project = ? AND kind = 'file'")?;
        st.query_map(params![p.name], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    let mut count: Vec<u32> = Vec::new();
    let mut pairs: HashMap<(u32, u32), u32> = HashMap::new();
    for c in commits {
        if c.len() > 50 {
            continue;
        }
        let mut fs: Vec<u32> = c
            .iter()
            .filter(|f| ids.contains(*f))
            .map(|f| {
                *index.entry(f.clone()).or_insert_with(|| {
                    names.push(f.clone());
                    count.push(0);
                    (names.len() - 1) as u32
                })
            })
            .collect();
        fs.sort_unstable();
        fs.dedup();
        for &a in &fs {
            count[a as usize] += 1;
        }
        for i in 0..fs.len() {
            for j in i + 1..fs.len() {
                *pairs.entry((fs[i], fs[j])).or_insert(0) += 1;
            }
        }
    }
    let mut per: HashMap<u32, Vec<(u32, f64, u32)>> = HashMap::new();
    for ((a, b), n) in pairs {
        if n < 2 {
            continue;
        }
        let j = n as f64 / (count[a as usize] + count[b as usize] - n) as f64;
        if j < 0.2 {
            continue;
        }
        per.entry(a).or_default().push((b, j, n));
        per.entry(b).or_default().push((a, j, n));
    }
    for (a, mut v) in per {
        v.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
        v.truncate(10);
        for (b, j, n) in v {
            conn.execute(
                "INSERT OR REPLACE INTO edges (source, target, kind, weight, origin, evidence)
                 VALUES (?, ?, 'co-changes', ?, 'code', ?)",
                params![
                    format!("{}:{}", p.name, names[a as usize]),
                    format!("{}:{}", p.name, names[b as usize]),
                    (j * 1000.0).round() / 1000.0,
                    format!("{n} commits together")
                ],
            )?;
        }
    }
    Ok(())
}

/// Rebuilds `depends-on` and `documented-by` across every project, from the
/// stored manifests and doc references. Returns edge counts by kind and how
/// many references resolved nowhere.
fn resolve_edges(conn: &Connection) -> Result<(BTreeMap<String, usize>, usize)> {
    conn.execute(
        "DELETE FROM edges WHERE origin = 'code' AND kind IN ('depends-on', 'documented-by')",
        [],
    )?;
    conn.execute("DELETE FROM dangling", [])?;
    // Entities by absolute path, and project roots.
    let mut by_path: HashMap<String, String> = HashMap::new();
    let mut keys: HashMap<String, String> = HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT id, path, content_key FROM entities WHERE kind IN ('project', 'dir', 'file')",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, path, key) = row?;
            by_path.insert(path, id.clone());
            keys.insert(id, key);
        }
    }
    let projects = roots::list_projects(conn)?;
    let project_path: HashMap<&str, &str> = projects
        .iter()
        .map(|p| (p.name.as_str(), p.path.as_str()))
        .collect();
    let root_paths: Vec<String> = {
        let mut st = conn.prepare("SELECT path FROM roots")?;
        st.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let lookup = |abs: &Path| -> Option<String> {
        let s = normalize(abs);
        by_path.get(&s).cloned()
    };
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut add_edge = |conn: &Connection,
                        src: &str,
                        dst: &str,
                        kind: &str,
                        evidence: &str|
     -> Result<()> {
        if src == dst {
            return Ok(());
        }
        let n = conn.execute(
            "INSERT INTO edges (source, target, kind, weight, origin, evidence, source_key, target_key)
             VALUES (?1, ?2, ?3, 1, 'code', ?4, ?5, ?6)
             ON CONFLICT(source, target, kind) DO UPDATE SET weight = weight + 1",
            params![
                src,
                dst,
                kind,
                evidence,
                keys.get(src).cloned().unwrap_or_default(),
                keys.get(dst).cloned().unwrap_or_default()
            ],
        )?;
        if n > 0 {
            *counts.entry(kind.to_string()).or_insert(0) += 1;
        }
        Ok(())
    };

    // depends-on
    let mans: Vec<(String, String, String, Manifest)> = {
        let mut st = conn.prepare("SELECT file, dir, project, data FROM manifests")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut v = Vec::new();
        for row in rows {
            let (f, d, p, data) = row?;
            if let Ok(m) = serde_json::from_str::<Manifest>(&data) {
                v.push((f, d, p, m));
            }
        }
        v
    };
    let mut by_pkg: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (_, d, _, m) in &mans {
        if !m.name.is_empty() {
            by_pkg
                .entry((m.ecosystem.clone(), m.name.to_lowercase().replace('_', "-")))
                .or_default()
                .push(d.clone());
        }
    }
    let dir_path = |d: &str| -> Option<String> {
        conn.query_row("SELECT path FROM entities WHERE id = ?", params![d], |r| {
            r.get(0)
        })
        .optional()
        .ok()
        .flatten()
    };
    for (file, d, project, m) in &mans {
        let Some(dpath) = dir_path(d) else { continue };
        let evidence = file
            .split_once(':')
            .map(|(_, r)| r)
            .unwrap_or(file)
            .to_string();
        for dep in &m.deps {
            let mut target: Option<String> = None;
            if let Some(rel) = &dep.path {
                target = lookup(&Path::new(&dpath).join(rel));
            } else if dep.workspace && m.ecosystem == "cargo" {
                // The nearest workspace above, in the same project.
                for (_, wd, wp, wm) in &mans {
                    if wp != project || !wm.is_workspace || wm.ecosystem != "cargo" {
                        continue;
                    }
                    let Some(wpath) = dir_path(wd) else { continue };
                    if (dpath == wpath || dpath.starts_with(&format!("{wpath}/")))
                        && let Some(rel) = wm.workspace_paths.get(&dep.name)
                    {
                        target = lookup(&Path::new(&wpath).join(rel));
                        break;
                    }
                }
            }
            if target.is_none() {
                let key = (
                    m.ecosystem.clone(),
                    dep.name.to_lowercase().replace('_', "-"),
                );
                if let Some(c) = by_pkg.get(&key)
                    && c.len() == 1
                {
                    target = Some(c[0].clone());
                }
            }
            if let Some(t) = target {
                add_edge(conn, d, &t, "depends-on", &evidence)?;
            }
        }
    }

    // documented-by: the thing a doc names is documented by the doc.
    let refs: Vec<(String, String, i64, String)> = {
        let mut st = conn.prepare(
            "SELECT r.doc, r.ref, r.line, e.path FROM refs r JOIN entities e ON e.id = r.doc",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    // Bare file names (`stale.rs`) resolve to the one file of that name in the
    // doc's project.
    let mut by_base: HashMap<(String, String), Vec<String>> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT id, project, name FROM entities WHERE kind = 'file'")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, project, name) = row?;
            by_base.entry((project, name)).or_default().push(id);
        }
    }
    let mut dangling = 0;
    for (doc, r, line, doc_path) in refs {
        let project = crate::model::project_of_id(&doc).to_string();
        let doc_dir = Path::new(&doc_path)
            .parent()
            .unwrap_or(Path::new("/"))
            .to_path_buf();
        let trimmed = r.trim_end_matches('/');
        let mut hit: Option<String> = None;
        // An id: `alpha:src/x.rs`.
        if let Some((proj, rest)) = trimmed.split_once(':')
            && project_path.contains_key(proj)
        {
            let id = dir_id(proj, rest);
            if keys.contains_key(&id) {
                hit = Some(id);
            }
        }
        if hit.is_none() && trimmed.starts_with('/') {
            hit = lookup(Path::new(trimmed));
        }
        if hit.is_none() && !trimmed.starts_with('/') {
            let mut bases: Vec<&str> = vec![doc_dir.to_str().unwrap_or("")];
            if let Some(pp) = project_path.get(project.as_str()) {
                bases.push(pp);
            }
            for b in &bases {
                if let Some(h) = lookup(&Path::new(b).join(trimmed)) {
                    hit = Some(h);
                    break;
                }
            }
            if hit.is_none() {
                // Another project's path (planning docs often live in a sibling
                // repo), or a path from a root (`gomuxai/docs/X.md`).
                for p in &projects {
                    if p.name == project {
                        continue;
                    }
                    if let Some(h) = lookup(&Path::new(&p.path).join(trimmed)) {
                        hit = Some(h);
                        break;
                    }
                }
            }
            if hit.is_none() {
                for rp in &root_paths {
                    if let Some(h) = lookup(&Path::new(rp).join(trimmed)) {
                        hit = Some(h);
                        break;
                    }
                }
            }
            if hit.is_none()
                && !trimmed.contains('/')
                && let Some(c) = by_base.get(&(project.clone(), trimmed.to_string()))
                && c.len() == 1
            {
                hit = Some(c[0].clone());
            }
        }
        match hit {
            Some(h) => {
                let doc_rel = doc.split_once(':').map(|(_, r)| r).unwrap_or(&doc);
                add_edge(
                    conn,
                    &h,
                    &doc,
                    "documented-by",
                    &format!("{doc_rel}:{line}"),
                )?;
            }
            None => {
                dangling += 1;
                conn.execute(
                    "INSERT OR IGNORE INTO dangling (doc, ref, line) VALUES (?, ?, ?)",
                    params![doc, r, line],
                )?;
            }
        }
    }
    let cc: i64 = conn.query_row(
        "SELECT count(*) FROM edges WHERE kind = 'co-changes'",
        [],
        |r| r.get(0),
    )?;
    let tests: i64 =
        conn.query_row("SELECT count(*) FROM edges WHERE kind = 'tests'", [], |r| {
            r.get(0)
        })?;
    counts.insert("co-changes".into(), cc as usize);
    counts.insert("tests".into(), tests as usize);
    let dep: i64 = conn.query_row(
        "SELECT count(*) FROM edges WHERE kind = 'depends-on'",
        [],
        |r| r.get(0),
    )?;
    let doc: i64 = conn.query_row(
        "SELECT count(*) FROM edges WHERE kind = 'documented-by'",
        [],
        |r| r.get(0),
    )?;
    counts.insert("depends-on".into(), dep as usize);
    counts.insert("documented-by".into(), doc as usize);
    Ok((counts, dangling))
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

/// Importance: fan-in on `depends-on` and `documented-by`, plus co-change
/// degree (no PageRank yet); a directory or project adds half its children's.
fn importance(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE entities SET importance =
           (SELECT count(DISTINCT source) FROM edges WHERE target = entities.id AND kind IN ('depends-on', 'documented-by'))
         + (SELECT count(*) FROM edges WHERE source = entities.id AND kind = 'co-changes')",
        [],
    )?;
    let rows: Vec<(String, String, f64, usize)> = {
        let mut st = conn.prepare(
            "SELECT id, parent, importance, kind FROM entities WHERE kind IN ('file', 'dir', 'project')",
        )?;
        st.query_map([], |r| {
            let kind: String = r.get(3)?;
            let id: String = r.get(0)?;
            let depth = if kind == "project" {
                0
            } else {
                id.split_once(':')
                    .map(|(_, r)| r.matches('/').count() + 1)
                    .unwrap_or(0)
            };
            Ok((id, r.get(1)?, r.get(2)?, depth))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut imp: HashMap<String, f64> = rows.iter().map(|(i, _, v, _)| (i.clone(), *v)).collect();
    let mut ordered = rows.clone();
    ordered.sort_by_key(|r| std::cmp::Reverse(r.3));
    for (id, parent, _, _) in &ordered {
        let v = imp.get(id).copied().unwrap_or(0.0);
        if v > 0.0
            && let Some(pv) = imp.get_mut(parent)
        {
            *pv += v * 0.5;
        }
    }
    let mut st = conn.prepare("UPDATE entities SET importance = ? WHERE id = ?")?;
    for (id, _, old, _) in &rows {
        let v = imp.get(id).copied().unwrap_or(0.0);
        if (v - old).abs() > f64::EPSILON {
            st.execute(params![(v * 100.0).round() / 100.0, id])?;
        }
    }
    Ok(())
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

pub fn search_text(conn: &Connection, id: &str) -> Result<Option<SearchText>> {
    let Some(e) = crate::model::get(conn, id)? else {
        return Ok(None);
    };
    let s = crate::model::summary(conn, id)?;
    let labels: Vec<String> = {
        let mut st = conn.prepare("SELECT label FROM labels WHERE entity = ? ORDER BY label")?;
        st.query_map(params![id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    // A file's symbols; a dir's or project's children's names.
    let kids: Vec<String> = {
        let mut st = conn.prepare(
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
    let name_text = if e.kind == "symbol" {
        names::whole_and_split(e.name.split_whitespace().last().unwrap_or(&e.name))
    } else {
        // A file's name whole, its stem split (`gather_global.rs`).
        let stem = e.name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&e.name);
        format!("{} {}", e.name.to_lowercase(), names::whole_and_split(stem))
    };
    let rel = if e.rel.is_empty() {
        e.project.clone()
    } else {
        e.rel.clone()
    };
    let path_text = format!("{} {}", rel, names::split_words(&rel).join(" "));
    Ok(Some(SearchText {
        title: if e.kind == "symbol" {
            String::new()
        } else {
            e.title.clone()
        },
        name: name_text,
        path: path_text,
        summary: s.as_ref().map(|s| s.text.clone()).unwrap_or_default(),
        symbols,
        why: s.map(|s| s.why).unwrap_or_default(),
        labels: labels.join(" | "),
    }))
}

/// Rewrites the search rows of the given entities, and queues those whose
/// embedding text changed. Returns (rows written, entities queued).
pub fn refresh_search(conn: &Connection, ids: &BTreeSet<String>) -> Result<(usize, usize)> {
    let mut rows = 0;
    let mut queued = 0;
    let mut ins = conn.prepare(
        "INSERT INTO search (id, title, name, path, summary, symbols, why, labels) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut ins_tri = conn.prepare("INSERT INTO search_tri (rowid, id, text) VALUES (?, ?, ?)")?;
    let mut map = conn.prepare("INSERT OR REPLACE INTO search_rowid (id, rid) VALUES (?, ?)")?;
    for id in ids {
        delete_search(conn, id)?;
        let Some(t) = search_text(conn, id)? else {
            continue;
        };
        ins.execute(params![
            id, t.title, t.name, t.path, t.summary, t.symbols, t.why, t.labels
        ])?;
        let rid = conn.last_insert_rowid();
        map.execute(params![id, rid])?;
        let (rel, name) = conn.query_row(
            "SELECT rel, name FROM entities WHERE id = ?",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        ins_tri.execute(params![rid, id, format!("{name} {rel} {}", t.labels)])?;
        rows += 1;
        let text_key = hash(&[
            t.title.as_bytes(),
            t.path.as_bytes(),
            t.summary.as_bytes(),
            t.why.as_bytes(),
            t.labels.as_bytes(),
            t.symbols.as_bytes(),
        ]);
        let old: String = conn.query_row(
            "SELECT text_key FROM entities WHERE id = ?",
            params![id],
            |r| r.get(0),
        )?;
        if old != text_key {
            conn.execute(
                "UPDATE entities SET text_key = ? WHERE id = ?",
                params![text_key, id],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO embed_queue (entity_id, text_key, reason, queued_at) VALUES (?, ?, ?, ?)",
                params![id, text_key, if old.is_empty() { "new" } else { "changed" }, now()],
            )?;
            queued += 1;
        }
    }
    Ok((rows, queued))
}

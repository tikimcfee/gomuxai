//! Roots and projects: what the index covers (docs/INDEX.md, "What gets
//! indexed"). Opt-in: a root is added by a person; every directory directly
//! under it is a project, a repo or not; a deeper one is added with
//! `project add`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::db::{Index, now};
use crate::files::SKIP_DIRS;
use crate::git;

pub const DEPTHS: &[&str] = &["full", "files", "shallow"];

pub fn check_depth(d: &str) -> Result<()> {
    if !DEPTHS.contains(&d) {
        bail!("{d:?} isn't a depth: use full, files or shallow");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Root {
    pub id: String,
    pub path: String,
    pub depth: String,
    pub added_at: String,
    pub projects: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub path: String,
    pub root: String,
    pub explicit: bool,
    pub depth: String,
    pub is_git: bool,
    pub root_commit: String,
    pub remote: String,
    pub last_commit: String,
    pub last_crawled_at: String,
    pub cochange_commit: String,
    pub cochange_at: String,
}

const PROJECT_COLS: &str = "name, path, root, explicit, depth, is_git, root_commit, remote, last_commit, last_crawled_at, cochange_commit, cochange_at";

fn project_row(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project {
        name: r.get(0)?,
        path: r.get(1)?,
        root: r.get(2)?,
        explicit: r.get::<_, i64>(3)? != 0,
        depth: r.get(4)?,
        is_git: r.get::<_, i64>(5)? != 0,
        root_commit: r.get(6)?,
        remote: r.get(7)?,
        last_commit: r.get(8)?,
        last_crawled_at: r.get(9)?,
        cochange_commit: r.get(10)?,
        cochange_at: r.get(11)?,
    })
}

/// A directory, canonical, which must exist.
pub fn canonical_dir(dir: &Path) -> Result<PathBuf> {
    let p = std::fs::canonicalize(dir).with_context(|| format!("{}", dir.display()))?;
    if !p.is_dir() {
        bail!("{} isn't a directory", p.display());
    }
    Ok(p)
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Adds a root; returns it. Refuses one already added, or one inside or
/// around another root.
pub fn add_root(ix: &Index, dir: &Path, depth: &str) -> Result<Root> {
    check_depth(depth)?;
    let p = canonical_dir(dir)?;
    let path = s(&p);
    for r in list_roots(&ix.conn)? {
        if r.path == path {
            bail!("{path} is already a root ({})", r.id);
        }
        if path.starts_with(&format!("{}/", r.path)) || r.path.starts_with(&format!("{path}/")) {
            bail!(
                "{path} overlaps root {} ({}): remove that one first (agent index root remove {})",
                r.id,
                r.path,
                r.path
            );
        }
    }
    let base = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let mut id = format!("@{base}");
    let mut n = 2;
    while ix
        .conn
        .query_row("SELECT 1 FROM roots WHERE id = ?", params![id], |_| Ok(()))
        .optional()?
        .is_some()
    {
        id = format!("@{base}-{n}");
        n += 1;
    }
    ix.conn.execute(
        "INSERT INTO roots (id, path, depth, added_at) VALUES (?, ?, ?, ?)",
        params![id, path, depth, now()],
    )?;
    ix.conn.execute(
        "INSERT OR REPLACE INTO entities (id, kind, name, path, title, crawled_at) VALUES (?, 'root', ?, ?, ?, ?)",
        params![id, base, path, base, now()],
    )?;
    Ok(Root {
        id,
        path,
        depth: depth.to_string(),
        added_at: now(),
        projects: 0,
    })
}

pub fn list_roots(conn: &Connection) -> Result<Vec<Root>> {
    let mut st = conn.prepare(
        "SELECT r.id, r.path, r.depth, r.added_at, (SELECT count(*) FROM projects p WHERE p.root = r.id)
         FROM roots r ORDER BY r.path",
    )?;
    let rows = st.query_map([], |r| {
        Ok(Root {
            id: r.get(0)?,
            path: r.get(1)?,
            depth: r.get(2)?,
            added_at: r.get(3)?,
            projects: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Removes a root (by path or id) and every project it found (not those added
/// with project add); returns the root and the projects removed.
pub fn remove_root(ix: &Index, which: &str) -> Result<(Root, Vec<String>)> {
    let roots = list_roots(&ix.conn)?;
    let canon = std::fs::canonicalize(which).ok().map(|p| s(&p));
    let Some(r) = roots
        .into_iter()
        .find(|r| r.id == which || r.path == which || Some(&r.path) == canon.as_ref())
    else {
        bail!("no root {which}: agent index root list shows them");
    };
    let names: Vec<String> = {
        let mut st = ix
            .conn
            .prepare("SELECT name FROM projects WHERE root = ? AND explicit = 0")?;
        st.query_map(params![r.id], |x| x.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let tx = ix.conn.unchecked_transaction()?;
    for n in &names {
        delete_project(&tx, n)?;
    }
    tx.execute(
        "UPDATE projects SET root = '' WHERE root = ?",
        params![r.id],
    )?;
    tx.execute("DELETE FROM roots WHERE id = ?", params![r.id])?;
    tx.execute("DELETE FROM entities WHERE id = ?", params![r.id])?;
    tx.commit()?;
    Ok((r, names))
}

/// Deletes a project and everything of it the crawl made. Summaries, model
/// and person edges and labels stay: they are keyed by content, and come back
/// if the project does.
pub fn delete_project(conn: &Connection, name: &str) -> Result<()> {
    // An id of the project: the project itself, or one starting "name:". (Not
    // LIKE: `_` in a name would match any character.)
    let prefix = format!("{name}:");
    let of = |col: &str| format!("({col} = ?1 OR substr({col}, 1, length(?2)) = ?2)");
    for (table, col, extra) in [
        ("search", "id", ""),
        ("search_tri", "id", ""),
        ("refs", "doc", ""),
        ("dangling", "doc", ""),
        ("labels", "entity", " AND origin = 'code'"),
        ("embed_queue", "entity_id", ""),
        ("vectors", "entity_id", ""),
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE {}{extra}", of(col)),
            params![name, prefix],
        )?;
    }
    conn.execute("DELETE FROM manifests WHERE project = ?", params![name])?;
    conn.execute(
        &format!(
            "DELETE FROM edges WHERE origin = 'code' AND ({} OR {})",
            of("source"),
            of("target")
        ),
        params![name, prefix],
    )?;
    conn.execute("DELETE FROM entities WHERE project = ?", params![name])?;
    conn.execute("DELETE FROM projects WHERE name = ?", params![name])?;
    Ok(())
}

pub fn list_projects(conn: &Connection) -> Result<Vec<Project>> {
    let mut st = conn.prepare(&format!(
        "SELECT {PROJECT_COLS} FROM projects ORDER BY name"
    ))?;
    let rows = st.query_map([], project_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_project(conn: &Connection, name: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PROJECT_COLS} FROM projects WHERE name = ?"),
            params![name],
            project_row,
        )
        .optional()?)
}

/// What project add did.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectAdded {
    pub name: String,
    pub path: String,
    pub depth: String,
    /// "added", "updated" (depth changed), "unchanged".
    pub outcome: String,
}

/// Adds a directory as a project of its own, or changes an existing project's
/// depth. `name` defaults to the directory's name; a clash is refused.
pub fn add_project(ix: &Index, dir: &Path, depth: &str, name: &str) -> Result<ProjectAdded> {
    check_depth(depth)?;
    let p = canonical_dir(dir)?;
    let path = s(&p);
    if let Some(wt) = git::worktree_of(&p) {
        bail!(
            "{path} is a git worktree of {}: a worktree isn't a project of its own; add the main repo",
            wt.display()
        );
    }
    if let Some(existing) = list_projects(&ix.conn)?
        .into_iter()
        .find(|x| x.path == path)
    {
        if !name.is_empty() && name != existing.name {
            bail!(
                "{path} is already project {}: remove its root or keep the name",
                existing.name
            );
        }
        let outcome = if existing.depth == depth && existing.explicit {
            "unchanged"
        } else {
            "updated"
        };
        ix.conn.execute(
            "UPDATE projects SET depth = ?, explicit = 1 WHERE name = ?",
            params![depth, existing.name],
        )?;
        return Ok(ProjectAdded {
            name: existing.name,
            path,
            depth: depth.into(),
            outcome: outcome.into(),
        });
    }
    let name = if name.is_empty() {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        name.to_string()
    };
    check_project_name(&name)?;
    if let Some(other) = get_project(&ix.conn, &name)? {
        bail!(
            "project name {name} is taken by {}: give this one another with --name",
            other.path
        );
    }
    let root = list_roots(&ix.conn)?
        .into_iter()
        .find(|r| path.starts_with(&format!("{}/", r.path)))
        .map(|r| r.id)
        .unwrap_or_default();
    ix.conn.execute(
        "INSERT INTO projects (name, path, root, explicit, depth) VALUES (?, ?, ?, 1, ?)",
        params![name, path, root, depth],
    )?;
    Ok(ProjectAdded {
        name,
        path,
        depth: depth.into(),
        outcome: "added".into(),
    })
}

pub fn check_project_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('@')
        || name.starts_with('.')
        || name.contains([':', '#', '/', ' '])
        || name == "concept"
    {
        bail!("{name:?} can't name a project: no ':', '#', '/', spaces, or a leading '@' or '.'");
    }
    Ok(())
}

/// What syncing roots with disk found.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Sync {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Directories not made projects, and why.
    pub skipped: Vec<String>,
}

/// Makes every directory directly under each root a project (new ones at the
/// root's depth), and drops found projects whose directory is gone.
pub fn sync_roots(conn: &Connection) -> Result<Sync> {
    let mut out = Sync::default();
    let roots = list_roots(conn)?;
    let projects = list_projects(conn)?;
    for r in &roots {
        let Ok(rd) = std::fs::read_dir(&r.path) else {
            out.skipped.push(format!("{}: can't read it", r.path));
            continue;
        };
        let mut children: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        children.sort();
        for c in children {
            let name = c
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = s(&c);
            if projects.iter().any(|p| p.path == path) {
                continue;
            }
            if let Some(wt) = git::worktree_of(&c) {
                out.skipped.push(format!(
                    "{path}: a git worktree of {}, not a project of its own",
                    wt.display()
                ));
                continue;
            }
            if check_project_name(&name).is_err() {
                out.skipped
                    .push(format!("{path}: {name:?} can't name a project"));
                continue;
            }
            if let Some(other) = get_project(conn, &name)? {
                out.skipped.push(format!(
                    "{path}: the name {name} is taken by {} (agent index project add {path} --name OTHER)",
                    other.path
                ));
                continue;
            }
            conn.execute(
                "INSERT INTO projects (name, path, root, explicit, depth) VALUES (?, ?, ?, 0, ?)",
                params![name, path, r.id, r.depth],
            )?;
            out.added.push(name);
        }
    }
    for p in list_projects(conn)? {
        if !Path::new(&p.path).is_dir() {
            delete_project(conn, &p.name)?;
            out.removed.push(p.name);
        }
    }
    Ok(out)
}

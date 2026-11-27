//! The query-time stale check (docs/INDEX-V2.md, "Freshness"): a search
//! never silently serves a file that changed since it was indexed. For the
//! files behind the returned hits it reads what the index holds (size,
//! mtime, content key) in one query, then stats each file: a size that
//! differs is a change; an mtime that differs means hashing the file the way
//! the crawl keyed it, and comparing; a file gone is gone. The hit is marked
//! (`stale`: changed or gone), not re-cut: search only reads, and the
//! watcher (the one writer) re-cuts within seconds once told.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use super::Pg;
use crate::query::{Hit, SearchResult};

/// What the index holds for a file.
struct Indexed {
    path: String,
    size: i64,
    mtime: i64,
    key: String,
    project_dir: String,
    git: bool,
}

/// How a file on disk compares with its indexed copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disk {
    Fresh,
    Changed,
    Gone,
}

impl Disk {
    pub fn word(self) -> &'static str {
        match self {
            Disk::Fresh => "",
            Disk::Changed => "changed",
            Disk::Gone => "gone",
        }
    }
}

/// The file entity behind a hit: itself, or the file a symbol or section is
/// in. None for what isn't in a file (projects, dirs, side notes).
fn file_of(h: &Hit) -> Option<&str> {
    if !matches!(h.kind.as_str(), "file" | "symbol" | "section") {
        return None;
    }
    if matches!(
        h.corpus.as_str(),
        "memory" | "friction" | "session" | "pasted"
    ) {
        return None;
    }
    Some(h.id.split('#').next().unwrap_or(&h.id))
}

/// A git blob id (what `git hash-object` gives for unfiltered content).
fn blob_id(bytes: &[u8]) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    h.digest().to_string()
}

fn mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Compares one file with what the index holds.
fn compare(f: &Indexed) -> Disk {
    let p = Path::new(&f.path);
    let Ok(md) = std::fs::symlink_metadata(p) else {
        return Disk::Gone;
    };
    if !md.file_type().is_file() {
        // A symlink or something else: the crawl keyed its link text, not
        // its target; nothing here to compare.
        return Disk::Fresh;
    }
    if md.len() as i64 != f.size {
        return Disk::Changed;
    }
    let mtime = mtime_ns(&md);
    if f.mtime != 0 && mtime == f.mtime {
        return Disk::Fresh;
    }
    // The mtime moved (or the index doesn't know it): the content decides.
    if md.len() > crate::files::MAX_CONTENT {
        // Keyed by size and mtime, never read.
        return if f.mtime == 0 {
            Disk::Fresh
        } else {
            Disk::Changed
        };
    }
    let mut bytes = Vec::new();
    if std::fs::File::open(p)
        .and_then(|fh| {
            fh.take(crate::files::MAX_CONTENT + 1)
                .read_to_end(&mut bytes)
        })
        .is_err()
    {
        return Disk::Fresh;
    }
    let same = match f.key.len() {
        40 => {
            blob_id(&bytes) == f.key
                // Git may filter content on the way in (line endings,
                // LFS): ask it before calling a git file changed.
                || (f.git && git_key(&f.project_dir, &f.path).as_deref() == Some(f.key.as_str()))
        }
        20 => crate::crawl::hash(&[&bytes]) == f.key,
        // A key from size and mtime (unreadable, symlink): the mtime moved.
        _ => f.mtime == 0,
    };
    if same { Disk::Fresh } else { Disk::Changed }
}

/// `git hash-object` of one file, as the crawl keys a changed one.
fn git_key(dir: &str, path: &str) -> Option<String> {
    let rel = Path::new(path).strip_prefix(dir).ok()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("hash-object")
        .arg("--")
        .arg(rel)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Marks the hits (flat and grouped) whose files changed or went since they
/// were indexed; returns those files' absolute paths, to tell the watcher.
/// Its time goes in the result's `ms` as "stale check".
pub fn check(pg: &Pg, r: &mut SearchResult) -> Result<Vec<String>> {
    let t0 = Instant::now();
    let mut ids: HashSet<String> = HashSet::new();
    let groups = r.groups.iter().flatten().flat_map(|g| g.hits.iter());
    for h in r.hits.iter().chain(groups) {
        if let Some(f) = file_of(h) {
            ids.insert(f.to_string());
        }
    }
    let mut seen: HashMap<String, Disk> = HashMap::new();
    if !ids.is_empty() {
        let ids: Vec<String> = ids.into_iter().collect();
        let rows = pg.query(
            "SELECT e.id, e.path, e.size, e.mtime, e.content_key, coalesce(p.path, ''), coalesce(p.is_git, false)
             FROM entities e LEFT JOIN projects p ON p.name = e.project
             WHERE e.id = ANY($1) AND e.kind = 'file'",
            &[&ids],
        )?;
        for row in rows {
            let f = Indexed {
                path: row.get(1),
                size: row.get(2),
                mtime: row.get(3),
                key: row.get(4),
                project_dir: row.get(5),
                git: row.get(6),
            };
            seen.insert(row.get(0), compare(&f));
        }
    }
    let mut paths = Vec::new();
    let mut mark = |h: &mut Hit| {
        if let Some(d) = file_of(h).and_then(|f| seen.get(f)).copied()
            && d != Disk::Fresh
        {
            h.stale = d.word().to_string();
            let p = h.path.clone();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    };
    for h in r.hits.iter_mut() {
        mark(h);
    }
    for g in r.groups.iter_mut().flatten() {
        for h in g.hits.iter_mut() {
            mark(h);
        }
    }
    r.ms.insert("stale check".into(), t0.elapsed().as_secs_f64() * 1000.0);
    Ok(paths)
}

//! Edges code makes (docs/INDEX.md, "Edges"): `tests` by naming, `co-changes`
//! from history, `depends-on` from manifests and `documented-by` from docs'
//! backticked references, resolved across every project; and importance.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::crawl::{base_name, dir_id, normalize, parent_rel, prefix_range};
use crate::git;
use crate::manifest::Manifest;
use crate::roots::{self, Project};

/// Deletes a project's code edges of one kind, by a range on `source` (the
/// primary key's first column).
fn delete_project_edges(conn: &Connection, project: &str, kind: &str) -> Result<()> {
    let (lo, hi) = prefix_range(&format!("{project}:"));
    conn.execute(
        "DELETE FROM edges WHERE source >= ?1 AND source < ?2 AND kind = ?3 AND origin = 'code'",
        params![lo, hi, kind],
    )?;
    Ok(())
}

/// `tests` edges by naming: `x_test.rs`, `test_x.py`, `x.test.ts`, `x.spec.js`,
/// `XTests.swift` → the file they test, in the same dir if there is one, else
/// the one file of that name in the project.
pub fn tests_edges(conn: &Connection, project: &str) -> Result<()> {
    delete_project_edges(conn, project, "tests")?;
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
        let Some(target) = tested_name(base_name(rel)) else {
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
pub fn cochange(conn: &Connection, p: &Project, dir: &Path) -> Result<()> {
    delete_project_edges(conn, &p.name, "co-changes")?;
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

/// Edge counts by kind.
pub fn edge_counts(conn: &Connection) -> Result<BTreeMap<String, usize>> {
    let mut out = BTreeMap::new();
    for k in ["co-changes", "depends-on", "documented-by", "tests"] {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM edges WHERE kind = ?",
            params![k],
            |r| r.get(0),
        )?;
        out.insert(k.to_string(), n as usize);
    }
    Ok(out)
}

/// Rebuilds `depends-on` and `documented-by` across every project, from the
/// stored manifests and doc references. Returns how many references resolve
/// nowhere.
pub fn resolve_edges(conn: &Connection) -> Result<usize> {
    conn.execute(
        "DELETE FROM edges WHERE origin = 'code' AND kind IN ('depends-on', 'documented-by')",
        [],
    )?;
    conn.execute("DELETE FROM dangling", [])?;
    // Projects, dirs and files by absolute path.
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
    let lookup = |abs: &Path| -> Option<String> { by_path.get(&normalize(abs)).cloned() };
    // A symbol's or section's key, for an id that names one.
    let key_of = |id: &str| -> Option<String> {
        if let Some(k) = keys.get(id) {
            return Some(k.clone());
        }
        conn.query_row(
            "SELECT content_key FROM entities WHERE id = ?",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    };
    let add_edge = |src: &str, dst: &str, kind: &str, evidence: &str| -> Result<()> {
        if src == dst {
            return Ok(());
        }
        conn.execute(
            "INSERT INTO edges (source, target, kind, weight, origin, evidence, source_key, target_key)
             VALUES (?1, ?2, ?3, 1, 'code', ?4, ?5, ?6)
             ON CONFLICT(source, target, kind) DO UPDATE SET weight = weight + 1",
            params![
                src,
                dst,
                kind,
                evidence,
                key_of(src).unwrap_or_default(),
                key_of(dst).unwrap_or_default()
            ],
        )?;
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
                add_edge(d, &t, "depends-on", &evidence)?;
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
    // A bare name or a partial path (`stale.rs`, `service/held.rs`, `run/`)
    // resolves to the one file or dir in the doc's project whose path ends
    // with it; one that several end with is ambiguous, not drift.
    let mut by_suffix: HashMap<(String, String), Vec<String>> = HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT id, project, rel FROM entities WHERE kind IN ('file', 'dir') AND rel != ''",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, project, rel) = row?;
            let mut rest = rel.as_str();
            loop {
                by_suffix
                    .entry((project.clone(), rest.to_string()))
                    .or_default()
                    .push(id.clone());
                match rest.split_once('/') {
                    Some((_, r)) => rest = r,
                    None => break,
                }
            }
        }
    }
    let mut dangling = 0;
    for (doc, r, line, doc_path) in refs {
        let project = crate::model::project_of_id(&doc).to_string();
        let own_root = project_path.get(project.as_str()).copied().unwrap_or("");
        let doc_dir = Path::new(&doc_path)
            .parent()
            .unwrap_or(Path::new("/"))
            .to_path_buf();
        let trimmed = r.trim_end_matches('/');
        let mut hit: Option<String> = None;
        let is_id = trimmed
            .split_once(':')
            .is_some_and(|(proj, _)| project_path.contains_key(proj));
        if is_id {
            // An id (`alpha:src/x.rs`, `alpha:src/x.rs#parse`): it is there, or
            // it is drift.
            let (proj, rest) = trimmed.split_once(':').unwrap_or_default();
            let id = dir_id(proj, rest);
            if key_of(&id).is_some() {
                hit = Some(id);
            }
        } else if trimmed.starts_with('/') {
            hit = lookup(Path::new(trimmed));
            // A path outside every indexed tree (`/bin/bash`), or a slash
            // command (`/resume`): nothing the index can judge.
            let inside = |base: &str| trimmed.starts_with(&format!("{base}/"));
            if hit.is_none()
                && !projects.iter().any(|p| inside(&p.path))
                && !root_paths.iter().any(|r| inside(r))
            {
                continue;
            }
        } else {
            for b in [doc_dir.as_path(), Path::new(own_root)] {
                if let Some(h) = lookup(&b.join(trimmed)) {
                    hit = Some(h);
                    break;
                }
            }
            // A path from a root names another project (`beta/notes.md`).
            // (Not any other project's root: a broken `src/lib.rs` would bind
            // to whichever sibling repo has one.)
            if hit.is_none() {
                hit = root_paths
                    .iter()
                    .find_map(|rp| lookup(&Path::new(rp).join(trimmed)));
            }
            if hit.is_none()
                && let Some(c) = by_suffix.get(&(project.clone(), trimmed.to_string()))
            {
                if c.len() == 1 {
                    hit = Some(c[0].clone());
                } else {
                    // Several files answer: no edge, and no drift either.
                    continue;
                }
            }
            // On disk inside the project but not indexed (ignored, build
            // output, a runtime dir): not drift.
            if hit.is_none() && !own_root.is_empty() {
                let on_disk = [doc_dir.as_path(), Path::new(own_root)].iter().any(|b| {
                    let p = normalize(&b.join(trimmed));
                    p.starts_with(&format!("{own_root}/")) && std::fs::symlink_metadata(&p).is_ok()
                });
                if on_disk {
                    continue;
                }
            }
        }
        match hit {
            Some(h) => {
                let doc_rel = doc.split_once(':').map(|(_, r)| r).unwrap_or(&doc);
                add_edge(&h, &doc, "documented-by", &format!("{doc_rel}:{line}"))?;
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
    Ok(dangling)
}

/// Importance: how many dependents and docs point at an entity (`depends-on`
/// fan-in, the docs that document it), plus its co-change degree (no PageRank
/// yet); a directory or project adds half its children's. Files, dirs and
/// projects only: symbols and sections have none of these edges.
pub fn importance(conn: &Connection) -> Result<()> {
    let rows: Vec<(String, String, f64, usize, f64)> = {
        let mut st = conn.prepare(
            "SELECT e.id, e.parent, e.importance, e.kind,
               (SELECT count(DISTINCT source) FROM edges WHERE target = e.id AND kind = 'depends-on')
             + (SELECT count(DISTINCT target) FROM edges WHERE source = e.id AND kind = 'documented-by')
             + (SELECT count(*) FROM edges WHERE target = e.id AND kind = 'co-changes')
             FROM entities e WHERE e.kind IN ('file', 'dir', 'project')",
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
            Ok((id, r.get(1)?, r.get(2)?, depth, r.get::<_, i64>(4)? as f64))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut imp: HashMap<String, f64> = rows.iter().map(|r| (r.0.clone(), r.4)).collect();
    let mut ordered = rows.clone();
    ordered.sort_by_key(|r| std::cmp::Reverse(r.3));
    for (id, parent, ..) in &ordered {
        let v = imp.get(id).copied().unwrap_or(0.0);
        if v > 0.0
            && let Some(pv) = imp.get_mut(parent)
        {
            *pv += v * 0.5;
        }
    }
    let mut st = conn.prepare("UPDATE entities SET importance = ? WHERE id = ?")?;
    for (id, _, old, ..) in &rows {
        let v = (imp.get(id).copied().unwrap_or(0.0) * 100.0).round() / 100.0;
        if (v - old).abs() > f64::EPSILON {
            st.execute(params![v, id])?;
        }
    }
    Ok(())
}

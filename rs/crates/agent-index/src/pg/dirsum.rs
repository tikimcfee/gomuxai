//! The bottom-up pass (docs/INDEX-V2.md, "Summaries and asks, by a local
//! model"): after the files, each directory holding a selected file is
//! summarized from what it holds, deepest first, and each project last.
//!
//! A unit's input is composed, not read: its README's start (when it has
//! one), its listing (every child's name, summarized or not) and an entry
//! per child, the child's live summary or else its seed. A directory whose
//! entries don't fit one request is cut into parts: each part is summarized
//! from some of its entries (kept as status `part`, never shown, reused by
//! a resume), and the directory from its README, listing and parts.
//!
//! Units go through [`super::summ::run`] like files, under the directory
//! prompt's fingerprint: the same request, checks, stops, stats and resume.
//! A unit's content key is its directory's (a hash of everything under it),
//! so its summary shows while nothing under it changed; a child summarized
//! again later doesn't stale it (run the pass after the files).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use serde::Serialize;

use super::Pg;
use super::summ::{self, Coverage, Options, Stats};
use crate::summarize::{self as sm, Candidate};

/// Characters of a README's start a unit's input carries.
pub const README_CHARS: usize = 4_000;
/// Characters of the listing (names) a unit's input carries.
pub const LISTING_CHARS: usize = 3_000;
/// Characters of one entry's text.
pub const ENTRY_CHARS: usize = 420;

/// A directory or project to summarize.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Unit {
    pub id: String,
    pub project: String,
    pub rel: String,
    /// `dir` or `project`.
    pub kind: String,
    pub path: String,
    pub content_key: String,
    pub seed: String,
    /// 0 for a project, 1 for a directory at its top, and so on.
    pub depth: usize,
}

/// The directories holding one of `files` at any depth, and their projects,
/// deepest first (by id within a depth).
pub fn units(pg: &Pg, files: &[Candidate]) -> Result<Vec<Unit>> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for c in files {
        ids.insert(c.project.clone());
        let mut d = c.rel.as_str();
        while let Some((up, _)) = d.rsplit_once('/') {
            ids.insert(format!("{}:{up}", c.project));
            d = up;
        }
    }
    let ids: Vec<String> = ids.into_iter().collect();
    let rows = pg.query(
        "SELECT id, project, rel, kind, path, content_key, seed FROM entities
         WHERE id = ANY($1) AND kind IN ('dir', 'project')",
        &[&ids],
    )?;
    let mut out: Vec<Unit> = rows
        .iter()
        .map(|r| {
            let kind: String = r.get(3);
            let rel: String = r.get(2);
            let depth = if kind == "project" || rel.is_empty() {
                0
            } else {
                rel.matches('/').count() + 1
            };
            Unit {
                id: r.get(0),
                project: r.get(1),
                rel,
                kind,
                path: r.get(4),
                content_key: r.get(5),
                seed: r.get(6),
                depth,
            }
        })
        .collect();
    out.sort_by(|a, b| b.depth.cmp(&a.depth).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// At most `n` units, evenly spaced over the deepest-first order (a
/// preflight's sample); 0 is all.
pub fn sample(units: Vec<Unit>, n: usize) -> Vec<Unit> {
    if n == 0 || units.len() <= n {
        return units;
    }
    let step = units.len() as f64 / n as f64;
    (0..n)
        .map(|i| units[(i as f64 * step) as usize].clone())
        .collect()
}

/// One child of a unit, as its input shows it.
#[derive(Debug, Clone, Default)]
pub struct Child {
    pub name: String,
    pub dir: bool,
    pub file_kind: String,
    /// Its live summary, else its seed; empty: in the listing only.
    pub text: String,
    /// Whether `text` is a model's summary (not a seed).
    pub summarized: bool,
    pub path: String,
}

/// A child's kind in a word or two.
fn child_kind(c: &Child) -> &'static str {
    if c.dir {
        return "directory";
    }
    match c.file_kind.as_str() {
        "source" => "source",
        "test" => "test",
        "doc" => "document",
        "config" => "config",
        "build" => "build file",
        _ => "file",
    }
}

/// Cuts `s` to at most `n` characters at a word, marking the cut.
fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    let cut = match cut.rfind(char::is_whitespace) {
        Some(i) if i > n / 2 => cut[..i].to_string(),
        _ => cut,
    };
    format!("{cut} …")
}

/// An entry line.
pub fn entry_line(c: &Child) -> String {
    format!(
        "- {}{} ({}{}): {}",
        c.name,
        if c.dir { "/" } else { "" },
        child_kind(c),
        if c.summarized { "" } else { ", its first line" },
        clip(&c.text, ENTRY_CHARS).replace('\n', " ")
    )
}

/// The listing: every child's name (directories end in `/`), cut at
/// [`LISTING_CHARS`] with how many more there are.
pub fn listing(children: &[Child]) -> String {
    let mut out = String::new();
    for (i, c) in children.iter().enumerate() {
        let name = format!("{}{}", c.name, if c.dir { "/" } else { "" });
        if out.len() + name.len() + 2 > LISTING_CHARS {
            out.push_str(&format!(", … and {} more", children.len() - i));
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&name);
    }
    out
}

/// A unit's composed text: its README's start, its listing, and either
/// its entries or its parts' summaries.
pub fn text(readme: &(String, String), children: &[Child], body: &str) -> String {
    let mut t = String::new();
    if !readme.1.trim().is_empty() {
        t.push_str(&format!(
            "README ({}, its start):\n{}\n\n",
            readme.0,
            clip(&readme.1, README_CHARS)
        ));
    }
    t.push_str(&format!(
        "Listing ({} entries; directories end in /):\n{}\n\n",
        children.len(),
        listing(children)
    ));
    t.push_str(body);
    t
}

/// The entries' lines cut into groups of at most `budget` characters
/// (one group when they fit).
pub fn groups(lines: &[String], budget: usize) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    let mut size = 0;
    for l in lines {
        if size + l.len() + 1 > budget && !out.last().unwrap().is_empty() {
            out.push(Vec::new());
            size = 0;
        }
        size += l.len() + 1;
        out.last_mut().unwrap().push(l.clone());
    }
    out
}

/// The characters a unit's entries may take in one request: the prompt's
/// bound less the rules, header, README and listing (about 3 characters a
/// token, to be safe; the server's count is checked again per request).
pub fn entries_budget(cfg: &crate::config::Summaries) -> usize {
    (cfg.max_input_tokens * 3).saturating_sub(1_200 * 3 + README_CHARS + LISTING_CHARS + 600)
}

/// A part's content key: its directory's, its place and its entries.
fn part_key(unit: &Unit, k: usize, n: usize, body: &str) -> String {
    sm::sha256(format!("{}\npart {k} of {n}\n{body}", unit.content_key).as_bytes())[..40]
        .to_string()
}

/// The children of `units`, by parent id, dirs first then files, by name;
/// with their live summaries (any writer's) or seeds, and each unit's
/// README (name, text read and clipped).
fn children(pg: &Pg, units: &[Unit], o: &Options) -> Result<(Children, Readmes)> {
    let ids: Vec<String> = units.iter().map(|u| u.id.clone()).collect();
    let rows = pg.query(
        "SELECT e.parent, e.id, e.kind, e.rel, e.file_kind, e.seed, e.path,
                (SELECT s.summary FROM summaries s WHERE s.entity_id = e.id AND s.status = 'live'
                 ORDER BY s.id DESC LIMIT 1)
         FROM entities e WHERE e.parent = ANY($1) AND e.kind IN ('dir', 'file')
         ORDER BY e.parent, e.kind, e.rel",
        &[&ids],
    )?;
    let mut by: Children = HashMap::new();
    let mut readme: Readmes = HashMap::new();
    for r in rows {
        let (parent, id, kind, rel): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        // A child left out of the pass (`--skip-prefix`) is named, not
        // described.
        let left_out = o
            .skip_prefixes
            .iter()
            .any(|p| id.starts_with(p.as_str()) || format!("{id}/").starts_with(p.as_str()));
        let summary: Option<String> = r.get(7);
        let seed: String = r.get(5);
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let path: String = r.get(6);
        let (text, summarized) = match summary.filter(|s| !s.trim().is_empty()) {
            _ if left_out => (String::new(), false),
            Some(s) => (s, true),
            None => (seed, false),
        };
        if kind == "file"
            && !readme.contains_key(&parent)
            && name.to_ascii_lowercase().starts_with("readme")
            && let Ok(t) = std::fs::read_to_string(&path)
        {
            readme.insert(
                parent.clone(),
                (name.clone(), t.chars().take(README_CHARS * 2).collect()),
            );
        }
        by.entry(parent).or_default().push(Child {
            name,
            dir: kind == "dir",
            file_kind: r.get(4),
            text,
            summarized,
            path,
        });
    }
    for v in by.values_mut() {
        v.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.cmp(&b.name)));
    }
    Ok((by, readme))
}

/// Each unit's children, by its id.
type Children = HashMap<String, Vec<Child>>;
/// Each unit's README: (its name, its text's start).
type Readmes = HashMap<String, (String, String)>;

/// A unit as the run's candidate.
fn candidate(u: &Unit, composed: String, names: Vec<String>) -> Candidate {
    Candidate {
        id: u.id.clone(),
        project: u.project.clone(),
        rel: u.rel.clone(),
        path: u.path.clone(),
        file_kind: u.kind.clone(),
        size: composed.len() as u64,
        content_key: u.content_key.clone(),
        seed: u.seed.clone(),
        symbols: names,
        composed: Some(composed),
        ..Default::default()
    }
}

/// What one depth's units need: the parts to summarize first, and the
/// units themselves (a large one from its parts' summaries, when `key`'s
/// are there; else from the entries that fit, the rest named in the
/// listing only).
pub struct Wave {
    pub parts: Vec<Candidate>,
    pub units: Vec<Candidate>,
    /// Units cut into parts.
    pub large: usize,
}

/// Composes `units` (one depth) under the directory fingerprint `key`.
pub fn compose(pg: &Pg, units: &[Unit], key: &str, o: &Options) -> Result<Wave> {
    let cfg = &pg.cfg.llm.summaries;
    let budget = entries_budget(cfg);
    let (kids, readmes) = children(pg, units, o)?;
    let mut wave = Wave {
        parts: Vec::new(),
        units: Vec::new(),
        large: 0,
    };
    let none = (String::new(), String::new());
    for u in units {
        let empty = Vec::new();
        let ch = kids.get(&u.id).unwrap_or(&empty);
        let readme = readmes.get(&u.id).unwrap_or(&none);
        let names: Vec<String> = ch.iter().map(|c| c.name.clone()).collect();
        let lines: Vec<String> = ch
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .map(entry_line)
            .collect();
        let gs = groups(&lines, budget);
        if gs.len() <= 1 {
            let body = format!("Entries:\n{}", lines.join("\n"));
            wave.units
                .push(candidate(u, text(readme, ch, &body), names));
            continue;
        }
        wave.large += 1;
        let n = gs.len();
        let mut got: Vec<(usize, String)> = Vec::new();
        for (i, g) in gs.iter().enumerate() {
            let k = i + 1;
            let body = format!("Entries (part {k} of {n}):\n{}", g.join("\n"));
            let pk = part_key(u, k, n, &body);
            let done = pg.query(
                "SELECT summary FROM summaries WHERE entity_id = $1 AND content_key = $2
                   AND fingerprint = $3 AND status = 'part' ORDER BY id DESC LIMIT 1",
                &[&u.id, &pk, &key],
            )?;
            if let Some(r) = done.first() {
                got.push((k, r.get(0)));
                continue;
            }
            let mut c = candidate(u, body, names.clone());
            c.file_kind = "part".into();
            c.part = format!("part {k} of {n}");
            c.content_key = pk;
            wave.parts.push(c);
        }
        let body = if got.len() == n {
            format!(
                "Parts (each summarized from some of its entries):\n{}",
                got.iter()
                    .map(|(k, s)| format!("- part {k} of {n}: {}", s.replace('\n', " ")))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        } else {
            // Parts missing (not run yet, or failed): the entries that fit.
            format!(
                "Entries (the first {} of {}; the rest are named in the listing only):\n{}",
                gs[0].len(),
                lines.len(),
                gs[0].join("\n")
            )
        };
        wave.units
            .push(candidate(u, text(readme, ch, &body), names));
    }
    Ok(wave)
}

/// What the pass would do, without a model: units by depth, how many are
/// cut into parts and into how many, and the composed input's size.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DirCoverage {
    pub units: usize,
    pub projects: usize,
    pub by_depth: BTreeMap<usize, usize>,
    pub large: usize,
    pub parts: usize,
    /// Units whose every entry is a seed (no child summarized).
    pub seeds_only: usize,
    /// The composed inputs' characters, all units and parts.
    pub chars: usize,
}

/// [`DirCoverage`] for `units`, composed from the summaries there now.
pub fn coverage(pg: &Pg, units: &[Unit], o: &Options) -> Result<DirCoverage> {
    let mut cov = DirCoverage {
        units: units.len(),
        ..Default::default()
    };
    let mut by_depth: BTreeMap<usize, Vec<Unit>> = BTreeMap::new();
    for u in units {
        by_depth.entry(u.depth).or_default().push(u.clone());
        if u.kind == "project" {
            cov.projects += 1;
        }
    }
    let (kids, _) = children(pg, units, o)?;
    for u in units {
        if kids
            .get(&u.id)
            .is_none_or(|v| v.iter().all(|c| !c.summarized))
        {
            cov.seeds_only += 1;
        }
    }
    for (d, us) in by_depth {
        cov.by_depth.insert(d, us.len());
        let w = compose(pg, &us, "", o)?;
        cov.large += w.large;
        cov.parts += w.parts.len();
        cov.chars += w
            .parts
            .iter()
            .chain(w.units.iter())
            .map(|c| c.composed.as_ref().map_or(0, String::len))
            .sum::<usize>();
    }
    Ok(cov)
}

/// The pass: each depth, deepest first, its parts then its units, each a
/// run of [`summ::run`] under the directory fingerprint. Stops at the
/// first run that stops (its error), with every finished run's stats.
pub fn run(
    pg: &Pg,
    units: Vec<Unit>,
    o: &Options,
    progress: &mut dyn FnMut(&str, &Stats),
) -> Result<Vec<(String, Stats)>> {
    let cfg = &pg.cfg.llm.summaries;
    let (_, info, _) = summ::fingerprint(cfg, &o.code)?;
    let key = sm::Fingerprint::new_dirs(cfg, &info, &o.code).key();
    let o = Options {
        dirs: true,
        ..o.clone()
    };
    let mut by_depth: BTreeMap<usize, Vec<Unit>> = BTreeMap::new();
    for u in units {
        by_depth.entry(u.depth).or_default().push(u);
    }
    let mut out = Vec::new();
    for (d, us) in by_depth.into_iter().rev() {
        let w = compose(pg, &us, &key, &o)?;
        if !w.parts.is_empty() {
            let label = format!("depth {d}, parts of {} large directories", w.large);
            let cov = Coverage {
                files: w.parts.len(),
                summarize: w.parts.len(),
                ..Default::default()
            };
            let s = summ::run(pg, w.parts, &o, &cov, &mut |s| progress(&label, s))?;
            out.push((label, s));
        }
        // Composed again: the parts just written are there now.
        let w = compose(pg, &us, &key, &o)?;
        let label = if d == 0 {
            "projects".to_string()
        } else {
            format!("depth {d}")
        };
        let cov = Coverage {
            files: w.units.len(),
            summarize: w.units.len(),
            ..Default::default()
        };
        let s = summ::run(pg, w.units, &o, &cov, &mut |s| progress(&label, s))?;
        out.push((label, s));
    }
    Ok(out)
}

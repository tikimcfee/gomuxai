//! The bottom-up pass (docs/INDEX-V2.md, "Summaries and asks, by a local
//! model"): after the files, each directory holding a selected file is
//! summarized from what it holds, deepest first, and each project last.
//!
//! A unit's input is composed, not read: its README's start (when it has
//! one, scrubbed), its listing (every child's name, summarized or not) and
//! an entry per child, the child's live summary or else its seed. A child
//! left out (`--skip-prefix`, `[summaries].withhold`) is named in the
//! listing only: no entry, no README read from it. A directory whose
//! entries don't fit one request is cut into parts: each part is summarized
//! from some of its entries (kept as status `part`, never shown, reused by
//! a resume), and the directory from its README, listing and parts only;
//! while a part is missing (failed or not answered) the directory waits,
//! with no row, so a resume (`--retry-failed` for a failed part) does it.
//!
//! Units go through [`super::summ::run_watched`] like files, under the
//! directory prompt's fingerprint: the same request, checks, stats and
//! resume, and one [`summ::Watch`] for the whole pass (one wall-clock cap,
//! one stall clock, one error window). A unit's content key is its
//! directory's (a hash of everything under it), so its summary shows while
//! nothing under it changed. A unit composed while one of its children in
//! the pass wasn't settled (no summary and no failure, or a provisional
//! summary of its own) is written provisional (`meta.incomplete`): shown,
//! but not done, so a resume composes it again; a preflight's sample can't
//! leave a parent built from its children's seeds for good.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use super::Pg;
use super::summ::{self, Coverage, Options, Stats, Watch};
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

/// The pass's scope: the ids of `files` and of every directory and project
/// holding one. A child in scope is expected to be summarized.
pub fn scope(files: &[Candidate]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for c in files {
        ids.insert(c.id.clone());
        ids.insert(c.project.clone());
        let mut d = c.rel.as_str();
        while let Some((up, _)) = d.rsplit_once('/') {
            ids.insert(format!("{}:{up}", c.project));
            d = up;
        }
    }
    ids
}

/// Whether `id` is under one of `prefixes` (`project:dir/`), or is the
/// directory one names.
fn under(id: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        !p.is_empty() && (id.starts_with(p.as_str()) || format!("{id}/").starts_with(p.as_str()))
    })
}

/// Left out of the pass: under a `--skip-prefix` or a
/// `[summaries].withhold` prefix. Its name may show; nothing of it is read.
pub fn left_out(pg: &Pg, o: &Options, id: &str) -> bool {
    under(id, &o.skip_prefixes) || under(id, &pg.cfg.summaries.withhold)
}

/// The directories holding one of `files` at any depth, and their projects,
/// deepest first (by id within a depth); none left out.
pub fn units(pg: &Pg, files: &[Candidate], o: &Options) -> Result<Vec<Unit>> {
    let ids: BTreeSet<String> = scope(files)
        .into_iter()
        .filter(|id| !left_out(pg, o, id))
        .collect();
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
    /// In the pass's scope and not left out: it should have a summary.
    pub expected: bool,
    /// Its summary is final for now: a live one that isn't provisional,
    /// or its own failure (a resume with `--retry-failed` asks again); a
    /// failed part of it (same entity id) leaves it waiting, not settled.
    pub settled: bool,
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

/// Each unit's children, by its id.
type Children = HashMap<String, Vec<Child>>;
/// Each unit's README: (its name, its text's start, scrubbed).
type Readmes = HashMap<String, (String, String)>;

/// The children of `units`, by parent id, dirs first then files, by name;
/// with their live summaries (any writer's) or seeds, whether each is
/// expected and settled, and each unit's README (name, text read, scrubbed
/// and clipped). A child left out is named only, and its README unread.
fn children(
    pg: &Pg,
    units: &[Unit],
    scope: &HashSet<String>,
    o: &Options,
) -> Result<(Children, Readmes)> {
    let ids: Vec<String> = units.iter().map(|u| u.id.clone()).collect();
    let rows = pg.query(
        "SELECT e.parent, e.id, e.kind, e.rel, e.file_kind, e.seed, e.path, l.summary, l.incomplete,
                EXISTS (SELECT 1 FROM summaries f WHERE f.entity_id = e.id AND f.status = 'failed'
                          AND coalesce(f.meta->>'unit', '') <> 'part')
         FROM entities e
         LEFT JOIN LATERAL (
             SELECT s.summary, coalesce((s.meta->>'incomplete')::int, 0) AS incomplete
             FROM summaries s WHERE s.entity_id = e.id AND s.status = 'live'
             ORDER BY s.id DESC LIMIT 1) l ON true
         WHERE e.parent = ANY($1) AND e.kind IN ('dir', 'file')
         ORDER BY e.parent, e.kind, e.rel",
        &[&ids],
    )?;
    let words = &pg.cfg.llm.summaries.scrub_words;
    let mut by: Children = HashMap::new();
    let mut readme: Readmes = HashMap::new();
    for r in rows {
        let (parent, id, kind, rel): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let out = left_out(pg, o, &id);
        let summary: Option<String> = r.get(7);
        let incomplete: Option<i32> = r.get(8);
        let failed: bool = r.get(9);
        let seed: String = r.get(5);
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let path: String = r.get(6);
        let (text, summarized) = match summary.filter(|s| !s.trim().is_empty()) {
            _ if out => (String::new(), false),
            Some(s) => (s, true),
            None => (seed, false),
        };
        let settled = (summarized && incomplete.unwrap_or(0) == 0) || failed;
        if kind == "file"
            && !out
            && !readme.contains_key(&parent)
            && name.to_ascii_lowercase().starts_with("readme")
            && let Ok(t) = std::fs::read_to_string(&path)
        {
            let start: String = t.chars().take(README_CHARS * 2).collect();
            readme.insert(parent.clone(), (name.clone(), sm::scrub(&start, words).0));
        }
        by.entry(parent).or_default().push(Child {
            name,
            dir: kind == "dir",
            file_kind: r.get(4),
            text,
            summarized,
            path,
            expected: !out && scope.contains(&id),
            settled,
        });
    }
    for v in by.values_mut() {
        v.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.cmp(&b.name)));
    }
    Ok((by, readme))
}

/// A unit as the run's candidate.
fn candidate(u: &Unit, composed: String, names: Vec<String>, incomplete: usize) -> Candidate {
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
        incomplete,
        ..Default::default()
    }
}

/// What one depth's units need: the parts to summarize first, and the
/// units themselves (a large one only from its parts' summaries, when
/// `key`'s are all there; else it waits).
#[derive(Default)]
pub struct Wave {
    pub parts: Vec<Candidate>,
    pub units: Vec<Candidate>,
    /// Units cut into parts.
    pub large: usize,
    /// Their parts, done or not.
    pub parts_total: usize,
    /// Large units with a part missing: not composed, no row.
    pub waiting: Vec<String>,
    /// Their composed input's estimated size (README, listing, parts).
    pub waiting_chars: usize,
}

/// Composes `units` (one depth) under the directory fingerprint `key`.
pub fn compose(
    pg: &Pg,
    units: &[Unit],
    scope: &HashSet<String>,
    key: &str,
    o: &Options,
) -> Result<Wave> {
    let cfg = &pg.cfg.llm.summaries;
    let budget = entries_budget(cfg);
    let (kids, readmes) = children(pg, units, scope, o)?;
    let mut wave = Wave::default();
    let none = (String::new(), String::new());
    for u in units {
        let empty = Vec::new();
        let ch = kids.get(&u.id).unwrap_or(&empty);
        let readme = readmes.get(&u.id).unwrap_or(&none);
        let names: Vec<String> = ch.iter().map(|c| c.name.clone()).collect();
        let unsettled = ch.iter().filter(|c| c.expected && !c.settled).count();
        let lines: Vec<String> = ch
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .map(entry_line)
            .collect();
        let gs = groups(&lines, budget);
        if gs.len() <= 1 {
            let body = format!("Entries:\n{}", lines.join("\n"));
            wave.units
                .push(candidate(u, text(readme, ch, &body), names, unsettled));
            continue;
        }
        wave.large += 1;
        let n = gs.len();
        wave.parts_total += n;
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
            let mut c = candidate(u, body, names.clone(), 0);
            c.file_kind = "part".into();
            c.part = format!("part {k} of {n}");
            c.content_key = pk;
            wave.parts.push(c);
        }
        if got.len() < n {
            // A part missing (not run yet, failed, not answered): the
            // directory waits; never composed from some of its entries.
            wave.waiting.push(u.id.clone());
            wave.waiting_chars += text(readme, ch, "").len() + n * (ENTRY_CHARS + 20);
            continue;
        }
        let body = format!(
            "Parts (each summarized from some of its entries):\n{}",
            got.iter()
                .map(|(k, s)| format!("- part {k} of {n}: {}", s.replace('\n', " ")))
                .collect::<Vec<_>>()
                .join("\n")
        );
        wave.units
            .push(candidate(u, text(readme, ch, &body), names, unsettled));
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
    pub parts_by_depth: BTreeMap<usize, usize>,
    /// Units whose every entry is a seed (no child summarized).
    pub seeds_only: usize,
    /// The composed inputs' characters, all units and parts (a large
    /// unit's from its README, listing and parts, estimated).
    pub chars: usize,
}

/// [`DirCoverage`] for `units`, composed from the summaries there now.
pub fn coverage(
    pg: &Pg,
    units: &[Unit],
    scope: &HashSet<String>,
    o: &Options,
) -> Result<DirCoverage> {
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
    let (kids, _) = children(pg, units, scope, o)?;
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
        let w = compose(pg, &us, scope, "", o)?;
        cov.large += w.large;
        cov.parts += w.parts_total;
        cov.parts_by_depth.insert(d, w.parts_total);
        cov.chars += w.waiting_chars
            + w.parts
                .iter()
                .chain(w.units.iter())
                .map(|c| c.composed.as_ref().map_or(0, String::len))
                .sum::<usize>();
    }
    Ok(cov)
}

/// One finished run of the pass, in its status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WaveRow {
    pub wave: String,
    pub run_id: i64,
    pub selected: usize,
    pub done_before: usize,
    pub written: usize,
    pub failed: usize,
    pub errors: usize,
    pub wall_s: f64,
}

/// The whole pass's status, rewritten as it goes (`--stats`): units and
/// parts done of the total, errors, rate and time left over the pass, the
/// wave running, and each finished wave.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Pass {
    /// `running`, `finished` or `stopped`.
    pub state: String,
    /// The wave running (or the last).
    pub wave: String,
    pub fingerprint: String,
    pub units: usize,
    /// Parts composed so far, and estimated for the depths not reached.
    pub parts: usize,
    pub total: usize,
    /// Written, failed, done before, and large units left waiting.
    pub done: usize,
    pub remaining: usize,
    pub written: usize,
    pub failed: usize,
    /// Failed, not answered and pending.
    pub errors: usize,
    /// Large units left waiting for a missing part (no row).
    pub waiting_on_parts: usize,
    /// Units written provisional (a child not settled): a resume redoes them.
    pub provisional: usize,
    pub units_per_s: f64,
    pub prefill_tok_s: f64,
    pub wall_s: f64,
    pub eta_s: f64,
    pub aborted: Option<String>,
    pub waves: Vec<WaveRow>,
    /// The running wave's own stats.
    pub current: Option<Stats>,
    pub updated: String,
    #[serde(skip)]
    t0: Option<Instant>,
    #[serde(skip)]
    parts_by_depth: BTreeMap<usize, usize>,
    #[serde(skip)]
    in_tokens: u64,
}

impl Pass {
    /// The figures, from the finished waves and `cur` (the one running).
    fn figure(&mut self, cur: Option<&Stats>) {
        self.parts = self.parts_by_depth.values().sum();
        self.total = self.units + self.parts;
        let fin = |f: &dyn Fn(&WaveRow) -> usize| self.waves.iter().map(f).sum::<usize>();
        let (mut done, mut written, mut failed, mut errors) = (
            fin(&|w| w.done_before + w.written + w.failed),
            fin(&|w| w.written),
            fin(&|w| w.failed),
            fin(&|w| w.errors),
        );
        let mut tokens = self.in_tokens;
        if let Some(s) = cur {
            let f: usize = s.failed.values().sum();
            done += s.done_before + s.written + f;
            written += s.written;
            failed += f;
            errors += s.errors;
            tokens += s.in_tokens;
        }
        self.done = done + self.waiting_on_parts;
        self.written = written;
        self.failed = failed;
        self.errors = errors;
        self.remaining = self.total.saturating_sub(self.done);
        let wall = self.t0.map_or(0.0, |t| t.elapsed().as_secs_f64());
        self.wall_s = (wall * 10.0).round() / 10.0;
        if wall > 0.0 {
            self.units_per_s = ((written + failed) as f64 / wall * 100.0).round() / 100.0;
            self.prefill_tok_s = (tokens as f64 / wall).round();
        }
        self.eta_s = if self.units_per_s > 0.0 {
            (self.remaining as f64 / self.units_per_s).round()
        } else {
            0.0
        };
        self.current = cur.cloned();
        self.updated = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    }

    /// A finished (or stopped) wave into the totals.
    fn close(&mut self, label: &str, s: &Stats) {
        self.in_tokens += s.in_tokens;
        self.waves.push(WaveRow {
            wave: label.to_string(),
            run_id: s.run_id,
            selected: s.selected,
            done_before: s.done_before,
            written: s.written,
            failed: s.failed.values().sum(),
            errors: s.errors,
            wall_s: s.wall_s,
        });
    }

    fn save(&self, path: Option<&std::path::PathBuf>) {
        let Some(p) = path else { return };
        let tmp = p.with_extension("tmp");
        if let Ok(s) = serde_json::to_string_pretty(self)
            && std::fs::write(&tmp, s).is_ok()
        {
            let _ = std::fs::rename(&tmp, p);
        }
    }
}

/// Units written provisional by run `run_id`.
fn provisional(pg: &Pg, run_id: i64) -> Result<usize> {
    let r = pg.query(
        "SELECT count(*) FROM summaries WHERE run_id = $1 AND status = 'live'
           AND coalesce((meta->>'incomplete')::int, 0) > 0",
        &[&run_id],
    )?;
    Ok(r.first().map_or(0, |r| r.get::<_, i64>(0) as usize))
}

/// The pass: each depth, deepest first, its parts then its units, each a
/// run of [`summ::run_watched`] under the directory fingerprint, all under
/// one [`Watch`] (`o`'s cap, stall and error window span the pass). The
/// pass's status goes to `o.stats_path` as it runs. Stops at the first run
/// that stops (its error), with the status saying why.
pub fn run(
    pg: &Pg,
    units: Vec<Unit>,
    scope: &HashSet<String>,
    o: &Options,
    progress: &mut dyn FnMut(&str, &Stats),
) -> Result<Vec<(String, Stats)>> {
    let cfg = &pg.cfg.llm.summaries;
    let (_, info, _) = summ::fingerprint(cfg, &o.code)?;
    let key = sm::Fingerprint::new_dirs(cfg, &info, &o.code).key();
    let path = o.stats_path.clone();
    // The runs' own stats stay in their `runs` rows; the file is the pass's.
    let o = Options {
        dirs: true,
        stats_path: None,
        ..o.clone()
    };
    let mut watch = Watch::default();
    let est = coverage(pg, &units, scope, &o)?;
    let mut pass = Pass {
        state: "running".into(),
        wave: "starting".into(),
        fingerprint: key.clone(),
        units: units.len(),
        t0: Some(watch.t0),
        parts_by_depth: est.parts_by_depth,
        ..Default::default()
    };
    pass.figure(None);
    pass.save(path.as_ref());
    let mut by_depth: BTreeMap<usize, Vec<Unit>> = BTreeMap::new();
    for u in units {
        by_depth.entry(u.depth).or_default().push(u);
    }
    let mut out = Vec::new();
    let res: Result<()> = (|| {
        for (d, us) in by_depth.into_iter().rev() {
            let w = compose(pg, &us, scope, &key, &o)?;
            pass.parts_by_depth.insert(d, w.parts_total);
            if !w.parts.is_empty() {
                let label = format!("depth {d}, parts of {} large directories", w.large);
                one_wave(
                    pg, w.parts, &o, &label, &mut watch, &mut pass, &path, progress, &mut out,
                )?;
            }
            // Composed again: the parts just written are there now.
            let w = compose(pg, &us, scope, &key, &o)?;
            pass.waiting_on_parts += w.waiting.len();
            let label = if d == 0 {
                "projects".to_string()
            } else {
                format!("depth {d}")
            };
            one_wave(
                pg, w.units, &o, &label, &mut watch, &mut pass, &path, progress, &mut out,
            )?;
        }
        Ok(())
    })();
    match &res {
        Ok(()) => pass.state = "finished".into(),
        Err(e) => {
            pass.state = "stopped".into();
            pass.aborted = Some(format!("{e:#}"));
        }
    }
    pass.figure(None);
    pass.save(path.as_ref());
    res.map(|_| out)
}

/// One run of the pass, its figures folded into `pass`.
#[allow(clippy::too_many_arguments)]
fn one_wave(
    pg: &Pg,
    cands: Vec<Candidate>,
    o: &Options,
    label: &str,
    watch: &mut Watch,
    pass: &mut Pass,
    path: &Option<std::path::PathBuf>,
    progress: &mut dyn FnMut(&str, &Stats),
    out: &mut Vec<(String, Stats)>,
) -> Result<()> {
    pass.wave = label.to_string();
    let cov = Coverage {
        files: cands.len(),
        summarize: cands.len(),
        ..Default::default()
    };
    let mut last: Option<Stats> = None;
    let res = summ::run_watched(pg, cands, o, &cov, watch, &mut |s| {
        pass.figure(Some(s));
        pass.save(path.as_ref());
        last = Some(s.clone());
        progress(label, s);
    });
    // A stopped run saved its last stats in the progress call.
    let s = match &res {
        Ok(s) => Some(s.clone()),
        Err(_) => last,
    };
    if let Some(s) = s {
        pass.provisional += provisional(pg, s.run_id).unwrap_or(0);
        pass.close(label, &s);
        out.push((label.to_string(), s));
    }
    pass.figure(None);
    pass.save(path.as_ref());
    res.map(|_| ())
}

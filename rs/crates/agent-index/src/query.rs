//! Search (docs/INDEX.md, "Ranking", "Embeddings" and "Query expansion",
//! stage 1): an identifier's definitions and a label match first, then the
//! ranked lists fused by reciprocal rank (k = 60): bm25 over the entity
//! cards (any word, and every word), bm25 over files' full text, the
//! trigram table, and a semantic list when vectors exist; then the priors:
//! the caller's own project, kind, importance, tests and fixtures, and
//! directories kept out of the way ranked down. At most two results per file.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::files;
use crate::model::{self, Entity};
use crate::names;

/// The rank fusion constant.
pub const RRF_K: f64 = 60.0;

/// Where a semantic ranked list plugs into fusion: (entity id, score) pairs,
/// best first, at most `limit`. The embedding lane implements it over
/// `vectors`; with no vectors it returns nothing, and the other lists rank
/// alone. Fusion uses the order only; the score shows in `--explain`. Ids
/// that aren't entities (a vector left from a removed file) are dropped, and
/// the search's kind, project and under filters apply to them as to every
/// list.
pub trait Semantic {
    fn ranked(&self, conn: &Connection, query: &str, limit: usize) -> Result<Vec<(String, f64)>>;
}

/// No semantic list (no embedder yet).
pub struct NoSemantic;

impl Semantic for NoSemantic {
    fn ranked(&self, _: &Connection, _: &str, _: usize) -> Result<Vec<(String, f64)>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub query: String,
    /// An entity kind (`file`, `symbol`, ...) or a file kind (`doc`, `test`, ...).
    pub kind: String,
    pub project: String,
    /// An absolute, canonical path results must be at or under.
    pub under: String,
    pub limit: usize,
    /// The project the caller works in, ranked first.
    pub caller_project: String,
    /// Only these projects (a JSON array of names; "" for all). Search sets
    /// it to fetch each list again near the caller.
    pub scope: String,
}

/// One label the query matched, and the entity it named.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Mapping {
    /// The words of the query that matched.
    pub phrase: String,
    pub label: String,
    pub kind: String,
    pub origin: String,
    pub entity: String,
    /// The whole query matched (it goes to the top).
    pub exact: bool,
}

/// One result.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub rank: usize,
    pub id: String,
    pub kind: String,
    pub file_kind: String,
    pub project: String,
    pub freshness: String,
    /// Absolute, to open.
    pub path: String,
    /// A symbol's or section's line.
    pub line: i64,
    pub title: String,
    pub summary: String,
    pub why: String,
    pub untracked: bool,
    /// Why it matched: `name "deliver"`, `summary "held"`, `label "word wall"`.
    pub matched: Vec<String>,
    pub score: f64,
    /// Which lists found it and at what rank, and the priors applied
    /// (`--explain`).
    pub explain: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub query: String,
    pub terms: Vec<String>,
    pub hits: Vec<Hit>,
    /// The label mapping used ("word wall" → `alpha:word-wall`, alt).
    pub mapping: Vec<Mapping>,
    /// Query terms no entity mentions.
    pub absent: Vec<String>,
    /// How many entities matched before the limit.
    pub candidates: usize,
    /// Milliseconds per stage, to see where a slow search goes.
    pub ms: BTreeMap<String, f64>,
}

/// Reciprocal rank fusion: each list adds 1 / (k + rank) to every id in it
/// (rank from 1; an id repeated in one list counts once, at its best rank).
/// Best first; ties by id.
pub fn rrf(lists: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut score: HashMap<String, f64> = HashMap::new();
    for list in lists {
        let mut seen = HashSet::new();
        for id in list {
            if !seen.insert(id) {
                continue;
            }
            *score.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + seen.len() as f64);
        }
    }
    let mut v: Vec<(String, f64)> = score.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn quote(t: &str) -> String {
    format!("\"{}\"", t.replace('"', ""))
}

/// The SQL filter of kind, project and under, on entity alias `e`, with its
/// parameters starting at `?first`.
fn filter_sql(first: usize) -> String {
    let (k, p, u, s) = (first, first + 1, first + 2, first + 3);
    format!(
        "(?{k} = '' OR e.kind = ?{k} OR e.file_kind = ?{k}) AND (?{p} = '' OR e.project = ?{p}) \
         AND (?{u} = '' OR e.path = ?{u} OR substr(e.path, 1, length(?{u}) + 1) = ?{u} || '/') \
         AND (?{s} = '' OR e.project IN (SELECT value FROM json_each(?{s})))"
    )
}

/// One term for the porter table: itself, or for an identifier also its
/// words as a phrase (`parse_config` finds `parse_config_old`, whose split
/// words the cards hold).
fn term_query(t: &str) -> String {
    let words = names::split_words(t);
    if words.len() > 1 {
        format!("({} OR {})", quote(t), quote(&words.join(" ")))
    } else {
        quote(t)
    }
}

/// bm25 over the cards, columns weighted: title 4, name 4, path 2, summary 2,
/// symbols 1, why 1, labels 3.
const CARD_BM25: &str = "bm25(search, 4.0, 4.0, 2.0, 2.0, 1.0, 1.0, 3.0)";

/// A ranked list's SQL: the FTS rows, to their entities through
/// search_rowid's rid index (without it each list scanned the map: 70 ms a
/// list on a dev tree).
pub(crate) fn fts_sql(table: &str, bm25: &str, limit: usize) -> String {
    format!(
        "SELECT m.id FROM {table} s JOIN search_rowid m ON m.rid = s.rowid JOIN entities e ON e.id = m.id
         WHERE {table} MATCH ?1 AND {} ORDER BY {bm25} LIMIT {limit}",
        filter_sql(2)
    )
}

fn fts_list(
    conn: &Connection,
    table: &str,
    bm25: &str,
    q: &str,
    o: &SearchOptions,
    limit: usize,
) -> Result<Vec<String>> {
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let sql = fts_sql(table, bm25, limit);
    let mut st = conn.prepare_cached(&sql)?;
    let rows = st.query_map(params![q, o.kind, o.project, o.under, o.scope], |r| {
        r.get::<_, String>(0)
    });
    match rows {
        Ok(rows) => Ok(rows.collect::<rusqlite::Result<_>>()?),
        Err(e) => bail!("searching {table} for {q}: {e}"),
    }
}

/// Whether any entity the search may return (its kind, project and under
/// filters) mentions a term: a porter token, or a substring of a name, path
/// or label.
fn mentioned(conn: &Connection, term: &str, o: &SearchOptions) -> Result<bool> {
    Ok(
        !fts_list(conn, "search", "s.rowid", &term_query(term), o, 1)?.is_empty()
            || (term.chars().count() >= 3
                && !fts_list(conn, "search_tri", "s.rowid", &quote(term), o, 1)?.is_empty())
            || !full_text_list(conn, &quote(term), o, 1)?.is_empty(),
    )
}

/// bm25 over files' full text (`search_full`, full-depth projects), as
/// entity ids, best first.
fn full_text_list(
    conn: &Connection,
    q: &str,
    o: &SearchOptions,
    limit: usize,
) -> Result<Vec<String>> {
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT m.id FROM search_full f JOIN search_full_ids m ON m.rid = f.rowid JOIN entities e ON e.id = m.id
         WHERE search_full MATCH ?1 AND {} ORDER BY bm25(search_full) LIMIT {limit}",
        filter_sql(2)
    );
    let mut st = conn.prepare_cached(&sql)?;
    let rows = st.query_map(params![q, o.kind, o.project, o.under, o.scope], |r| {
        r.get::<_, String>(0)
    });
    match rows {
        Ok(rows) => Ok(rows.collect::<rusqlite::Result<_>>()?),
        Err(e) => bail!("searching full text for {q}: {e}"),
    }
}

/// Whether a query is one identifier: a word of letters, digits and `_`,
/// with a letter or `_` first.
fn is_identifier(q: &str) -> bool {
    let q = q.trim();
    !q.is_empty()
        && q.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && q.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// How much the caller's project (3) and the projects it depends on (2)
/// weigh, by project name; empty when the caller is in no project.
fn near_projects(conn: &Connection, caller: &str) -> Result<HashMap<String, f64>> {
    let mut out = HashMap::new();
    if caller.is_empty() {
        return Ok(out);
    }
    let (lo, hi) = crate::crawl::prefix_range(&format!("{caller}:"));
    let mut st = conn.prepare_cached(
        "SELECT DISTINCT target FROM edges WHERE kind = 'depends-on'
         AND (source = ?3 OR (source >= ?1 AND source < ?2))",
    )?;
    for t in st.query_map(params![lo, hi, caller], |r| r.get::<_, String>(0))? {
        let t = t?;
        let p = model::project_of_id(&t);
        if !p.is_empty() && p != caller {
            out.insert(p.to_string(), 2.0);
        }
    }
    out.insert(caller.to_string(), 3.0);
    Ok(out)
}

/// The semantic list, kept to entities the filters let through.
fn semantic_list(
    conn: &Connection,
    semantic: &dyn Semantic,
    o: &SearchOptions,
    limit: usize,
) -> Result<(Vec<String>, HashMap<String, f64>)> {
    // The filters apply after the scan, so a filtered search asks for more:
    // on a tree of many projects, the nearest few hundred overall can hold
    // almost nothing from the one asked for. An exact scan costs the same
    // either way.
    let filtered = !(o.kind.is_empty() && o.project.is_empty() && o.under.is_empty());
    let want = if filtered {
        limit.saturating_mul(20)
    } else {
        limit
    };
    let pairs = semantic.ranked(conn, &o.query, want)?;
    if pairs.is_empty() {
        return Ok((Vec::new(), HashMap::new()));
    }
    let sql = format!(
        "SELECT 1 FROM entities e WHERE e.id = ?1 AND {}",
        filter_sql(2)
    );
    let mut st = conn.prepare_cached(&sql)?;
    let mut ids = Vec::new();
    let mut scores = HashMap::new();
    for (id, score) in pairs {
        if ids.len() >= limit {
            break;
        }
        if st.exists(params![id, o.kind, o.project, o.under, o.scope])? && !scores.contains_key(&id)
        {
            scores.insert(id.clone(), score);
            ids.push(id);
        }
    }
    Ok((ids, scores))
}

/// Stage 0: a query that is one identifier (`parse_config`, `WallLayout`,
/// `deliver.rs`) names a definition: symbols whose name it is exactly, then
/// files, dirs and projects of that name; then the same ignoring case.
/// Best first.
fn exact_names(conn: &Connection, query: &str, o: &SearchOptions) -> Result<Vec<String>> {
    let q = query.trim();
    if q.is_empty() || q.chars().any(char::is_whitespace) {
        return Ok(Vec::new());
    }
    // The exact name first (the name index answers it), else ignoring case.
    for cmp in ["e.name = ?1", "lower(e.name) = lower(?1)"] {
        let sql = format!(
            "SELECT e.id FROM entities e WHERE {cmp}
             AND e.kind IN ('symbol', 'file', 'dir', 'project') AND e.file_kind != 'impl' AND {}
             ORDER BY CASE e.kind WHEN 'symbol' THEN 0 WHEN 'file' THEN 1 ELSE 2 END,
                      e.importance DESC, e.id LIMIT 20",
            filter_sql(2)
        );
        let mut st = conn.prepare_cached(&sql)?;
        let ids: Vec<String> = st
            .query_map(params![q, o.kind, o.project, o.under, o.scope], |r| {
                r.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        if !ids.is_empty() {
            return Ok(ids);
        }
    }
    Ok(Vec::new())
}

/// Stage 1: labels the query, or a run of its words, answers to.
fn label_matches(conn: &Connection, query: &str, o: &SearchOptions) -> Result<Vec<Mapping>> {
    let words: Vec<String> = names::norm(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let mut out: Vec<Mapping> = Vec::new();
    if words.is_empty() {
        return Ok(out);
    }
    // One row per entity (its best label), so a dir's five spellings of one
    // name don't use up the limit.
    let sql = format!(
        "SELECT l.entity, l.label, l.kind, l.origin, max(l.weight) AS w FROM labels l JOIN entities e ON e.id = l.entity \
         WHERE (l.norm = ?1 OR l.compact = ?2) AND {} GROUP BY l.entity ORDER BY w DESC, e.importance DESC LIMIT 25",
        filter_sql(3)
    );
    let mut st = conn.prepare(&sql)?;
    let mut covered: HashSet<usize> = HashSet::new();
    // Runs of up to six words, and the whole query however long.
    let lengths: Vec<usize> = (1..=words.len())
        .rev()
        .filter(|&n| n <= 6 || n == words.len())
        .collect();
    for n in lengths {
        for i in 0..=words.len() - n {
            if (i..i + n).all(|j| covered.contains(&j)) {
                continue;
            }
            let gram = &words[i..i + n];
            // One word of several is left to the lexical lists: a word that
            // happens to name a dir somewhere ("memory") isn't what the query
            // names. A one-word query is matched whole.
            if n == 1 && (words.len() > 1 || names::is_stop(&gram[0]) || gram[0].len() < 3) {
                continue;
            }
            if gram.iter().all(|w| names::is_stop(w)) {
                continue;
            }
            let phrase = gram.join(" ");
            let compact = gram.concat();
            let rows: Vec<(String, String, String, String)> = st
                .query_map(
                    params![phrase, compact, o.kind, o.project, o.under, o.scope],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?
                .collect::<rusqlite::Result<_>>()?;
            if rows.is_empty() {
                continue;
            }
            for j in i..i + n {
                covered.insert(j);
            }
            let exact = n == words.len();
            let mut seen = HashSet::new();
            for (entity, label, kind, origin) in rows {
                if !seen.insert(entity.clone()) {
                    continue;
                }
                out.push(Mapping {
                    phrase: phrase.clone(),
                    label,
                    kind,
                    origin,
                    entity,
                    exact,
                });
            }
        }
    }
    Ok(out)
}

/// The file kinds `--kind` takes, besides the entity kinds.
pub const FILE_KINDS: &[&str] = &[
    "source",
    "test",
    "fixture",
    "doc",
    "config",
    "build",
    "data",
    "asset",
    "submodule",
];

pub fn search(
    conn: &Connection,
    o: &SearchOptions,
    semantic: &dyn Semantic,
) -> Result<SearchResult> {
    if o.kind == "file" || FILE_KINDS.contains(&o.kind.as_str()) {
        return search_files(conn, o, semantic);
    }
    search_entities(conn, o, semantic)
}

/// A search for files (`--kind file`, or a file kind): every kind of entity
/// is searched, and a symbol or section counts for its file, so the list
/// has one row per file, best first, each saying what in it matched.
fn search_files(
    conn: &Connection,
    o: &SearchOptions,
    semantic: &dyn Semantic,
) -> Result<SearchResult> {
    let limit = if o.limit == 0 { 10 } else { o.limit };
    let inner = SearchOptions {
        kind: String::new(),
        limit: limit * 8,
        ..o.clone()
    };
    let mut r = search_entities(conn, &inner, semantic)?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut hits: Vec<Hit> = Vec::new();
    for h in std::mem::take(&mut r.hits) {
        let file_id = match h.kind.as_str() {
            "file" => h.id.clone(),
            "symbol" | "section" => model::file_of(&h.id).to_string(),
            _ => continue,
        };
        if seen.contains(&file_id) {
            continue;
        }
        let Some(e) = model::get(conn, &file_id)? else {
            continue;
        };
        if o.kind != "file" && e.file_kind != o.kind {
            continue;
        }
        seen.insert(file_id.clone());
        let hit = if h.kind == "file" {
            h
        } else {
            let summ = model::summary(conn, &file_id)?;
            let mut explain = h.explain.clone();
            explain.insert(0, format!("by {}", h.id));
            let mut matched = h.matched.clone();
            matched.insert(0, format!("{} {}", h.kind, h.title));
            Hit {
                rank: 0,
                id: file_id.clone(),
                kind: e.kind.clone(),
                file_kind: e.file_kind.clone(),
                project: e.project.clone(),
                freshness: model::freshness(&e.content_key, summ.as_ref()).to_string(),
                path: e.path.clone(),
                // Where in the file: the symbol's or section's line.
                line: h.line,
                title: e.display_title().to_string(),
                summary: summ.as_ref().map(|s| s.text.clone()).unwrap_or_default(),
                why: summ.map(|s| s.why).unwrap_or_default(),
                untracked: e.untracked,
                matched,
                score: h.score,
                explain,
            }
        };
        hits.push(Hit {
            rank: hits.len() + 1,
            ..hit
        });
        if hits.len() >= limit {
            break;
        }
    }
    r.hits = hits;
    Ok(r)
}

fn search_entities(
    conn: &Connection,
    o: &SearchOptions,
    semantic: &dyn Semantic,
) -> Result<SearchResult> {
    let terms = names::query_terms(&o.query);
    if terms.is_empty() {
        bail!(
            "nothing to search for in {:?}: give words, a name or a path",
            o.query
        );
    }
    let limit = if o.limit == 0 { 10 } else { o.limit };
    let content: Vec<&String> = {
        let c: Vec<&String> = terms.iter().filter(|t| !names::is_stop(t)).collect();
        if c.is_empty() {
            terms.iter().collect()
        } else {
            c
        }
    };

    let mut ms: BTreeMap<String, f64> = BTreeMap::new();
    let mut t0 = std::time::Instant::now();
    let mut lap = |ms: &mut BTreeMap<String, f64>, name: &str| {
        let d = t0.elapsed().as_secs_f64() * 1000.0;
        *ms.entry(name.to_string()).or_insert(0.0) += (d * 10.0).round() / 10.0;
        t0 = std::time::Instant::now();
    };
    // The caller's project and the projects it depends on. Every list is
    // fetched for them too, not only across the tree, so a local candidate
    // the global top 300 cut off still reaches the fusion and its prior.
    let near = near_projects(conn, &o.caller_project)?;
    let local: Option<SearchOptions> =
        (!near.is_empty() && o.project.is_empty() && o.scope.is_empty()).then(|| {
            let mut names: Vec<&String> = near.keys().collect();
            names.sort();
            SearchOptions {
                scope: serde_json::to_string(&names).unwrap_or_default(),
                ..o.clone()
            }
        });
    let mapping = label_matches(conn, &o.query, o)?;
    lap(&mut ms, "labels");
    let mut lists: Vec<(&str, Vec<String>)> = Vec::new();
    // An identifier's definitions go first; then the whole query as a label
    // pins its entities; part of it is a list.
    let exact = exact_names(conn, &o.query, o)?;
    lap(&mut ms, "exact names");
    let pinned: Vec<String> = mapping
        .iter()
        .filter(|m| m.exact)
        .map(|m| m.entity.clone())
        .collect();
    let partial: Vec<String> = mapping
        .iter()
        .filter(|m| !m.exact)
        .map(|m| m.entity.clone())
        .collect();
    if !partial.is_empty() {
        lists.push(("label", partial));
    }

    // Each term, and an identifier's words as a phrase too: `parse_config`
    // finds `parse_config_old`, whose split words the cards hold.
    let porter_q = content
        .iter()
        .map(|t| term_query(t))
        .collect::<Vec<_>>()
        .join(" OR ");
    lists.push((
        "lexical",
        fts_list(conn, "search", CARD_BM25, &porter_q, o, 300)?,
    ));
    if let Some(l) = &local {
        lists.push((
            "lexical here",
            fts_list(conn, "search", CARD_BM25, &porter_q, l, 300)?,
        ));
    }

    // Every word at once: an entity that has them all outranks one that has
    // one word many times.
    if content.len() > 1 {
        let all_q = content
            .iter()
            .map(|t| quote(t))
            .collect::<Vec<_>>()
            .join(" AND ");
        lists.push((
            "all words",
            fts_list(conn, "search", CARD_BM25, &all_q, o, 300)?,
        ));
        if let Some(l) = &local {
            lists.push((
                "all words here",
                fts_list(conn, "search", CARD_BM25, &all_q, l, 300)?,
            ));
        }
    }
    lap(&mut ms, "cards");

    let mut absent: Vec<String> = Vec::new();
    for t in &content {
        // A label match covers its words, run together too (`shinyglow`).
        let labeled = mapping.iter().any(|m| {
            m.phrase.split(' ').any(|w| w == t.as_str()) || m.phrase.replace(' ', "") == **t
        });
        if !labeled && !mentioned(conn, t, o)? {
            absent.push((*t).clone());
        }
    }
    lap(&mut ms, "absent terms");
    // Trigrams: substrings of names, paths and labels.
    let tri_q = content
        .iter()
        .filter(|t| t.chars().count() >= 3)
        .map(|t| quote(t))
        .collect::<Vec<_>>()
        .join(" OR ");
    lists.push((
        "trigram",
        fts_list(conn, "search_tri", "bm25(search_tri)", &tri_q, o, 300)?,
    ));
    if let Some(l) = &local {
        lists.push((
            "trigram here",
            fts_list(conn, "search_tri", "bm25(search_tri)", &tri_q, l, 300)?,
        ));
    }
    // A term nothing mentions is looked for by its trigrams, to catch a typo:
    // a list of its own, so its partial matches can't crowd out real ones.
    let mut fuzzy: Vec<String> = Vec::new();
    for t in &absent {
        let cs: Vec<char> = t.chars().collect();
        if cs.len() >= 5 {
            for w in cs.windows(3) {
                fuzzy.push(quote(&w.iter().collect::<String>()));
            }
        }
    }
    if !fuzzy.is_empty() {
        lists.push((
            "typo",
            fts_list(
                conn,
                "search_tri",
                "bm25(search_tri)",
                &fuzzy.join(" OR "),
                o,
                50,
            )?,
        ));
    }
    lap(&mut ms, "trigrams");
    // bm25 over what files say, not only their cards.
    let full = full_text_list(conn, &porter_q, o, 300)?;
    if !full.is_empty() {
        lists.push(("full text", full));
    }
    if let Some(l) = &local {
        let full = full_text_list(conn, &porter_q, l, 300)?;
        if !full.is_empty() {
            lists.push(("full text here", full));
        }
    }
    // A rare identifier no definition answers to: the few files that hold
    // it (nearby ones first) are what the query is after.
    let rare = if exact.is_empty() && is_identifier(&o.query) {
        let q = quote(o.query.trim());
        let mut f = Vec::new();
        if let Some(l) = &local {
            f = full_text_list(conn, &q, l, 4)?;
        }
        if f.is_empty() || f.len() > 3 {
            f = full_text_list(conn, &q, o, 4)?;
        }
        if f.len() <= 3 { f } else { Vec::new() }
    } else {
        Vec::new()
    };
    lap(&mut ms, "full text");
    let (sem, sem_scores) = semantic_list(conn, semantic, o, 300)?;
    if !sem.is_empty() {
        lists.push(("semantic", sem));
    }
    lap(&mut ms, "semantic");

    // Fuse.
    let mut score: HashMap<String, f64> =
        rrf(&lists.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>())
            .into_iter()
            .collect();
    let mut explain: HashMap<String, Vec<String>> = HashMap::new();
    for (name, list) in &lists {
        let mut seen = HashSet::new();
        for id in list {
            if seen.insert(id) {
                let mut what = format!("{name} #{}", seen.len());
                if *name == "semantic"
                    && let Some(s) = sem_scores.get(id)
                {
                    what = format!("{what} ({s:.3})");
                }
                explain.entry(id.clone()).or_default().push(what);
            }
        }
    }
    // The caller's project, and the projects it depends on, come first: a
    // pin (an exact name, a whole-query label, a rare identifier's file)
    // elsewhere counts only when nothing near matched at all, and every
    // score there is multiplied. (An agent working in a project and asking
    // for `effort` wants its own uses before another repo's definition.)
    // Tiers: the caller's own project (0), the projects it depends on (1),
    // the rest (2). A pin counts only in the nearest tier with any match.
    let tier = |id: &str| -> u8 {
        let p = model::project_of_id(id);
        if !o.caller_project.is_empty() && p == o.caller_project {
            0
        } else if near.contains_key(p) {
            1
        } else {
            2
        }
    };
    let best_tier = exact
        .iter()
        .chain(pinned.iter())
        .chain(rare.iter())
        .chain(score.keys())
        .map(|id| tier(id))
        .min()
        .unwrap_or(2);
    let pin_counts = |id: &str| tier(id) <= best_tier;
    for (i, id) in rare.iter().enumerate() {
        if !pin_counts(id) {
            continue;
        }
        *score.entry(id.clone()).or_insert(0.0) += 1.5 - i as f64 * 0.001;
        explain.entry(id.clone()).or_default().insert(
            0,
            format!("one of {} files holding \"{}\"", rare.len(), o.query.trim()),
        );
    }
    for (i, id) in exact.iter().enumerate() {
        if !pin_counts(id) {
            explain
                .entry(id.clone())
                .or_default()
                .push(format!("named \"{}\", in another project", o.query.trim()));
            continue;
        }
        *score.entry(id.clone()).or_insert(0.0) += 2.0 - i as f64 * 0.001;
        explain
            .entry(id.clone())
            .or_default()
            .insert(0, format!("named \"{}\"", o.query.trim()));
    }
    for (i, id) in pinned.iter().enumerate() {
        if !pin_counts(id) {
            continue;
        }
        *score.entry(id.clone()).or_insert(0.0) += 1.0 - i as f64 * 0.001;
        if let Some(m) = mapping.iter().find(|m| &m.entity == id) {
            explain.entry(id.clone()).or_default().insert(
                0,
                format!("label \"{}\" ({}, {})", m.label, m.kind, m.origin),
            );
        }
    }
    for m in mapping.iter().filter(|m| !m.exact) {
        if let Some(e) = explain.get_mut(&m.entity) {
            e.push(format!(
                "label \"{}\" for \"{}\" ({}, {})",
                m.label, m.phrase, m.kind, m.origin
            ));
        }
    }
    let candidates = score.len();

    // Priors.
    let max_imp: f64 = conn.query_row(
        "SELECT coalesce(max(importance), 0) FROM entities",
        [],
        |r| r.get(0),
    )?;
    // What --under asks for, by its path inside its project: a checkout under
    // `/var/tmp/_work/` or `~/old/` isn't a request for kept-out-of-the-way dirs.
    let under_parts: Vec<String> = if o.under.is_empty() {
        Vec::new()
    } else {
        let projects = crate::roots::list_projects(conn)?;
        projects
            .iter()
            .filter(|p| o.under == p.path || o.under.starts_with(&format!("{}/", p.path)))
            .max_by_key(|p| p.path.len())
            .map(|p| {
                o.under[p.path.len()..]
                    .split('/')
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let under_targets = under_parts.iter().any(|c| files::is_out_of_the_way(c));
    let asks_tests = matches!(o.kind.as_str(), "test" | "fixture")
        || under_parts.iter().any(|c| {
            files::FIXTURE_DIRS.contains(&c.as_str())
                || matches!(c.as_str(), "tests" | "test" | "__tests__")
        });
    let mut ents: HashMap<String, Entity> = HashMap::new();
    let ids: Vec<String> = score.keys().cloned().collect();
    for id in ids {
        let Some(e) = model::get(conn, &id)? else {
            score.remove(&id);
            continue;
        };
        let mut s = score[&id];
        let ex = explain.entry(id.clone()).or_default();
        let asked_kind = !o.kind.is_empty();
        if !asked_kind {
            match e.kind.as_str() {
                "section" => {
                    s -= 0.002;
                    ex.push("section −".into());
                }
                "file" | "symbol" | "dir" | "project" => {}
                _ => {}
            }
        }
        if e.importance > 0.0 && max_imp > 0.0 {
            let b = 0.004 * (1.0 + e.importance).ln() / (1.0 + max_imp).ln();
            s += b;
            ex.push(format!("importance {:.1} +", e.importance));
        }
        let rel = if e.rel.is_empty() { "" } else { e.rel.as_str() };
        let out_of_way = rel.split('/').any(files::is_out_of_the_way);
        if out_of_way && !under_targets {
            s -= 0.008;
            ex.push("kept-out-of-the-way dir −".into());
        }
        // Fixtures answer many words (a fixture is named for the feature it
        // records), tests fewer; the code itself comes first unless asked.
        if !e.rel.is_empty() && !asks_tests {
            if e.kind != "dir" && files::is_fixture(&e.rel) {
                // Below any entity of the code that matched at all (a fixture
                // that tops three lists still scores under 0.05 there), but
                // above nothing: one is still found when nothing else matches.
                s -= 0.03;
                ex.push("fixture −".into());
            } else if files::is_test(&e.rel) || files::is_fixture(&format!("{}/x", e.rel)) {
                s -= 0.004;
                ex.push("test −".into());
            }
        }
        if let Some(f) = near.get(e.project.as_str()) {
            // Only a positive score grows: a penalty isn't made larger.
            if s > 0.0 {
                s *= f;
            }
            ex.push(if e.project == o.caller_project {
                format!("own project ×{f}")
            } else {
                format!("a project yours depends on ×{f}")
            });
        }
        score.insert(id.clone(), s);
        ents.insert(id, e);
    }
    let mut order: Vec<(String, f64)> = score.into_iter().collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    lap(&mut ms, "fuse and priors");

    // At most two results per file.
    let mut per_file: HashMap<String, usize> = HashMap::new();
    let mut hits: Vec<Hit> = Vec::new();
    for (id, s) in order {
        let e = &ents[&id];
        let file = model::file_of(&id).to_string();
        let n = per_file.entry(file).or_insert(0);
        if *n >= 2 {
            continue;
        }
        *n += 1;
        let summ = model::summary(conn, &id)?;
        let fresh = model::freshness(&e.content_key, summ.as_ref()).to_string();
        let mut matched = matched_fields(conn, &id, &content)?;
        for m in mapping.iter().filter(|m| m.entity == id) {
            matched.push(format!("label \"{}\"", m.label));
        }
        hits.push(Hit {
            rank: hits.len() + 1,
            id: id.clone(),
            kind: e.kind.clone(),
            file_kind: e.file_kind.clone(),
            project: e.project.clone(),
            freshness: fresh,
            path: e.path.clone(),
            line: e.line,
            title: e.display_title().to_string(),
            summary: summ.as_ref().map(|s| s.text.clone()).unwrap_or_default(),
            why: summ.map(|s| s.why).unwrap_or_default(),
            untracked: e.untracked,
            matched,
            score: (s * 10000.0).round() / 10000.0,
            explain: explain.remove(&id).unwrap_or_default(),
        });
        if hits.len() >= limit {
            break;
        }
    }
    lap(&mut ms, "results");
    Ok(SearchResult {
        query: o.query.clone(),
        terms: terms.clone(),
        hits,
        mapping,
        absent,
        candidates,
        ms,
    })
}

/// Whether two words are one word inflected, as the porter stemmer would
/// have them match: both letters only, one the other plus at most three
/// letters (`deliver`, `delivery`, `delivered`), and four letters at least.
/// Not `parse_int` for `parse_config`, nor `renderer` for `renderglyphs`.
fn same_stem(w: &str, term: &str) -> bool {
    let alpha = |s: &str| s.chars().all(|c| c.is_alphabetic());
    if !alpha(w) || !alpha(term) {
        return false;
    }
    let (short, long) = if w.len() <= term.len() {
        (w, term)
    } else {
        (term, w)
    };
    short.len() >= 4 && long.starts_with(short) && long.len() - short.len() <= 3
}

/// Which of an entity's fields hold which terms: `name "deliver"`.
fn matched_fields(conn: &Connection, id: &str, terms: &[&String]) -> Result<Vec<String>> {
    let Some(t) = crate::crawl::search_text(conn, id)? else {
        return Ok(Vec::new());
    };
    let fields = [
        ("title", &t.title),
        ("name", &t.name),
        ("path", &t.path),
        ("summary", &t.summary),
        ("symbols", &t.symbols),
        ("why", &t.why),
        ("labels", &t.labels),
    ];
    let mut by_field: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for term in terms {
        for (i, (_, text)) in fields.iter().enumerate() {
            let lower = text.to_lowercase();
            let hit = lower
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|w| w == term.as_str() || same_stem(w, term));
            if hit {
                by_field.entry(i).or_default().push(format!("\"{term}\""));
                break;
            }
        }
    }
    Ok(by_field
        .into_iter()
        .map(|(i, ts)| format!("{} {}", fields[i].0, ts.join(", ")))
        .collect())
}

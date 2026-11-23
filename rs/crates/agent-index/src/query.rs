//! Search (docs/INDEX.md, "Ranking" and "Query expansion", stage 1): a label
//! match first, then bm25 over the porter table and the trigram table (and a
//! semantic list when vectors exist), fused by reciprocal rank (k = 60), then
//! the priors: the caller's own project, kind, importance, and directories
//! kept out of the way ranked down. At most two results per file.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::files;
use crate::model::{self, Entity};
use crate::names;

/// The rank fusion constant.
pub const RRF_K: f64 = 60.0;

/// Where a semantic ranked list plugs into fusion: entity ids, best first.
/// The embedding lane implements it over `vectors`; with no vectors it
/// contributes nothing, and the other lists rank alone.
pub trait Semantic {
    fn ranked(&self, conn: &Connection, query: &str, limit: usize) -> Result<Vec<String>>;
}

/// No semantic list (no embedder yet).
pub struct NoSemantic;

impl Semantic for NoSemantic {
    fn ranked(&self, _: &Connection, _: &str, _: usize) -> Result<Vec<String>> {
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
    let (k, p, u) = (first, first + 1, first + 2);
    format!(
        "(?{k} = '' OR e.kind = ?{k} OR e.file_kind = ?{k}) AND (?{p} = '' OR e.project = ?{p}) \
         AND (?{u} = '' OR e.path = ?{u} OR substr(e.path, 1, length(?{u}) + 1) = ?{u} || '/')"
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
    let sql = format!(
        "SELECT s.id FROM {table} s JOIN entities e ON e.id = s.id WHERE {table} MATCH ?1 AND {} ORDER BY {bm25} LIMIT {limit}",
        filter_sql(2)
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(params![q, o.kind, o.project, o.under], |r| {
        r.get::<_, String>(0)
    });
    match rows {
        Ok(rows) => Ok(rows.collect::<rusqlite::Result<_>>()?),
        Err(e) => bail!("searching {table} for {q}: {e}"),
    }
}

/// Whether any entity mentions a term: a porter token, a substring of a name,
/// path or label, or a label word.
fn mentioned(conn: &Connection, term: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM search WHERE search MATCH ?1 LIMIT 1)",
        params![quote(term)],
        |r| r.get(0),
    )?;
    if n > 0 {
        return Ok(true);
    }
    if term.chars().count() >= 3 {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM (SELECT 1 FROM search_tri WHERE search_tri MATCH ?1 LIMIT 1)",
            params![quote(term)],
            |r| r.get(0),
        )?;
        if n > 0 {
            return Ok(true);
        }
    }
    Ok(false)
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
    let sql = format!(
        "SELECT l.entity, l.label, l.kind, l.origin FROM labels l JOIN entities e ON e.id = l.entity \
         WHERE (l.norm = ?1 OR l.compact = ?2) AND {} ORDER BY l.weight DESC, e.importance DESC LIMIT 25",
        filter_sql(3)
    );
    let mut st = conn.prepare(&sql)?;
    let mut covered: HashSet<usize> = HashSet::new();
    for n in (1..=words.len().min(6)).rev() {
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
                .query_map(params![phrase, compact, o.kind, o.project, o.under], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
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

pub fn search(
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

    let mapping = label_matches(conn, &o.query, o)?;
    let mut lists: Vec<(&str, Vec<String>)> = Vec::new();
    // The whole query as a label pins its entities; part of it is a list.
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

    let porter_q = content
        .iter()
        .map(|t| quote(t))
        .collect::<Vec<_>>()
        .join(" OR ");
    lists.push((
        "lexical",
        fts_list(
            conn,
            "search",
            "bm25(search, 0, 4.0, 4.0, 2.0, 2.0, 1.0, 1.0, 3.0)",
            &porter_q,
            o,
            300,
        )?,
    ));

    let mut absent: Vec<String> = Vec::new();
    for t in &content {
        if !mentioned(conn, t)?
            && !mapping
                .iter()
                .any(|m| m.phrase.split(' ').any(|w| w == t.as_str()))
        {
            absent.push((*t).clone());
        }
    }
    // Trigrams: substrings of names, paths and labels; a term nothing mentions
    // is looked for by its trigrams, to catch a typo.
    let mut tri_terms: Vec<String> = content
        .iter()
        .filter(|t| t.chars().count() >= 3)
        .map(|t| quote(t))
        .collect();
    for t in &absent {
        let cs: Vec<char> = t.chars().collect();
        if cs.len() >= 5 {
            for w in cs.windows(3) {
                tri_terms.push(quote(&w.iter().collect::<String>()));
            }
        }
    }
    let tri_q = tri_terms.join(" OR ");
    lists.push((
        "trigram",
        fts_list(conn, "search_tri", "bm25(search_tri)", &tri_q, o, 300)?,
    ));
    let sem = semantic.ranked(conn, &o.query, 300)?;
    if !sem.is_empty() {
        lists.push(("semantic", sem));
    }

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
                explain
                    .entry(id.clone())
                    .or_default()
                    .push(format!("{name} #{}", seen.len()));
            }
        }
    }
    for (i, id) in pinned.iter().enumerate() {
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
    let mut ents: HashMap<String, Entity> = HashMap::new();
    let ids: Vec<String> = score.keys().cloned().collect();
    for id in ids {
        let Some(e) = model::get(conn, &id)? else {
            score.remove(&id);
            continue;
        };
        let mut s = score[&id];
        let ex = explain.entry(id.clone()).or_default();
        if !o.caller_project.is_empty() && e.project == o.caller_project {
            s += 0.004;
            ex.push("own project +".into());
        }
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
        let under_targets = !o.under.is_empty()
            && o.under
                .rsplit('/')
                .take_while(|c| !c.is_empty())
                .any(files::is_out_of_the_way);
        if out_of_way && !under_targets {
            s -= 0.008;
            ex.push("kept-out-of-the-way dir −".into());
        }
        // Tests and their fixtures answer many words (a fixture named for the
        // feature it records); the code itself comes first unless asked.
        let fixture = |c: &str| {
            matches!(
                c,
                "testdata" | "fixtures" | "__fixtures__" | "corpus" | "snapshots" | "golden"
            )
        };
        let is_testish =
            !e.rel.is_empty() && (files::is_test(&e.rel) || e.rel.split('/').any(fixture));
        let asks_tests = o.kind == "test" || o.under.split('/').any(|c| fixture(c) || c == "tests");
        if is_testish && !asks_tests {
            s -= 0.004;
            ex.push("test or fixture −".into());
        }
        score.insert(id.clone(), s);
        ents.insert(id, e);
    }
    let mut order: Vec<(String, f64)> = score.into_iter().collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

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
    Ok(SearchResult {
        query: o.query.clone(),
        terms: terms.clone(),
        hits,
        mapping,
        absent,
        candidates,
    })
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
        // A stem: "delivery" matches "deliver", "delivered".
        let stem: String = term
            .chars()
            .take(term.chars().count().clamp(3, 6))
            .collect();
        for (i, (_, text)) in fields.iter().enumerate() {
            let lower = text.to_lowercase();
            let hit = lower
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|w| w == term.as_str() || (w.starts_with(&stem) && stem.len() >= 4))
                || (term.len() >= 4 && lower.contains(term.as_str()));
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

//! Answers that span projects (docs/INDEX-V2.md, "Across projects";
//! `[cross]` in the configuration): which projects a query is about, the
//! files sessions touched beside the top hits, a reranker pool that every
//! one of the query's projects gets places in, and a cap on one project's
//! share of the answer's top. `search.rs` calls these; each part has its
//! own switch, and with all of them off a search is what it was.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use super::Pg;
use super::search::Lexical;
use crate::config::Cross;

/// Words that make a query read as a comparison of two things.
const COMPARES: &[&str] = &[
    "both",
    "each",
    "compare",
    "compared",
    "compares",
    "comparing",
    "comparison",
    "versus",
    "vs",
    "differ",
    "differs",
    "difference",
    "differences",
    "contrast",
    "between",
];

/// Whether the query compares (one of [`COMPARES`] among its terms).
pub fn compares(terms: &[String]) -> bool {
    terms.iter().any(|t| COMPARES.contains(&t.as_str()))
}

/// The projects other than `caller` the query names: a term equal to a
/// project's name, ignoring case (`web-app` as `web-app` or `webapp`).
pub fn named_projects(query: &str, projects: &[String], caller: &str) -> Vec<String> {
    let q = query.to_lowercase();
    let words: Vec<&str> = q
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'))
        .filter(|w| !w.is_empty())
        .collect();
    let mut out: Vec<String> = projects
        .iter()
        .filter(|p| p.as_str() != caller)
        .filter(|p| {
            let n = p.to_lowercase();
            let compact: String = n.chars().filter(|c| c.is_alphanumeric()).collect();
            words.iter().any(|w| {
                *w == n
                    || (compact.len() >= 4
                        && w.chars()
                            .filter(|c| c.is_alphanumeric())
                            .collect::<String>()
                            == compact)
            })
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The projects' route scores, best first (`[cross]` `route`): the mean,
/// weighted, of three lists' scores, each scaled to its best:
///
/// - `summary`: project and folder cards (kind `project` and `dir`) by BM25
///   on their summary, asks, seed and why (native full text on a schema
///   without pg_search), the best `route_cards`, each project's best
///   `route_per_project` summed;
/// - `dense`: the query's vector against each project's card, scaled from
///   the worst project to the best;
/// - `fused`: the fused candidates (`mass`, project and fusion score, best
///   first), each project's best `route_per_project` summed.
///
/// Only projects in `full` (crawled at full depth: the others hold no
/// files) are ranked.
pub fn route(
    pg: &Pg,
    terms: &[String],
    qvec: Option<&String>,
    mass: &[(String, f64)],
    named: &[String],
    full: &HashSet<String>,
    lexical: Lexical,
) -> Result<Vec<(String, f64)>> {
    let x = &pg.cfg.cross;
    let per = x.route_per_project;
    let mut lists: Vec<(f64, HashMap<String, f64>)> = Vec::new();
    // Summaries: project and folder cards.
    if x.route_summary > 0.0 && !terms.is_empty() {
        let rows = if lexical != Lexical::Fts && super::schema::bm25_fields(pg)? {
            let b = &pg.cfg.bm25;
            let q = "array_to_string($1::text[], ' ')";
            pg.query(
                &format!(
                    "SELECT e.project, pdb.score(e.id)::float8 AS s FROM entities e \
                     WHERE (e.summary ||| {q}::pdb.boost({}) OR words_of(e.asks) ||| {q}::pdb.boost({}) \
                     OR e.seed ||| {q}::pdb.boost({}) OR e.why ||| {q}::pdb.boost({})) \
                     AND e.kind === ARRAY['project', 'dir']::text[]::pdb.const(0) \
                     ORDER BY s DESC, e.id LIMIT $2",
                    b.summary, b.asks, b.seed, b.why
                ),
                &[&terms, &x.route_cards],
            )?
        } else {
            pg.query(
                "WITH q AS (SELECT tsq_or(plainto_tsquery('english', t)) AS q FROM unnest($1::text[]) AS t) \
                 SELECT e.project, ts_rank_cd(e.tsv, q.q)::float8 AS s FROM entities e, q \
                 WHERE e.tsv @@ q.q AND e.kind IN ('project', 'dir') ORDER BY s DESC, e.id LIMIT $2",
                &[&terms, &x.route_cards],
            )?
        };
        let rows: Vec<(String, f64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        lists.push((
            x.route_summary,
            best_sums(&rows, x.route_summary_best.max(1), full),
        ));
    }
    // The query's vector against the projects' cards.
    if x.route_dense > 0.0
        && let Some(v) = qvec
    {
        let names: Vec<String> = full.iter().cloned().collect();
        let rows = pg.query(
            "SELECT e.id, (1 - (e.card_vec <=> $1::text::halfvec(384)))::float8 FROM entities e \
             WHERE e.id = ANY($2) AND e.kind = 'project' AND e.card_vec IS NOT NULL",
            &[v, &names],
        )?;
        let sims: Vec<(String, f64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        let lo = sims.iter().map(|s| s.1).fold(f64::INFINITY, f64::min);
        let hi = sims.iter().map(|s| s.1).fold(f64::NEG_INFINITY, f64::max);
        if hi > lo {
            lists.push((
                x.route_dense,
                sims.into_iter()
                    .map(|(p, s)| (p, (s - lo) / (hi - lo)))
                    .collect(),
            ));
        }
    }
    // The fused candidates' mass.
    if x.route_fused > 0.0 {
        let top: Vec<(String, f64)> = mass.iter().take(x.route_fused_depth).cloned().collect();
        lists.push((x.route_fused, best_sums(&top, per, full)));
    }
    // The projects it names.
    if x.route_names > 0.0 && !named.is_empty() {
        lists.push((
            x.route_names,
            named
                .iter()
                .filter(|p| full.contains(*p))
                .map(|p| (p.clone(), 1.0))
                .collect(),
        ));
    }
    Ok(combine(&lists))
}

/// Each project's best `per` scores summed, scaled to the best project's.
fn best_sums(rows: &[(String, f64)], per: usize, full: &HashSet<String>) -> HashMap<String, f64> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut sum: HashMap<String, f64> = HashMap::new();
    for (p, s) in rows {
        if !full.contains(p) || *s <= 0.0 {
            continue;
        }
        let n = seen.entry(p.as_str()).or_insert(0);
        if *n >= per {
            continue;
        }
        *n += 1;
        *sum.entry(p.clone()).or_insert(0.0) += s;
    }
    let hi = sum.values().copied().fold(0.0, f64::max);
    if hi > 0.0 {
        for v in sum.values_mut() {
            *v /= hi;
        }
    }
    sum
}

/// The weighted mean of scaled lists, best first (ties by name).
pub fn combine(lists: &[(f64, HashMap<String, f64>)]) -> Vec<(String, f64)> {
    let total: f64 = lists.iter().map(|l| l.0).sum();
    let mut score: HashMap<String, f64> = HashMap::new();
    if total <= 0.0 {
        return Vec::new();
    }
    for (w, l) in lists {
        for (p, v) in l {
            *score.entry(p.clone()).or_insert(0.0) += w * v / total;
        }
    }
    let mut out: Vec<(String, f64)> = score.into_iter().filter(|s| s.1 > 0.0).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// The routed projects: the best, and those after it with at least `ratio`
/// of its score, `most` at most.
pub fn routed(scores: &[(String, f64)], most: usize, ratio: f64) -> Vec<String> {
    let Some(top) = scores.first().map(|s| s.1) else {
        return Vec::new();
    };
    scores
        .iter()
        .take(most)
        .filter(|s| s.1 >= ratio * top)
        .map(|s| s.0.clone())
        .collect()
}

/// A file a session touched along with a seed file.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    pub id: String,
    pub strength: f64,
    /// The seed it was reached from most strongly, and through how many
    /// sessions in all.
    pub via: String,
    pub sessions: usize,
}

/// The files sessions touched along with `seeds` (file ids, best first),
/// strongest first (`[cross]` `touched`): a neighbor's strength sums, over
/// each seed and each session that touched both, the seed's weight (1 /
/// sqrt of its place) times the session's IDF (ln of all the touched files
/// over the session's own: a session that touched hundreds says little
/// about any two of them) times the neighbor's (ln of the sessions with
/// touched files over those that touched it: a file every session reads,
/// a CLAUDE.md, says little), times `touched_cross` when the neighbor is in
/// another project than the seed. `touched_max` at most.
pub fn touched(pg: &Pg, seeds: &[String]) -> Result<Vec<Neighbor>> {
    let x: &Cross = &pg.cfg.cross;
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let rows = pg.query(
        "WITH t AS (SELECT src, dst FROM edges WHERE kind = 'touched'), \
              sz AS (SELECT src, count(*)::float8 AS n FROM t GROUP BY src), \
              df AS (SELECT dst, count(*)::float8 AS n FROM t GROUP BY dst), \
              tot AS (SELECT count(DISTINCT dst)::float8 AS files, count(DISTINCT src)::float8 AS sessions FROM t) \
         SELECT b.dst, s.f, s.ord, a.src, ln(tot.files / sz.n)::float8, ln(tot.sessions / df.n)::float8 \
         FROM unnest($1::text[]) WITH ORDINALITY AS s(f, ord) \
         JOIN t a ON a.dst = s.f \
         JOIN t b ON b.src = a.src AND b.dst <> s.f \
         JOIN sz ON sz.src = a.src JOIN df ON df.dst = b.dst, tot",
        &[&seeds],
    )?;
    // neighbor -> (strength, (best seed, its part), sessions)
    type Acc = (f64, (String, f64), HashSet<String>);
    let mut acc: HashMap<String, Acc> = HashMap::new();
    for r in rows {
        let (dst, seed, ord, session): (String, String, i64, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let (idf, ndf): (f64, f64) = (r.get(4), r.get(5));
        let cross = crate::model::project_of_id(&dst) != crate::model::project_of_id(&seed);
        let part = idf.max(0.0) * ndf.max(0.0) / (ord as f64).sqrt()
            * if cross { x.touched_cross } else { 1.0 };
        let e = acc
            .entry(dst)
            .or_insert_with(|| (0.0, (String::new(), 0.0), HashSet::new()));
        e.0 += part;
        if part > e.1.1 {
            e.1 = (seed, part);
        }
        e.2.insert(session);
    }
    let mut out: Vec<Neighbor> = acc
        .into_iter()
        .filter(|(_, (s, _, _))| *s > 0.0)
        .map(|(id, (strength, (via, _), sessions))| Neighbor {
            id,
            strength,
            via,
            sessions: sessions.len(),
        })
        .collect();
    out.sort_by(|a, b| {
        b.strength
            .total_cmp(&a.strength)
            .then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(x.touched_max);
    Ok(out)
}

/// The pool's shape: its size, each key's guaranteed places, the most one
/// key may hold, and the most one file may.
#[derive(Debug, Clone, Copy)]
pub struct Slots {
    pub depth: usize,
    pub min: usize,
    pub cap: usize,
    pub per_file: usize,
}

/// The reranker's pool (`[cross]` `slots`): indexes into `order` (best
/// first), in their order. Each key in `keys` (best first) gets its best
/// `min` files in, one candidate each: best by the key's own ranking in
/// `own` (its sub-query's, best first) when it has one, else by place in
/// `order`. Then the rest by place, while no key holds more than `cap` and
/// no file more than `per_file`; then, if room is left, by place
/// regardless of the cap. `key_of` gives a candidate's key ("" for none)
/// and `file_of` its file.
pub fn pool(
    order: &[(String, f64)],
    keys: &[String],
    own: &HashMap<String, Vec<String>>,
    s: Slots,
    key_of: &dyn Fn(&str) -> String,
    file_of: &dyn Fn(&str) -> String,
) -> Vec<usize> {
    let Slots {
        depth,
        min,
        cap,
        per_file,
    } = s;
    let place: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();
    let mut chosen: Vec<usize> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut by_file: HashMap<String, usize> = HashMap::new();
    let take = |i: usize,
                chosen: &mut Vec<usize>,
                by_key: &mut HashMap<String, usize>,
                by_file: &mut HashMap<String, usize>| {
        chosen.push(i);
        *by_key.entry(key_of(&order[i].0)).or_insert(0) += 1;
        *by_file.entry(file_of(&order[i].0)).or_insert(0) += 1;
    };
    let room = |chosen: &Vec<usize>| chosen.len() < depth;
    // Each key's guaranteed files, by its own ranking.
    for k in keys {
        let ranked: Vec<usize> = match own.get(k) {
            Some(ids) => ids
                .iter()
                .filter_map(|id| place.get(id.as_str()).copied())
                .collect(),
            None => (0..order.len()).collect(),
        };
        let mut n = 0;
        for i in ranked {
            if n >= min || !room(&chosen) {
                break;
            }
            let id = &order[i].0;
            if chosen.contains(&i) || key_of(id) != *k {
                continue;
            }
            if by_file.contains_key(&file_of(id)) {
                continue;
            }
            take(i, &mut chosen, &mut by_key, &mut by_file);
            n += 1;
        }
    }
    // The rest by place, under the cap; then regardless of it.
    for capped in [true, false] {
        for (i, (id, _)) in order.iter().enumerate() {
            if !room(&chosen) {
                break;
            }
            if chosen.contains(&i) {
                continue;
            }
            if by_file.get(&file_of(id)).copied().unwrap_or(0) >= per_file {
                continue;
            }
            let k = key_of(id);
            if capped && !k.is_empty() && by_key.get(&k).copied().unwrap_or(0) >= cap {
                continue;
            }
            take(i, &mut chosen, &mut by_key, &mut by_file);
        }
    }
    chosen.sort_unstable();
    chosen
}

/// `order` with the pool's members first (in their order), then the rest
/// in theirs.
pub fn pool_first<T: Clone>(order: &[T], pool: &[usize]) -> Vec<T> {
    let inpool: HashSet<usize> = pool.iter().copied().collect();
    pool.iter()
        .map(|&i| order[i].clone())
        .chain(
            order
                .iter()
                .enumerate()
                .filter(|(i, _)| !inpool.contains(i))
                .map(|(_, x)| x.clone()),
        )
        .collect()
}

/// The answer's first `limit` with at most `cap` from one of `spans`
/// while another of them still has a result further down (`[cross]`
/// `top_cap`): an over-cap result moves down past the next result that
/// isn't over. Indexes into the projects given, in their new order.
pub fn cap_top(projects: &[String], spans: &[String], limit: usize, cap: usize) -> Vec<usize> {
    let n = projects.len();
    if cap == 0 || spans.len() < 2 {
        return (0..n).collect();
    }
    let spanned: HashSet<&str> = spans.iter().map(String::as_str).collect();
    let mut out: Vec<usize> = Vec::new();
    let mut left: Vec<usize> = (0..n).collect();
    let mut count: HashMap<&str, usize> = HashMap::new();
    while out.len() < limit && !left.is_empty() {
        // The first left that isn't over its cap while another spanned
        // project waits below.
        let pick = left.iter().position(|&i| {
            let p = projects[i].as_str();
            let over = spanned.contains(p) && count.get(p).copied().unwrap_or(0) >= cap;
            !over
                || !left
                    .iter()
                    .any(|&j| j != i && spanned.contains(projects[j].as_str()) && projects[j] != p)
        });
        let k = pick.unwrap_or(0);
        let i = left.remove(k);
        *count.entry(projects[i].as_str()).or_insert(0) += 1;
        out.push(i);
    }
    out.extend(left);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn reads_cross() {
        assert!(compares(&s(&["how", "both", "start"])));
        assert!(!compares(&s(&["how", "start"])));
        let projects = s(&["alpha", "web-app", "beta"]);
        assert_eq!(
            named_projects("how does Alpha talk to beta", &projects, "beta"),
            s(&["alpha"])
        );
        assert_eq!(
            named_projects("webapp layout", &projects, ""),
            s(&["web-app"])
        );
        assert!(named_projects("alphabet soup", &projects, "").is_empty());
    }

    #[test]
    fn route_combines_and_routes() {
        let l1: HashMap<String, f64> = [("a".into(), 1.0), ("b".into(), 0.5)].into();
        let l2: HashMap<String, f64> = [("b".into(), 1.0), ("c".into(), 0.2)].into();
        let c = combine(&[(1.0, l1), (1.0, l2)]);
        assert_eq!(c[0], ("b".into(), 0.75));
        assert_eq!(c[1], ("a".into(), 0.5));
        assert_eq!(routed(&c, 3, 0.5), s(&["b", "a"]));
        assert_eq!(routed(&c, 1, 0.0), s(&["b"]));
        let full: HashSet<String> = ["a".to_string(), "b".to_string()].into();
        let rows = vec![
            ("a".to_string(), 4.0),
            ("a".to_string(), 3.0),
            ("a".to_string(), 3.0),
            ("b".to_string(), 2.0),
            ("z".to_string(), 9.0),
        ];
        let m = best_sums(&rows, 2, &full);
        assert_eq!(m["a"], 1.0);
        assert!((m["b"] - 2.0 / 7.0).abs() < 1e-9);
        assert!(!m.contains_key("z"));
    }

    #[test]
    fn pool_gives_each_key_places_and_caps() {
        // Ten from a, then three from b.
        let mut order: Vec<(String, f64)> = (0..10).map(|i| (format!("a:f{i}"), 1.0)).collect();
        order.extend((0..3).map(|i| (format!("b:g{i}"), 0.5)));
        let key = |id: &str| id.split(':').next().unwrap().to_string();
        let file = |id: &str| id.to_string();
        let shape = |depth, min, cap| Slots {
            depth,
            min,
            cap,
            per_file: 2,
        };
        let none = HashMap::new();
        let p = pool(&order, &s(&["a", "b"]), &none, shape(6, 2, 4), &key, &file);
        // b's two places, a capped at 4.
        assert_eq!(p, vec![0, 1, 2, 3, 10, 11]);
        // b's own ranking (its sub-query's) picks its places.
        let own: HashMap<String, Vec<String>> = [("b".to_string(), s(&["b:g2", "b:g0"]))].into();
        let p2 = pool(&order, &s(&["a", "b"]), &own, shape(6, 2, 4), &key, &file);
        assert_eq!(p2, vec![0, 1, 2, 3, 10, 12]);
        let first = pool_first(&order, &p);
        assert_eq!(first[4].0, "b:g0");
        assert_eq!(first[6].0, "a:f4");
        // Per file: the same file twice at most.
        let order2: Vec<(String, f64)> = (0..5).map(|i| (format!("a:x#s{i}"), 1.0)).collect();
        let fileof = |id: &str| id.split('#').next().unwrap().to_string();
        assert_eq!(
            pool(&order2, &s(&["a"]), &none, shape(4, 2, 4), &key, &fileof),
            vec![0, 1]
        );
    }

    #[test]
    fn cap_moves_extras_down_while_another_waits() {
        let p = s(&["a", "a", "a", "a", "b", "c", "a"]);
        // Cap 2: the third a waits until b is in.
        assert_eq!(
            cap_top(&p, &s(&["a", "b"]), 5, 2),
            vec![0, 1, 4, 2, 3, 5, 6]
        );
        // One project spanned: unchanged.
        assert_eq!(cap_top(&p, &s(&["a"]), 5, 2), (0..7).collect::<Vec<_>>());
        assert_eq!(
            cap_top(&p, &s(&["a", "b"]), 5, 0),
            (0..7).collect::<Vec<_>>()
        );
    }
}

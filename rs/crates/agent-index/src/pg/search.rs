//! Search on Postgres (docs/INDEX-V2.md, "The pipeline", steps 2 and 3): the
//! candidate lists and their fusion in one statement, then v1's priors.
//!
//! Round trips: the context (projects near the caller, whether vectors
//! exist), the label n-grams, the fused statement, and the hits' details.
//! The fused statement holds every list as a CTE, each also fetched scoped to
//! the caller's project and the projects it depends on (v1's evaluation
//! showed a global cut, before the prior, loses local results):
//!
//! - `lexical`: the entity cards' weighted `tsvector` (names, terms and
//!   labels with `simple` at A; title and summary with `english` at B; seed
//!   and why at C; children's names at D), by `ts_rank_cd`, any word;
//! - `all words`: the same, every word;
//! - `full text`: chunks of files' text (`english` and `simple`), each file
//!   ranked by its best chunk;
//! - `trigram`: `word_similarity` of each term in names, paths and labels;
//! - `semantic`: `card_vec` by cosine on HNSW, iterative scan in relaxed
//!   order, re-sorted through a `MATERIALIZED` CTE;
//! - `label`: the label n-grams that matched part of the query;
//!
//! each ranked `ROW_NUMBER() OVER (ORDER BY score DESC, id)`, then `UNION
//! ALL` and `GROUP BY` for reciprocal rank fusion (k = 60). The same
//! statement finds the pins v1 has (an identifier's definitions, the few
//! files that hold a rare identifier) and the query terms nothing mentions.
//! v1's priors (the caller's project and its dependencies, kind, importance,
//! tests, fixtures, kept-out-of-the-way dirs) and its at-most-two-per-file
//! rule apply in Rust, on the fused list, so `--explain` and `--json` read
//! as v1's.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::Value;

use super::Pg;
use crate::config::{CORPORA, Config};
use crate::files;
use crate::model::{self, Summary};
use crate::names;
use crate::query::{CrossInfo, FILE_KINDS, Hit, Mapping, RerankInfo, SearchOptions, SearchResult};

// Every number a search tunes on is in crate::config (the runtime TOML):
// the lists' depth, the rarest-term budgets (ranking a chunk detoasts its
// tsvectors, and a common word's 20,000 chunks took most of a second; a
// word common in files' text is common in names and seeds too, so the
// cards' budget reads the same chunk frequencies), the trigram terms, the
// rank weights, RRF's k and each list's weight, and the priors.

/// A number as SQL text that reads back exactly (`1` for 1.0).
fn num(x: f64) -> String {
    format!("{x}")
}

/// The filter of kind, project, under and corpus on entity alias `e`:
/// parameters $2, $3, $4 and $13 (the corpora searched).
const FILTER: &str = "($2 = '' OR e.kind = $2 OR e.file_kind = $2) AND ($3 = '' OR e.project = $3) \
     AND ($4 = '' OR e.path = $4 OR starts_with(e.path, $4 || '/')) AND e.corpus = ANY($13::text[])";
/// The corpus filter on chunk alias `c`, which records its entity's corpus
/// in its meta (no join; a chunk from before records nothing: code).
const CHUNK_CORPUS: &str = "coalesce(c.meta->>'corpus', 'code') = ANY($13::text[])";

/// The corpora a search covers when it names none: what the crawl found.
pub const CRAWLED: &[&str] = &["code", "prose"];

/// The corpora a search covers: its own, else [`CRAWLED`].
fn corpora_of(o: &SearchOptions) -> Vec<String> {
    if o.corpora.is_empty() {
        CRAWLED.iter().map(|s| s.to_string()).collect()
    } else {
        o.corpora.clone()
    }
}
/// The local scope: parameter $5, the caller's project and its dependencies.
const HERE: &str = "e.project = ANY($5::text[])";

/// Which lexical lists a search fuses: native full text (`fts`, the
/// default), BM25 (`bm25`, ParadeDB's `pg_search`, for the A/B in
/// docs/INDEX-V2.md), or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lexical {
    #[default]
    Fts,
    Bm25,
    Both,
}

impl Lexical {
    pub fn parse(s: &str) -> Result<Lexical> {
        Ok(match s {
            "" | "fts" => Lexical::Fts,
            "bm25" => Lexical::Bm25,
            "both" => Lexical::Both,
            _ => bail!("--lexical takes fts, bm25 or both, not {s:?}"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Lexical::Fts => "fts",
            Lexical::Bm25 => "bm25",
            Lexical::Both => "both",
        }
    }

    /// What a search runs: `asked` (`--lexical`, `AGENT_INDEX_LEXICAL`) when
    /// given, else `[search].lexical`; `auto` or nothing is bm25 where the
    /// schema has its indexes, fts elsewhere.
    pub fn resolve(pg: &Pg, asked: &str) -> Result<Lexical> {
        let s = if asked.is_empty() {
            pg.cfg.search.lexical.as_str()
        } else {
            asked
        };
        match s {
            "" | "auto" => Ok(if super::schema::has_bm25(pg)? {
                Lexical::Bm25
            } else {
                Lexical::Fts
            }),
            s => Lexical::parse(s),
        }
    }
}

/// What a BM25 card scan filters inside pg_search's query (`[bm25]
/// pushdown`, on an index with the filter fields, `schema::bm25_fields`):
/// which filters were asked for, since one not asked for is left out of
/// the query rather than matched against ''.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Push {
    pub kind: bool,
    pub project: bool,
    pub under: bool,
    /// With `under`: the projects an entity at or under that path can be
    /// in (the one holding it, those above it, those inside it), pushed as
    /// a term set before the path is checked on the rows kept. Empty: the
    /// path alone (it is in no project).
    pub under_projects: Vec<String>,
}

/// The projects an entity at or under `under` can belong to: a project's
/// entities lie under its directory, so only one whose directory holds
/// `under`, or lies inside it.
pub fn projects_under(under: &str, projects: &[(String, String)]) -> Vec<String> {
    let under = under.trim_end_matches('/');
    if under.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = projects
        .iter()
        .filter(|(_, p)| {
            let p = p.trim_end_matches('/');
            !p.is_empty()
                && (under == p
                    || under.starts_with(&format!("{p}/"))
                    || p.starts_with(&format!("{under}/")))
        })
        .map(|(n, _)| n.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Text as a SQL array literal (each element quoted, `'` doubled).
fn text_array(v: &[String]) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect();
    format!("ARRAY[{}]::text[]", items.join(", "))
}

/// THE BM25 HOOK. The A/B's lists over the same card text and chunks, by
/// `pg_search`'s BM25: (CTEs, lists as `name:cte`). The indexes are
/// migration 2 in schema.rs, built only where the database has `pg_search`:
/// `schema::ensure_bm25` builds them later, or says the extension is
/// missing. Each CTE yields `(id, s)`, best first, at most `$12` rows, with
/// [`FILTER`] on entity alias `e` (and [`HERE`] for the `_here` ones when
/// `local`); the query's terms are `$1::text[]` (content words,
/// lowercased), the whole query is `$7` when it is one word. Name the lists
/// `bm25` / `bm25 here` (cards) and `bm25 full text` / `bm25 full text here`
/// (chunks, an entity ranked by its best chunk), so `--explain` tells them
/// from the native ones. With `push`, the cards' kind, project and corpus
/// filters (and `--under` as the projects it can hold) are terms of the
/// query itself; the local scope is read off the scan's rows.
pub fn bm25_ctes(
    local: bool,
    push: Option<&Push>,
    cfg: &Config,
) -> Option<(Vec<String>, Vec<String>)> {
    let f = FILTER;
    let here = format!(" AND {HERE}");
    // Any term in any card field, each field boosted (`[bm25]` in the
    // configuration; by default in the ratio of the native list's weights,
    // A 1.0, B 0.4, C 0.2, D 0.1). `|||` cuts the query with each field's
    // own tokenizer, so `held_messages` and `running` meet the indexed
    // `held`, `messag` and `run`.
    let b = &cfg.bm25;
    let fields = [
        ("e.terms", b.terms),
        ("e.labels_text", b.labels),
        ("e.title", b.title),
        ("e.summary", b.summary),
        ("words_of(e.asks)", b.asks),
        ("e.seed", b.seed),
        ("e.why", b.why),
        ("e.symbols", b.symbols),
    ];
    let matches = fields
        .iter()
        .map(|(col, boost)| format!("{col} ||| {BM25_QUERY}::pdb.boost({})", num(*boost)))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut ctes = Vec::new();
    let mut lists = Vec::new();
    if let Some(p) = push {
        // The filters as terms on the index's literal fields, each
        // `pdb.const(0)`: a term query would add its own score, and a plain
        // `=` here is pushed down as one too (it adds about 2.4). Under (a
        // path prefix) stays a filter on the rows kept, and so does the
        // local scope: a second scan limited to the caller's projects cost
        // more (24 ms median against 16) than reading `project` off the
        // one scan's rows.
        let mut w = String::from(" AND e.corpus === $13::text[]::pdb.const(0)");
        if p.kind {
            w.push_str(
                " AND (e.kind === $2::text::pdb.const(0) OR e.file_kind === $2::text::pdb.const(0))",
            );
        }
        if p.project {
            w.push_str(" AND e.project === $3::text::pdb.const(0)");
        }
        if p.under && !p.under_projects.is_empty() {
            w.push_str(&format!(
                " AND e.project === {}::pdb.const(0)",
                text_array(&p.under_projects)
            ));
        }
        let mut cols = String::new();
        if p.under {
            cols.push_str(", e.path");
        }
        if local {
            cols.push_str(", e.project");
        }
        ctes.push(format!(
            "bm25_hits AS MATERIALIZED (SELECT e.id, pdb.score(e.id)::float8 AS s{cols} \
             FROM entities e WHERE ({matches}){w})"
        ));
        let under = if p.under {
            "($4 = '' OR e.path = $4 OR starts_with(e.path, $4 || '/'))"
        } else {
            "true"
        };
        let card = |name: &str, scope: &str| {
            format!(
                "{name} AS (SELECT e.id, e.s FROM bm25_hits e WHERE {under}{scope} ORDER BY e.s DESC, e.id LIMIT $12)"
            )
        };
        ctes.push(card("bm25_cards", ""));
        lists.push("bm25:bm25_cards".to_string());
        if local {
            ctes.push(card("bm25_cards_here", &here));
            lists.push("bm25 here:bm25_cards_here".into());
        }
    } else {
        // One unfiltered scan, then the filters on its rows: given in the
        // same WHERE as plain conditions, pg_search checks them against
        // every row of the table (690 ms with --under or --kind, against
        // 17 ms this way; same ranking).
        ctes.push(format!(
            "bm25_hits AS MATERIALIZED (SELECT e.id, pdb.score(e.id)::float8 AS s, e.project, e.kind, e.file_kind, e.path, e.corpus \
             FROM entities e WHERE ({matches}))"
        ));
        let card = |name: &str, scope: &str| {
            format!(
                "{name} AS (SELECT e.id, e.s FROM bm25_hits e WHERE {f}{scope} ORDER BY e.s DESC, e.id LIMIT $12)"
            )
        };
        ctes.push(card("bm25_cards", ""));
        lists.push("bm25:bm25_cards".to_string());
        if local {
            ctes.push(card("bm25_cards_here", &here));
            lists.push("bm25 here:bm25_cards_here".into());
        }
    }
    // An entity by its best chunk; the chunks' filters stay on their
    // entities' rows (a chunk holds no project of its own).
    let full = |name: &str, scope: &str| {
        format!(
            "{name} AS (SELECT bch.id, bch.s FROM bch JOIN entities e ON e.id = bch.id \
             WHERE {f}{scope} ORDER BY bch.s DESC, bch.id LIMIT $12)"
        )
    };
    ctes.push(format!(
        "bch AS (SELECT c.entity_id AS id, max(pdb.score(c.entity_id))::float8 AS s FROM chunks c \
         WHERE c.text ||| {BM25_QUERY} GROUP BY c.entity_id)"
    ));
    ctes.push(full("bm25_full", ""));
    lists.push("bm25 full text:bm25_full".into());
    if local {
        ctes.push(full("bm25_full_here", &here));
        lists.push("bm25 full text here:bm25_full_here".into());
    }
    Some((ctes, lists))
}

/// The query's terms as one string for `pg_search`'s match operator.
const BM25_QUERY: &str = "array_to_string($1::text[], ' ')";

/// The statement's text: which lists it holds depends on the query (every
/// word only for several terms, the local lists only with a caller project,
/// the semantic list only with a query vector) and on `lexical`.
///
/// `filtered` (a kind, project or under asked for) joins the lists made
/// from chunks to their entities to filter them; without it they don't
/// touch `entities` at all, and their local scope is the project an id
/// names (`alpha:src/x.rs` → `alpha`; a project name holds no `:`).
pub fn fused_sql(
    several_terms: bool,
    local: bool,
    dense: &[DenseSpec],
    lexical: Lexical,
    filtered: bool,
    push: Option<&Push>,
    cfg: &Config,
) -> Result<String> {
    let f = FILTER;
    let s = &cfg.search;
    let (chunk_budget, card_budget, trigram_terms) =
        (s.chunk_budget, s.card_budget, s.trigram_terms);
    let weights = s.rank_weights.map(num).join(",");
    let norm = s.rank_normalization;
    let k = num(s.rrf_k);
    let h = HERE;
    // A list keyed by entity id (alias `x`): joined to its entity only to
    // filter; scoped locally by the id's project otherwise.
    let by_id = |x: &str, local: bool| -> (String, String) {
        let join = if filtered {
            format!(" JOIN entities e ON e.id = {x}.id")
        } else {
            String::new()
        };
        let mut cond: Vec<String> = Vec::new();
        if filtered {
            cond.push(f.to_string());
        }
        if local {
            cond.push(format!("split_part({x}.id, ':', 1) = ANY($5::text[])"));
        }
        let wher = if cond.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", cond.join(" AND "))
        };
        (join, wher)
    };
    let mut ctes: Vec<String> = Vec::new();
    let mut lists: Vec<String> = Vec::new();
    let native = lexical != Lexical::Bm25;
    // Each term's document frequency in the chunks (0 when rarer than the
    // lexemes table counts; a term of several lexemes, its rarest): the
    // chunk list takes the rarest terms until their chunks add up to
    // CHUNK_BUDGET, and always the rarest one. The card lists take every term.
    let df = |cfg: &str| {
        format!(
            "coalesce((SELECT CASE WHEN count(l.word) < cardinality(a.w) THEN 0 ELSE min(l.ndoc) END \
             FROM (SELECT tsvector_to_array(to_tsvector('{cfg}', t)) AS w) a \
             LEFT JOIN lexemes l ON l.config = '{cfg}' AND l.word = ANY(a.w) GROUP BY a.w), 0)"
        )
    };
    ctes.push(format!(
        "qt AS (SELECT t, plainto_tsquery('simple', t) AS qs, plainto_tsquery('english', t) AS qe, \
         greatest({}, {}) AS df FROM unnest($1::text[]) AS t)",
        df("simple"),
        df("english")
    ));
    ctes.push(format!(
        "q AS (SELECT tsq_or(qs || qe) FILTER (WHERE n = 1 OR cum <= {card_budget}) AS any_c, \
         tsq_and(qs || qe) AS all_q, \
         tsq_or(qs) FILTER (WHERE n = 1 OR cum <= {chunk_budget}) AS any_s, \
         tsq_or(qe) FILTER (WHERE n = 1 OR cum <= {chunk_budget}) AS any_e \
         FROM (SELECT *, sum(df) OVER (ORDER BY df, t) AS cum, row_number() OVER (ORDER BY df, t) AS n FROM qt) x)"
    ));
    let card = |name: &str, cond: &str, scope: &str| {
        format!(
            "{name} AS (SELECT e.id, ts_rank_cd('{{{weights}}}'::float4[], e.tsv, q.{cond}, {norm})::float8 AS s \
             FROM entities e, q WHERE e.tsv @@ q.{cond} AND {f}{scope} ORDER BY s DESC, e.id LIMIT $12)"
        )
    };
    let here = format!(" AND {h}");
    if native {
        ctes.push(card("cards", "any_c", ""));
        lists.push("lexical:cards".into());
    }
    if native && local {
        ctes.push(card("cards_here", "any_c", &here));
        lists.push("lexical here:cards_here".into());
    }
    if native && several_terms {
        ctes.push(card("cards_all", "all_q", ""));
        lists.push("all words:cards_all".into());
        if local {
            ctes.push(card("cards_all_here", "all_q", &here));
            lists.push("all words here:cards_all_here".into());
        }
    }
    // Trigrams for the query's rarest few terms (a typo counts as rare):
    // each term is a probe, and a long question's common words took most of
    // its time while matching most names.
    ctes.push(format!(
        "tri_hits AS (SELECT e.id, sum(word_similarity(t, e.tri))::float8 AS s, e.project, e.kind, e.file_kind, e.path, e.corpus \
         FROM (SELECT t FROM unnest($6::text[]) AS t JOIN qt USING (t) ORDER BY qt.df, t LIMIT {trigram_terms}) tt \
         JOIN entities e ON e.tri %> tt.t GROUP BY e.id)"
    ));
    ctes.push(format!(
        "tri AS (SELECT e.id, e.s FROM tri_hits e WHERE {f} ORDER BY e.s DESC, e.id LIMIT $12)"
    ));
    lists.push("trigram:tri".into());
    if local {
        ctes.push(format!(
            "tri_here AS (SELECT e.id, e.s FROM tri_hits e WHERE {f}{here} ORDER BY e.s DESC, e.id LIMIT $12)"
        ));
        lists.push("trigram here:tri_here".into());
    }
    if native {
        ctes.push(format!(
            "ch AS (SELECT c.entity_id AS id, max(greatest(ts_rank_cd(c.tsv_simple, q.any_s, {norm}), \
             ts_rank_cd(c.tsv_en, q.any_e, {norm})))::float8 AS s \
             FROM chunks c, q WHERE (c.tsv_simple @@ q.any_s OR c.tsv_en @@ q.any_e) AND {CHUNK_CORPUS} \
             GROUP BY c.entity_id)"
        ));
        let (j, w) = by_id("ch", false);
        ctes.push(format!(
            "full_text AS (SELECT ch.id, ch.s FROM ch{j}{w} ORDER BY ch.s DESC, ch.id LIMIT $12)"
        ));
        lists.push("full text:full_text".into());
        if local {
            let (j, w) = by_id("ch", true);
            ctes.push(format!(
                "full_here AS (SELECT ch.id, ch.s FROM ch{j}{w} ORDER BY ch.s DESC, ch.id LIMIT $12)"
            ));
            lists.push("full text here:full_here".into());
        }
    }
    if lexical != Lexical::Fts {
        let Some((c, l)) = bm25_ctes(local, push, cfg) else {
            bail!(
                "--lexical {}: this schema has no BM25 index yet (pg_search); --lexical fts uses native full text",
                if lexical == Lexical::Bm25 {
                    "bm25"
                } else {
                    "both"
                }
            );
        };
        ctes.extend(c);
        lists.extend(l);
    }
    // The dense lists, each with its query vector at $14, $15, ...
    for (i, d) in dense.iter().enumerate() {
        let p = 14 + i;
        let dims = d.dims;
        let only = if d.only.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = d
                .only
                .iter()
                .filter(|c| CORPORA.contains(&c.as_str()))
                .map(|c| format!("'{c}'"))
                .collect();
            format!(" AND e.corpus IN ({})", names.join(", "))
        };
        let sql = |name: &str, scope: &str| -> String {
            match (&d.table, d.chunks) {
                (None, _) => format!(
                    "{name}_0 AS MATERIALIZED (SELECT e.id, (e.card_vec <=> ${p}::text::halfvec(384))::float8 AS d FROM entities e \
                     WHERE e.card_vec IS NOT NULL AND {f}{scope}{only} ORDER BY e.card_vec <=> ${p}::text::halfvec(384) LIMIT $12), \
                     {name} AS (SELECT id, 1 - d AS s FROM {name}_0 ORDER BY d + 0, id)"
                ),
                (Some(t), false) => format!(
                    "{name}_0 AS MATERIALIZED (SELECT e.entity_id AS id, (e.vec <=> ${p}::text::halfvec({dims}))::float8 AS d FROM {t} e \
                     WHERE {f}{scope}{only} ORDER BY e.vec <=> ${p}::text::halfvec({dims}) LIMIT $12), \
                     {name} AS (SELECT id, 1 - d AS s FROM {name}_0 ORDER BY d + 0, id)"
                ),
                // A chunk list: the nearest windows, each entity by its best.
                (Some(t), true) => format!(
                    "{name}_0 AS MATERIALIZED (SELECT e.entity_id AS id, (e.vec <=> ${p}::text::halfvec({dims}))::float8 AS d FROM {t} e \
                     WHERE {f}{scope}{only} ORDER BY e.vec <=> ${p}::text::halfvec({dims}) LIMIT $12 * 4), \
                     {name} AS (SELECT id, 1 - min(d) AS s FROM {name}_0 GROUP BY id ORDER BY 1 - min(d) DESC, id LIMIT $12)"
                ),
            }
        };
        ctes.push(sql(&d.cte, ""));
        lists.push(format!("{}:{}", d.list, d.cte));
        if local {
            ctes.push(sql(&format!("{}_here", d.cte), &here));
            lists.push(format!("{} here:{}_here", d.list, d.cte));
        }
    }
    // Pins: an identifier's definitions, exact name first, else ignoring case.
    let exact = |name: &str, cmp: &str, extra: &str| {
        format!(
            "{name} AS (SELECT e.id, row_number() OVER (ORDER BY CASE e.kind WHEN 'symbol' THEN 0 WHEN 'file' THEN 1 ELSE 2 END, \
             e.importance DESC, e.id) AS r FROM entities e WHERE $7 <> '' AND {cmp} \
             AND e.kind IN ('symbol', 'file', 'dir', 'project') AND e.file_kind <> 'impl' AND {f}{extra})"
        )
    };
    ctes.push(exact("exact_cs", "e.name = $7", ""));
    ctes.push(exact(
        "exact_ci",
        "lower(e.name) = lower($7)",
        " AND NOT EXISTS (SELECT 1 FROM exact_cs)",
    ));
    ctes.push(
        format!("exact AS (SELECT id, r FROM exact_cs WHERE r <= {el} UNION ALL SELECT id, r FROM exact_ci WHERE r <= {el})", el = s.exact_limit),
    );
    // A rare identifier no definition answers to: the few files holding it.
    ctes.push(format!(
        "rare_ch AS (SELECT c.entity_id AS id, max(ts_rank_cd(c.tsv_simple, rq.q, {norm}))::float8 AS s \
         FROM chunks c, (SELECT phraseto_tsquery('simple', $7) AS q) rq \
         WHERE $8 AND NOT EXISTS (SELECT 1 FROM exact) AND c.tsv_simple @@ rq.q AND {CHUNK_CORPUS} \
         GROUP BY c.entity_id)"
    ));
    // One more than the rare identifier's files may number, to tell too many.
    let rare_limit = s.rare_files + 1;
    let (j, w) = by_id("r", true);
    ctes.push(format!(
        "rare_here AS (SELECT r.id, row_number() OVER (ORDER BY r.s DESC, r.id) AS n FROM rare_ch r{j}{w} \
         ORDER BY r.s DESC, r.id LIMIT {rare_limit})"
    ));
    let (j, w) = by_id("r", false);
    ctes.push(format!(
        "rare_all AS (SELECT r.id, row_number() OVER (ORDER BY r.s DESC, r.id) AS n FROM rare_ch r{j}{w} \
         ORDER BY r.s DESC, r.id LIMIT {rare_limit})"
    ));
    // Terms nothing the search may return mentions: each test in turn (a
    // CASE runs them in order, so a word the cards hold costs one probe).
    let (cj, cw) = if filtered {
        (
            " JOIN entities e ON e.id = c.entity_id".to_string(),
            format!(" AND {f}"),
        )
    } else {
        (String::new(), format!(" AND {CHUNK_CORPUS}"))
    };
    ctes.push(format!(
        "absent AS (SELECT u.t FROM unnest($11::text[]) WITH ORDINALITY AS u(t, n) WHERE CASE \
         WHEN EXISTS (SELECT 1 FROM entities e WHERE e.tsv @@ (plainto_tsquery('simple', u.t) || plainto_tsquery('english', u.t)) AND {f}) THEN false \
         WHEN EXISTS (SELECT 1 FROM chunks c{cj} WHERE (c.tsv_simple @@ plainto_tsquery('simple', u.t) \
             OR c.tsv_en @@ plainto_tsquery('english', u.t)){cw}) THEN false \
         WHEN length(u.t) >= 3 AND EXISTS (SELECT 1 FROM entities e WHERE e.tri LIKE '%' || replace(u.t, '_', '\\_') || '%' AND {f}) THEN false \
         ELSE true END ORDER BY u.n)"
    ));
    let mut union: Vec<String> = lists
        .iter()
        .map(|l| {
            let (name, cte) = l.split_once(':').expect("name:cte");
            let w = num(cfg.list_weight(name));
            format!(
                "SELECT '{name}'::text AS list, id, s, row_number() OVER (ORDER BY s DESC, id) AS r, {w}::numeric AS w FROM {cte}"
            )
        })
        .collect();
    union.push(
        format!(
            "SELECT 'label'::text, u.id, 0::float8, u.ord, {}::numeric FROM unnest($9::text[]) WITH ORDINALITY AS u(id, ord)",
            num(cfg.list_weight("label"))
        ),
    );
    ctes.push(format!("lists AS ({})", union.join(" UNION ALL ")));
    ctes.push(format!(
        "fused AS (SELECT id, sum(w * (1.0 / ({k} + r)))::float8 AS rrf, \
         jsonb_agg(jsonb_build_array(list, r, round(s::numeric, 3)) ORDER BY r, list) AS lists FROM lists GROUP BY id)"
    ));
    ctes.push(
        "cand AS (SELECT id FROM fused UNION SELECT id FROM exact UNION SELECT id FROM rare_here \
         UNION SELECT id FROM rare_all UNION SELECT unnest($10::text[]))"
            .to_string(),
    );
    Ok(format!(
        "WITH {} SELECT \
         (SELECT coalesce(jsonb_agg(jsonb_build_array(e.id, e.kind, e.file_kind, e.project, e.rel, e.importance, \
             coalesce(f.rrf, 0), coalesce(f.lists, '[]'::jsonb), e.corpus)), '[]'::jsonb) \
          FROM cand c JOIN entities e ON e.id = c.id LEFT JOIN fused f ON f.id = c.id), \
         (SELECT coalesce(jsonb_agg(id ORDER BY r), '[]'::jsonb) FROM exact), \
         (SELECT coalesce(jsonb_agg(id ORDER BY n), '[]'::jsonb) FROM rare_here), \
         (SELECT coalesce(jsonb_agg(id ORDER BY n), '[]'::jsonb) FROM rare_all), \
         (SELECT coalesce(jsonb_agg(t), '[]'::jsonb) FROM absent)",
        ctes.join(",\n")
    ))
}

/// A candidate as the fused statement returns it.
struct Cand {
    corpus: String,
    kind: String,
    project: String,
    rel: String,
    importance: f64,
    rrf: f64,
    lists: Vec<(String, i64, f64)>,
}

/// What a search needs to know before its lists: the projects the caller's
/// depends on, whether any vectors exist, the largest importance, and the
/// projects (to read `--under`).
struct Context {
    near: HashMap<String, f64>,
    /// bge's card vectors exist (`entities.card_vec`).
    vectors: bool,
    /// The search's dense lists (set once the corpora are known).
    dense: Vec<DenseSpec>,
    max_imp: f64,
    projects: Vec<(String, String)>,
    /// The projects crawled at full depth (the others hold no files).
    full: HashSet<String>,
}

fn context(pg: &Pg, caller: &str) -> Result<Context> {
    let lo = format!("{caller}:");
    let hi = format!("{caller};");
    let r = pg
        .query(
            "SELECT ARRAY(SELECT DISTINCT dst FROM edges WHERE kind = 'depends-on'
                          AND $1 <> '' AND (src = $1 OR (src >= $2 AND src < $3))),
                    EXISTS (SELECT 1 FROM meta WHERE key = 'vectors' AND value <> '0'),
                    (SELECT coalesce(max(importance), 0) FROM entities),
                    ARRAY(SELECT name FROM projects), ARRAY(SELECT path FROM projects),
                    ARRAY(SELECT name FROM projects WHERE depth <> 'shallow')",
            &[&caller, &lo, &hi],
        )?
        .into_iter()
        .next()
        .expect("one row");
    let deps: Vec<String> = r.get(0);
    let mut near = HashMap::new();
    if !caller.is_empty() {
        for t in deps {
            let p = model::project_of_id(&t);
            if !p.is_empty() && p != caller {
                near.insert(p.to_string(), pg.cfg.priors.dependency);
            }
        }
        near.insert(caller.to_string(), pg.cfg.priors.own_project);
    }
    let names: Vec<String> = r.get(3);
    let paths: Vec<String> = r.get(4);
    Ok(Context {
        near,
        vectors: r.get(1),
        dense: Vec::new(),
        max_imp: r.get(2),
        projects: names.into_iter().zip(paths).collect(),
        full: r.get::<_, Vec<String>>(5).into_iter().collect(),
    })
}

/// v1's stage 1: labels the query, or a run of its words, answers to. The
/// n-grams go to the database in one statement; the choice (longest first,
/// one row per entity, 25 a gram, words a longer match covered skipped) is
/// v1's.
fn label_matches(pg: &Pg, query: &str, o: &SearchOptions) -> Result<Vec<Mapping>> {
    let words: Vec<String> = names::norm(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let mut out: Vec<Mapping> = Vec::new();
    if words.is_empty() {
        return Ok(out);
    }
    let lengths: Vec<usize> = (1..=words.len())
        .rev()
        .filter(|&n| n <= pg.cfg.search.label_max_words || n == words.len())
        .collect();
    let mut grams: Vec<(usize, usize, String, String)> = Vec::new();
    for &n in &lengths {
        for i in 0..=words.len() - n {
            let gram = &words[i..i + n];
            if n == 1 && (words.len() > 1 || names::is_stop(&gram[0]) || gram[0].len() < 3) {
                continue;
            }
            if gram.iter().all(|w| names::is_stop(w)) {
                continue;
            }
            grams.push((i, n, gram.join(" "), gram.concat()));
        }
    }
    if grams.is_empty() {
        return Ok(out);
    }
    let phrases: Vec<String> = grams.iter().map(|g| g.2.clone()).collect();
    let compacts: Vec<String> = grams.iter().map(|g| g.3.clone()).collect();
    let rows = pg.query(
        &format!(
            "SELECT g.ord::int8, l.entity_id, l.label, l.kind, l.origin, l.weight, e.importance
             FROM unnest($1::text[], $5::text[]) WITH ORDINALITY AS g(phrase, compact, ord)
             JOIN labels l ON l.norm = g.phrase OR l.compact = g.compact
             JOIN entities e ON e.id = l.entity_id
             WHERE {}
             ORDER BY g.ord, l.entity_id, l.weight DESC, l.label",
            FILTER.replace("$13", "$6")
        ),
        &[
            &phrases,
            &o.kind,
            &o.project,
            &o.under,
            &compacts,
            &corpora_of(o),
        ],
    )?;
    // Per gram: one row per entity (its best label), by weight then importance.
    // (entity, label, kind, origin, weight, importance) by gram.
    type Hit = (String, String, String, String, f64, f64);
    let mut per: HashMap<usize, Vec<Hit>> = HashMap::new();
    for r in rows {
        let g = r.get::<_, i64>(0) as usize - 1;
        let v = per.entry(g).or_default();
        let id: String = r.get(1);
        if v.iter().any(|x| x.0 == id) {
            continue;
        }
        v.push((id, r.get(2), r.get(3), r.get(4), r.get(5), r.get(6)));
    }
    let mut covered: HashSet<usize> = HashSet::new();
    for (gi, (i, n, phrase, _)) in grams.iter().enumerate() {
        if (*i..i + n).all(|j| covered.contains(&j)) {
            continue;
        }
        let Some(rows) = per.get_mut(&gi) else {
            continue;
        };
        rows.sort_by(|a, b| b.4.total_cmp(&a.4).then_with(|| b.5.total_cmp(&a.5)));
        rows.truncate(pg.cfg.search.labels_per_run);
        if rows.is_empty() {
            continue;
        }
        for j in *i..i + n {
            covered.insert(j);
        }
        let exact = *n == words.len();
        let mut seen = HashSet::new();
        for (entity, label, kind, origin, _, _) in rows.iter() {
            if !seen.insert(entity.clone()) {
                continue;
            }
            out.push(Mapping {
                phrase: phrase.clone(),
                label: label.clone(),
                kind: kind.clone(),
                origin: origin.clone(),
                entity: entity.clone(),
                exact,
            });
        }
    }
    Ok(out)
}

/// Whether a query is one identifier (v1's rule).
fn is_identifier(q: &str) -> bool {
    let q = q.trim();
    !q.is_empty()
        && q.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && q.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// The query embedder: (the model's name, the query) to the model's query
/// vector. Called only for a model whose vectors the store has.
pub type QueryEmbed<'a> = &'a dyn Fn(&str, &str) -> Result<Vec<f32>>;

/// A dense list of the fused statement: one model's card or chunk vectors
/// over the corpora it serves (crate::pg::vecs).
#[derive(Debug, Clone)]
pub struct DenseSpec {
    /// The model's name (what the query embedder takes).
    pub model: String,
    pub key: String,
    pub dims: usize,
    /// None: bge's card vectors in `entities.card_vec`; else the table.
    pub table: Option<String>,
    pub chunks: bool,
    /// The corpora this list serves, when they aren't all the search's
    /// (one model per corpus); empty otherwise.
    pub only: Vec<String>,
    /// The list's name and its CTE's.
    pub list: String,
    pub cte: String,
}

/// The dense lists a search over `corpora` has: for each model serving
/// some of them, its cards and (with `[embedder.chunks]`) its chunk
/// windows, where the store holds them.
fn dense_specs(pg: &Pg, corpora: &[String], legacy_vectors: bool) -> Result<Vec<DenseSpec>> {
    let plan = super::vecs::plan(&pg.cfg, corpora)?;
    let several = plan.len() > 1;
    let mut out = Vec::new();
    for (m, served) in &plan {
        let only = if several { served.clone() } else { Vec::new() };
        let tag = if several {
            format!(" {}", m.key)
        } else {
            String::new()
        };
        let cards = if m.legacy_cards {
            legacy_vectors.then_some(None)
        } else {
            let t = m.cards_table();
            super::vecs::has_rows(pg, &t)?.then_some(Some(t))
        };
        if let Some(table) = cards {
            let first = out.is_empty();
            out.push(DenseSpec {
                model: m.name.clone(),
                key: m.key.clone(),
                dims: m.dims,
                table,
                chunks: false,
                only: only.clone(),
                list: format!("semantic{tag}"),
                cte: if first {
                    "dense".into()
                } else {
                    format!("dense_{}", m.key)
                },
            });
        }
        if pg.cfg.embedder.chunks.enabled {
            let t = m.chunks_table();
            if super::vecs::has_rows(pg, &t)? {
                out.push(DenseSpec {
                    model: m.name.clone(),
                    key: m.key.clone(),
                    dims: m.dims,
                    table: Some(t),
                    chunks: true,
                    only: only.clone(),
                    list: format!("semantic chunks{tag}"),
                    cte: format!("dchunk_{}", m.key),
                });
            }
        }
    }
    Ok(out)
}

/// The query's vector for each dense list (one embedding per model).
fn query_vecs(embed: Option<QueryEmbed>, specs: &[DenseSpec], q: &str) -> Result<Vec<String>> {
    let Some(f) = embed else {
        return Ok(Vec::new());
    };
    let mut by_model: HashMap<&str, String> = HashMap::new();
    let mut out = Vec::new();
    for s in specs {
        if !by_model.contains_key(s.model.as_str()) {
            by_model.insert(&s.model, super::vec_text(&f(&s.model, q)?));
        }
        out.push(by_model[s.model.as_str()].clone());
    }
    Ok(out)
}

/// A search. `rerank`, when given, reorders each corpus's top candidates
/// after fusion and the priors (crate::config's `[reranker]`).
pub fn search(
    pg: &Pg,
    o: &SearchOptions,
    embed: Option<QueryEmbed>,
    lexical: Lexical,
    rerank: Option<&super::rerank::Client>,
) -> Result<SearchResult> {
    if o.kind == "file" || FILE_KINDS.contains(&o.kind.as_str()) {
        return search_files(pg, o, embed, lexical, rerank);
    }
    search_entities(pg, o, embed, lexical, rerank)
}

/// A grouped search (docs/INDEX-V2.md, "Corpora, composed"): one answer per
/// corpus asked for (`o.corpora`; empty: every one), in [`CORPORA`] order,
/// each with its own limit and share of the budget (`[corpora.NAME]`), and
/// a one-line note for a corpus with nothing.
///
/// Two fused statements: the crawled corpora (code and prose) as one flat
/// search, whose first `o.limit` are `hits` exactly as a flat search gives
/// them and whose order the code and prose groups take; and the side
/// corpora (memory, friction) as another, each reranked on its own.
pub fn search_grouped(
    pg: &Pg,
    o: &SearchOptions,
    embed: Option<QueryEmbed>,
    lexical: Lexical,
    rerank: Option<&super::rerank::Client>,
) -> Result<SearchResult> {
    let cfg = &pg.cfg;
    let asked: Vec<String> = if o.corpora.is_empty() {
        CORPORA.iter().map(|s| s.to_string()).collect()
    } else {
        o.corpora.clone()
    };
    for c in &asked {
        if !CORPORA.contains(&c.as_str()) {
            bail!("--corpus takes {}, not {c:?}", CORPORA.join(", "));
        }
    }
    let limit = if o.limit == 0 {
        cfg.search.limit
    } else {
        o.limit
    };
    let crawled: Vec<String> = asked
        .iter()
        .filter(|c| CRAWLED.contains(&c.as_str()))
        .cloned()
        .collect();
    let side: Vec<String> = asked
        .iter()
        .filter(|c| !CRAWLED.contains(&c.as_str()))
        .cloned()
        .collect();
    let group_limit = |cs: &[String]| cs.iter().map(|c| cfg.corpus_group(c).0).sum::<usize>();
    let mut out: Option<SearchResult> = None;
    let mut by_corpus: BTreeMap<String, (Vec<Hit>, usize)> = BTreeMap::new();
    if !crawled.is_empty() {
        // Deep enough for both groups and the flat list; the order is the
        // flat search's, so its first `limit` are what a flat search gives.
        let o2 = SearchOptions {
            corpora: crawled.clone(),
            limit: limit.max(2 * group_limit(&crawled)),
            ..o.clone()
        };
        let mut r = search_entities(pg, &o2, embed, lexical, rerank)?;
        for h in &r.hits {
            by_corpus
                .entry(h.corpus.clone())
                .or_default()
                .0
                .push(h.clone());
        }
        r.hits.truncate(limit);
        out = Some(r);
    }
    if !side.is_empty() {
        let o2 = SearchOptions {
            corpora: side.clone(),
            limit: 2 * group_limit(&side),
            ..o.clone()
        };
        let r = search_entities(pg, &o2, embed, lexical, rerank)?;
        for h in &r.hits {
            by_corpus
                .entry(h.corpus.clone())
                .or_default()
                .0
                .push(h.clone());
        }
        match &mut out {
            Some(main) => {
                for (k, v) in r.ms {
                    *main
                        .ms
                        .entry(format!("{k} (memory, friction)"))
                        .or_insert(0.0) += v;
                }
                main.absent.retain(|t| r.absent.contains(t));
                for m in r.mapping {
                    main.mapping.push(m);
                }
            }
            None => {
                let mut r = r;
                r.hits.clear();
                out = Some(r);
            }
        }
    }
    let mut r = out.expect("a corpus at least");
    for (hits, n) in by_corpus.values_mut() {
        *n = hits.len();
    }
    let mut groups = Vec::new();
    for c in &asked {
        let (lim, budget) = cfg.corpus_group(c);
        let (mut hits, n) = by_corpus.remove(c).unwrap_or_default();
        hits.truncate(lim);
        for (i, h) in hits.iter_mut().enumerate() {
            h.rank = i + 1;
        }
        let note = hits
            .is_empty()
            .then(|| format!("nothing good here: no {c} matched"));
        groups.push(crate::query::Group {
            corpus: c.clone(),
            hits,
            candidates: n,
            budget,
            note,
        });
    }
    r.groups = Some(groups);
    Ok(r)
}

/// Details of the hits, by id.
struct Detail {
    e: model::Entity,
    seed: String,
    summary: String,
    why: String,
    writer: String,
    summary_key: String,
    labels_text: String,
    symbols: String,
    corpus: String,
}

impl Detail {
    fn shown(&self) -> Option<Summary> {
        if self.writer.is_empty() {
            return None;
        }
        Some(Summary {
            text: if self.summary.is_empty() {
                self.seed.clone()
            } else {
                self.summary.clone()
            },
            why: self.why.clone(),
            writer: self.writer.clone(),
            content_key: self.summary_key.clone(),
            written_at: String::new(),
        })
    }
}

const DETAIL_COLS: &str = "id, kind, parent, project, rel, name, path, file_kind, lang, size, lines, line, \
     content_key, title, signature, untracked, no_content, importance, seed, summary, why, summary_writer, \
     summary_key, labels_text, symbols, corpus";

fn details(pg: &Pg, ids: &[String]) -> Result<HashMap<String, Detail>> {
    let rows = pg.query(
        &format!("SELECT {DETAIL_COLS} FROM entities WHERE id = ANY($1)"),
        &[&ids],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let e = super::view::entity_of(&r);
            (
                e.id.clone(),
                Detail {
                    e,
                    seed: r.get(18),
                    summary: r.get(19),
                    why: r.get(20),
                    writer: r.get(21),
                    summary_key: r.get(22),
                    labels_text: r.get(23),
                    symbols: r.get(24),
                    corpus: r.get(25),
                },
            )
        })
        .collect())
}

/// v1's `--kind file`: every kind searched, a symbol or section counting for
/// its file, one row per file.
fn search_files(
    pg: &Pg,
    o: &SearchOptions,
    embed: Option<QueryEmbed>,
    lexical: Lexical,
    rerank: Option<&super::rerank::Client>,
) -> Result<SearchResult> {
    let limit = if o.limit == 0 {
        pg.cfg.search.limit
    } else {
        o.limit
    };
    let inner = SearchOptions {
        kind: String::new(),
        limit: limit * pg.cfg.search.files_depth_factor,
        ..o.clone()
    };
    let mut r = search_entities(pg, &inner, embed, lexical, rerank)?;
    let t0 = Instant::now();
    let capped = pg.cfg.cross.top_cap > 0 && r.cross.as_ref().is_some_and(|c| c.spans.len() >= 2);
    let file_ids: Vec<String> = r
        .hits
        .iter()
        .map(|h| model::file_of(&h.id).to_string())
        .collect();
    let det = details(pg, &file_ids)?;
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
        let Some(d) = det.get(&file_id) else { continue };
        let e = &d.e;
        if o.kind != "file" && e.file_kind != o.kind {
            continue;
        }
        seen.insert(file_id.clone());
        let hit = if h.kind == "file" {
            h
        } else {
            let summ = d.shown();
            let mut explain = h.explain.clone();
            explain.insert(0, format!("by {}", h.id));
            let mut matched = h.matched.clone();
            matched.insert(0, format!("{} {}", h.kind, h.title));
            Hit {
                rank: 0,
                id: file_id.clone(),
                corpus: d.corpus.clone(),
                kind: e.kind.clone(),
                file_kind: e.file_kind.clone(),
                project: e.project.clone(),
                freshness: model::freshness(&e.content_key, summ.as_ref()).to_string(),
                path: e.path.clone(),
                line: h.line,
                title: e.display_title().to_string(),
                summary: summ.as_ref().map(|s| s.text.clone()).unwrap_or_default(),
                why: summ.map(|s| s.why).unwrap_or_default(),
                untracked: e.untracked,
                matched,
                score: h.score,
                explain,
                stale: String::new(),
            }
        };
        hits.push(Hit {
            rank: hits.len() + 1,
            ..hit
        });
        if hits.len() >= limit && !capped {
            break;
        }
    }
    // [cross] top_cap: one project's share of the first `limit`.
    if capped {
        let projects: Vec<String> = hits.iter().map(|h| h.project.clone()).collect();
        let spans = &r.cross.as_ref().expect("capped").spans;
        let new = super::cross::cap_top(&projects, spans, limit, pg.cfg.cross.top_cap);
        let mut old: Vec<Option<Hit>> = hits.into_iter().map(Some).collect();
        hits = new
            .into_iter()
            .filter_map(|i| old[i].take())
            .take(limit)
            .enumerate()
            .map(|(i, h)| {
                let mut h = h;
                h.rank = i + 1;
                h
            })
            .collect();
    }
    r.hits = hits;
    *r.ms.entry("files".into()).or_insert(0.0) += round1(t0.elapsed().as_secs_f64() * 1000.0);
    Ok(r)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// A query's content words: its terms less stop words (all of them when
/// nothing else is left).
fn content_of(terms: &[String]) -> Vec<String> {
    let c: Vec<String> = terms
        .iter()
        .filter(|t| !names::is_stop(t))
        .cloned()
        .collect();
    if c.is_empty() { terms.to_vec() } else { c }
}

/// What one fused statement is asked: the query as its lists read it.
struct Ask<'a> {
    content: &'a [String],
    tri: &'a [String],
    exact_name: &'a str,
    ident: bool,
    partial: &'a [String],
    pinned: &'a [String],
    check: &'a [String],
    /// The query's vector for each of the search's dense lists (none: no
    /// dense list).
    qvecs: &'a [String],
}

impl<'a> Ask<'a> {
    /// A sub-query's ask: the lists only, no pins, labels or absent terms.
    fn lists_only(content: &'a [String], tri: &'a [String], qvecs: &'a [String]) -> Ask<'a> {
        Ask {
            content,
            tri,
            exact_name: "",
            ident: false,
            partial: &[],
            pinned: &[],
            check: &[],
            qvecs,
        }
    }
}

/// The fused statement's answer: the candidates, the pins it found and the
/// terms nothing mentions.
struct Fused {
    cands: HashMap<String, Cand>,
    exact: Vec<String>,
    rare_here: Vec<String>,
    rare_all: Vec<String>,
    absent: Vec<String>,
}

/// Runs the fused statement for `o`'s filters and `a`'s query.
fn run_fused(
    pg: &Pg,
    o: &SearchOptions,
    a: &Ask,
    lexical: Lexical,
    local: bool,
    near_names: &[String],
    ctx: &Context,
) -> Result<Fused> {
    let cfg = &pg.cfg;
    let filtered = !(o.kind.is_empty() && o.project.is_empty() && o.under.is_empty());
    let push = if lexical != Lexical::Fts && cfg.bm25.pushdown && super::schema::bm25_fields(pg)? {
        Some(Push {
            kind: !o.kind.is_empty(),
            project: !o.project.is_empty(),
            under: !o.under.is_empty(),
            under_projects: projects_under(&o.under, &ctx.projects),
        })
    } else {
        None
    };
    let dense: &[DenseSpec] = if !a.qvecs.is_empty() && a.qvecs.len() == ctx.dense.len() {
        &ctx.dense
    } else {
        &[]
    };
    let sql = fused_sql(
        a.content.len() > 1,
        local,
        dense,
        lexical,
        filtered,
        push.as_ref(),
        cfg,
    )?;
    let lim = cfg.search.list_depth;
    let corpora = corpora_of(o);
    let near_names = near_names.to_vec();
    let (content, tri, partial, pinned, check) = (
        a.content.to_vec(),
        a.tri.to_vec(),
        a.partial.to_vec(),
        a.pinned.to_vec(),
        a.check.to_vec(),
    );
    let exact_name = a.exact_name.to_string();
    let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
        &content,
        &o.kind,
        &o.project,
        &o.under,
        &near_names,
        &tri,
        &exact_name,
        &a.ident,
        &partial,
        &pinned,
        &check,
        &lim,
        &corpora,
    ];
    if !dense.is_empty() {
        for v in a.qvecs {
            params.push(v);
        }
    }
    // AGENT_INDEX_EXPLAIN=1 prints the fused statement's plan, run, on
    // stderr (for tuning; the search then runs as usual).
    if std::env::var_os("AGENT_INDEX_EXPLAIN").is_some_and(|v| !v.is_empty() && v != "0") {
        for r in pg.query(&format!("EXPLAIN (ANALYZE, BUFFERS) {sql}"), &params)? {
            eprintln!("{}", r.get::<_, String>(0));
        }
    }
    let row = pg
        .query(&sql, &params)?
        .into_iter()
        .next()
        .expect("one row");
    let cands_v: Value = row.get(0);
    let mut cands: HashMap<String, Cand> = HashMap::new();
    for c in cands_v.as_array().into_iter().flatten() {
        let a = c.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let s = |i: usize| a.get(i).and_then(Value::as_str).unwrap_or("").to_string();
        let lists = a
            .get(7)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let l = l.as_array()?;
                Some((
                    l.first()?.as_str()?.to_string(),
                    l.get(1)?.as_i64()?,
                    l.get(2).and_then(Value::as_f64).unwrap_or(0.0),
                ))
            })
            .collect();
        cands.insert(
            s(0),
            Cand {
                corpus: s(8),
                kind: s(1),
                project: s(3),
                rel: s(4),
                importance: a.get(5).and_then(Value::as_f64).unwrap_or(0.0),
                rrf: a.get(6).and_then(Value::as_f64).unwrap_or(0.0),
                lists,
            },
        );
    }
    Ok(Fused {
        cands,
        exact: serde_json::from_value(row.get(1))?,
        rare_here: serde_json::from_value(row.get(2))?,
        rare_all: serde_json::from_value(row.get(3))?,
        absent: serde_json::from_value(row.get(4))?,
    })
}

/// A sub-query's candidates pooled into `cands` (`[cross]`): its best
/// `take` by fusion score; each scores the better of its two fusion scores,
/// its lists named after `label` (`in alpha: bm25`). `origin` keeps,
/// for each candidate, the sub-query that scored it best and that score.
fn pool_sub(
    cands: &mut HashMap<String, Cand>,
    sub: Fused,
    take: usize,
    label: &str,
    key: &str,
    origin: &mut HashMap<String, (String, f64)>,
) -> Vec<String> {
    let mut ranked: Vec<(String, Cand)> = sub
        .cands
        .into_iter()
        .filter(|(_, c)| !c.lists.is_empty())
        .collect();
    ranked.sort_by(|a, b| b.1.rrf.total_cmp(&a.1.rrf).then_with(|| a.0.cmp(&b.0)));
    // Its best `take` files' candidates, in its order.
    let mut files: HashSet<String> = HashSet::new();
    let ranked: Vec<(String, Cand)> = ranked
        .into_iter()
        .take_while(|(id, _)| {
            files.insert(model::file_of(id).to_string());
            files.len() <= take
        })
        .collect();
    let own: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
    for (id, mut c) in ranked {
        let lists: Vec<(String, i64, f64)> = c
            .lists
            .iter()
            .map(|(n, r, s)| (format!("{label}: {n}"), *r, *s))
            .collect();
        let better = origin.get(&id).is_none_or(|(_, s)| c.rrf > *s);
        if better {
            origin.insert(id.clone(), (key.to_string(), c.rrf));
        }
        match cands.get_mut(&id) {
            Some(e) => {
                e.rrf = e.rrf.max(c.rrf);
                e.lists.extend(lists);
            }
            None => {
                c.lists = lists;
                cands.insert(id, c);
            }
        }
    }
    own
}

fn search_entities(
    pg: &Pg,
    o: &SearchOptions,
    embed: Option<QueryEmbed>,
    lexical: Lexical,
    rerank: Option<&super::rerank::Client>,
) -> Result<SearchResult> {
    let terms = names::query_terms(&o.query);
    if terms.is_empty() {
        bail!(
            "nothing to search for in {:?}: give words, a name or a path",
            o.query
        );
    }
    let cfg = &pg.cfg;
    let pri = &cfg.priors;
    let x = &cfg.cross;
    let limit = if o.limit == 0 {
        cfg.search.limit
    } else {
        o.limit
    };
    let content = content_of(&terms);
    let mut ms: BTreeMap<String, f64> = BTreeMap::new();
    let mut t0 = Instant::now();
    let mut lap = |ms: &mut BTreeMap<String, f64>, name: &str| {
        *ms.entry(name.to_string()).or_insert(0.0) += round1(t0.elapsed().as_secs_f64() * 1000.0);
        t0 = Instant::now();
    };
    let mut ctx = context(pg, &o.caller_project)?;
    let corpora = corpora_of(o);
    ctx.dense = if embed.is_some() {
        dense_specs(pg, &corpora, ctx.vectors)?
    } else {
        Vec::new()
    };
    // [cross]: a search over the whole tree, of the crawled corpora, may
    // span projects; the caller's parts of a query (`o.queries`) are
    // searched each on their own.
    let spans_tree = o.project.is_empty()
        && o.under.is_empty()
        && o.scope.is_empty()
        && corpora.iter().all(|c| CRAWLED.contains(&c.as_str()));
    let parts: Vec<String> = if o.queries.len() >= 2 && spans_tree {
        o.queries.clone()
    } else {
        Vec::new()
    };
    let crossing = spans_tree && (x.any() || !parts.is_empty());
    let mut info = CrossInfo::default();
    let mut near = ctx.near.clone();
    let full_names: Vec<String> = {
        let mut v: Vec<String> = ctx.full.iter().cloned().collect();
        v.sort();
        v
    };
    let named = if crossing {
        super::cross::named_projects(&o.query, &full_names, &o.caller_project)
    } else {
        Vec::new()
    };
    // Why the query reads as spanning projects, known before any list: it
    // comes in parts, compares, or names another project (with routing on,
    // a name counts once the route agrees).
    if crossing {
        info.reason = if !parts.is_empty() {
            Some("queries".into())
        } else if super::cross::compares(&terms) {
            Some("compares".into())
        } else if !named.is_empty() && !x.route {
            Some(format!("names {}", named.join(", ")))
        } else {
            None
        };
    }
    let soften = |near: &mut HashMap<String, f64>| {
        for f in near.values_mut() {
            *f = f.min(x.own_project);
        }
    };
    let can_soften = crossing && x.own_project > 0.0 && !o.caller_project.is_empty();
    if can_soften && info.reason.is_some() {
        soften(&mut near);
        info.softened = true;
    }
    let mut local = cfg.search.local_lists
        && !near.is_empty()
        && o.project.is_empty()
        && o.scope.is_empty()
        && !info.softened;
    let mut near_names: Vec<String> = near.keys().cloned().collect();
    near_names.sort();
    lap(&mut ms, "context");
    let mapping = label_matches(pg, &o.query, o)?;
    lap(&mut ms, "labels");
    super::vecs::take_embed_on();
    let qvecs = query_vecs(embed, &ctx.dense, &o.query)?;
    // Where the query's vector was made (the embedder's server, or the CPU
    // when it didn't answer): the search says so.
    let embedded_on = super::vecs::take_embed_on();
    // [cross] routing compares with bge's card vectors: the query's, when
    // bge serves the cards.
    let qvec: Option<String> = ctx
        .dense
        .iter()
        .position(|d| d.table.is_none())
        .map(|i| qvecs[i].clone());
    lap(&mut ms, "embed query");
    let pinned: Vec<String> = mapping
        .iter()
        .filter(|m| m.exact)
        .map(|m| m.entity.clone())
        .collect();
    let partial: Vec<String> = {
        let mut seen = HashSet::new();
        mapping
            .iter()
            .filter(|m| !m.exact)
            .map(|m| m.entity.clone())
            .filter(|e| seen.insert(e.clone()))
            .collect()
    };
    // A term a label match covers isn't checked for absence (v1).
    let check: Vec<String> = content
        .iter()
        .filter(|t| {
            !mapping.iter().any(|m| {
                m.phrase.split(' ').any(|w| w == t.as_str()) || m.phrase.replace(' ', "") == **t
            })
        })
        .cloned()
        .collect();
    let tri: Vec<String> = content
        .iter()
        .filter(|t| t.chars().count() >= 3)
        .cloned()
        .collect();
    let q = o.query.trim();
    let exact_name = if q.is_empty() || q.chars().any(char::is_whitespace) {
        ""
    } else {
        q
    };
    let ident = is_identifier(&o.query);
    if lexical != Lexical::Fts {
        // A schema migrated before the database had pg_search gets its
        // BM25 indexes now, or the search says what's missing.
        super::schema::ensure_bm25(pg)?;
    }
    let ask = Ask {
        content: &content,
        tri: &tri,
        exact_name,
        ident,
        partial: &partial,
        pinned: &pinned,
        check: &check,
        qvecs: &qvecs,
    };
    let mut fused = run_fused(pg, o, &ask, lexical, local, &near_names, &ctx)?;
    lap(&mut ms, "fused statement");

    // [cross] routing: the projects the query is about.
    let mut spans: Vec<String> = Vec::new();
    let mass = |cands: &HashMap<String, Cand>| -> Vec<(String, f64)> {
        let mut m: Vec<(&String, &Cand)> =
            cands.iter().filter(|(_, c)| !c.lists.is_empty()).collect();
        m.sort_by(|a, b| b.1.rrf.total_cmp(&a.1.rrf).then_with(|| a.0.cmp(b.0)));
        m.into_iter()
            .map(|(_, c)| (c.project.clone(), c.rrf))
            .collect()
    };
    if crossing && x.route {
        let scores = super::cross::route(
            pg,
            &content,
            qvec.as_ref(),
            &mass(&fused.cands),
            &named,
            &ctx.full,
            lexical,
        )?;
        let routed = super::cross::routed(&scores, x.route_projects, x.route_ratio);
        let top: Vec<&String> = scores.iter().take(x.route_projects).map(|s| &s.0).collect();
        let confirmed: Vec<String> = named.iter().filter(|n| top.contains(n)).cloned().collect();
        if info.reason.is_none() {
            if !confirmed.is_empty() {
                info.reason = Some(format!("names {}", confirmed.join(", ")));
            } else if routed.len() >= 2 && (o.caller_project.is_empty() || x.route_caller) {
                // From inside a project the route alone doesn't say the
                // query spans projects (unless `route_caller`): almost any
                // query has a second project with some mass.
                info.reason = Some("routed".into());
            }
        }
        if parts.is_empty() && info.reason.is_some() {
            // The routed projects, the confirmed names, and for a
            // comparison routed to one project, the next one too.
            spans = routed.clone();
            for n in &confirmed {
                if !spans.contains(n) {
                    spans.push(n.clone());
                }
            }
            if spans.len() < 2
                && let Some(next) = scores.iter().map(|s| &s.0).find(|p| !spans.contains(p))
            {
                spans.push(next.clone());
            }
        }
        info.routed = scores
            .iter()
            .take(8)
            .map(|(p, s)| (p.clone(), (s * 1000.0).round() / 1000.0))
            .collect();
        lap(&mut ms, "route");
    } else if crossing && parts.is_empty() && info.reason.is_some() && !named.is_empty() {
        spans = named.clone();
    }
    // Softened late (the route said so): the lists scoped to the caller's
    // project ran, so the statement runs again without them.
    if can_soften && !info.softened && info.reason.is_some() {
        soften(&mut near);
        info.softened = true;
        if local {
            local = false;
            near_names = near.keys().cloned().collect();
            near_names.sort();
            fused = run_fused(pg, o, &ask, lexical, local, &near_names, &ctx)?;
            lap(&mut ms, "fused statement");
        }
    }
    let _ = local;
    let Fused {
        mut cands,
        exact,
        rare_here,
        rare_all,
        absent,
    } = fused;
    // [cross] sub-queries: each spanned project searched on its own, and
    // each of the caller's parts; pooled into the candidates.
    let mut origin: HashMap<String, (String, f64)> = HashMap::new();
    // Each key's own ranking (a spanned project's sub-query, a part's), for
    // its places in the pool.
    let mut own: HashMap<String, Vec<String>> = HashMap::new();
    if crossing && x.split && spans.len() >= 2 {
        for p in &spans {
            let sub_o = SearchOptions {
                project: p.clone(),
                ..o.clone()
            };
            // The whole ask (labels and pins too), inside the project.
            let mut sub = run_fused(pg, &sub_o, &ask, lexical, false, &[], &ctx)?;
            for c in sub.cands.values_mut() {
                c.rrf *= x.split_raise;
            }
            let ids = pool_sub(
                &mut cands,
                sub,
                x.split_depth,
                &format!("in {p}"),
                p,
                &mut origin,
            );
            own.insert(p.clone(), ids);
            info.sub.push(format!("project:{p}"));
        }
        lap(&mut ms, "split");
    }
    let mut part_text: HashMap<String, String> = HashMap::new();
    for (i, part) in parts.iter().enumerate() {
        let pt = names::query_terms(part);
        if pt.is_empty() {
            continue;
        }
        let pc = content_of(&pt);
        let ptri: Vec<String> = pc
            .iter()
            .filter(|t| t.chars().count() >= 3)
            .cloned()
            .collect();
        let pv = query_vecs(embed, &ctx.dense, part)?;
        let key = format!("query:{}", i + 1);
        let a = Ask::lists_only(&pc, &ptri, &pv);
        let sub = run_fused(pg, o, &a, lexical, local, &near_names, &ctx)?;
        let ids = pool_sub(
            &mut cands,
            sub,
            cfg.search.list_depth as usize,
            &format!("query {}", i + 1),
            &key,
            &mut origin,
        );
        own.insert(key.clone(), ids);
        part_text.insert(key.clone(), part.clone());
        info.sub.push(key);
    }
    if !parts.is_empty() {
        lap(&mut ms, "parts");
    }
    info.spans = spans.clone();
    // The rare identifier's files: nearby ones first (v1).
    let rare: Vec<String> = if exact.is_empty() && ident {
        let mut f = if local { rare_here.clone() } else { Vec::new() };
        let most = cfg.search.rare_files;
        if f.is_empty() || f.len() > most {
            f = rare_all.clone();
        }
        if f.len() <= most { f } else { Vec::new() }
    } else {
        Vec::new()
    };

    // Fusion's scores, and what each list said.
    let mut score: HashMap<String, f64> = HashMap::new();
    let mut explain: HashMap<String, Vec<String>> = HashMap::new();
    for (id, c) in &cands {
        if c.lists.is_empty() {
            continue;
        }
        score.insert(id.clone(), c.rrf);
        let ex = explain.entry(id.clone()).or_default();
        for (name, r, s) in &c.lists {
            if name.starts_with("semantic") {
                ex.push(format!("{name} #{r} ({s:.3})"));
            } else {
                ex.push(format!("{name} #{r}"));
            }
        }
    }
    // v1's pins and tiers.
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
        *score.entry(id.clone()).or_insert(0.0) += pri.pin_rare - i as f64 * 0.001;
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
        *score.entry(id.clone()).or_insert(0.0) += pri.pin_exact - i as f64 * 0.001;
        explain
            .entry(id.clone())
            .or_default()
            .insert(0, format!("named \"{}\"", o.query.trim()));
    }
    for (i, id) in pinned.iter().enumerate() {
        if !pin_counts(id) {
            continue;
        }
        *score.entry(id.clone()).or_insert(0.0) += pri.pin_label - i as f64 * 0.001;
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

    // v1's priors.
    let under_parts: Vec<String> = if o.under.is_empty() {
        Vec::new()
    } else {
        ctx.projects
            .iter()
            .filter(|(_, p)| o.under == *p || o.under.starts_with(&format!("{p}/")))
            .max_by_key(|(_, p)| p.len())
            .map(|(_, p)| {
                o.under[p.len()..]
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
    let ids: Vec<String> = score.keys().cloned().collect();
    for id in ids {
        let Some(e) = cands.get(&id) else {
            score.remove(&id);
            continue;
        };
        let mut s = score[&id];
        let ex = explain.entry(id.clone()).or_default();
        if o.kind.is_empty() && e.kind == "section" {
            s += pri.section;
            ex.push("section −".into());
        }
        if e.importance > 0.0 && ctx.max_imp > 0.0 {
            s += pri.importance * (1.0 + e.importance).ln() / (1.0 + ctx.max_imp).ln();
            ex.push(format!("importance {:.1} +", e.importance));
        }
        let out_of_way = e.rel.split('/').any(files::is_out_of_the_way);
        if out_of_way && !under_targets {
            s += pri.out_of_the_way;
            ex.push("kept-out-of-the-way dir −".into());
        }
        if !e.rel.is_empty() && !asks_tests {
            if e.kind != "dir" && files::is_fixture(&e.rel) {
                s += pri.fixture;
                ex.push("fixture −".into());
            } else if files::is_test(&e.rel) || files::is_fixture(&format!("{}/x", e.rel)) {
                s += pri.test;
                ex.push("test −".into());
            }
        }
        let boost = cfg.corpus_boost(&e.corpus);
        if boost != 1.0 && s > 0.0 {
            s *= boost;
            ex.push(format!("{} corpus ×{boost}", e.corpus));
        }
        if let Some(f) = near.get(e.project.as_str()) {
            if s > 0.0 {
                s *= f;
            }
            ex.push(if e.project == o.caller_project {
                format!("own project ×{f}")
            } else {
                format!("a project yours depends on ×{f}")
            });
        }
        score.insert(id, s);
    }
    let by_score = |score: &HashMap<String, f64>| {
        let mut order: Vec<(String, f64)> = score.iter().map(|(k, v)| (k.clone(), *v)).collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        order
    };
    let pool_depth = if x.pool_depth > 0 {
        x.pool_depth
    } else {
        cfg.reranker.flat_depth
    };
    lap(&mut ms, "fuse and priors");
    // [cross] touched: files sessions touched along with the top files.
    if crossing && x.touched && o.kind.is_empty() {
        let order = by_score(&score);
        let mut seeds: Vec<String> = Vec::new();
        for (id, _) in &order {
            let f = model::file_of(id).to_string();
            if !seeds.contains(&f) {
                seeds.push(f);
            }
            if seeds.len() >= x.touched_seeds {
                break;
            }
        }
        let nbrs = super::cross::touched(pg, &seeds)?;
        let cutoff = order
            .get(pool_depth.saturating_sub(1))
            .or(order.last())
            .map(|s| s.1.max(0.0))
            .unwrap_or(0.0);
        let strongest = nbrs.first().map(|n| n.strength).unwrap_or(0.0);
        let missing: Vec<String> = nbrs
            .iter()
            .filter(|n| !cands.contains_key(&n.id))
            .map(|n| n.id.clone())
            .collect();
        if !missing.is_empty() {
            for r in pg.query(
                "SELECT id, kind, project, rel, importance, corpus FROM entities \
                 WHERE id = ANY($1) AND corpus = ANY($2)",
                &[&missing, &corpora],
            )? {
                cands.insert(
                    r.get(0),
                    Cand {
                        kind: r.get(1),
                        project: r.get(2),
                        rel: r.get(3),
                        importance: r.get(4),
                        corpus: r.get(5),
                        rrf: 0.0,
                        lists: Vec::new(),
                    },
                );
            }
        }
        for n in &nbrs {
            if !cands.contains_key(&n.id) || strongest <= 0.0 {
                continue;
            }
            let bonus = x.touched_weight * n.strength / strongest * cutoff;
            *score.entry(n.id.clone()).or_insert(0.0) += bonus;
            explain.entry(n.id.clone()).or_default().push(format!(
                "touched in {} session{} with {} +{bonus:.4}",
                n.sessions,
                if n.sessions == 1 { "" } else { "s" },
                n.via
            ));
            info.touched += 1;
        }
        lap(&mut ms, "touched");
    }
    let mut order = by_score(&score);
    // Each key's own candidates by their scores after the priors (a
    // project's tests and fixtures down, as in its own search).
    for ids in own.values_mut() {
        ids.sort_by(|a, b| {
            let (sa, sb) = (score.get(a).copied(), score.get(b).copied());
            sb.unwrap_or(f64::MIN)
                .total_cmp(&sa.unwrap_or(f64::MIN))
                .then_with(|| a.cmp(b))
        });
    }
    // [cross] slots: each of the query's projects (or parts) gets places in
    // the reranker's pool, and none takes more than its cap.
    let mut flat_pool = cfg.reranker.flat_depth;
    let keys: Vec<String> = if rerank.is_none() {
        // No reranker, no pool.
        Vec::new()
    } else if !parts.is_empty() {
        info.sub.clone()
    } else if crossing && x.slots && spans.len() >= 2 {
        spans.clone()
    } else if crossing && x.slots && !x.route {
        // No route: the projects with the most fused mass.
        let mut m: Vec<(String, f64)> = Vec::new();
        for (id, s) in &order {
            let Some(c) = cands.get(id) else { continue };
            match m.iter_mut().find(|(p, _)| *p == c.project) {
                Some(e) => e.1 += s.max(0.0),
                None => m.push((c.project.clone(), s.max(0.0))),
            }
            if m.len() > 4 * x.slot_projects.max(1) {
                break;
            }
        }
        m.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        super::cross::routed(&m, x.slot_projects, x.route_ratio)
    } else {
        Vec::new()
    };
    if keys.len() >= 2 {
        let cap = x
            .slot_min
            .max((x.slot_cap * pool_depth as f64).ceil() as usize);
        let key_of = |id: &str| -> String {
            if parts.is_empty() {
                cands.get(id).map(|c| c.project.clone()).unwrap_or_default()
            } else {
                origin.get(id).map(|o| o.0.clone()).unwrap_or_default()
            }
        };
        let file_of = |id: &str| model::file_of(id).to_string();
        let chosen = super::cross::pool(
            &order,
            &keys,
            &own,
            super::cross::Slots {
                depth: pool_depth,
                min: x.slot_min,
                cap,
                per_file: cfg.search.per_file,
            },
            &key_of,
            &file_of,
        );
        let mut counts: Vec<(String, usize)> = Vec::new();
        for &i in &chosen {
            let k = key_of(&order[i].0);
            match counts.iter_mut().find(|(p, _)| *p == k) {
                Some(e) => e.1 += 1,
                None => counts.push((k, 1)),
            }
        }
        info.pool = counts;
        order = super::cross::pool_first(&order, &chosen);
        flat_pool = pool_depth;
        lap(&mut ms, "slots");
    }
    // Each part's candidates are reranked against that part.
    let query_of: HashMap<String, String> = origin
        .iter()
        .filter_map(|(id, (k, _))| part_text.get(k).map(|t| (id.clone(), t.clone())))
        .collect();
    // The reranker, on each corpus's top candidates.
    let rerank_info = rerank.map(|rc| {
        let pins: HashSet<&String> = if cfg.reranker.keep_pins {
            exact
                .iter()
                .chain(pinned.iter())
                .chain(rare.iter())
                .filter(|id| pin_counts(id))
                .collect()
        } else {
            HashSet::new()
        };
        let info = apply_rerank(
            pg,
            rc,
            &o.query,
            &query_of,
            &content,
            &mut order,
            &cands,
            &pins,
            &mut explain,
            // The crawled corpora rerank as one pool (a flat answer); the
            // side corpora each their own top.
            corpora
                .iter()
                .all(|c| CRAWLED.contains(&c.as_str()))
                .then_some(flat_pool),
        );
        lap(&mut ms, "rerank");
        info
    });
    // At most two results per file.
    let mut per_file: HashMap<String, usize> = HashMap::new();
    let mut chosen: Vec<(String, f64)> = Vec::new();
    for (id, s) in order {
        let n = per_file.entry(model::file_of(&id).to_string()).or_insert(0);
        if *n >= cfg.search.per_file {
            continue;
        }
        *n += 1;
        chosen.push((id, s));
        if chosen.len() >= limit {
            break;
        }
    }
    // (Added to the stage's time before the reranker.)
    lap(&mut ms, "fuse and priors");
    let ids: Vec<String> = chosen.iter().map(|(i, _)| i.clone()).collect();
    let det = details(pg, &ids)?;
    let mut hits = Vec::new();
    for (id, s) in chosen {
        let Some(d) = det.get(&id) else { continue };
        let e = &d.e;
        let summ = d.shown();
        let mut matched = matched_fields(d, &content);
        for m in mapping.iter().filter(|m| m.entity == id) {
            matched.push(format!("label \"{}\"", m.label));
        }
        hits.push(Hit {
            rank: hits.len() + 1,
            id: id.clone(),
            corpus: d.corpus.clone(),
            kind: e.kind.clone(),
            file_kind: e.file_kind.clone(),
            project: e.project.clone(),
            freshness: model::freshness(&e.content_key, summ.as_ref()).to_string(),
            path: e.path.clone(),
            line: e.line,
            title: e.display_title().to_string(),
            summary: summ.as_ref().map(|s| s.text.clone()).unwrap_or_default(),
            why: summ.map(|s| s.why).unwrap_or_default(),
            untracked: e.untracked,
            matched,
            score: (s * 10000.0).round() / 10000.0,
            explain: explain.remove(&id).unwrap_or_default(),
            stale: String::new(),
        });
    }
    lap(&mut ms, "results");
    Ok(SearchResult {
        query: o.query.clone(),
        terms,
        hits,
        mapping,
        absent,
        candidates,
        ms,
        config: Some(cfg.fingerprint()),
        rerank: rerank_info,
        groups: None,
        lexical: None,
        cross: crossing.then_some(info),
        embedder: embedded_on,
    })
}

/// Reorders each corpus's top `[reranker].depth` candidates by the
/// reranker's scores, in the places they held (another corpus's candidates
/// keep theirs); pins, when kept, keep their places and aren't sent. On a
/// timeout or an error nothing moves, and the report says why.
#[allow(clippy::too_many_arguments)]
fn apply_rerank(
    pg: &Pg,
    rc: &super::rerank::Client,
    query: &str,
    query_of: &HashMap<String, String>,
    terms: &[String],
    order: &mut [(String, f64)],
    cands: &HashMap<String, Cand>,
    pins: &HashSet<&String>,
    explain: &mut HashMap<String, Vec<String>>,
    pool: Option<usize>,
) -> RerankInfo {
    let cfg = &pg.cfg.reranker;
    let mut info = RerankInfo {
        model: cfg.model.clone(),
        ..Default::default()
    };
    // Each corpus's top places (indexes into order), pins left out; one
    // pool of `pool` across corpora for a flat search.
    let mut by_corpus: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, (id, _)) in order.iter().enumerate() {
        let corpus = match pool {
            Some(_) => "flat",
            None => cands.get(id).map(|c| c.corpus.as_str()).unwrap_or("code"),
        };
        let depth = pool.unwrap_or_else(|| cfg.depth.get(corpus).copied().unwrap_or(0));
        let n = seen.entry(corpus.to_string()).or_insert(0);
        if *n >= depth {
            continue;
        }
        *n += 1;
        if !pins.contains(id) {
            by_corpus.entry(corpus.to_string()).or_default().push(i);
        }
    }
    info.depth = match pool {
        Some(d) => [("flat".to_string(), d)].into(),
        None => cfg.depth.clone(),
    };
    let t0 = Instant::now();
    // Each slot scored against its own query (a caller's part, else the
    // whole query): one call per query.
    let result = (|| -> Result<Vec<(Vec<usize>, Vec<f64>)>> {
        let mut out = Vec::new();
        for slots in by_corpus.values() {
            let mut by_query: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
            for &i in slots {
                let q = query_of.get(&order[i].0).map_or(query, String::as_str);
                by_query.entry(q).or_default().push(i);
            }
            let mut scores: HashMap<usize, f64> = HashMap::new();
            for (q, qs) in &by_query {
                let ids: Vec<String> = qs.iter().map(|&i| order[i].0.clone()).collect();
                let docs = super::rerank::texts(pg, &ids, terms)?;
                for (&i, s) in qs.iter().zip(rc.score(q, &docs)?) {
                    scores.insert(i, s);
                }
            }
            let s: Vec<f64> = slots.iter().map(|i| scores[i]).collect();
            out.push((slots.clone(), s));
        }
        Ok(out)
    })();
    info.ms = round1(t0.elapsed().as_secs_f64() * 1000.0);
    match result {
        Ok(scored) => {
            for (slots, scores) in scored {
                info.scored += slots.len();
                // The slots' entities, best score first (ties by fused place).
                let mut ranked: Vec<(usize, f64)> = slots.iter().copied().zip(scores).collect();
                ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                let moved: Vec<(String, f64)> =
                    ranked.iter().map(|(i, _)| order[*i].clone()).collect();
                for (k, (&slot, (id_score, (_, s)))) in
                    slots.iter().zip(moved.into_iter().zip(&ranked)).enumerate()
                {
                    explain
                        .entry(id_score.0.clone())
                        .or_default()
                        .insert(0, format!("rerank #{} ({s:.3})", k + 1));
                    order[slot] = id_score;
                }
            }
            info.ok = true;
        }
        Err(e) => {
            info.error = Some(format!("{e:#}"));
            for slots in by_corpus.values() {
                for &i in slots {
                    explain
                        .entry(order[i].0.clone())
                        .or_default()
                        .push("rerank failed: fused order kept".into());
                }
            }
        }
    }
    info
}

/// v1's stemming test for `matched`.
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

/// Which of an entity's fields hold which terms (v1's fields).
fn matched_fields(d: &Detail, terms: &[String]) -> Vec<String> {
    let e = &d.e;
    let title = if e.kind == "symbol" {
        String::new()
    } else {
        e.title.clone()
    };
    let name = super::sync::name_terms_of(e);
    let rel = if e.rel.is_empty() {
        e.project.as_str()
    } else {
        e.rel.as_str()
    };
    let path = format!("{rel} {}", super::sync::path_terms(rel));
    let summary = if d.summary.is_empty() {
        d.seed.clone()
    } else {
        d.summary.clone()
    };
    let fields: [(&str, &str); 7] = [
        ("title", &title),
        ("name", &name),
        ("path", &path),
        ("summary", &summary),
        ("symbols", &d.symbols),
        ("why", &d.why),
        ("labels", &d.labels_text),
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
    by_field
        .into_iter()
        .map(|(i, ts)| format!("{} {}", fields[i].0, ts.join(", ")))
        .collect()
}

/// Records a search in `queries` (what was asked, the ids returned, the
/// stages' milliseconds), without waiting for the disk.
pub fn log_query(pg: &Pg, caller: &str, r: &SearchResult) -> Result<()> {
    let ids: Vec<&str> = r.hits.iter().map(|h| h.id.as_str()).collect();
    let mut c = pg.client();
    let mut tx = c.transaction()?;
    tx.batch_execute("SET LOCAL synchronous_commit TO off")?;
    tx.execute(
        // The configuration's fingerprint rides in `ms` (a column of its own
        // would be a migration older binaries refuse).
        "INSERT INTO queries (caller, text, results, ms) VALUES ($1, $2, $3, $4::jsonb || jsonb_build_object('config', $5::text))",
        &[
            &caller,
            &r.query,
            &serde_json::to_value(&ids)?,
            &serde_json::to_value(&r.ms)?,
            &r.config.clone().unwrap_or_default(),
        ],
    )?;
    tx.commit()?;
    Ok(())
}

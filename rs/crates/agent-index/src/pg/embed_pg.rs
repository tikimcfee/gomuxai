//! The embedder's side on Postgres: `card_vec` (halfvec(384), HNSW cosine)
//! filled from v1's embedder for every entity whose card text changed since
//! its vector was made; the queue the sync fills says which.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;

use super::{Pg, vec_text};
use crate::crawl::hash;
use crate::embed::{DrainReport, Encode};

/// Past this many to embed, the HNSW index is dropped and built again after:
/// one build is far faster than as many inserts into the graph.
const REBUILD_INDEX_PAST: usize = 5000;

const CREATE_INDEX: &str = "CREATE INDEX IF NOT EXISTS entities_card_vec ON entities USING hnsw (card_vec halfvec_cosine_ops)";

/// Brings `card_vec` up to date: clears the vectors of entities that no
/// longer get one, embeds every entity that wants one and whose vector is
/// missing or of other text (at most `limit`, 0 for all), and settles the
/// queue.
pub fn drain<E: Encode + ?Sized>(
    pg: &Pg,
    emb: &E,
    limit: usize,
    mut progress: impl FnMut(usize, usize),
) -> Result<DrainReport> {
    let start = Instant::now();
    let mut rep = DrainReport {
        removed: pg.execute(
            "UPDATE entities SET card_vec = NULL, vec_key = '' WHERE NOT embed AND card_vec IS NOT NULL",
            &[],
        )? as usize,
        ..Default::default()
    };
    rep.settled = pg.execute(
        "DELETE FROM embed_queue q WHERE NOT EXISTS (
             SELECT 1 FROM entities e WHERE e.id = q.entity_id AND e.embed AND e.vec_key <> e.text_key)",
        &[],
    )? as usize;
    let cfg = &pg.cfg;
    let run_id: i64 = pg.query(
        "INSERT INTO runs (kind, config, fingerprint, started) VALUES ('embed', $1, $2, now()) RETURNING id",
        &[
            &serde_json::json!({"model": crate::embed::MODEL_ID, "dim": emb.dim(), "limit": limit, "config": cfg}),
            &cfg.fingerprint(),
        ],
    )?[0]
        .get(0);
    let todo = cards(
        pg,
        pg.query(
            "SELECT id, text_key, project, rel, title, seed, summary, why, symbols, asks FROM entities
             WHERE embed AND vec_key <> text_key ORDER BY importance DESC, id",
            &[],
        )?,
    );
    let total = if limit > 0 {
        todo.len().min(limit)
    } else {
        todo.len()
    };
    let rebuild_index = total > REBUILD_INDEX_PAST;
    if rebuild_index {
        pg.batch("DROP INDEX IF EXISTS entities_card_vec")?;
    }
    let result = embed_cards(pg, emb, &todo[..total], &mut rep, &mut progress);
    if rebuild_index {
        pg.batch(CREATE_INDEX)?;
    }
    result?;
    rep.remaining = todo.len() - total;
    let n: i64 = pg.query(
        "SELECT count(*) FROM entities WHERE card_vec IS NOT NULL",
        &[],
    )?[0]
        .get(0);
    pg.execute(
        "INSERT INTO meta (key, value) VALUES ('vectors', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        &[&n.to_string()],
    )?;
    rep.secs = start.elapsed().as_secs_f64();
    pg.execute(
        "UPDATE runs SET finished = now(), coverage = $2 WHERE id = $1",
        &[
            &run_id,
            &serde_json::json!({"embedded": rep.embedded, "removed": rep.removed, "settled": rep.settled,
                                "remaining": rep.remaining, "secs": rep.secs, "vectors": n}),
        ],
    )?;
    Ok(rep)
}

/// The watcher's drain: only the cards the sync queued, with no scan of
/// every entity and no run row (a flush's sync recorded one); the full
/// [`drain`] (agent index embed, the tidy) does the rest, and takes over
/// when the queue is long enough to want the index built again.
pub fn drain_queued<E: Encode + ?Sized>(pg: &Pg, emb: &E) -> Result<DrainReport> {
    let start = Instant::now();
    let todo = cards(
        pg,
        pg.query(
            "SELECT e.id, e.text_key, e.project, e.rel, e.title, e.seed, e.summary, e.why, e.symbols, e.asks
             FROM embed_queue q JOIN entities e ON e.id = q.entity_id
             WHERE e.embed AND e.vec_key <> e.text_key ORDER BY e.importance DESC, e.id",
            &[],
        )?,
    );
    if todo.len() > REBUILD_INDEX_PAST {
        return drain(pg, emb, 0, |_, _| {});
    }
    let mut rep = DrainReport::default();
    embed_cards(pg, emb, &todo, &mut rep, &mut |_, _| {})?;
    // Queue rows whose entity wants no vector, or has its vector already.
    rep.settled += pg.execute(
        "DELETE FROM embed_queue q USING entities e
         WHERE e.id = q.entity_id AND (NOT e.embed OR e.vec_key = e.text_key)",
        &[],
    )? as usize;
    rep.secs = start.elapsed().as_secs_f64();
    Ok(rep)
}

/// Cards from rows of (id, text_key, project, rel, title, seed, summary,
/// why, symbols, asks): (id, text_key, card text).
fn cards(pg: &Pg, rows: Vec<postgres::Row>) -> Vec<(String, String, String)> {
    rows.into_iter()
        .map(|r| {
            let (seed, summary): (String, String) = (r.get(5), r.get(6));
            let shown = if summary.is_empty() { seed } else { summary };
            let card = super::sync::card(
                &r.get::<_, String>(2),
                &r.get::<_, String>(3),
                &r.get::<_, String>(4),
                &shown,
                &r.get::<_, String>(7),
                &r.get::<_, String>(8),
                &r.get::<_, Vec<String>>(9),
                &pg.cfg.cards,
            );
            (r.get(0), r.get(1), card)
        })
        .collect()
}

/// Embeds these cards in batches, storing each vector under its key and
/// settling its queue row.
fn embed_cards<E: Encode + ?Sized>(
    pg: &Pg,
    emb: &E,
    todo: &[(String, String, String)],
    rep: &mut DrainReport,
    progress: &mut impl FnMut(usize, usize),
) -> Result<()> {
    let total = todo.len();
    {
        for batch in todo.chunks(pg.cfg.embedder.batch.max(1)) {
            // A card whose text isn't what its key says (the sync wrote it
            // from other text) is skipped rather than stored under a key
            // that would never settle.
            let keep: Vec<&(String, String, String)> = batch
                .iter()
                .filter(|(_, key, card)| &hash(&[card.as_bytes()]) == key)
                .collect();
            let cards: Vec<String> = keep.iter().map(|x| x.2.clone()).collect();
            let vecs = emb.embed(&cards, false)?;
            let ids: Vec<String> = keep.iter().map(|x| x.0.clone()).collect();
            let keys: Vec<String> = keep.iter().map(|x| x.1.clone()).collect();
            let texts: Vec<String> = vecs.iter().map(|v| vec_text(v)).collect();
            let mut c = pg.client();
            let mut tx = c.transaction()?;
            tx.execute(
                "UPDATE entities e SET card_vec = v.vec::halfvec(384), vec_key = v.key
                 FROM unnest($1::text[], $2::text[], $3::text[]) AS v(id, key, vec)
                 WHERE e.id = v.id AND e.text_key = v.key",
                &[&ids, &keys, &texts],
            )?;
            rep.settled += tx.execute(
                "DELETE FROM embed_queue q USING unnest($1::text[], $2::text[]) AS v(id, key)
                 WHERE q.entity_id = v.id AND q.text_key = v.key",
                &[&ids, &keys],
            )? as usize;
            tx.commit()?;
            rep.embedded += keep.len();
            progress(rep.embedded, total);
        }
    }
    Ok(())
}

/// An entity's columns for its windows: kind, file_kind, project, rel,
/// path, corpus, its summary (else seed), title.
type EntCols = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

/// What a model's pass did.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ModelReport {
    pub model: String,
    pub key: String,
    pub cards: usize,
    pub windows: usize,
    /// Windows looked at (the embedded ones among them).
    pub windows_seen: usize,
    /// Windows whose text held and whose lines were updated.
    pub moved: usize,
    pub removed: usize,
    pub secs: f64,
    /// Counted, not done.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
}

/// A batch of rows into a model's table, by `unnest`, replacing a row with
/// the same key.
fn upsert(
    pg: &Pg,
    table: &str,
    dims: usize,
    chunks: bool,
    rows: &[(Vec<String>, Vec<i32>, Vec<f32>)],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    // (entity_id, kind, file_kind, project, path, corpus, text_key) and, for
    // chunks, (ord, part, line_start, line_end).
    let col = |i: usize| -> Vec<String> { rows.iter().map(|r| r.0[i].clone()).collect() };
    let num = |i: usize| -> Vec<i32> { rows.iter().map(|r| r.1[i]).collect() };
    let vecs: Vec<String> = rows.iter().map(|r| vec_text(&r.2)).collect();
    let (ids, kinds, fks, projects, paths, corpora, keys) =
        (col(0), col(1), col(2), col(3), col(4), col(5), col(6));
    if chunks {
        let (ord, part, a, b) = (num(0), num(1), num(2), num(3));
        pg.execute(
            &format!(
                "INSERT INTO {table} (entity_id, ord, part, line_start, line_end, kind, file_kind, project, path, corpus, text_key, vec)
                 SELECT id, o, p, a, b, k, f, pr, pa, c, t, v::halfvec({dims}) FROM unnest($1::text[], $2::int[], $3::int[], $4::int[], $5::int[],
                     $6::text[], $7::text[], $8::text[], $9::text[], $10::text[], $11::text[], $12::text[]) AS x(id, o, p, a, b, k, f, pr, pa, c, t, v)
                 ON CONFLICT (entity_id, ord, part) DO UPDATE SET line_start = excluded.line_start, line_end = excluded.line_end,
                     kind = excluded.kind, file_kind = excluded.file_kind, project = excluded.project, path = excluded.path,
                     corpus = excluded.corpus, text_key = excluded.text_key, vec = excluded.vec"
            ),
            &[&ids, &ord, &part, &a, &b, &kinds, &fks, &projects, &paths, &corpora, &keys, &vecs],
        )?;
    } else {
        pg.execute(
            &format!(
                "INSERT INTO {table} (entity_id, kind, file_kind, project, path, corpus, text_key, vec)
                 SELECT id, k, f, pr, pa, c, t, v::halfvec({dims}) FROM unnest($1::text[], $2::text[], $3::text[], $4::text[],
                     $5::text[], $6::text[], $7::text[], $8::text[]) AS x(id, k, f, pr, pa, c, t, v)
                 ON CONFLICT (entity_id) DO UPDATE SET kind = excluded.kind, file_kind = excluded.file_kind,
                     project = excluded.project, path = excluded.path, corpus = excluded.corpus,
                     text_key = excluded.text_key, vec = excluded.vec"
            ),
            &[&ids, &kinds, &fks, &projects, &paths, &corpora, &keys, &vecs],
        )?;
    }
    Ok(())
}

/// What a [`drain_model`] pass covers.
#[derive(Default)]
pub struct Pass<'a> {
    /// The corpora the model serves.
    pub corpora: Vec<String>,
    /// The chunks' windows too (`[embedder.chunks]`), else cards alone.
    pub chunks: Option<&'a crate::config::ChunkVecs>,
    /// Only these entities (the watcher's flush).
    pub only: Option<&'a [String]>,
    /// Build the HNSW indexes after the rows instead of into them.
    pub bulk: bool,
    /// Count what would be embedded and removed; write nothing.
    pub dry_run: bool,
    /// With `dry_run`: compare with this schema's tables of the model
    /// rather than this one's (what a copy from it would leave to do).
    pub have_from: Option<&'a str>,
    /// Called with each window's text (a dry run's token count).
    pub on_window: Option<&'a dyn Fn(&str)>,
}

/// Brings a model's vectors (docs/INDEX-V2.md, "A code-trained embedder")
/// up to date for the pass's corpora: the cards whose text changed, and
/// (with chunks) every chunk's windows whose text changed, the lines of
/// those whose text didn't but moved; rows of what is gone removed.
pub fn drain_model<E: Encode + ?Sized>(
    pg: &Pg,
    emb: &E,
    m: &super::vecs::Model,
    p: &Pass,
    progress: &mut dyn FnMut(&str),
) -> Result<ModelReport> {
    use super::vecs;
    let start = Instant::now();
    let mut rep = ModelReport {
        model: m.name.clone(),
        key: m.key.clone(),
        dry_run: p.dry_run,
        ..Default::default()
    };
    let (chunks, only, bulk, dry) = (p.chunks, p.only, p.bulk, p.dry_run);
    if !dry {
        vecs::ensure_tables(pg, m)?;
        if bulk {
            vecs::drop_indexes(pg, m)?;
        }
    }
    let batch = pg.cfg.embedder.batch.clamp(1, 256);
    let corpora_v: Vec<String> = p.corpora.clone();
    let only_v: Option<Vec<String>> = only.map(<[String]>::to_vec);
    let (cards_t, chunks_t, d) = (m.cards_table(), m.chunks_table(), m.dims);
    // The tables compared with: this schema's, or (a dry run) another's.
    let (have_cards, have_chunks) = match p.have_from {
        Some(s) if dry => (format!("{s}.{cards_t}"), format!("{s}.{chunks_t}")),
        _ => (cards_t.clone(), chunks_t.clone()),
    };
    if dry {
        for t in [&have_cards, &have_chunks] {
            let (schema, table) = t.split_once('.').unwrap_or((pg.schema.as_str(), t));
            if pg
                .query(
                    "SELECT 1 FROM pg_tables WHERE schemaname = $1 AND tablename = $2",
                    &[&schema, &table],
                )?
                .is_empty()
            {
                anyhow::bail!("a dry run compares with {schema}.{table}, which isn't there");
            }
        }
    }
    // Cards.
    // bge's cards are `entities.card_vec` ([`drain`]): here only its chunks.
    let rows = if m.legacy_cards {
        Vec::new()
    } else {
        pg.query(
        &format!(
            "SELECT e.id, e.text_key, e.project, e.rel, e.title, e.seed, e.summary, e.why, e.symbols, e.asks,
                    e.kind, e.file_kind, e.path, e.corpus
             FROM entities e LEFT JOIN {have_cards} v ON v.entity_id = e.id
             WHERE e.embed AND e.corpus = ANY($1) AND ($2::text[] IS NULL OR e.id = ANY($2))
               AND (v.entity_id IS NULL OR v.text_key <> e.text_key)
             ORDER BY e.importance DESC, e.id"
        ),
        &[&corpora_v, &only_v],
    )?
    };
    let mut todo: Vec<(Vec<String>, String)> = Vec::new();
    for r in &rows {
        let (seed, summary): (String, String) = (r.get(5), r.get(6));
        let shown = if summary.is_empty() { seed } else { summary };
        let card = super::sync::card(
            &r.get::<_, String>(2),
            &r.get::<_, String>(3),
            &r.get::<_, String>(4),
            &shown,
            &r.get::<_, String>(7),
            &r.get::<_, String>(8),
            &r.get::<_, Vec<String>>(9),
            &pg.cfg.cards,
        );
        let cols: Vec<String> = vec![
            r.get(0),
            r.get(10),
            r.get(11),
            r.get(2),
            r.get(12),
            r.get(13),
            r.get(1),
        ];
        todo.push((cols, card));
    }
    progress(&format!("{}: {} cards to embed", m.name, todo.len()));
    if dry {
        rep.cards = todo.len();
        todo.clear();
    }
    for part in todo.chunks(batch) {
        let texts: Vec<String> = part.iter().map(|t| t.1.clone()).collect();
        let vs = emb.embed(&texts, false)?;
        let rows: Vec<(Vec<String>, Vec<i32>, Vec<f32>)> = part
            .iter()
            .zip(vs)
            .map(|(t, v)| (t.0.clone(), Vec::new(), v))
            .collect();
        upsert(pg, &cards_t, d, false, &rows)?;
        rep.cards += part.len();
        progress(&format!(
            "{}: cards {} of {}",
            m.name,
            rep.cards,
            todo.len()
        ));
    }
    if only.is_none() && !m.legacy_cards {
        let gone = format!(
            "FROM {have_cards} v WHERE NOT EXISTS (SELECT 1 FROM entities e
                 WHERE e.id = v.entity_id AND e.embed AND e.corpus = ANY($1))"
        );
        rep.removed += if dry {
            pg.query(&format!("SELECT count(*) {gone}"), &[&corpora_v])?[0].get::<_, i64>(0)
                as usize
        } else {
            pg.execute(&format!("DELETE {gone}"), &[&corpora_v])? as usize
        };
    }
    // Chunks: by entity, a batch of entities at a time.
    if let Some(cc) = chunks.filter(|c| c.enabled) {
        let ids: Vec<String> = pg
            .query(
                "SELECT DISTINCT c.entity_id FROM chunks c JOIN entities e ON e.id = c.entity_id
                 WHERE e.corpus = ANY($1) AND ($2::text[] IS NULL OR c.entity_id = ANY($2)) ORDER BY 1",
                &[&corpora_v, &only_v],
            )?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        progress(&format!("{}: chunks of {} entities", m.name, ids.len()));
        let mut done = 0usize;
        for group in ids.chunks(200) {
            let g: Vec<String> = group.to_vec();
            let ents: HashMap<String, EntCols> = pg
                .query(
                    "SELECT id, kind, file_kind, project, rel, path, corpus,
                            CASE WHEN summary <> '' THEN summary ELSE seed END, title
                     FROM entities WHERE id = ANY($1)",
                    &[&g],
                )?
                .into_iter()
                .map(|r| {
                    let id: String = r.get(0);
                    let summary: String = r.get(7);
                    let title: String = r.get(8);
                    (
                        id,
                        (
                            r.get(1),
                            r.get(2),
                            r.get(3),
                            r.get(4),
                            r.get(5),
                            r.get(6),
                            summary,
                            title,
                        ),
                    )
                })
                .collect();
            let mut syms: HashMap<String, Vec<(i64, String)>> = HashMap::new();
            for r in pg.query(
                "SELECT parent, line, name FROM entities WHERE kind = 'symbol' AND parent = ANY($1) ORDER BY parent, line",
                &[&g],
            )? {
                syms.entry(r.get(0)).or_default().push((r.get(1), r.get(2)));
            }
            // Each window's key and lines as stored.
            type Have = HashMap<(String, i32, i32), (String, i32, i32)>;
            let have: Have = pg
                .query(&format!("SELECT entity_id, ord, part, text_key, line_start, line_end FROM {have_chunks} WHERE entity_id = ANY($1)"), &[&g])?
                .into_iter()
                .map(|r| ((r.get(0), r.get(1), r.get(2)), (r.get(3), r.get(4), r.get(5))))
                .collect();
            let mut want: Vec<(String, i32, i32)> = Vec::new();
            let mut todo: Vec<(Vec<String>, Vec<i32>, String)> = Vec::new();
            // Windows whose text holds but whose lines moved (an edit above
            // them): (entity, ord, part, line_start, line_end).
            let mut moved: Vec<(String, i32, i32, i32, i32)> = Vec::new();
            for r in pg.query(
                "SELECT entity_id, ord, line_start, line_end, heading, text, meta FROM chunks
                 WHERE entity_id = ANY($1) ORDER BY entity_id, ord",
                &[&g],
            )? {
                let id: String = r.get(0);
                let Some(e) = ents.get(&id) else { continue };
                let (ord, a): (i32, i32) = (r.get(1), r.get(2));
                let heading: String = r.get(4);
                let text: String = r.get(5);
                let meta: serde_json::Value = r.get(6);
                let mut headings: Vec<String> = meta["headings"]
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if headings.is_empty() && !heading.is_empty() {
                    headings.push(heading.clone());
                }
                if e.5 == "session" && !e.7.is_empty() {
                    headings.insert(0, e.7.clone());
                }
                let parts = vecs::HeaderParts {
                    project: &e.2,
                    rel: &e.3,
                    summary: &e.6,
                    headings,
                };
                let empty = Vec::new();
                let s = syms.get(&id).unwrap_or(&empty);
                let defines = |lo: usize, hi: usize| -> Vec<String> {
                    let mut v: Vec<String> = s
                        .iter()
                        .filter(|(l, _)| (*l as usize) >= lo && (*l as usize) <= hi)
                        .map(|(_, n)| n.clone())
                        .collect();
                    v.dedup();
                    v.truncate(40);
                    v
                };
                for w in vecs::windows(
                    &text,
                    a.max(1) as usize,
                    m.window_chars,
                    &cc.header,
                    &parts,
                    &defines,
                ) {
                    let key = hash(&[
                        m.key.as_bytes(),
                        vecs::HEADER_VERSION.as_bytes(),
                        cc.header.as_bytes(),
                        w.text.as_bytes(),
                    ]);
                    let k = (id.clone(), ord, w.part as i32);
                    rep.windows_seen += 1;
                    if let Some(f) = p.on_window {
                        f(&w.text);
                    }
                    want.push(k.clone());
                    let (a, b) = (w.line_start as i32, w.line_end as i32);
                    if let Some(h) = have.get(&k).filter(|h| h.0 == key) {
                        if (h.1, h.2) != (a, b) {
                            moved.push((id.clone(), ord, w.part as i32, a, b));
                        }
                        continue;
                    }
                    let cols = vec![
                        id.clone(),
                        e.0.clone(),
                        e.1.clone(),
                        e.2.clone(),
                        e.4.clone(),
                        e.5.clone(),
                        key,
                    ];
                    todo.push((
                        cols,
                        vec![ord, w.part as i32, w.line_start as i32, w.line_end as i32],
                        w.text,
                    ));
                }
            }
            rep.moved += moved.len();
            if dry {
                rep.windows += todo.len();
                todo.clear();
            } else if !moved.is_empty() {
                let col_s = |f: fn(&(String, i32, i32, i32, i32)) -> String| -> Vec<String> {
                    moved.iter().map(f).collect()
                };
                let col_i = |f: fn(&(String, i32, i32, i32, i32)) -> i32| -> Vec<i32> {
                    moved.iter().map(f).collect()
                };
                pg.execute(
                    &format!(
                        "UPDATE {chunks_t} v SET line_start = x.a, line_end = x.b
                         FROM unnest($1::text[], $2::int[], $3::int[], $4::int[], $5::int[]) AS x(i, o, p, a, b)
                         WHERE v.entity_id = x.i AND v.ord = x.o AND v.part = x.p"
                    ),
                    &[&col_s(|m| m.0.clone()), &col_i(|m| m.1), &col_i(|m| m.2), &col_i(|m| m.3), &col_i(|m| m.4)],
                )?;
            }
            for part in todo.chunks(batch) {
                let texts: Vec<String> = part.iter().map(|t| t.2.clone()).collect();
                let vs = emb.embed(&texts, false)?;
                let rows: Vec<(Vec<String>, Vec<i32>, Vec<f32>)> = part
                    .iter()
                    .zip(vs)
                    .map(|(t, v)| (t.0.clone(), t.1.clone(), v))
                    .collect();
                upsert(pg, &chunks_t, d, true, &rows)?;
                rep.windows += part.len();
            }
            // Windows of these entities no longer cut.
            let (wi, wo, wp): (Vec<String>, Vec<i32>, Vec<i32>) = {
                let mut a = (Vec::new(), Vec::new(), Vec::new());
                for (i, o, p) in &want {
                    a.0.push(i.clone());
                    a.1.push(*o);
                    a.2.push(*p);
                }
                a
            };
            let stale = format!(
                "FROM {have_chunks} v WHERE v.entity_id = ANY($1) AND NOT EXISTS (
                     SELECT 1 FROM unnest($2::text[], $3::int[], $4::int[]) AS w(i, o, p)
                     WHERE w.i = v.entity_id AND w.o = v.ord AND w.p = v.part)"
            );
            rep.removed += if dry {
                pg.query(&format!("SELECT count(*) {stale}"), &[&g, &wi, &wo, &wp])?[0]
                    .get::<_, i64>(0) as usize
            } else {
                pg.execute(&format!("DELETE {stale}"), &[&g, &wi, &wo, &wp])? as usize
            };
            done += group.len();
            progress(&format!(
                "{}: chunks of {done} of {} entities; {} windows embedded of {} seen",
                m.name,
                ids.len(),
                rep.windows,
                rep.windows_seen
            ));
        }
        // Windows of entities with no chunks now (all of them, or of the
        // entities asked about).
        let gone = format!(
            "FROM {have_chunks} v WHERE ($2::text[] IS NULL OR v.entity_id = ANY($2)) AND NOT EXISTS (
                 SELECT 1 FROM chunks c JOIN entities e ON e.id = c.entity_id
                 WHERE c.entity_id = v.entity_id AND e.corpus = ANY($1))"
        );
        rep.removed += if dry {
            pg.query(&format!("SELECT count(*) {gone}"), &[&corpora_v, &only_v])?[0]
                .get::<_, i64>(0) as usize
        } else {
            pg.execute(&format!("DELETE {gone}"), &[&corpora_v, &only_v])? as usize
        };
    }
    if !dry {
        vecs::ensure_indexes(pg, m)?;
        if bulk {
            pg.batch(&format!("ANALYZE {cards_t}; ANALYZE {chunks_t}"))?;
        }
    }
    rep.secs = start.elapsed().as_secs_f64();
    Ok(rep)
}

/// What [`keep_up`] did, all models together and by model.
#[derive(Debug, Clone, Default)]
pub struct KeepReport {
    pub embedded: usize,
    /// "model: N cards, M windows in T ms", one per model that had work.
    pub parts: Vec<String>,
}

/// Brings every live model's vectors up to date (vecs::live, with an
/// encoder each): after a flush's sync, for the entities the sync queued
/// (their cards changed) and those whose chunks it cut again; after the
/// tidy (`full`), for everything. bge's card vectors go through [`drain`]
/// and [`drain_queued`], which settle the queue; with no bge model, the
/// queue rows read are deleted here.
pub fn keep_up(
    pg: &Pg,
    models: &[(&super::vecs::Live, &(dyn Encode + Sync))],
    full: bool,
    rechunked: &[String],
) -> Result<KeepReport> {
    let mut rep = KeepReport::default();
    let ids: Option<Vec<String>> = if full {
        None
    } else {
        let mut v: Vec<String> = pg
            .query("SELECT DISTINCT entity_id FROM embed_queue", &[])?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        v.extend(rechunked.iter().cloned());
        v.sort();
        v.dedup();
        if v.is_empty() {
            return Ok(rep);
        }
        Some(v)
    };
    let chunk_cfg = pg.cfg.embedder.chunks.clone();
    for (l, enc) in models {
        let t = Instant::now();
        let (mut cards, mut windows) = (0, 0);
        if l.model.legacy_cards {
            let r = if full {
                drain(pg, *enc, 0, |_, _| {})?
            } else {
                drain_queued(pg, *enc)?
            };
            cards += r.embedded;
        }
        if !l.model.legacy_cards || l.chunks {
            let r = drain_model(
                pg,
                *enc,
                &l.model,
                &Pass {
                    corpora: l.corpora.clone(),
                    chunks: l.chunks.then_some(&chunk_cfg),
                    only: ids.as_deref(),
                    ..Default::default()
                },
                &mut |_| {},
            )?;
            cards += r.cards;
            windows += r.windows;
        }
        rep.embedded += cards + windows;
        if cards + windows > 0 {
            rep.parts.push(format!(
                "{}: {cards} cards, {windows} windows in {} ms",
                l.model.name,
                t.elapsed().as_millis()
            ));
        }
    }
    if !models.iter().any(|(l, _)| l.model.legacy_cards) {
        match &ids {
            Some(v) => pg.execute("DELETE FROM embed_queue WHERE entity_id = ANY($1)", &[v])?,
            None => pg.execute("DELETE FROM embed_queue", &[])?,
        };
    }
    Ok(rep)
}

/// What [`copy_from`] copied.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CopyReport {
    pub from: String,
    pub cards: usize,
    /// The source's card rows left behind: their entity is gone or its card
    /// changed (the drain after embeds those again).
    pub cards_left: usize,
    pub windows: usize,
    pub windows_left: usize,
}

/// Copies a model's vectors from another schema of the same database (the
/// scratch copy a bulk pass filled) into this one's tables, made when
/// missing, matched by entity key: a card row when this schema's entity has
/// the same card text (its key), every window row of an entity this schema
/// has (the [`drain_model`] after re-keys and removes the windows whose text
/// changed). The filter columns are this schema's. The HNSW indexes are
/// dropped first and left for the drain to build.
pub fn copy_from(pg: &Pg, m: &super::vecs::Model, from: &str) -> Result<CopyReport> {
    use super::vecs;
    if !from.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || from == pg.schema {
        anyhow::bail!("copy from {from:?}: another schema's name");
    }
    vecs::ensure_tables(pg, m)?;
    vecs::drop_indexes(pg, m)?;
    let (cards_t, chunks_t, d) = (m.cards_table(), m.chunks_table(), m.dims);
    let count = |t: &str| -> Result<usize> {
        Ok(
            pg.query(&format!("SELECT count(*) FROM {from}.{t}"), &[])?[0].get::<_, i64>(0)
                as usize,
        )
    };
    let mut rep = CopyReport {
        from: from.to_string(),
        ..Default::default()
    };
    if !m.legacy_cards {
        rep.cards = pg.execute(
            &format!(
                "INSERT INTO {cards_t} (entity_id, kind, file_kind, project, path, corpus, text_key, vec)
                 SELECT e.id, e.kind, e.file_kind, e.project, e.path, e.corpus, v.text_key, v.vec::halfvec({d})
                 FROM {from}.{cards_t} v JOIN entities e ON e.id = v.entity_id AND e.text_key = v.text_key AND e.embed
                 ON CONFLICT (entity_id) DO NOTHING"
            ),
            &[],
        )? as usize;
        rep.cards_left = count(&cards_t)?.saturating_sub(rep.cards);
    }
    rep.windows = pg.execute(
        &format!(
            "INSERT INTO {chunks_t} (entity_id, ord, part, line_start, line_end, kind, file_kind, project, path, corpus, text_key, vec)
             SELECT v.entity_id, v.ord, v.part, v.line_start, v.line_end, e.kind, e.file_kind, e.project, e.path, e.corpus,
                    v.text_key, v.vec::halfvec({d})
             FROM {from}.{chunks_t} v JOIN entities e ON e.id = v.entity_id
             WHERE EXISTS (SELECT 1 FROM chunks c WHERE c.entity_id = e.id)
             ON CONFLICT (entity_id, ord, part) DO NOTHING"
        ),
        &[],
    )? as usize;
    rep.windows_left = count(&chunks_t)?.saturating_sub(rep.windows);
    Ok(rep)
}

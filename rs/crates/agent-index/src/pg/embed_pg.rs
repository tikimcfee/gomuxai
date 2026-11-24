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
    pub removed: usize,
    pub secs: f64,
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

/// Brings a model's vectors (docs/INDEX-V2.md, "A code-trained embedder")
/// up to date for `corpora`: the cards whose text changed, and (with
/// `chunks`) every chunk's windows whose text changed; rows of what is gone
/// removed. `only` limits it to these entities (the watcher's flush);
/// `bulk` builds the HNSW indexes after the rows instead of into them.
#[allow(clippy::too_many_arguments)]
pub fn drain_model<E: Encode + ?Sized>(
    pg: &Pg,
    emb: &E,
    m: &super::vecs::Model,
    corpora: &[String],
    chunks: Option<&crate::config::ChunkVecs>,
    only: Option<&[String]>,
    bulk: bool,
    progress: &mut dyn FnMut(&str),
) -> Result<ModelReport> {
    use super::vecs;
    let start = Instant::now();
    let mut rep = ModelReport {
        model: m.name.clone(),
        key: m.key.clone(),
        ..Default::default()
    };
    vecs::ensure_tables(pg, m)?;
    if bulk {
        vecs::drop_indexes(pg, m)?;
    }
    let batch = pg.cfg.embedder.batch.clamp(1, 256);
    let corpora_v: Vec<String> = corpora.to_vec();
    let only_v: Option<Vec<String>> = only.map(<[String]>::to_vec);
    let (cards_t, chunks_t, d) = (m.cards_table(), m.chunks_table(), m.dims);
    // Cards.
    // bge's cards are `entities.card_vec` ([`drain`]): here only its chunks.
    let rows = if m.legacy_cards {
        Vec::new()
    } else {
        pg.query(
        &format!(
            "SELECT e.id, e.text_key, e.project, e.rel, e.title, e.seed, e.summary, e.why, e.symbols, e.asks,
                    e.kind, e.file_kind, e.path, e.corpus
             FROM entities e LEFT JOIN {cards_t} v ON v.entity_id = e.id
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
        rep.removed += pg.execute(
            &format!(
                "DELETE FROM {cards_t} v WHERE NOT EXISTS (SELECT 1 FROM entities e
                     WHERE e.id = v.entity_id AND e.embed AND e.corpus = ANY($1))"
            ),
            &[&corpora_v],
        )? as usize;
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
            let have: HashMap<(String, i32, i32), String> = pg
                .query(&format!("SELECT entity_id, ord, part, text_key FROM {chunks_t} WHERE entity_id = ANY($1)"), &[&g])?
                .into_iter()
                .map(|r| ((r.get(0), r.get(1), r.get(2)), r.get(3)))
                .collect();
            let mut want: Vec<(String, i32, i32)> = Vec::new();
            let mut todo: Vec<(Vec<String>, Vec<i32>, String)> = Vec::new();
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
                    want.push(k.clone());
                    if have.get(&k) == Some(&key) {
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
            rep.removed += pg.execute(
                &format!(
                    "DELETE FROM {chunks_t} v WHERE v.entity_id = ANY($1) AND NOT EXISTS (
                         SELECT 1 FROM unnest($2::text[], $3::int[], $4::int[]) AS w(i, o, p)
                         WHERE w.i = v.entity_id AND w.o = v.ord AND w.p = v.part)"
                ),
                &[&g, &wi, &wo, &wp],
            )? as usize;
            done += group.len();
            progress(&format!(
                "{}: chunks of {done} of {} entities; {} windows embedded of {} seen",
                m.name,
                ids.len(),
                rep.windows,
                rep.windows_seen
            ));
        }
        if only.is_none() {
            rep.removed += pg.execute(
                &format!(
                    "DELETE FROM {chunks_t} v WHERE NOT EXISTS (SELECT 1 FROM chunks c JOIN entities e ON e.id = c.entity_id
                         WHERE c.entity_id = v.entity_id AND e.corpus = ANY($1))"
                ),
                &[&corpora_v],
            )? as usize;
        }
    }
    vecs::ensure_indexes(pg, m)?;
    if bulk {
        pg.batch(&format!("ANALYZE {cards_t}; ANALYZE {chunks_t}"))?;
    }
    rep.secs = start.elapsed().as_secs_f64();
    Ok(rep)
}

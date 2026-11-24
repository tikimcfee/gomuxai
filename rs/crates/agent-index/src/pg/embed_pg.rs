//! The embedder's side on Postgres: `card_vec` (halfvec(384), HNSW cosine)
//! filled from v1's embedder for every entity whose card text changed since
//! its vector was made; the queue the sync fills says which.

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
pub fn drain<E: Encode>(
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
pub fn drain_queued<E: Encode>(pg: &Pg, emb: &E) -> Result<DrainReport> {
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
fn embed_cards<E: Encode>(
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

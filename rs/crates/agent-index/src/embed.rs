//! The embedder (docs/INDEX.md, "Embeddings"): each entity's card (its path,
//! title, summary or seed, why, and symbol names) embedded by a small model on
//! the CPU, stored in `vectors`, and searched by an exact scan.
//!
//! The model is bge-small-en-v1.5 (384 dimensions), run by candle with no
//! ONNX Runtime. It was chosen by measurement: in the hybrid (bm25 over cards,
//! bm25 over file text, and this list, fused by rank), it lifted MRR@10 over the
//! best lexical pair by +0.15 at 1,114 files and +0.13 at 5,074 on plain-word
//! questions, and neither a static model (model2vec) nor larger models beat it
//! significantly there (the embedding lane's hybrid-retrieval experiments, 2026-11-23).
//!
//! Vectors are L2-normalized and stored as little-endian f16. No mean-centering
//! for this model: centering helped the 8B model and hurt this one.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use half::f16;
use rusqlite::{Connection, params};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::query::Semantic;

/// The model column's value: the model, and the version of the card text and
/// prompt. Changing either is a new value, so old vectors are never mixed in.
pub const MODEL_ID: &str = "bge-small-en-v1.5/card-v1";
/// bge's instruction for queries (documents get none).
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
/// Kinds that get a vector. Symbols don't: their names are already in their
/// file's card, and there are ten times as many of them.
pub const EMBED_KINDS: &[&str] = &["project", "dir", "file", "section", "concept"];
/// Cards are clipped to this many characters (about 500 tokens).
const CARD_CHARS: usize = 2000;
const SYMBOLS_IN_CARD: usize = 60;
const BATCH: usize = 8;

/// A loaded model.
pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    pub dim: usize,
    /// At most this many worker threads (0: half the logical CPUs, for a
    /// bulk run; a background process such as the watcher takes fewer).
    pub max_workers: usize,
}

/// Where the model's files are: `config.json`, `tokenizer.json` and
/// `model.safetensors`. Given, or else BAAI/bge-small-en-v1.5 in the Hugging
/// Face cache (`HF_HUB_CACHE`, else `HF_HOME/hub`, else
/// `~/.cache/huggingface/hub`): the snapshot `refs/main` names, else any
/// complete one.
pub fn model_dir(given: Option<&Path>) -> Result<PathBuf> {
    if let Some(d) = given {
        return Ok(d.to_path_buf());
    }
    let hub = if let Some(c) = std::env::var_os("HF_HUB_CACHE") {
        PathBuf::from(c)
    } else if let Some(h) = std::env::var_os("HF_HOME") {
        PathBuf::from(h).join("hub")
    } else {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("no HOME"))?;
        PathBuf::from(home).join(".cache/huggingface/hub")
    };
    let repo = hub.join("models--BAAI--bge-small-en-v1.5");
    let complete =
        |p: &Path| p.join("model.safetensors").exists() && p.join("tokenizer.json").exists();
    if let Ok(main) = std::fs::read_to_string(repo.join("refs/main")) {
        let p = repo.join("snapshots").join(main.trim());
        if complete(&p) {
            return Ok(p);
        }
    }
    let snaps = repo.join("snapshots");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&snaps)
        .with_context(|| format!("no model at {} (pass the model directory)", snaps.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| complete(p))
        .collect();
    found.sort();
    found
        .pop()
        .ok_or_else(|| anyhow!("no complete snapshot under {}", snaps.display()))
}

/// What turns texts into unit vectors. The model, or a stand-in in tests.
pub trait Encode {
    fn dim(&self) -> usize;
    /// One L2-normalized vector per text, in order. `query` marks search
    /// queries (bge gives them an instruction; documents get none).
    fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>>;
    /// Whether it embeds on the CPU now because its server failed
    /// (embed_models::Served): the watcher then embeds only a few windows a
    /// flush and leaves the rest for when the server is back.
    fn degraded(&self) -> bool {
        false
    }
}

impl Encode for Embedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        Embedder::embed(self, texts, query)
    }
}

impl Embedder {
    pub fn load(dir: &Path) -> Result<Self> {
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json")).context("config.json")?,
        )?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("tokenizer.json: {e}"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| anyhow!("truncation: {e}"))?;
        // SAFETY: the file is mapped read-only and not changed while we run.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &device,
            )?
        };
        let model = BertModel::load(vb, &config)?;
        Ok(Self {
            model,
            tokenizer,
            device,
            dim: config.hidden_size,
            max_workers: 0,
        })
    }

    /// Embeds texts (CLS pooling, L2-normalized), in the order given.
    /// Batches of similar length run side by side on worker threads, half as
    /// many as there are logical CPUs. A model this small leaves most cores
    /// idle when only its matrix products are parallel; measured on 1,114
    /// cards (16 cores, 32 threads): 39 cards/s with candle's threads alone,
    /// 61/s with 16 workers whose products run on one thread each, which is
    /// what `single_threaded_products` sets up for a bulk run.
    pub fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let chunks: Vec<&[usize]> = order.chunks(BATCH).collect();
        let half = std::thread::available_parallelism().map_or(4, |n| n.get()) / 2;
        let cap = if self.max_workers > 0 {
            self.max_workers.min(half.max(1))
        } else {
            half
        };
        let workers = cap.clamp(1, chunks.len().max(1));
        let next = std::sync::atomic::AtomicUsize::new(0);
        let done: Vec<Vec<(usize, Vec<f32>)>> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..workers)
                .map(|_| {
                    sc.spawn(|| -> Result<Vec<(usize, Vec<f32>)>> {
                        let mut mine = Vec::new();
                        loop {
                            let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(chunk) = chunks.get(k) else { break };
                            mine.extend(self.embed_batch(texts, chunk, query)?);
                        }
                        Ok(mine)
                    })
                })
                .collect();
            hs.into_iter()
                .map(|h| h.join().map_err(|_| anyhow!("an embed worker panicked"))?)
                .collect::<Result<_>>()
        })?;
        let mut out = vec![Vec::new(); texts.len()];
        for (i, v) in done.into_iter().flatten() {
            out[i] = v;
        }
        Ok(out)
    }

    fn embed_batch(
        &self,
        texts: &[String],
        chunk: &[usize],
        query: bool,
    ) -> Result<Vec<(usize, Vec<f32>)>> {
        let batch: Vec<String> = chunk
            .iter()
            .map(|&i| {
                if query {
                    format!("{QUERY_PREFIX}{}", texts[i])
                } else {
                    texts[i].clone()
                }
            })
            .collect();
        let enc = self
            .tokenizer
            .encode_batch(batch, true)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let ids: Vec<Vec<u32>> = enc.iter().map(|e| e.get_ids().to_vec()).collect();
        let mask: Vec<Vec<u32>> = enc
            .iter()
            .map(|e| e.get_attention_mask().to_vec())
            .collect();
        let ids = Tensor::new(ids, &self.device)?;
        let mask = Tensor::new(mask, &self.device)?;
        let types = ids.zeros_like()?;
        let hidden = self.model.forward(&ids, &types, Some(&mask))?;
        let cls = hidden.narrow(1, 0, 1)?.squeeze(1)?; // [batch, dim]
        let norm = cls.sqr()?.sum_keepdim(1)?.sqrt()?.clamp(1e-12, f64::MAX)?;
        let v: Vec<Vec<f32>> = cls.broadcast_div(&norm)?.to_vec2()?;
        Ok(chunk.iter().copied().zip(v).collect())
    }
}

/// For a bulk embed: makes candle's matrix products single-threaded, so the
/// workers in `Embedder::embed` don't contend with them for cores. candle
/// reads `RAYON_NUM_THREADS` on every product and has no other setting. A
/// person's own `RAYON_NUM_THREADS` is kept (then each worker's products use
/// that many threads).
///
/// # Safety
///
/// No other thread may be running: setting the environment while another
/// thread reads it is undefined behavior. Call it first thing in a command
/// that has started no thread (`agent index embed` does).
pub unsafe fn single_threaded_products() {
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        // SAFETY: the caller guarantees no other thread is running.
        unsafe { std::env::set_var("RAYON_NUM_THREADS", "1") };
    }
}

/// The text embedded for an entity: its project and path, title, summary
/// (or seed), why, and symbol names. None when the entity is gone.
pub fn card(conn: &Connection, id: &str) -> Result<Option<String>> {
    let row = conn
        .prepare_cached(
            "SELECT e.project, e.rel, e.title, s.summary, s.why, s.symbols
             FROM entities e
             LEFT JOIN search_rowid r ON r.id = e.id
             LEFT JOIN search s ON s.rowid = r.rid
             WHERE e.id = ?",
        )?
        .query_row(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            ))
        });
    let (project, rel, title, summary, why, symbols) = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut s = if rel.is_empty() {
        project
    } else {
        format!("{project}/{rel}")
    };
    for part in [title.as_str(), summary.as_str()] {
        if !part.is_empty() && !s.contains(part) {
            s.push('\n');
            s.push_str(part);
        }
    }
    if !why.is_empty() {
        s.push_str("\nWhy: ");
        s.push_str(&why);
    }
    let mut seen = std::collections::HashSet::new();
    let syms: Vec<&str> = symbols
        .split_whitespace()
        .filter(|w| seen.insert(*w))
        .take(SYMBOLS_IN_CARD)
        .collect();
    if !syms.is_empty() {
        s.push_str("\ndefines: ");
        s.push_str(&syms.join(", "));
    }
    let clipped: String = s.chars().take(CARD_CHARS).collect();
    Ok(Some(clipped))
}

fn f16_blob(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 2);
    for x in v {
        b.extend_from_slice(&f16::from_f32(*x).to_le_bytes());
    }
    b
}

/// What a drain did.
#[derive(Debug, Default, Clone)]
pub struct DrainReport {
    pub embedded: usize,
    /// Vectors dropped: their entity is gone or no longer wanted, or they are
    /// another model's once this one is complete.
    pub removed: usize,
    /// Queue rows cleared without embedding: their entity wants no vector,
    /// or its vector already matches.
    pub settled: usize,
    pub remaining: usize,
    pub secs: f64,
}

/// Whether an entity gets a vector: an embedded kind, and not test data. A
/// fixture file, or a directory that is or sits under a fixture directory,
/// answers many words for the feature it records and floods results.
pub fn wants_vector(kind: &str, rel: &str, file_kind: &str) -> bool {
    EMBED_KINDS.contains(&kind)
        && file_kind != "fixture"
        && !(kind == "file" && crate::files::is_fixture(rel))
        && !(kind == "dir" && crate::files::is_fixture(&format!("{rel}/x")))
}

/// Entities embedded per transaction: enough that every worker has batches.
const PER_TX: usize = 512;

/// Brings `vectors` up to date for MODEL_ID: drops the vectors of entities
/// that are gone or no longer wanted, embeds every wanted entity whose vector
/// is missing or was made from other text (so a new model fills itself in,
/// and the queue is a hint, not the only source of work), clears every queue
/// row that is settled, and once this model is complete drops other models'
/// vectors. Stops after `limit` embeddings (0: no limit); what's left is done
/// next time. Each batch is its own transaction.
///
/// The work list is read before embedding, outside a transaction: a crawl
/// that rewrites an entity's text meanwhile can leave a vector of the new
/// text under the old key, and the next drain embeds it again (one wasted
/// embedding, never a wrong one kept).
pub fn drain<E: Encode>(
    conn: &mut Connection,
    emb: &E,
    limit: usize,
    mut progress: impl FnMut(usize, usize),
) -> Result<DrainReport> {
    let start = Instant::now();
    let mut rep = DrainReport::default();
    // Every entity, with whether it wants a vector and the key of the vector it has.
    let rows: Vec<(String, String, bool, Option<String>)> = conn
        .prepare(
            "SELECT e.id, e.text_key, e.kind, e.rel, e.file_kind, v.text_key FROM entities e
             LEFT JOIN vectors v ON v.entity_id = e.id AND v.chunk = 0 AND v.model = ?
             ORDER BY e.importance DESC, e.id",
        )?
        .query_map(params![MODEL_ID], |r| {
            let (kind, rel, fk): (String, String, String) = (r.get(2)?, r.get(3)?, r.get(4)?);
            Ok((
                r.get(0)?,
                r.get(1)?,
                wants_vector(&kind, &rel, &fk),
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let queued: std::collections::HashSet<String> = conn
        .prepare("SELECT entity_id FROM embed_queue")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    {
        let tx = conn.transaction()?;
        rep.removed = tx.execute(
            "DELETE FROM vectors WHERE entity_id NOT IN (SELECT id FROM entities)",
            [],
        )?;
        rep.settled = tx.execute(
            "DELETE FROM embed_queue WHERE entity_id NOT IN (SELECT id FROM entities)",
            [],
        )?;
        // Only entities that have something to drop: a run with nothing
        // changed touches no row.
        for (id, _, wanted, have) in &rows {
            if !wanted && have.is_some() {
                rep.removed += tx
                    .prepare_cached("DELETE FROM vectors WHERE entity_id = ?")?
                    .execute(params![id])?;
            }
            if !wanted && queued.contains(id) {
                rep.settled += tx
                    .prepare_cached("DELETE FROM embed_queue WHERE entity_id = ?")?
                    .execute(params![id])?;
            }
        }
        tx.commit()?;
    }
    let todo: Vec<(&String, &String)> = rows
        .iter()
        .filter(|(_, key, wanted, have)| *wanted && !key.is_empty() && have.as_ref() != Some(key))
        .map(|(id, key, _, _)| (id, key))
        .collect();
    let total = if limit > 0 {
        todo.len().min(limit)
    } else {
        todo.len()
    };
    for batch in todo[..total].chunks(PER_TX) {
        let mut cards = Vec::with_capacity(batch.len());
        let mut keep = Vec::with_capacity(batch.len());
        for &(id, key) in batch {
            if let Some(c) = card(conn, id)? {
                cards.push(c);
                keep.push((id, key));
            }
        }
        let vecs = emb.embed(&cards, false)?;
        let tx = conn.transaction()?;
        for ((id, key), v) in keep.iter().zip(&vecs) {
            tx.prepare_cached(
                "INSERT OR REPLACE INTO vectors (entity_id, chunk, model, text_key, dim, vec)
                 VALUES (?, 0, ?, ?, ?, ?)",
            )?
            .execute(params![id, MODEL_ID, key, v.len() as i64, f16_blob(v)])?;
        }
        tx.commit()?;
        rep.embedded += keep.len();
        progress(rep.embedded, total);
    }
    // Settled queue rows: the entity's vector now has the queued text's key,
    // or the entity has no text to embed.
    rep.settled += conn.execute(
        "DELETE FROM embed_queue WHERE EXISTS (
             SELECT 1 FROM vectors v WHERE v.entity_id = embed_queue.entity_id
             AND v.chunk = 0 AND v.model = ?1 AND v.text_key = embed_queue.text_key)
         OR entity_id IN (SELECT id FROM entities WHERE text_key = '')",
        params![MODEL_ID],
    )?;
    rep.remaining = todo.len() - total;
    if rep.remaining == 0 {
        // This model is complete: other models' vectors are no longer read.
        rep.removed += conn.execute("DELETE FROM vectors WHERE model != ?", params![MODEL_ID])?;
    }
    rep.secs = start.elapsed().as_secs_f64();
    Ok(rep)
}

/// The vectors of one model, in memory, for exact scans.
pub struct Vectors {
    pub ids: Vec<String>,
    pub dim: usize,
    data: Vec<f32>,
}

impl Vectors {
    /// Loads and checks: every blob is `dim` half floats, all of one dim.
    /// A store that fails is refused rather than ranked on.
    pub fn load(conn: &Connection, model: &str) -> Result<Self> {
        let mut st =
            conn.prepare("SELECT entity_id, dim, vec FROM vectors WHERE model = ? AND chunk = 0")?;
        let mut rows = st.query(params![model])?;
        let (mut ids, mut data, mut dim) = (Vec::new(), Vec::new(), 0usize);
        while let Some(r) = rows.next()? {
            let id: String = r.get(0)?;
            let d = r.get::<_, i64>(1)?;
            if !(1..=8192).contains(&d) {
                bail!(
                    "vectors for {model} are corrupt at {id}: dimension {d}; re-run `agent index embed`"
                );
            }
            let d = d as usize;
            let blob: Vec<u8> = r.get(2)?;
            if dim == 0 {
                dim = d;
            }
            if d != dim || blob.len() != d * 2 {
                bail!(
                    "vectors for {model} are inconsistent at {id}: dim {d}, {} bytes (expected dim {dim}, {} bytes); re-run `agent index embed`",
                    blob.len(),
                    dim * 2
                );
            }
            data.extend(
                blob.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| f16::from_le_bytes(*b).to_f32()),
            );
            ids.push(id);
        }
        Ok(Self { ids, dim, data })
    }

    /// The `limit` nearest entities by cosine (vectors are unit length), best first.
    pub fn nearest(&self, q: &[f32], limit: usize) -> Vec<(String, f32)> {
        if self.dim == 0 || q.len() != self.dim {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32)> = self
            .data
            .chunks_exact(self.dim)
            .enumerate()
            .map(|(i, v)| (i, v.iter().zip(q).map(|(a, b)| a * b).sum()))
            .collect();
        let k = limit.min(scored.len());
        if k == 0 {
            return Vec::new();
        }
        scored.select_nth_unstable_by(k - 1, |a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
            .into_iter()
            .map(|(i, s)| (self.ids[i].clone(), s))
            .collect()
    }
}

/// The semantic list for search: the query embedded, then an exact scan.
pub struct VectorSemantic {
    pub embedder: Embedder,
    pub vectors: Vectors,
}

impl VectorSemantic {
    /// None when there are no vectors for the model yet (search runs without
    /// the semantic list, as before).
    pub fn open(conn: &Connection, dir: &Path) -> Result<Option<Self>> {
        let vectors = Vectors::load(conn, MODEL_ID)?;
        if vectors.ids.is_empty() {
            return Ok(None);
        }
        let embedder = Embedder::load(dir)?;
        if embedder.dim != vectors.dim {
            bail!(
                "the model's dimension {} doesn't match the stored vectors' {}",
                embedder.dim,
                vectors.dim
            );
        }
        Ok(Some(Self { embedder, vectors }))
    }
}

impl Semantic for VectorSemantic {
    fn ranked(&self, _: &Connection, query: &str, limit: usize) -> Result<Vec<(String, f64)>> {
        let q = self.embedder.embed(&[query.to_string()], true)?;
        Ok(self
            .vectors
            .nearest(&q[0], limit)
            .into_iter()
            .map(|(id, s)| (id, f64::from(s)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Index;

    /// A stand-in model: a unit vector from the text's bytes, and a count of
    /// texts it embedded.
    struct Fake(std::cell::Cell<usize>);

    impl Encode for Fake {
        fn dim(&self) -> usize {
            4
        }
        fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
            self.0.set(self.0.get() + texts.len());
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = [1.0f32, 0.0, 0.0, 0.0];
                    for (i, b) in t.bytes().enumerate() {
                        v[i % 4] += f32::from(b) / 255.0;
                    }
                    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    v.iter().map(|x| x / n).collect()
                })
                .collect())
        }
    }

    fn entity(ix: &Index, id: &str, kind: &str, rel: &str, file_kind: &str, key: &str) {
        ix.conn
            .execute(
                "INSERT OR REPLACE INTO entities (id, kind, project, rel, name, file_kind, text_key)
                 VALUES (?, ?, 'p', ?, ?, ?, ?)",
                params![id, kind, rel, rel, file_kind, key],
            )
            .unwrap();
        ix.conn
            .execute(
                "INSERT OR REPLACE INTO embed_queue (entity_id, text_key, reason, queued_at)
                 VALUES (?, ?, 'new', '')",
                params![id, key],
            )
            .unwrap();
    }

    fn count(ix: &Index, sql: &str) -> i64 {
        ix.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn drain_embeds_what_is_wanted_settles_the_queue_and_resumes() {
        let mut ix = Index::open_in_memory().unwrap();
        entity(&ix, "p:a.rs", "file", "a.rs", "source", "k1");
        entity(&ix, "p:b.rs", "file", "b.rs", "source", "k1");
        entity(&ix, "p:docs", "dir", "docs", "", "k1");
        entity(&ix, "p:a.rs#f", "symbol", "a.rs", "source", "k1");
        entity(
            &ix,
            "p:testdata/x.jsonl",
            "file",
            "testdata/x.jsonl",
            "fixture",
            "k1",
        );
        entity(&ix, "p:empty", "file", "empty", "source", "");
        let fake = Fake(Default::default());

        // A partial run: one embedding, the rest left for next time.
        let r = drain(&mut ix.conn, &fake, 1, |_, _| {}).unwrap();
        assert_eq!((r.embedded, r.remaining), (1, 2));
        // Symbols and fixtures never get a vector; their queue rows are settled.
        assert_eq!(
            count(
                &ix,
                "SELECT count(*) FROM embed_queue WHERE entity_id IN ('p:a.rs#f', 'p:testdata/x.jsonl')"
            ),
            0
        );

        let r = drain(&mut ix.conn, &fake, 0, |_, _| {}).unwrap();
        assert_eq!((r.embedded, r.remaining), (2, 0));
        assert_eq!(count(&ix, "SELECT count(*) FROM vectors"), 3);
        assert_eq!(
            count(&ix, "SELECT count(*) FROM embed_queue"),
            0,
            "every queue row settled"
        );

        // Nothing changed: nothing embedded, nothing written.
        let before = fake.0.get();
        let r = drain(&mut ix.conn, &fake, 0, |_, _| {}).unwrap();
        assert_eq!((r.embedded, r.removed, r.settled), (0, 0, 0));
        assert_eq!(fake.0.get(), before);

        // New text: re-embedded under its new key. A text changed and changed
        // back before a drain: its queue row is settled without embedding.
        entity(&ix, "p:a.rs", "file", "a.rs", "source", "k2");
        ix.conn
            .execute(
                "UPDATE embed_queue SET text_key = 'k1' WHERE entity_id = 'p:a.rs'",
                [],
            )
            .unwrap();
        entity(&ix, "p:b.rs", "file", "b.rs", "source", "k1");
        let r = drain(&mut ix.conn, &fake, 0, |_, _| {}).unwrap();
        assert_eq!(r.embedded, 1);
        assert_eq!(
            count(
                &ix,
                "SELECT count(*) FROM vectors WHERE entity_id = 'p:a.rs' AND text_key = 'k2'"
            ),
            1
        );
        assert_eq!(
            count(
                &ix,
                "SELECT count(*) FROM embed_queue WHERE entity_id = 'p:b.rs'"
            ),
            0
        );

        // A file that becomes a fixture, one that is removed, and another
        // model's vector: all dropped.
        ix.conn
            .execute(
                "UPDATE entities SET file_kind = 'fixture' WHERE id = 'p:b.rs'",
                [],
            )
            .unwrap();
        ix.conn
            .execute("DELETE FROM entities WHERE id = 'p:docs'", [])
            .unwrap();
        ix.conn
            .execute(
                "INSERT INTO vectors (entity_id, chunk, model, text_key, dim, vec) VALUES ('p:a.rs', 0, 'old', 'k', 1, x'0000')",
                [],
            )
            .unwrap();
        let r = drain(&mut ix.conn, &fake, 0, |_, _| {}).unwrap();
        assert_eq!(r.removed, 3);
        assert_eq!(count(&ix, "SELECT count(*) FROM vectors"), 1);
    }

    #[test]
    fn a_corrupt_store_is_refused() {
        let ix = Index::open_in_memory().unwrap();
        for (id, dim, blob) in [("a", 2i64, vec![0u8; 4]), ("b", 2, vec![0u8; 3])] {
            ix.conn
                .execute(
                    "INSERT INTO vectors (entity_id, chunk, model, text_key, dim, vec) VALUES (?, 0, 'm', 'k', ?, ?)",
                    params![id, dim, blob],
                )
                .unwrap();
        }
        assert!(Vectors::load(&ix.conn, "m").is_err(), "a short blob");
        ix.conn
            .execute("DELETE FROM vectors WHERE entity_id = 'b'", [])
            .unwrap();
        ix.conn
            .execute(
                "INSERT INTO vectors (entity_id, chunk, model, text_key, dim, vec) VALUES ('c', 0, 'm', 'k', -1, x'')",
                [],
            )
            .unwrap();
        assert!(
            Vectors::load(&ix.conn, "m").is_err(),
            "a negative dimension"
        );
    }

    #[test]
    fn f16_round_trip_and_nearest() {
        let a = [0.6f32, 0.8, 0.0];
        let b = [0.0f32, 0.0, 1.0];
        let mut data = Vec::new();
        for v in [&a, &b] {
            let blob = f16_blob(v);
            data.extend(
                blob.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| f16::from_le_bytes(*c).to_f32()),
            );
        }
        let vs = Vectors {
            ids: vec!["a".into(), "b".into()],
            dim: 3,
            data,
        };
        let got = vs.nearest(&[0.0, 0.0, 1.0], 5);
        assert_eq!(got[0].0, "b");
        assert_eq!(got.len(), 2);
        assert!((got[0].1 - 1.0).abs() < 1e-3);
        assert!(
            vs.nearest(&[1.0, 0.0], 5).is_empty(),
            "a wrong-dimension query matches nothing"
        );
    }

    #[test]
    fn fixtures_and_symbols_get_no_vector() {
        assert!(wants_vector("file", "src/lib.rs", "source"));
        assert!(wants_vector("dir", "src", ""));
        assert!(wants_vector("section", "docs/X.md", ""));
        assert!(!wants_vector("symbol", "src/lib.rs", "source"));
        assert!(!wants_vector("file", "tests/testdata/a.rs", "source"));
        assert!(!wants_vector("file", "x.jsonl", "fixture"));
        assert!(!wants_vector("dir", "crates/t/testdata", ""));
        assert!(!wants_vector("dir", "crates/t/testdata/corpus/agy", ""));
    }
}

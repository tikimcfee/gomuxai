//! The embedders beyond bge-small (docs/INDEX-V2.md, "A code-trained
//! embedder"): Qwen3-Embedding-0.6B and CodeRankEmbed on the CPU by candle,
//! and any model behind an OpenAI-compatible `/v1/embeddings` server (vLLM's
//! pooling runner on the GPU, for a bulk pass or as a query server). Each
//! gives L2-normalized vectors, with the model's own query instruction on
//! queries and none on documents.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::{nomic_bert, qwen3};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::embed::Encode;

pub use crate::pg::vecs::{SPECS, Spec, query_text, spec};

/// The model's files in the Hugging Face cache (`HF_HUB_CACHE`, else
/// `HF_HOME/hub`, else `~/.cache/huggingface/hub`), or `given`: the snapshot
/// `refs/main` names, else any complete one.
pub fn model_dir(repo: &str, given: Option<&Path>) -> Result<PathBuf> {
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
    let dir = hub.join(format!("models--{}", repo.replace('/', "--")));
    let complete =
        |p: &Path| p.join("model.safetensors").exists() && p.join("tokenizer.json").exists();
    if let Ok(main) = std::fs::read_to_string(dir.join("refs/main")) {
        let p = dir.join("snapshots").join(main.trim());
        if complete(&p) {
            return Ok(p);
        }
    }
    let snaps = dir.join("snapshots");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&snaps)
        .with_context(|| format!("no {repo} at {}", snaps.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| complete(p))
        .collect();
    found.sort();
    found
        .pop()
        .ok_or_else(|| anyhow!("no complete snapshot of {repo} under {}", snaps.display()))
}

/// A text cut at a token boundary to at most `limit` tokens (without the
/// special tokens): what the model reads of it. The server and candle both
/// embed the cut text, so an over-long window gives one vector either way
/// rather than two truncations (vLLM's `truncate_prompt_tokens` keeps a
/// prompt's last tokens; candle keeps its first).
pub fn fit<'a>(tok: &Tokenizer, text: &'a str, limit: usize) -> &'a str {
    let Ok(enc) = tok.encode(text, false) else {
        return text;
    };
    if enc.len() <= limit {
        return text;
    }
    let mut n = limit;
    loop {
        // Byte offsets of the token that ends the cut.
        let mut end = enc
            .get_offsets()
            .get(n.saturating_sub(1))
            .map_or(0, |o| o.1);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let cut = &text[..end.min(text.len())];
        // Tokens can merge differently at the new end: cut again until the
        // cut text itself fits.
        match tok.encode(cut, false) {
            Ok(e) if e.len() > limit && n > 1 => n -= 1,
            _ => return cut,
        }
    }
}

/// Cuts a vector to its first `dims` (Matryoshka) and renormalizes it.
pub fn cut(mut v: Vec<f32>, dims: usize) -> Vec<f32> {
    if dims > 0 && dims < v.len() {
        v.truncate(dims);
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    v.iter_mut().for_each(|x| *x /= n);
    v
}

fn workers(max: usize, n: usize) -> usize {
    let half = std::thread::available_parallelism().map_or(4, |n| n.get()) / 2;
    let cap = if max > 0 { max.min(half.max(1)) } else { half };
    cap.clamp(1, n.max(1))
}

/// Runs `f` over the texts on worker threads, in order.
fn par<F>(texts: &[String], max_workers: usize, f: F) -> Result<Vec<Vec<f32>>>
where
    F: Fn(&str) -> Result<Vec<f32>> + Sync,
{
    let next = std::sync::atomic::AtomicUsize::new(0);
    let n = workers(max_workers, texts.len());
    let done: Vec<Vec<(usize, Vec<f32>)>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..n)
            .map(|_| {
                sc.spawn(|| -> Result<Vec<(usize, Vec<f32>)>> {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(t) = texts.get(i) else { break };
                        mine.push((i, f(t)?));
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

/// Qwen3-Embedding on the CPU: a causal decoder, the last token's hidden
/// state (after the final norm) as the vector. One sequence at a time (the
/// model has no padding mask on the CPU), sequences spread over workers.
pub struct Qwen3 {
    model: qwen3::Model,
    tokenizer: Tokenizer,
    eos: Option<u32>,
    spec: &'static Spec,
    instruction: String,
    pub dims: usize,
    pub max_workers: usize,
    pub max_tokens: usize,
}

impl Qwen3 {
    pub fn load(dir: &Path, instruction: &str, dims: usize) -> Result<Qwen3> {
        let spec = spec("qwen3-embedding-0.6b")?;
        let cfg: qwen3::Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json")).context("config.json")?,
        )?;
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("tokenizer.json: {e}"))?;
        let eos = tokenizer.token_to_id("<|endoftext|>");
        // SAFETY: the file is mapped read-only and not changed while we run.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &Device::Cpu,
            )?
        };
        // The embedding model's weights have no "model." prefix; candle's
        // Qwen3 reads them under it.
        let vb = if vb.contains_tensor("model.embed_tokens.weight") {
            vb
        } else {
            vb.rename_f(|n: &str| n.strip_prefix("model.").unwrap_or(n).to_string())
        };
        let model = qwen3::Model::new(&cfg, vb)?;
        Ok(Qwen3 {
            model,
            tokenizer,
            eos,
            spec,
            instruction: instruction.to_string(),
            dims,
            max_workers: 0,
            max_tokens: spec.max_tokens,
        })
    }

    fn one(&self, text: &str) -> Result<Vec<f32>> {
        // Cut as the server's texts are ([`fit`]), so both read the same.
        let text = fit(
            &self.tokenizer,
            text,
            self.max_tokens.saturating_sub(1).max(1),
        );
        let enc = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let mut ids: Vec<u32> = enc.get_ids().to_vec();
        ids.truncate(self.max_tokens.saturating_sub(1).max(1));
        // The model card's tokenizer appends <|endoftext|>: the pooled token.
        if let Some(e) = self.eos
            && ids.last() != Some(&e)
        {
            ids.push(e);
        }
        let n = ids.len();
        let input = Tensor::new(ids, &Device::Cpu)?.unsqueeze(0)?;
        // A fresh copy for each sequence: the model keeps a KV cache, and
        // its weights are shared, so a copy is cheap.
        let mut m = self.model.clone();
        let h = m.forward(&input, 0)?; // [1, n, hidden]
        let last: Vec<f32> = h.narrow(1, n - 1, 1)?.squeeze(1)?.squeeze(0)?.to_vec1()?;
        Ok(cut(last, self.dims))
    }
}

impl Encode for Qwen3 {
    fn dim(&self) -> usize {
        if self.dims > 0 {
            self.dims
        } else {
            self.spec.dims
        }
    }
    fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<String> = if query {
            texts
                .iter()
                .map(|t| query_text(self.spec, &self.instruction, t))
                .collect()
        } else {
            texts.to_vec()
        };
        par(&texts, self.max_workers, |t| self.one(t))
    }
}

/// CodeRankEmbed (a NomicBert encoder) on the CPU, batched with a padding
/// mask; `pooling` is how its sentence-transformers config pools.
pub struct Nomic {
    model: nomic_bert::NomicBertModel,
    tokenizer: Tokenizer,
    spec: &'static Spec,
    instruction: String,
    cls: bool,
    pub dims: usize,
    pub max_workers: usize,
}

impl Nomic {
    pub fn load(dir: &Path, instruction: &str, dims: usize, max_tokens: usize) -> Result<Nomic> {
        let spec = spec("coderankembed")?;
        let cfg: nomic_bert::Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json")).context("config.json")?,
        )?;
        // sentence-transformers' pooling: CLS or mean.
        let pool = std::fs::read_to_string(dir.join("1_Pooling/config.json")).unwrap_or_default();
        let cls = pool.contains("\"pooling_mode_cls_token\": true");
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("tokenizer.json: {e}"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_tokens.min(spec.max_tokens),
                ..Default::default()
            }))
            .map_err(|e| anyhow!("truncation: {e}"))?;
        // SAFETY: as above.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &Device::Cpu,
            )?
        };
        let model = nomic_bert::NomicBertModel::load(vb, &cfg)?;
        Ok(Nomic {
            model,
            tokenizer,
            spec,
            instruction: instruction.to_string(),
            cls,
            dims,
            max_workers: 0,
        })
    }

    fn batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let enc = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let ids: Vec<Vec<u32>> = enc.iter().map(|e| e.get_ids().to_vec()).collect();
        let mask: Vec<Vec<u32>> = enc
            .iter()
            .map(|e| e.get_attention_mask().to_vec())
            .collect();
        let ids = Tensor::new(ids, &Device::Cpu)?;
        let mask = Tensor::new(mask, &Device::Cpu)?;
        let h = self.model.forward(&ids, None, Some(&mask))?;
        let pooled = if self.cls {
            h.narrow(1, 0, 1)?.squeeze(1)?
        } else {
            nomic_bert::mean_pooling(&h, &mask)?
        };
        let v: Vec<Vec<f32>> = pooled.to_vec2()?;
        Ok(v.into_iter().map(|x| cut(x, self.dims)).collect())
    }
}

impl Encode for Nomic {
    fn dim(&self) -> usize {
        if self.dims > 0 {
            self.dims
        } else {
            self.spec.dims
        }
    }
    fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<String> = if query {
            texts
                .iter()
                .map(|t| query_text(self.spec, &self.instruction, t))
                .collect()
        } else {
            texts.to_vec()
        };
        // Batches of 8 of similar length, spread over workers.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let groups: Vec<Vec<String>> = order
            .chunks(8)
            .map(|c| c.iter().map(|&i| texts[i].clone()).collect())
            .collect();
        let keys: Vec<String> = (0..groups.len()).map(|i| i.to_string()).collect();
        let done = par(&keys, self.max_workers, |k| {
            let g = &groups[k.parse::<usize>().unwrap_or(0)];
            Ok(self.batch(g)?.into_iter().flatten().collect())
        })?;
        let dim = self.dim();
        let mut out = vec![Vec::new(); texts.len()];
        for (gi, flat) in done.into_iter().enumerate() {
            for (j, v) in flat.chunks(dim).enumerate() {
                out[order[gi * 8 + j]] = v.to_vec();
            }
        }
        Ok(out)
    }
}

/// Any model behind an OpenAI-compatible `/v1/embeddings` (vLLM's pooling
/// runner): the query instruction is added here, as for the local models.
pub struct Http {
    pub endpoint: String,
    pub served: String,
    spec: &'static Spec,
    instruction: String,
    pub dims: usize,
    pub batch: usize,
    /// Texts are cut at this many tokens: here, with the model's tokenizer
    /// when it's on this host ([`fit`], as candle cuts), and by the server.
    pub max_tokens: usize,
    /// The model's tokenizer, for [`fit`] (Qwen3's, whose candle path cuts
    /// the same way).
    pub tokenizer: Option<Arc<Tokenizer>>,
    agent: ureq::Agent,
}

impl Http {
    pub fn new(
        endpoint: &str,
        served: &str,
        model: &str,
        instruction: &str,
        dims: usize,
    ) -> Result<Http> {
        Ok(Http {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            // The name the server knows it by: the model's own, unless given.
            served: if served.is_empty() { model } else { served }.to_string(),
            spec: spec(model)?,
            instruction: instruction.to_string(),
            dims,
            batch: 64,
            max_tokens: 2048,
            tokenizer: None,
            agent: agent_with(std::time::Duration::from_secs(300)),
        })
    }

    /// Requests give up after `t`.
    pub fn with_timeout(mut self, t: std::time::Duration) -> Http {
        self.agent = agent_with(t);
        self
    }

    /// Cuts texts with this model's tokenizer from `dir` ([`fit`]); Qwen3's
    /// only (its candle path cuts the same way).
    pub fn with_tokenizer(mut self, dir: &Path) -> Result<Http> {
        if self.spec.slug == "qwen3" {
            let t = Tokenizer::from_file(dir.join("tokenizer.json"))
                .map_err(|e| anyhow!("tokenizer.json: {e}"))?;
            self.tokenizer = Some(Arc::new(t));
        }
        Ok(self)
    }
}

fn agent_with(t: std::time::Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(t))
        .build()
        .into()
}

impl Encode for Http {
    fn dim(&self) -> usize {
        if self.dims > 0 {
            self.dims
        } else {
            self.spec.dims
        }
    }
    fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        // Up to 4 requests at once, so the server batches across them.
        let parts: Vec<&[String]> = texts.chunks(self.batch.max(1)).collect();
        let mut out = Vec::with_capacity(texts.len());
        // A batch's pause file (AGENT_INDEX_EMBED_PAUSE): while it exists, no
        // new request goes out (the GPU's other users come first).
        let pause = std::env::var_os("AGENT_INDEX_EMBED_PAUSE").filter(|v| !v.is_empty());
        for group in parts.chunks(4) {
            while pause.as_ref().is_some_and(|p| Path::new(p).exists()) {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let got: Vec<Result<Vec<Vec<f32>>>> = std::thread::scope(|sc| {
                let hs: Vec<_> = group
                    .iter()
                    .map(|p| sc.spawn(move || self.request(p, query)))
                    .collect();
                hs.into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(anyhow!("an embedding request panicked")))
                    })
                    .collect()
            });
            for g in got {
                out.extend(g?);
            }
        }
        Ok(out)
    }
}

impl Http {
    /// One request: `part`'s vectors.
    fn request(&self, part: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(part.len());
        let input: Vec<String> = part
            .iter()
            .map(|t| {
                let t = if query {
                    query_text(self.spec, &self.instruction, t)
                } else {
                    t.clone()
                };
                match &self.tokenizer {
                    // One token is the pooled end token the server adds.
                    Some(tok) => fit(tok, &t, self.max_tokens.saturating_sub(1).max(1)).to_string(),
                    None => t,
                }
            })
            .collect();
        // The server cuts as candle does: at the configured tokens.
        let body = serde_json::json!({"model": self.served, "input": input,
                                      "truncate_prompt_tokens": self.max_tokens});
        let text = self
            .agent
            .post(&format!("{}/v1/embeddings", self.endpoint))
            .header("content-type", "application/json")
            .send(serde_json::to_vec(&body)?.as_slice())
            .with_context(|| format!("asking {} for embeddings", self.endpoint))?
            .body_mut()
            .read_to_string()?;
        let v: serde_json::Value = serde_json::from_str(&text)?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("no data in the embeddings answer"))?;
        if data.len() != part.len() {
            bail!("{} embeddings for {} texts", data.len(), part.len());
        }
        for d in data {
            let e: Vec<f32> = d["embedding"]
                .as_array()
                .ok_or_else(|| anyhow!("no embedding"))?
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect();
            out.push(cut(e, self.dims));
        }
        Ok(out)
    }
}

/// An encoder threads may share.
pub type Shared = Arc<dyn Encode + Send + Sync>;

/// The live embedder for a model with a server: the server first (a short
/// timeout for a query, a longer one for a batch), candle on the CPU when
/// it doesn't answer. A failed server is left alone for `recheck` (so a
/// dead one costs one timeout, not one per query), then asked again. Where
/// each query's vector was made is noted for the search (vecs::EmbedOn),
/// and the server's health for `agent index serve --status`.
pub struct Served {
    query: Http,
    batch: Http,
    local: std::sync::OnceLock<std::result::Result<Shared, String>>,
    loader: Box<dyn Fn() -> Result<Shared> + Send + Sync>,
    model: String,
    recheck: std::time::Duration,
    down_until: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Served {
    pub fn new(
        query: Http,
        batch: Http,
        model: &str,
        recheck: std::time::Duration,
        loader: Box<dyn Fn() -> Result<Shared> + Send + Sync>,
    ) -> Served {
        crate::pg::vecs::note_server(|_| {}, &query.endpoint, model);
        Served {
            query,
            batch,
            local: std::sync::OnceLock::new(),
            loader,
            model: model.to_string(),
            recheck,
            down_until: std::sync::Mutex::new(None),
        }
    }

    /// The CPU's encoder, loaded the first time it's needed.
    fn local(&self) -> Result<&Shared> {
        self.local
            .get_or_init(|| (self.loader)().map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| anyhow!("the CPU embedder for {}: {e}", self.model))
    }

    /// Whether the server is being left alone, and until when.
    fn backing_off(&self) -> Option<std::time::Instant> {
        let d = self.down_until.lock().unwrap_or_else(|e| e.into_inner());
        d.filter(|t| std::time::Instant::now() < *t)
    }
}

impl Encode for Served {
    fn dim(&self) -> usize {
        self.query.dim()
    }
    fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        use crate::pg::vecs::{EmbedOn, note_embed_on, note_server};
        let ep = self.query.endpoint.clone();
        let why = match self.backing_off() {
            Some(t) => format!(
                "the embedding server at {ep} failed; asked again in {} s",
                t.saturating_duration_since(std::time::Instant::now())
                    .as_secs()
                    + 1
            ),
            None => {
                let got = if query {
                    self.query.embed(texts, true)
                } else {
                    self.batch.embed(texts, false)
                };
                match got {
                    Ok(v) => {
                        *self.down_until.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        note_server(
                            |s| {
                                s.up = true;
                                s.down_since.clear();
                                s.error.clear();
                                s.served += texts.len() as u64;
                            },
                            &ep,
                            &self.model,
                        );
                        if query {
                            note_embed_on(EmbedOn {
                                model: self.model.clone(),
                                on: "gpu".into(),
                                note: String::new(),
                            });
                        }
                        return Ok(v);
                    }
                    Err(e) => {
                        *self.down_until.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(std::time::Instant::now() + self.recheck);
                        let err = format!("{e:#}");
                        note_server(
                            |s| {
                                if s.up || s.down_since.is_empty() {
                                    s.down_since = chrono::Utc::now()
                                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                                }
                                s.up = false;
                                s.error = err.clone();
                            },
                            &ep,
                            &self.model,
                        );
                        format!("the embedding server at {ep} isn't answering ({err})")
                    }
                }
            }
        };
        let v = self.local()?.embed(texts, query)?;
        note_server(|s| s.fell_back += texts.len() as u64, &ep, &self.model);
        if query {
            note_embed_on(EmbedOn {
                model: self.model.clone(),
                on: "cpu".into(),
                note: why,
            });
        }
        Ok(v)
    }
}

/// The configured embedder: `[embedder]`'s model on the CPU (candle), with
/// at most `max_workers` threads of its own (0: half the logical CPUs), or
/// its server (`backend = "http"`).
pub fn load(cfg: &crate::config::Embedder, max_workers: usize) -> Result<Shared> {
    load_model(cfg, &cfg.model, max_workers)
}

/// [`load`] for a named model (one of the configuration's corpus models).
pub fn load_model(
    cfg: &crate::config::Embedder,
    model: &str,
    max_workers: usize,
) -> Result<Shared> {
    let main = model == cfg.model;
    let mut cfg = cfg.clone();
    if !main {
        // `model_dir` and the query instruction are the searched model's,
        // and so is the live server (an `http` backend, a bulk pass's
        // --endpoint, serves the model it is given).
        cfg.model_dir.clear();
        cfg.query_instruction.clear();
        if cfg.backend != "http" {
            cfg.endpoint.clear();
        }
    }
    cfg.model = model.to_string();
    let s = spec(&cfg.model)?;
    let given = (!cfg.model_dir.is_empty()).then(|| PathBuf::from(&cfg.model_dir));
    let http = |timeout: std::time::Duration| -> Result<Http> {
        let mut h = Http::new(
            &cfg.endpoint,
            &cfg.served,
            &cfg.model,
            &cfg.query_instruction,
            cfg.dims,
        )?
        .with_timeout(timeout);
        h.max_tokens = cfg.max_tokens.clamp(16, s.max_tokens);
        // Cut as candle does, when the model's files are here.
        if let Ok(dir) = model_dir(s.repo, given.as_deref()) {
            h = h.with_tokenizer(&dir)?;
        }
        Ok(h)
    };
    if cfg.backend == "http" {
        return Ok(Arc::new(http(std::time::Duration::from_secs(300))?));
    }
    if !cfg.endpoint.is_empty() {
        let local_cfg = {
            let mut c = cfg.clone();
            c.endpoint.clear();
            c
        };
        let name = cfg.model.clone();
        let loader: Box<dyn Fn() -> Result<Shared> + Send + Sync> =
            Box::new(move || load_model(&local_cfg, &name, max_workers));
        return Ok(Arc::new(Served::new(
            http(std::time::Duration::from_millis(cfg.timeout_ms))?,
            http(std::time::Duration::from_secs(cfg.batch_timeout_s))?,
            &cfg.model,
            std::time::Duration::from_secs(cfg.recheck_s),
            loader,
        )));
    }
    let cfg = &cfg;
    let dir = model_dir(s.repo, given.as_deref())?;
    match s.slug {
        "bge" => {
            let mut m = crate::embed::Embedder::load(&dir)?;
            m.max_workers = max_workers;
            Ok(Arc::new(m))
        }
        "qwen3" => {
            let mut m = Qwen3::load(&dir, &cfg.query_instruction, cfg.dims)?;
            m.max_tokens = cfg.max_tokens.max(16);
            m.max_workers = max_workers;
            Ok(Arc::new(m))
        }
        "crk" => {
            let mut m = Nomic::load(
                &dir,
                &cfg.query_instruction,
                cfg.dims,
                cfg.max_tokens.max(16),
            )?;
            m.max_workers = max_workers;
            Ok(Arc::new(m))
        }
        _ => bail!("no loader for {}", s.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pg::vecs::{servers, take_embed_on};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::time::{Duration, Instant};

    /// The CPU's stand-in: every vector [1, 0, ...].
    struct Cpu;
    impl Encode for Cpu {
        fn dim(&self) -> usize {
            4
        }
        fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
        }
    }

    fn served(endpoint: &str, recheck: Duration) -> Served {
        let h = |t| {
            Http::new(endpoint, "", "qwen3-embedding-0.6b", "", 4)
                .unwrap()
                .with_timeout(t)
        };
        Served::new(
            h(Duration::from_millis(300)),
            h(Duration::from_secs(2)),
            "qwen3-embedding-0.6b",
            recheck,
            Box::new(|| Ok(Arc::new(Cpu) as Shared)),
        )
    }

    /// A server that answers /v1/embeddings with [0, 1, 0, 0] per input,
    /// `n` times; its port.
    fn fake_server(n: usize) -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming().take(n) {
                let mut s = s.unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).unwrap();
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let n = req["input"].as_array().unwrap().len();
                let data: Vec<_> = (0..n)
                    .map(|_| serde_json::json!({"embedding": [0.0, 1.0, 0.0, 0.0]}))
                    .collect();
                let out = serde_json::json!({ "data": data }).to_string();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
                    out.len()
                );
            }
        });
        port
    }

    #[test]
    fn served_falls_back_to_the_cpu_and_backs_off() {
        // Nothing listens on this port.
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let ep = format!("http://127.0.0.1:{dead}");
        let s = served(&ep, Duration::from_secs(60));
        let v = s.embed(&["where is it".to_string()], true).unwrap();
        assert_eq!(v[0][0], 1.0, "the CPU's vector");
        let on = take_embed_on().expect("noted");
        assert_eq!(on.on, "cpu");
        assert!(on.note.contains("isn't answering"), "{}", on.note);
        let st = servers().into_iter().find(|x| x.endpoint == ep).unwrap();
        assert!(!st.up && !st.down_since.is_empty(), "{st:?}");
        assert_eq!(st.fell_back, 1);
        // Within the back-off the server isn't asked: no timeout paid.
        let t = Instant::now();
        s.embed(&["again".to_string()], true).unwrap();
        assert!(t.elapsed() < Duration::from_millis(100));
        let on = take_embed_on().unwrap();
        assert!(on.note.contains("asked again in"), "{}", on.note);
        // A batch goes to the CPU too.
        assert_eq!(s.embed(&["a".into(), "b".into()], false).unwrap().len(), 2);
    }

    #[test]
    fn served_uses_the_server_when_it_answers() {
        let port = fake_server(3);
        let ep = format!("http://127.0.0.1:{port}");
        let s = served(&ep, Duration::from_millis(1));
        let v = s.embed(&["q".to_string()], true).unwrap();
        assert_eq!(v[0][1], 1.0, "the server's vector");
        assert_eq!(take_embed_on().unwrap().on, "gpu");
        let v = s
            .embed(&["one".to_string(), "two".to_string()], false)
            .unwrap();
        assert_eq!(v.len(), 2);
        let st = servers().into_iter().find(|x| x.endpoint == ep).unwrap();
        assert!(st.up, "{st:?}");
        assert_eq!(st.served, 3);
    }

    #[test]
    fn fit_cuts_at_a_token_boundary() {
        let Ok(dir) = model_dir("Qwen/Qwen3-Embedding-0.6B", None) else {
            return;
        };
        let tok = Tokenizer::from_file(dir.join("tokenizer.json")).unwrap();
        let short = "fn main() {}";
        assert_eq!(fit(&tok, short, 100), short);
        let long = "let x = deliver_held(agent, msg);\n".repeat(400);
        let cut = fit(&tok, &long, 512);
        assert!(long.starts_with(cut) && cut.len() < long.len());
        let n = tok.encode(cut, false).unwrap().len();
        assert!(n <= 512 && n > 500, "{n}");
    }
}

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
    /// Texts are cut at this many tokens (by the server).
    pub max_tokens: usize,
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
            served: served.to_string(),
            spec: spec(model)?,
            instruction: instruction.to_string(),
            dims,
            batch: 64,
            max_tokens: 2048,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(300)))
                .build()
                .into(),
        })
    }
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
                if query {
                    query_text(self.spec, &self.instruction, t)
                } else {
                    t.clone()
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
    let mut cfg = cfg.clone();
    if model != cfg.model {
        // `model_dir` and the query instruction are the default model's.
        cfg.model_dir.clear();
        cfg.query_instruction.clear();
    }
    cfg.model = model.to_string();
    let cfg = &cfg;
    let s = spec(&cfg.model)?;
    if cfg.backend == "http" {
        let mut h = Http::new(
            &cfg.endpoint,
            &cfg.served,
            &cfg.model,
            &cfg.query_instruction,
            cfg.dims,
        )?;
        h.max_tokens = cfg.max_tokens.clamp(16, s.max_tokens);
        return Ok(Arc::new(h));
    }
    let given = (!cfg.model_dir.is_empty()).then(|| PathBuf::from(&cfg.model_dir));
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

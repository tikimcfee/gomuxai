//! The reranker's client (docs/INDEX-V2.md, "Reranker"): a cross-encoder
//! served by vLLM's pooling runner, asked over plain HTTP on the local host
//! for one score per (query, candidate) pair (`POST /score`, scores in the
//! candidates' order). The candidates' texts are made here: an entity's
//! card (path, title, summary or seed, the names it defines), and the chunk
//! of its file the query best matches (a symbol's or section's own chunk),
//! clipped to `[reranker].doc_chars`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::Pg;
use crate::config::Reranker;

/// A reranker client; it keeps its connection between searches (the warm
/// process holds one).
pub struct Client {
    agent: ureq::Agent,
    cfg: Reranker,
}

impl Client {
    pub fn new(cfg: &Reranker) -> Client {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(cfg.timeout_ms.max(1))))
            .build()
            .into();
        Client {
            agent,
            cfg: cfg.clone(),
        }
    }

    /// One score per document, in their order (higher is more relevant).
    pub fn score(&self, query: &str, docs: &[String]) -> Result<Vec<f64>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/score", self.cfg.endpoint.trim_end_matches('/'));
        let mut body = json!({
            "model": self.cfg.model,
            "queries": query,
            "documents": docs,
            "instruction": self.cfg.instruction,
        });
        if self.cfg.max_tokens_per_doc > 0 {
            body["max_tokens_per_doc"] = json!(self.cfg.max_tokens_per_doc);
        }
        let mut resp = self
            .agent
            .post(&url)
            .header("content-type", "application/json")
            .send(serde_json::to_vec(&body)?.as_slice())
            .with_context(|| format!("asking the reranker at {url}"))?;
        let text = resp
            .body_mut()
            .read_to_string()
            .context("reading the reranker's answer")?;
        let v: Value = serde_json::from_str(&text).context("the reranker's answer isn't JSON")?;
        let data = v
            .get("data")
            .and_then(Value::as_array)
            .context("the reranker's answer has no data")?;
        let mut out = vec![f64::NAN; docs.len()];
        for d in data {
            let (Some(i), Some(s)) = (
                d.get("index").and_then(Value::as_u64),
                d.get("score").and_then(Value::as_f64),
            ) else {
                continue;
            };
            if let Some(slot) = out.get_mut(i as usize) {
                *slot = s;
            }
        }
        if out.iter().any(|s| s.is_nan()) {
            bail!(
                "the reranker scored {} of {} candidates",
                data.len(),
                docs.len()
            );
        }
        Ok(out)
    }
}

/// The text the reranker reads for each id, in their order: the card, then
/// the best chunk (by the query's terms), clipped to `doc_chars`.
pub fn texts(pg: &Pg, ids: &[String], terms: &[String]) -> Result<Vec<String>> {
    let rows = pg.query(
        "WITH q AS (SELECT tsq_or(plainto_tsquery('simple', t)) AS s, tsq_or(plainto_tsquery('english', t)) AS e
                    FROM unnest($2::text[]) AS t)
         SELECT e.id, e.kind, e.project, e.rel, e.title, e.seed, e.summary, e.why, e.symbols, e.name, e.signature,
                (SELECT c.text FROM chunks c
                 WHERE c.entity_id = split_part(e.id, '#', 1)
                   AND (e.kind = 'file' OR (e.line BETWEEN c.line_start AND c.line_end))
                 ORDER BY greatest(ts_rank_cd(c.tsv_simple, q.s), ts_rank_cd(c.tsv_en, q.e)) DESC, c.ord
                 LIMIT 1)
         FROM entities e, q WHERE e.id = ANY($1)",
        &[&ids, &terms],
    )?;
    let cfg = &pg.cfg;
    let mut by_id = std::collections::HashMap::new();
    for r in rows {
        let id: String = r.get(0);
        let kind: String = r.get(1);
        let (seed, summary): (String, String) = (r.get(5), r.get(6));
        let shown = if summary.is_empty() { seed } else { summary };
        let mut t = super::sync::card(
            &r.get::<_, String>(2),
            &r.get::<_, String>(3),
            &r.get::<_, String>(4),
            &shown,
            &r.get::<_, String>(7),
            &r.get::<_, String>(8),
            &cfg.cards,
        );
        if kind == "symbol" {
            // A symbol's card is its file's path and doc: add its own name
            // and signature.
            let (name, sig): (String, String) = (r.get(9), r.get(10));
            t = format!("{t}\n{kind} {}", if sig.is_empty() { name } else { sig });
        }
        if let Some(chunk) = r.get::<_, Option<String>>(11) {
            t.push_str("\n\n");
            t.push_str(&chunk);
        }
        by_id.insert(
            id,
            t.chars().take(cfg.reranker.doc_chars).collect::<String>(),
        );
    }
    Ok(ids
        .iter()
        .map(|i| by_id.remove(i).unwrap_or_else(|| i.clone()))
        .collect())
}

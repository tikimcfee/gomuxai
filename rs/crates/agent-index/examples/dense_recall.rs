//! The dense lists alone, before fusion (docs/INDEX-V2.md, "A code-trained
//! embedder"): for each graded question, a model's nearest cards and chunk
//! windows at the question's scope (its project, or the tree for a "*" or
//! "projects" question), `--kind file` as the eval asks, rolled up to files;
//! the top 10 files of the cards, of the chunks, and of the two fused by RRF
//! (k = 60). Prints hit@10, recall@10 (grade >= 1) and MRR@10 per set and list.
//!
//! `dense_recall --config FILE --model NAME [--depth N] QRELS...`
//! The configuration names the store; `[embedder]` settings (dims, chunk
//! header) choose the model's tables as the search would.
use std::collections::{BTreeMap, HashMap};

use agent_index::config::Config;
use agent_index::pg::{Pg, vec_text, vecs};
use anyhow::{Context, Result, bail};

#[derive(Default, Clone, Copy)]
struct Score {
    n: usize,
    hit: f64,
    recall: f64,
    mrr: f64,
}

impl Score {
    fn add(&mut self, top: &[String], rel: &BTreeMap<String, i64>) {
        let good: Vec<&String> = rel
            .iter()
            .filter(|(_, g)| **g >= 1)
            .map(|(f, _)| f)
            .collect();
        if good.is_empty() {
            return;
        }
        self.n += 1;
        let found = top.iter().filter(|f| good.contains(f)).count();
        self.recall += found as f64 / good.len() as f64;
        if let Some(i) = top.iter().position(|f| good.contains(&f)) {
            self.hit += 1.0;
            self.mrr += 1.0 / (i + 1) as f64;
        }
    }
    fn line(&self) -> String {
        let n = self.n.max(1) as f64;
        format!(
            "n {:3}  hit@10 {:.3}  recall@10 {:.3}  mrr@10 {:.3}",
            self.n,
            self.hit / n,
            self.recall / n,
            self.mrr / n
        )
    }
}

/// The top `k` files of a nearest-first list of entity ids.
fn files(ids: &[String], k: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let f = agent_index::model::file_of(id).to_string();
        if !out.contains(&f) {
            out.push(f);
            if out.len() == k {
                break;
            }
        }
    }
    out
}

fn rrf(a: &[String], b: &[String], k: usize) -> Vec<String> {
    let mut s: HashMap<&String, f64> = HashMap::new();
    for l in [a, b] {
        for (i, f) in l.iter().enumerate() {
            *s.entry(f).or_default() += 1.0 / (60.0 + (i + 1) as f64);
        }
    }
    let mut v: Vec<(&String, f64)> = s.into_iter().collect();
    v.sort_by(|x, y| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(y.0)));
    v.into_iter().take(k).map(|(f, _)| f.clone()).collect()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut config, mut model, mut depth, mut sets) =
        (String::new(), String::new(), 200usize, Vec::new());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = args.next().context("--config FILE")?,
            "--model" => model = args.next().context("--model NAME")?,
            "--depth" => depth = args.next().context("--depth N")?.parse()?,
            _ => sets.push(a),
        }
    }
    if config.is_empty() || model.is_empty() || sets.is_empty() {
        bail!("dense_recall --config FILE --model NAME [--depth N] QRELS...");
    }
    let cfg = Config::parse(&std::fs::read_to_string(&config)?)?;
    let (url, schema) = cfg
        .pg_target()
        .context("the configuration names no pg store")?;
    let pg = Pg::connect_with(&url, &schema, cfg.clone())?;
    let m = vecs::Model::of(&cfg.embedder, &model)?;
    let enc = agent_index::embed_models::load_model(&cfg.embedder, &model, 0)?;
    let (d, cards_t, chunks_t) = (m.dims, m.cards_table(), m.chunks_table());
    let cards_sql = if m.legacy_cards {
        "SELECT e.id FROM entities e WHERE e.card_vec IS NOT NULL AND (e.kind = 'file' OR e.file_kind = 'file')
           AND ($2 = '' OR e.project = $2) ORDER BY e.card_vec <=> $1::text::halfvec(384) LIMIT $3"
            .to_string()
    } else {
        format!(
            "SELECT e.entity_id FROM {cards_t} e WHERE (e.kind = 'file' OR e.file_kind = 'file')
               AND ($2 = '' OR e.project = $2) ORDER BY e.vec <=> $1::text::halfvec({d}) LIMIT $3"
        )
    };
    let chunks_sql = format!(
        "SELECT e.entity_id FROM {chunks_t} e WHERE (e.kind = 'file' OR e.file_kind = 'file')
           AND ($2 = '' OR e.project = $2) ORDER BY e.vec <=> $1::text::halfvec({d}) LIMIT $3"
    );
    let has_chunks = vecs::has_rows(&pg, &chunks_t)?;
    let has_cards = m.legacy_cards || vecs::has_rows(&pg, &cards_t)?;
    println!(
        "model {} ({} dims), store {schema}: cards {has_cards}, chunks {has_chunks} ({chunks_t})",
        m.name, d
    );
    let lim = depth as i64;
    let mut total: BTreeMap<&str, Score> = BTreeMap::new();
    for set in &sets {
        let mut by: BTreeMap<&str, Score> = BTreeMap::new();
        let mut ms = Vec::new();
        for line in std::fs::read_to_string(set)?
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let v: serde_json::Value = serde_json::from_str(line)?;
            let q = v["query"].as_str().unwrap_or_default();
            let words: Vec<String> = agent_index::eval::words::content_words(q)
                .into_iter()
                .take(12)
                .collect();
            let project = match v["project"].as_str() {
                Some(p) if p != "*" && v.get("projects").is_none() => p.to_string(),
                _ => String::new(),
            };
            let rel: BTreeMap<String, i64> = v["rel"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, g)| (k.clone(), g.as_i64().unwrap_or(0)))
                        .collect()
                })
                .unwrap_or_default();
            let t = std::time::Instant::now();
            let qv = vec_text(&enc.embed(&[words.join(" ")], true)?.remove(0));
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
            let ids = |sql: &str| -> Result<Vec<String>> {
                Ok(pg
                    .query(sql, &[&qv, &project, &lim])?
                    .into_iter()
                    .map(|r| r.get(0))
                    .collect())
            };
            let c = if has_cards {
                files(&ids(&cards_sql)?, 10)
            } else {
                Vec::new()
            };
            let k = if has_chunks {
                files(&ids(&chunks_sql)?, 10)
            } else {
                Vec::new()
            };
            if has_cards {
                by.entry("cards").or_default().add(&c, &rel);
            }
            if has_chunks {
                by.entry("chunks").or_default().add(&k, &rel);
                if has_cards {
                    by.entry("cards+chunks")
                        .or_default()
                        .add(&rrf(&c, &k, 10), &rel);
                }
            }
        }
        ms.sort_by(f64::total_cmp);
        let name = std::path::Path::new(set)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for (list, s) in &by {
            println!("{name:20} {list:13} {}", s.line());
            let t = total.entry(list).or_default();
            t.n += s.n;
            t.hit += s.hit;
            t.recall += s.recall;
            t.mrr += s.mrr;
        }
        if !ms.is_empty() {
            println!("{name:20} query embed median {:.0} ms", ms[ms.len() / 2]);
        }
    }
    for (list, s) in &total {
        println!("{:20} {list:13} {}", "ALL", s.line());
    }
    Ok(())
}

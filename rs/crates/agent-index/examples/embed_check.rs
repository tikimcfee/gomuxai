//! candle (CPU) against a server (vLLM's /v1/embeddings) on the same texts:
//! windows of about 3.5 K characters of the code and docs under DIR, and
//! queries with the model's instruction. Prints each pair's cosine, the
//! least and the median, and whether the queries rank the windows the same.
//! Exits 1 if a cosine is under 0.99 or a query's nearest window differs.
//!
//! `embed_check MODEL ENDPOINT DIR [--served NAME] [--windows N] [--max-tokens N]`
use agent_index::config::Embedder;
use agent_index::embed::Encode;
use agent_index::embed_models::{Http, Shared, load_model};
use anyhow::{Context, Result, bail};

const QUERIES: &[&str] = &[
    "where does a held message get delivered once the agent is idle",
    "how is the reciprocal rank fusion of the lists computed",
    "parse the index configuration file and check its values",
    "deliver_held",
    "what happens when the reranker is down",
    "split a markdown file into chunks by its headings",
    "the watcher debounces file events before a flush",
    "embedding vectors stored as halfvec with an HNSW index",
];

fn windows(dir: &std::path::Path, chars: usize, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut es: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        es.sort();
        for p in es {
            if p.is_dir() {
                if !p.ends_with("target") && !p.ends_with(".git") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|e| e == "rs" || e == "md")
                && let Ok(t) = std::fs::read_to_string(&p)
            {
                let mut cur = String::new();
                for line in t.lines() {
                    if cur.len() + line.len() > chars && !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        if out.len() >= n {
                            return out;
                        }
                    }
                    cur.push_str(line);
                    cur.push('\n');
                }
            }
        }
    }
    out
}

fn cos(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let model = args.next().context("MODEL")?;
    let endpoint = args.next().context("ENDPOINT")?;
    let dir = args.next().context("DIR")?;
    let (mut served, mut n, mut max_tokens) = (model.clone(), 16usize, 2048usize);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--served" => served = args.next().context("--served NAME")?,
            "--windows" => n = args.next().context("--windows N")?.parse()?,
            "--max-tokens" => max_tokens = args.next().context("--max-tokens N")?.parse()?,
            _ => bail!("unknown argument {a}"),
        }
    }
    let cfg = Embedder {
        model: model.clone(),
        max_tokens,
        ..Embedder::default()
    };
    let cpu: Shared = load_model(&cfg, &model, 0)?;
    let mut h = Http::new(endpoint.trim_end_matches('/'), &served, &model, "", 0)?;
    h.max_tokens = max_tokens;
    let docs = windows(std::path::Path::new(&dir), 3500, n);
    let qs: Vec<String> = QUERIES.iter().map(|q| q.to_string()).collect();
    let t = std::time::Instant::now();
    let (dc, qc) = (cpu.embed(&docs, false)?, cpu.embed(&qs, true)?);
    let cpu_s = t.elapsed().as_secs_f64();
    let t = std::time::Instant::now();
    let (dg, qg) = (h.embed(&docs, false)?, h.embed(&qs, true)?);
    let gpu_s = t.elapsed().as_secs_f64();
    let mut c: Vec<f64> = Vec::new();
    for (i, (a, b)) in dc.iter().zip(&dg).enumerate() {
        let x = cos(a, b);
        println!("window {i:2} ({} chars): cos {x:.5}", docs[i].len());
        c.push(x);
    }
    for (i, (a, b)) in qc.iter().zip(&qg).enumerate() {
        let x = cos(a, b);
        println!("query {i}: cos {x:.5}  {}", QUERIES[i]);
        c.push(x);
    }
    let mut same = 0;
    for (a, b) in qc.iter().zip(&qg) {
        let top = |q: &Vec<f32>, ds: &Vec<Vec<f32>>| -> usize {
            (0..ds.len())
                .max_by(|&i, &j| cos(q, &ds[i]).total_cmp(&cos(q, &ds[j])))
                .unwrap_or(0)
        };
        if top(a, &dc) == top(b, &dg) {
            same += 1;
        }
    }
    let mut s = c.clone();
    s.sort_by(f64::total_cmp);
    let (least, median) = (s[0], s[s.len() / 2]);
    println!(
        "{model}: {} texts, cos least {least:.5} median {median:.5}; nearest window the same for {same} of {} queries; candle {cpu_s:.1}s, server {gpu_s:.1}s",
        c.len(),
        qs.len()
    );
    if least < 0.99 || same < qs.len() {
        println!("MISMATCH");
        std::process::exit(1);
    }
    println!("OK");
    Ok(())
}

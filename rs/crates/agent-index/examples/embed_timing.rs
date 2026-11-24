//! CPU timings of the embedders: query latency (candle's own threads, as a
//! search runs) and document throughput (single-threaded products on N
//! workers, as the watcher and a bulk run). Documents are windows of about
//! `--window-chars` characters cut from the files under DIR.
//! `cargo run --release --features embed --example embed_timing -- MODEL DIR [--workers N] [--windows N]`
use std::time::Instant;

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

fn pct(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((p * v.len() as f64) as usize).min(v.len() - 1)]
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = args
        .first()
        .cloned()
        .unwrap_or_else(|| "bge-small-en-v1.5".into());
    let dir = std::path::PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
    let flag = |k: &str, d: usize| -> usize {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (workers, n, chars) = (
        flag("--workers", 4),
        flag("--windows", 64),
        flag("--window-chars", 3500),
    );
    let cfg = agent_index::config::Embedder {
        model: model.clone(),
        ..Default::default()
    };
    let t = Instant::now();
    let emb = agent_index::embed_models::load(&cfg, 1)?;
    let load = t.elapsed().as_secs_f64();
    let queries: Vec<String> = [
        "where are held messages delivered when the agent is busy",
        "debounce file watcher git checkout",
        "reranker timeout falls back to fused order",
        "parse the session transcript into turns",
        "how is the bus service unit started",
        "fd glob pattern full path",
        "inotify watch limit",
        "summary fingerprint content key",
        "bootstrap confidence interval resampling",
        "deliver_held",
        "which config key turns asks off",
        "chunk text by markdown heading",
        "the reranker's health endpoint",
        "query log usage counts by agent",
        "Matryoshka dimensions cut renormalize",
        "the ledger guard refuses a sync",
        "symbol extraction regex for rust functions",
        "stale check marks a changed file",
        "project prior from the caller's directory",
        "why are asks left out of the cards",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // Warm up, then one query at a time, as a search asks.
    emb.embed(&queries[..1], true)?;
    let mut qms = Vec::new();
    for q in &queries {
        let t = Instant::now();
        emb.embed(std::slice::from_ref(q), true)?;
        qms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    eprintln!(
        "{model}: load {load:.1}s; query median {:.0} ms, p90 {:.0} ms, max {:.0} ms ({} queries, candle's threads)",
        pct(qms.clone(), 0.5),
        pct(qms.clone(), 0.9),
        pct(qms, 1.0),
        queries.len()
    );
    let docs = windows(&dir, chars, n);
    let total: usize = docs.iter().map(String::len).sum();
    // SAFETY: no other thread runs yet that reads the environment.
    unsafe { std::env::set_var("RAYON_NUM_THREADS", "1") };
    let emb = agent_index::embed_models::load(&cfg, workers)?;
    let t = Instant::now();
    let v = emb.embed(&docs, false)?;
    let s = t.elapsed().as_secs_f64();
    eprintln!(
        "{model}: {} windows of {:.0} chars on average, {workers} workers of one thread: {s:.1}s, {:.2} windows/s, about {:.2} s of CPU a window; dim {}",
        docs.len(),
        total as f64 / docs.len() as f64,
        docs.len() as f64 / s,
        s * workers as f64 / docs.len() as f64,
        v[0].len()
    );
    Ok(())
}

//! The watcher's flush, timed: a project's file changed, then the quick
//! crawl, the sync of that project and the embed with every live model
//! (embed_pg::keep_up), as `agent index watch` runs them. Each round
//! rewrites FILE (inside PROJECT) first: `small` appends a line, `big`
//! writes it anew as about WINDOWS windows of fresh code. With
//! [embedder].endpoint the server embeds, and the CPU when it doesn't
//! answer (point it at a dead port to time that).
//!
//! `flush_timing --config FILE --project NAME --file PATH --mode small|big [--windows N] [--rounds N]`
use std::time::Instant;

use agent_index::config::Config;
use agent_index::embed::Encode;
use agent_index::embed_models::{Shared, load_model};
use agent_index::pg::{Pg, embed_pg, sync, vecs};
use agent_index::{CrawlOptions, Index};
use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    // SAFETY: first thing, before any thread: as `agent index watch` runs
    // the CPU embedder (each worker's products on one thread).
    unsafe { agent_index::embed::single_threaded_products() };
    let mut args = std::env::args().skip(1);
    let (mut config, mut project, mut file, mut mode) = (
        String::new(),
        String::new(),
        String::new(),
        String::from("small"),
    );
    let (mut windows, mut rounds) = (40usize, 3usize);
    while let Some(a) = args.next() {
        let mut v = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--config" => config = v()?,
            "--project" => project = v()?,
            "--file" => file = v()?,
            "--mode" => mode = v()?,
            "--windows" => windows = v()?.parse()?,
            "--rounds" => rounds = v()?.parse()?,
            _ => bail!("unknown argument {a}"),
        }
    }
    if config.is_empty() || project.is_empty() || file.is_empty() {
        bail!("flush_timing --config FILE --project NAME --file PATH --mode small|big");
    }
    let cfg = Config::parse(&std::fs::read_to_string(&config)?)?;
    let (url, schema) = cfg.pg_target().context("no pg store")?;
    let pg = Pg::connect_with(&url, &schema, cfg.clone())?;
    if cfg.ledger.path.is_empty() {
        bail!("the configuration names no [ledger].path");
    }
    let ledger = cfg.ledger_path(std::path::PathBuf::new());
    let mut ix = Index::open(&ledger)?;
    let models: Vec<(vecs::Live, Shared)> = vecs::live(&cfg)?
        .into_iter()
        .map(|l| {
            let e = load_model(&cfg.embedder, &l.model.name, cfg.watch.embed_workers)?;
            Ok((l, e))
        })
        .collect::<Result<_>>()?;
    let mut waiting = embed_pg::Waiting::default();
    println!(
        "{mode} flushes of {file} ({project}), schema {schema}, endpoint {:?}, cpu_texts {}, embed_workers {}",
        cfg.embedder.endpoint, cfg.watch.cpu_texts, cfg.watch.embed_workers
    );
    for round in 0..=rounds {
        match mode.as_str() {
            "small" => {
                let mut t = std::fs::read_to_string(&file).unwrap_or_default();
                t.push_str(&format!(
                    "// edited in round {round} at {:?}\n",
                    Instant::now()
                ));
                std::fs::write(&file, t)?;
            }
            "big" => {
                let mut t = String::new();
                let mut i = 0;
                while t.len() < windows * 3300 {
                    t.push_str(&format!(
                        "/// Round {round}, item {i}: a helper that adds {i} to its input.\npub fn add_{round}_{i}(x: u64) -> u64 {{\n    let y = x.wrapping_add({i});\n    y.rotate_left({})\n}}\n\n",
                        i % 63
                    ));
                    i += 1;
                }
                std::fs::write(&file, t)?;
            }
            _ => bail!("--mode small or big"),
        }
        let t0 = Instant::now();
        agent_index::crawl(
            &mut ix,
            &CrawlOptions {
                projects: vec![project.clone()],
                quick: true,
                ..Default::default()
            },
        )?;
        let crawl_ms = t0.elapsed().as_millis();
        let t1 = Instant::now();
        let s = sync::sync_projects(&ix.conn, &pg, std::slice::from_ref(&project))?;
        let sync_ms = t1.elapsed().as_millis();
        let t2 = Instant::now();
        let m: Vec<(&vecs::Live, &(dyn Encode + Sync))> = models
            .iter()
            .map(|(l, e)| (l, e.as_ref() as &(dyn Encode + Sync)))
            .collect();
        let r = embed_pg::keep_up(&pg, &m, false, &s.rechunked, &mut waiting)?;
        let embed_ms = t2.elapsed().as_millis();
        println!(
            "round {round}{}: total {} ms (crawl {crawl_ms}, sync {sync_ms}, embed {embed_ms}); {} chunks; {} [{}]; {} texts waiting",
            if round == 0 { " (warm-up)" } else { "" },
            t0.elapsed().as_millis(),
            s.chunks,
            r.embedded,
            r.parts.join("; "),
            waiting.texts()
        );
    }
    Ok(())
}

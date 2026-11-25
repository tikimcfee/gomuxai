//! How long a model's chunk windows are, in its tokens: every window the
//! configuration's store would embed (a dry run of the model's pass, which
//! writes nothing), cut and counted with the model's tokenizer. Prints the
//! count, p50, p90, p99, p99.9 and the longest, and how many pass the
//! token cut (`[embedder].max_tokens`, less the pooled end token).
//!
//! `window_tokens --config FILE --model NAME`
use std::cell::RefCell;

use agent_index::config::Config;
use agent_index::embed::Encode;
use agent_index::embed_models::{model_dir, spec};
use agent_index::pg::{Pg, embed_pg, vecs};
use anyhow::{Context, Result, bail};
use tokenizers::Tokenizer;

struct Never;
impl Encode for Never {
    fn dim(&self) -> usize {
        0
    }
    fn embed(&self, _: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
        bail!("a dry run embeds nothing")
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut config, mut model) = (String::new(), String::new());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = args.next().context("--config FILE")?,
            "--model" => model = args.next().context("--model NAME")?,
            _ => bail!("unknown argument {a}"),
        }
    }
    if config.is_empty() || model.is_empty() {
        bail!("window_tokens --config FILE --model NAME");
    }
    let cfg = Config::parse(&std::fs::read_to_string(&config)?)?;
    let (url, schema) = cfg
        .pg_target()
        .context("the configuration names no pg store")?;
    let pg = Pg::connect_with(&url, &schema, cfg.clone())?;
    let s = spec(&model)?;
    let tok = Tokenizer::from_file(model_dir(s.repo, None)?.join("tokenizer.json"))
        .map_err(|e| anyhow::anyhow!("tokenizer.json: {e}"))?;
    let live = vecs::live(&cfg)?
        .into_iter()
        .find(|l| l.model.name == model)
        .context("the configuration doesn't embed with that model")?;
    let counts: RefCell<Vec<usize>> = RefCell::new(Vec::new());
    let count = |t: &str| {
        let n = tok.encode(t, false).map(|e| e.len()).unwrap_or(0);
        counts.borrow_mut().push(n);
    };
    let r = embed_pg::drain_model(
        &pg,
        &Never,
        &live.model,
        &embed_pg::Pass {
            corpora: live.corpora.clone(),
            chunks: Some(&cfg.embedder.chunks),
            dry_run: true,
            on_window: Some(&count),
            ..Default::default()
        },
        &mut |_| {},
    )?;
    let mut v = counts.into_inner();
    v.sort_unstable();
    if v.is_empty() {
        bail!("no windows");
    }
    let at = |p: f64| v[((v.len() as f64 - 1.0) * p).round() as usize];
    let cut = cfg.embedder.max_tokens.min(s.max_tokens).saturating_sub(1);
    let over = v.iter().filter(|&&n| n > cut).count();
    println!(
        "{model} windows in {schema} ({} chars at most): {} windows; tokens p50 {} p90 {} p99 {} p99.9 {} max {}; {over} over the cut of {cut} tokens ({:.3}%); {} would embed now",
        live.model.window_chars,
        v.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        at(0.999),
        v[v.len() - 1],
        100.0 * over as f64 / v.len() as f64,
        r.windows
    );
    Ok(())
}

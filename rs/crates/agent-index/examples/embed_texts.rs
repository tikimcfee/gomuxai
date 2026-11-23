//! Embeds a JSON list of strings from stdin with the index's embedder and
//! prints the vectors as JSON (for parity checks against other runtimes).
//! `cargo run --release --features embed --example embed_texts [--query] < texts.json`
use std::io::Read;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let query = std::env::args().any(|a| a == "--query");
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    let texts: Vec<String> = serde_json::from_str(&s)?;
    let t = Instant::now();
    let emb = agent_index::embed::Embedder::load(&agent_index::embed::model_dir(None)?)?;
    let load = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let v = emb.embed(&texts, query)?;
    eprintln!(
        "load {load:.2}s, {} texts in {:.2}s ({:.0}/s)",
        texts.len(),
        t.elapsed().as_secs_f64(),
        texts.len() as f64 / t.elapsed().as_secs_f64()
    );
    println!("{}", serde_json::to_string(&v)?);
    Ok(())
}

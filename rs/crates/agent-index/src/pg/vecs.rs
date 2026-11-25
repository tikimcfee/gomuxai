//! Vectors by model (docs/INDEX-V2.md, "A code-trained embedder"): the
//! models this build knows, their tables, and the chunk windows they embed.
//!
//! bge-small's card vectors stay in `entities.card_vec`, as before. Every
//! other model's vectors are tables of their own, so several models can sit
//! side by side in one schema (for an A/B, or one model per corpus):
//! `vec_cards_<key>` (one per entity) and `vec_chunks_<key>` (one per window
//! of a chunk), each with a copy of its entity's filter columns (kind,
//! file_kind, project, path, corpus), so the search's filters and its
//! HNSW iterative scan work on the one table. They are made when a model is
//! first embedded with, outside the versioned migrations: an older agent
//! still reads the schema.
//!
//! A window is up to `[embedder.chunks].window_chars` of a chunk, cut at
//! line ends, so a long chunk is several windows and no line goes
//! unembedded. Its vector text starts with a context header: the project
//! and path, the file's summary in a sentence, and what the window
//! defines (code) or its heading path (prose); no model call makes it.

use anyhow::{Result, anyhow};

use super::Pg;
use crate::config::{Config, Embedder};

/// A model this build can embed with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    /// The name `[embedder].model` takes.
    pub name: &'static str,
    /// Its Hugging Face repo.
    pub repo: &'static str,
    /// Its full width.
    pub dims: usize,
    /// The longest input it takes (tokens).
    pub max_tokens: usize,
    /// Its default query instruction ("" for none).
    pub instruction: &'static str,
    /// Its tables' suffix (a lowercase identifier).
    pub slug: &'static str,
}

pub const SPECS: &[Spec] = &[
    Spec {
        name: "bge-small-en-v1.5",
        repo: "BAAI/bge-small-en-v1.5",
        dims: 384,
        max_tokens: 512,
        instruction: "Represent this sentence for searching relevant passages: ",
        slug: "bge",
    },
    Spec {
        name: "qwen3-embedding-0.6b",
        repo: "Qwen/Qwen3-Embedding-0.6B",
        dims: 1024,
        max_tokens: 8192,
        instruction: "Given a code or technical search query, retrieve the code, documentation or notes that answer it",
        slug: "qwen3",
    },
    Spec {
        name: "coderankembed",
        repo: "nomic-ai/CodeRankEmbed",
        dims: 768,
        max_tokens: 8192,
        instruction: "Represent this query for searching relevant code: ",
        slug: "crk",
    },
];

pub fn spec(name: &str) -> Result<&'static Spec> {
    SPECS.iter().find(|s| s.name == name).ok_or_else(|| {
        anyhow!(
            "embedder model {name:?}: this agent embeds with {}",
            SPECS.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
        )
    })
}

/// A query as the model wants it: Qwen3's `Instruct: ...\nQuery:...` (no
/// space, as its sentence-transformers prompt), the others' prefix.
pub fn query_text(s: &Spec, instruction: &str, q: &str) -> String {
    let ins = if instruction.is_empty() {
        s.instruction
    } else {
        instruction
    };
    match s.slug {
        "qwen3" => format!("Instruct: {ins}\nQuery:{q}"),
        _ => format!("{ins}{q}"),
    }
}

/// Where a query's vector was made: by the embedding server, or on the CPU
/// because the server didn't answer (embed_models::Served). Set on the
/// thread that embedded it, so a search reads its own.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct EmbedOn {
    pub model: String,
    /// "gpu" (the server) or "cpu".
    pub on: String,
    /// Why the CPU did it ("" when the server did).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// An embedding server's health as this process last saw it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct ServerState {
    pub endpoint: String,
    pub model: String,
    pub up: bool,
    /// When it stopped answering (RFC 3339), while it's down.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub down_since: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// Texts it embedded, and texts the CPU took instead.
    pub served: u64,
    pub fell_back: u64,
}

thread_local! {
    static LAST_ON: std::cell::RefCell<Option<EmbedOn>> = const { std::cell::RefCell::new(None) };
}

static SERVERS: std::sync::Mutex<Vec<ServerState>> = std::sync::Mutex::new(Vec::new());

/// Records where this thread's last query vector was made.
pub fn note_embed_on(on: EmbedOn) {
    LAST_ON.with(|c| *c.borrow_mut() = Some(on));
}

/// Where this thread's last query vector was made, cleared.
pub fn take_embed_on() -> Option<EmbedOn> {
    LAST_ON.with(|c| c.borrow_mut().take())
}

/// Updates a server's state (by endpoint and model).
pub fn note_server(f: impl FnOnce(&mut ServerState), endpoint: &str, model: &str) {
    let mut v = SERVERS.lock().unwrap_or_else(|e| e.into_inner());
    let i = match v
        .iter()
        .position(|s| s.endpoint == endpoint && s.model == model)
    {
        Some(i) => i,
        None => {
            v.push(ServerState {
                endpoint: endpoint.to_string(),
                model: model.to_string(),
                up: true,
                ..Default::default()
            });
            v.len() - 1
        }
    };
    f(&mut v[i]);
}

/// The embedding servers this process has asked, and how they did.
pub fn servers() -> Vec<ServerState> {
    SERVERS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A model's vectors in this configuration: its name, width and tables.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub name: String,
    pub dims: usize,
    /// The tables' suffix: the slug, and the width when cut.
    pub key: String,
    /// Card vectors in `entities.card_vec` (bge-small at full width).
    pub legacy_cards: bool,
    /// The chunk table's suffix past the key: the header, when not `full`
    /// (so an ablation's windows sit beside the others').
    pub chunk_suffix: String,
    /// Its windows' most characters: `[embedder.chunks].window_chars`, cut
    /// to what the model reads (about 3 characters a token).
    pub window_chars: usize,
}

impl Model {
    pub fn of(cfg: &Embedder, name: &str) -> Result<Model> {
        let s = spec(name)?;
        let dims = if cfg.dims > 0 && cfg.dims < s.dims && s.slug != "bge" {
            cfg.dims
        } else {
            s.dims
        };
        let key = if dims == s.dims {
            s.slug.to_string()
        } else {
            format!("{}_{dims}", s.slug)
        };
        let h = &cfg.chunks.header;
        Ok(Model {
            name: name.to_string(),
            dims,
            key,
            legacy_cards: s.slug == "bge",
            chunk_suffix: if h == "full" {
                String::new()
            } else {
                format!("_h{h}")
            },
            window_chars: cfg.chunks.window_chars.min(s.max_tokens * 3),
        })
    }

    pub fn cards_table(&self) -> String {
        format!("vec_cards_{}", self.key)
    }

    pub fn chunks_table(&self) -> String {
        format!("vec_chunks_{}{}", self.key, self.chunk_suffix)
    }
}

/// The models serving `corpora`, each with the corpora it serves, in a
/// stable order: `[embedder.corpus]`'s choice, else `[embedder].model`.
pub fn plan(cfg: &Config, corpora: &[String]) -> Result<Vec<(Model, Vec<String>)>> {
    let mut out: Vec<(Model, Vec<String>)> = Vec::new();
    for c in corpora {
        let name = cfg.embedder.model_for(c);
        let m = Model::of(&cfg.embedder, name)?;
        match out.iter_mut().find(|(x, _)| x.key == m.key) {
            Some((_, cs)) => cs.push(c.clone()),
            None => out.push((m, vec![c.clone()])),
        }
    }
    Ok(out)
}

/// One model's part of keeping the index's vectors: the corpora it embeds,
/// and whether their chunks' windows too.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub model: Model,
    pub corpora: Vec<String>,
    pub chunks: bool,
    /// A `keep` model: kept up to date, not searched.
    pub kept: bool,
}

/// Every model the index keeps vectors for: [`plan`]'s over every corpus
/// (cards, and windows with `[embedder.chunks]`), then `[embedder].keep`'s
/// (cards alone, every corpus).
pub fn live(cfg: &Config) -> Result<Vec<Live>> {
    let corpora: Vec<String> = crate::config::CORPORA
        .iter()
        .map(|c| c.to_string())
        .collect();
    let mut out: Vec<Live> = plan(cfg, &corpora)?
        .into_iter()
        .map(|(model, corpora)| Live {
            model,
            corpora,
            chunks: cfg.embedder.chunks.enabled,
            kept: false,
        })
        .collect();
    for k in &cfg.embedder.keep {
        let model = Model::of(&cfg.embedder, k)?;
        if out.iter().any(|l| l.model.key == model.key) {
            continue;
        }
        out.push(Live {
            model,
            corpora: corpora.clone(),
            chunks: false,
            kept: true,
        });
    }
    Ok(out)
}

/// Makes a model's tables when missing (without their HNSW indexes: those
/// are built once their rows are in, see [`ensure_indexes`]).
pub fn ensure_tables(pg: &Pg, m: &Model) -> Result<()> {
    let (cards, chunks, d) = (m.cards_table(), m.chunks_table(), m.dims);
    pg.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {cards} (
             entity_id text PRIMARY KEY, kind text NOT NULL, file_kind text NOT NULL,
             project text NOT NULL, path text NOT NULL, corpus text NOT NULL,
             text_key text NOT NULL, vec halfvec({d}) NOT NULL);
         CREATE TABLE IF NOT EXISTS {chunks} (
             entity_id text NOT NULL, ord integer NOT NULL, part integer NOT NULL,
             line_start integer NOT NULL, line_end integer NOT NULL,
             kind text NOT NULL, file_kind text NOT NULL, project text NOT NULL,
             path text NOT NULL, corpus text NOT NULL,
             text_key text NOT NULL, vec halfvec({d}) NOT NULL,
             PRIMARY KEY (entity_id, ord, part));"
    ))
}

/// Builds a model's HNSW indexes when missing (after a bulk insert: one
/// build is far faster than as many inserts into the graph).
pub fn ensure_indexes(pg: &Pg, m: &Model) -> Result<()> {
    for t in [m.cards_table(), m.chunks_table()] {
        pg.batch(&format!(
            "CREATE INDEX IF NOT EXISTS {t}_hnsw ON {t} USING hnsw (vec halfvec_cosine_ops)"
        ))?;
    }
    Ok(())
}

pub fn drop_indexes(pg: &Pg, m: &Model) -> Result<()> {
    for t in [m.cards_table(), m.chunks_table()] {
        pg.batch(&format!("DROP INDEX IF EXISTS {t}_hnsw"))?;
    }
    Ok(())
}

/// Whether a table of this schema exists and holds a row.
pub fn has_rows(pg: &Pg, table: &str) -> Result<bool> {
    let exists = !pg
        .query(
            "SELECT 1 FROM pg_tables WHERE schemaname = $1 AND tablename = $2",
            &[&pg.schema, &table],
        )?
        .is_empty();
    if !exists {
        return Ok(false);
    }
    Ok(!pg
        .query(&format!("SELECT 1 FROM {table} LIMIT 1"), &[])?
        .is_empty())
}

/// The vector tables of this schema (`vec_cards_*`, `vec_chunks_*`).
pub fn tables(pg: &Pg) -> Result<Vec<String>> {
    Ok(pg
        .query(
            "SELECT tablename FROM pg_tables WHERE schemaname = $1 AND tablename LIKE 'vec\\_%' ORDER BY 1",
            &[&pg.schema],
        )?
        .into_iter()
        .map(|r| r.get(0))
        .collect())
}

/// One window of a chunk, ready to embed.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub part: usize,
    pub line_start: usize,
    pub line_end: usize,
    /// The header and the window's lines.
    pub text: String,
}

/// The header's version: changing what a header holds changes every
/// window's key, so each is embedded again.
pub const HEADER_VERSION: &str = "h1";

/// What a chunk's header is made of.
#[derive(Debug, Clone, Default)]
pub struct HeaderParts<'a> {
    pub project: &'a str,
    pub rel: &'a str,
    /// The file's summary (a model's, else its seed): its first sentence.
    pub summary: &'a str,
    /// The heading path (prose), or the chunk's heading.
    pub headings: Vec<String>,
}

/// A window's header, by `[embedder.chunks].header`: none, path, or full
/// (path, the summary's first sentence, the heading path or what the window
/// defines).
pub fn header(mode: &str, h: &HeaderParts, defines: &[String]) -> String {
    if mode == "none" {
        return String::new();
    }
    let mut s = if h.rel.is_empty() {
        h.project.to_string()
    } else {
        format!("{}/{}", h.project, h.rel)
    };
    s.push('\n');
    if mode == "path" {
        return s;
    }
    let first = crate::extract::first_sentence(h.summary);
    if !first.is_empty() {
        s.push_str(&crate::extract::clip(&first, 300));
        s.push('\n');
    }
    if !defines.is_empty() {
        s.push_str("defines: ");
        s.push_str(&defines.join(", "));
        s.push('\n');
    } else if !h.headings.is_empty() {
        s.push_str(&h.headings.join(" > "));
        s.push('\n');
    }
    s.push('\n');
    s
}

/// A chunk's text cut into windows of at most `chars` at line ends (a line
/// longer than that is cut where it must be), each starting with its header.
/// `line_start` is the chunk's first line; `defines(a, b)` names the symbols
/// defined on lines a..=b.
pub fn windows(
    text: &str,
    line_start: usize,
    chars: usize,
    mode: &str,
    h: &HeaderParts,
    defines: &dyn Fn(usize, usize) -> Vec<String>,
) -> Vec<Window> {
    let chars = chars.max(200);
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut first, mut last) = (line_start, line_start);
    let flush = |cur: &mut String, a: usize, b: usize, out: &mut Vec<Window>| {
        if cur.trim().is_empty() {
            cur.clear();
            return;
        }
        let head = header(mode, h, &defines(a, b));
        out.push(Window {
            part: out.len(),
            line_start: a,
            line_end: b,
            text: format!("{head}{cur}"),
        });
        cur.clear();
    };
    for (i, line) in text.lines().enumerate() {
        let n = line_start + i;
        let mut rest = line;
        loop {
            let room = chars.saturating_sub(cur.len());
            if rest.len() < room {
                if cur.is_empty() {
                    first = n;
                }
                cur.push_str(rest);
                cur.push('\n');
                last = n;
                break;
            }
            if !cur.is_empty() {
                flush(&mut cur, first, last, &mut out);
                continue;
            }
            // One line longer than a window: cut it at a char boundary.
            let mut cut = chars.min(rest.len());
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            cur.push_str(&rest[..cut]);
            first = n;
            last = n;
            flush(&mut cur, first, last, &mut out);
            rest = &rest[cut..];
            if rest.is_empty() {
                break;
            }
        }
    }
    flush(&mut cur, first, last, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_keys() {
        let mut e = Embedder::default();
        assert_eq!(Model::of(&e, "bge-small-en-v1.5").unwrap().key, "bge");
        assert!(Model::of(&e, "bge-small-en-v1.5").unwrap().legacy_cards);
        e.dims = 512;
        let q = Model::of(&e, "qwen3-embedding-0.6b").unwrap();
        assert_eq!((q.key.as_str(), q.dims), ("qwen3_512", 512));
        assert_eq!(Model::of(&e, "bge-small-en-v1.5").unwrap().dims, 384);
        assert_eq!(
            Model::of(&e, "bge-small-en-v1.5").unwrap().window_chars,
            1536
        );
        assert_eq!(q.window_chars, 3500);
        assert_eq!(q.chunks_table(), "vec_chunks_qwen3_512");
        e.chunks.header = "none".into();
        assert_eq!(
            Model::of(&e, "qwen3-embedding-0.6b")
                .unwrap()
                .chunks_table(),
            "vec_chunks_qwen3_512_hnone"
        );
        assert!(Model::of(&e, "nope").is_err());
        let s = spec("qwen3-embedding-0.6b").unwrap();
        assert_eq!(
            query_text(s, "", "x"),
            format!("Instruct: {}\nQuery:x", s.instruction)
        );
        let c = spec("coderankembed").unwrap();
        assert_eq!(
            query_text(c, "", "x"),
            "Represent this query for searching relevant code: x"
        );
    }

    #[test]
    fn windows_cover_every_line() {
        let text: String = (1..=100)
            .map(|i| format!("line {i:03} {}\n", "x".repeat(40)))
            .collect();
        let h = HeaderParts {
            project: "alpha",
            rel: "src/lib.rs",
            summary: "Holds the alpha library. More.",
            headings: vec![],
        };
        let defs = |a: usize, b: usize| -> Vec<String> {
            if a <= 450 && 450 <= b {
                vec!["deliver_held".into()]
            } else {
                vec![]
            }
        };
        let w = windows(&text, 401, 1000, "full", &h, &defs);
        assert!(w.len() >= 5, "{}", w.len());
        assert_eq!(w[0].line_start, 401);
        assert_eq!(w.last().unwrap().line_end, 500);
        for pair in w.windows(2) {
            assert_eq!(pair[1].line_start, pair[0].line_end + 1);
        }
        assert!(
            w[0].text
                .starts_with("alpha/src/lib.rs\nHolds the alpha library.\n\n")
        );
        assert!(w.iter().any(|x| x.text.contains("defines: deliver_held\n")));
        let none = windows(&text, 1, 1000, "none", &h, &defs);
        assert!(none[0].text.starts_with("line 001"));
        // A line longer than a window is cut, not lost.
        let long = "y".repeat(2500);
        let w = windows(&long, 1, 1000, "path", &h, &defs);
        assert_eq!(w.len(), 3);
        let body: usize = w
            .iter()
            .map(|x| x.text.len() - "alpha/src/lib.rs\n".len())
            .sum();
        // Its last piece keeps the line's end.
        assert_eq!(body, 2501);
    }
}

//! The index's runtime configuration (docs/INDEX-V2.md, "Configuration"):
//! every search and pipeline tunable, read from a TOML file at run time, so
//! a tuning run changes a file, not the code.
//!
//! The file is `--config PATH`, else `AGENT_INDEX_CONFIG`, else
//! `$XDG_CONFIG_HOME/gomuxai/index.toml` (`~/.config/gomuxai/index.toml`).
//! A missing default file means the defaults below, which are what the
//! code did before the file existed; a file named by `--config` or the
//! environment must exist. Every key is optional, and an unknown key is an
//! error (a typo must not pass silently as a default).
//!
//! [`Config::fingerprint`] hashes the resolved values: `search --json`
//! reports it, and every sync, embed and logged query records it, so a
//! result can be traced to the configuration that made it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The environment variable that names the file.
pub const CONFIG_ENV: &str = "AGENT_INDEX_CONFIG";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub search: Search,
    /// Each ranked list's weight in fusion, by list name (`lexical`, `all
    /// words`, `full text`, `trigram`, `semantic`, `label`, `bm25`, `bm25
    /// full text`); a list's `here` copy takes its weight. Unnamed lists
    /// weigh 1.
    pub lists: BTreeMap<String, f64>,
    pub priors: Priors,
    /// Per corpus (`code`, `prose`, `session`, `memory`, `friction`).
    pub corpora: BTreeMap<String, Corpus>,
    pub chunks: Chunks,
    pub cards: Cards,
    pub embedder: Embedder,
    pub reranker: Reranker,
    pub llm: Llm,
    /// The BM25 lists' field boosts (`--lexical bm25|both`, pg_search).
    pub bm25: Bm25,
}

/// Each card field's boost in the BM25 card list; by default in the ratio
/// of the native list's weights (A 1.0, B 0.4, C 0.2, D 0.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bm25 {
    pub terms: f64,
    pub labels: f64,
    pub title: f64,
    pub summary: f64,
    pub asks: f64,
    pub seed: f64,
    pub why: f64,
    pub symbols: f64,
}

impl Default for Bm25 {
    fn default() -> Self {
        Bm25 {
            terms: 10.0,
            labels: 10.0,
            title: 4.0,
            summary: 4.0,
            asks: 4.0,
            seed: 2.0,
            why: 2.0,
            symbols: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    /// Reciprocal rank fusion's k.
    pub rrf_k: f64,
    /// Results when `--limit` isn't given.
    pub limit: usize,
    /// How many each candidate list fetches.
    pub list_depth: i64,
    /// At most this many results from one file.
    pub per_file: usize,
    /// `--kind file` searches this many times the limit of entities, then
    /// maps them to files.
    pub files_depth_factor: usize,
    /// Fetch every list again scoped to the caller's project and its
    /// dependencies.
    pub local_lists: bool,
    /// The chunk list takes a query's rarest terms until their chunks add
    /// up to this (the rarest always counts).
    pub chunk_budget: i64,
    /// The same for the cards' any-word list.
    pub card_budget: i64,
    /// The trigram list matches this many of the query's rarest terms.
    pub trigram_terms: i64,
    /// pg_trgm's `word_similarity_threshold` for the trigram list.
    pub trigram_threshold: f64,
    /// `ts_rank_cd`'s weights for D, C, B and A (children's names; seed and
    /// why; title, summary and asks; terms and labels).
    pub rank_weights: [f64; 4],
    /// `ts_rank_cd`'s normalization flags.
    pub rank_normalization: i32,
    /// HNSW's `ef_search` (iterative scans go past it).
    pub hnsw_ef_search: i64,
    /// Label runs of at most this many words (and the whole query).
    pub label_max_words: usize,
    /// Entities a label run may name.
    pub labels_per_run: usize,
    /// Definitions an exact name pins.
    pub exact_limit: i64,
    /// A rare identifier pins its files when at most this many hold it.
    pub rare_files: usize,
}

impl Default for Search {
    fn default() -> Self {
        Search {
            rrf_k: 60.0,
            limit: 10,
            list_depth: 300,
            per_file: 2,
            files_depth_factor: 8,
            local_lists: true,
            chunk_budget: 4000,
            card_budget: 8000,
            trigram_terms: 4,
            trigram_threshold: 0.6,
            rank_weights: [0.1, 0.2, 0.4, 1.0],
            rank_normalization: 1,
            hnsw_ef_search: 100,
            label_max_words: 6,
            labels_per_run: 25,
            exact_limit: 20,
            rare_files: 3,
        }
    }
}

/// v1's priors, applied after fusion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Priors {
    /// Added to an exact name's definitions (less 0.001 a place).
    pub pin_exact: f64,
    /// Added to the entities a whole-query label names.
    pub pin_label: f64,
    /// Added to a rare identifier's files.
    pub pin_rare: f64,
    /// Added to a doc section when no kind is asked for.
    pub section: f64,
    /// The most an entity's importance adds (scaled by log importance).
    pub importance: f64,
    /// Added under a kept-out-of-the-way directory.
    pub out_of_the_way: f64,
    pub fixture: f64,
    pub test: f64,
    /// A positive score in the caller's own project is multiplied by this...
    pub own_project: f64,
    /// ...and in a project it depends on, by this.
    pub dependency: f64,
}

impl Default for Priors {
    fn default() -> Self {
        Priors {
            pin_exact: 2.0,
            pin_label: 1.0,
            pin_rare: 1.5,
            section: -0.002,
            importance: 0.004,
            out_of_the_way: -0.008,
            fixture: -0.03,
            test: -0.004,
            own_project: 3.0,
            dependency: 2.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Corpus {
    /// A positive score in this corpus is multiplied by this.
    pub boost: f64,
}

impl Default for Corpus {
    fn default() -> Self {
        Corpus { boost: 1.0 }
    }
}

/// How files are cut into chunks, and what is counted. Changing the cut
/// cuts every file again at the next sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Chunks {
    pub max_lines: usize,
    pub max_bytes: usize,
    /// Lexemes in fewer chunks than this aren't counted (search takes them
    /// for rare).
    pub lexeme_floor: i32,
    /// The file kinds whose text is cut into chunks (in full-depth
    /// projects).
    pub file_kinds: Vec<String>,
}

impl Default for Chunks {
    fn default() -> Self {
        Chunks {
            max_lines: 400,
            max_bytes: 32 * 1024,
            lexeme_floor: 50,
            file_kinds: ["source", "test", "doc", "config", "build"]
                .map(String::from)
                .to_vec(),
        }
    }
}

/// What an entity's card holds (the text its vector is made from).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cards {
    /// Children's names in a card's search text.
    pub children: usize,
    /// Names after `defines:` in the embedded card.
    pub symbols: usize,
    /// The embedded card is clipped to this many characters.
    pub max_chars: usize,
}

impl Default for Cards {
    fn default() -> Self {
        Cards {
            children: 200,
            symbols: 60,
            max_chars: 2000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Embedder {
    /// The model; this build runs bge-small-en-v1.5 (384 dimensions) only.
    pub model: String,
    /// Its directory (config.json, tokenizer.json, model.safetensors); ""
    /// is the Hugging Face cache's.
    pub model_dir: String,
    /// Cards embedded per round trip.
    pub batch: usize,
}

impl Default for Embedder {
    fn default() -> Self {
        Embedder {
            model: "bge-small-en-v1.5".into(),
            model_dir: String::new(),
            batch: 512,
        }
    }
}

/// The cross-encoder (docs/INDEX-V2.md, "Reranker"). Not wired into search
/// yet: the settings are here so a run records them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reranker {
    pub enabled: bool,
    /// The server's base URL (`/rerank` on it).
    pub endpoint: String,
    pub model: String,
    /// The task instruction sent with each query.
    pub instruction: String,
    pub timeout_ms: u64,
    /// Candidates reranked per corpus.
    pub depth: BTreeMap<String, usize>,
}

impl Default for Reranker {
    fn default() -> Self {
        Reranker {
            enabled: false,
            endpoint: "http://127.0.0.1:8100".into(),
            model: "Qwen/Qwen3-Reranker-4B".into(),
            instruction: "Given a code or technical search query, retrieve relevant code entities, SQL statements, or documentation passages that answer the query".into(),
            timeout_ms: 5000,
            depth: [
                ("code", 50),
                ("prose", 20),
                ("session", 20),
                ("memory", 10),
                ("friction", 10),
            ]
            .map(|(k, v)| (k.to_string(), v))
            .into(),
        }
    }
}

/// The local model that writes summaries and asks, and rewrites weak
/// queries (later iterations). An empty endpoint is none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Llm {
    pub summaries: Endpoint,
    pub query_rewrite: Endpoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Endpoint {
    /// An OpenAI-compatible base URL.
    pub endpoint: String,
    pub model: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub timeout_ms: u64,
}

/// A loaded configuration and where it came from.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// The file read, or None when the defaults apply (no file).
    pub path: Option<PathBuf>,
}

/// The default file: `$XDG_CONFIG_HOME/gomuxai/index.toml`, else
/// `~/.config/gomuxai/index.toml`.
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("gomuxai").join("index.toml"))
}

impl Config {
    /// Parses a file's text; an empty text is the defaults.
    pub fn parse(text: &str) -> Result<Config> {
        let c: Config = toml::from_str(text)?;
        c.check()?;
        Ok(c)
    }

    /// Values that would break a search rather than tune it.
    fn check(&self) -> Result<()> {
        let s = &self.search;
        if s.rrf_k < 0.0 || s.list_depth < 1 || s.per_file < 1 || s.files_depth_factor < 1 {
            bail!(
                "search: rrf_k must be at least 0, list_depth, per_file and files_depth_factor at least 1"
            );
        }
        if !(0.0..=1.0).contains(&s.trigram_threshold) {
            bail!("search.trigram_threshold must be between 0 and 1");
        }
        if self.chunks.max_lines < 1 || self.chunks.max_bytes < 256 {
            bail!("chunks: max_lines must be at least 1 and max_bytes at least 256");
        }
        if self.lists.values().any(|w| !w.is_finite() || *w < 0.0) {
            bail!("lists: a list's weight must be a number of 0 or more");
        }
        Ok(())
    }

    /// Reads `path`. None: `AGENT_INDEX_CONFIG`, else the default file,
    /// which may be missing (then the defaults).
    pub fn load(path: Option<&Path>) -> Result<Loaded> {
        let env = std::env::var_os(CONFIG_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let (file, must) = match (path, env) {
            (Some(p), _) => (Some(p.to_path_buf()), true),
            (None, Some(e)) => (Some(e), true),
            (None, None) => (default_path(), false),
        };
        let Some(file) = file else {
            return Ok(Loaded {
                config: Config::default(),
                path: None,
            });
        };
        match std::fs::read_to_string(&file) {
            Ok(text) => Ok(Loaded {
                config: Config::parse(&text).with_context(|| {
                    format!("reading the index configuration {}", file.display())
                })?,
                path: Some(file),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must => Ok(Loaded {
                config: Config::default(),
                path: None,
            }),
            Err(e) => Err(e)
                .with_context(|| format!("reading the index configuration {}", file.display())),
        }
    }

    /// The resolved values as TOML (every key, defaults included).
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// A short hash of the resolved values: equal configurations have equal
    /// fingerprints, however their files are written.
    pub fn fingerprint(&self) -> String {
        let v = serde_json::to_vec(self).unwrap_or_default();
        Sha256::digest(&v)
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A list's weight in fusion (its `here` copy takes the list's).
    pub fn list_weight(&self, list: &str) -> f64 {
        let base = list.strip_suffix(" here").unwrap_or(list);
        self.lists.get(base).copied().unwrap_or(1.0)
    }

    /// A corpus's boost.
    pub fn corpus_boost(&self, corpus: &str) -> f64 {
        self.corpora.get(corpus).map(|c| c.boost).unwrap_or(1.0)
    }

    /// Whether the chunk cut is the default one (chunk keys then are the
    /// files' content keys, as before the file existed).
    pub fn default_chunking(&self) -> bool {
        let d = Chunks::default();
        self.chunks.max_lines == d.max_lines && self.chunks.max_bytes == d.max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_full_default_files_are_the_defaults() {
        let d = Config::default();
        assert_eq!(Config::parse("").unwrap(), d);
        // Every key written out reads back the same, with the same fingerprint.
        let full = d.to_toml();
        let back = Config::parse(&full).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.fingerprint(), d.fingerprint());
        // A value changed is a new fingerprint; a typo is an error.
        let c = Config::parse("[search]\nrrf_k = 30\n").unwrap();
        assert_eq!(c.search.rrf_k, 30.0);
        assert_ne!(c.fingerprint(), d.fingerprint());
        assert!(Config::parse("[search]\nrrf = 30\n").is_err());
        assert!(Config::parse("[search]\ntrigram_threshold = 2\n").is_err());
        let c = Config::parse("[lists]\ntrigram = 0.5\n[corpora.code]\nboost = 2\n").unwrap();
        assert_eq!(c.list_weight("trigram here"), 0.5);
        assert_eq!(c.list_weight("lexical"), 1.0);
        assert_eq!(c.corpus_boost("code"), 2.0);
        assert_eq!(c.corpus_boost("prose"), 1.0);
    }

    #[test]
    fn loading() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("index.toml");
        // A named file must exist.
        assert!(Config::load(Some(&p)).is_err());
        std::fs::write(&p, "[search]\nlimit = 20\n").unwrap();
        let l = Config::load(Some(&p)).unwrap();
        assert_eq!(l.config.search.limit, 20);
        assert_eq!(l.path.as_deref(), Some(p.as_path()));
    }
}

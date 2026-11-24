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
    pub serve: Serve,
    pub memory: Memory,
    pub friction: Friction,
    /// `[summaries]`: which model summaries go live (not how they are
    /// written: that is `[llm.summaries]`).
    pub summaries: SummaryPolicy,
    pub sessions: Sessions,
    /// Which store the commands use, and where it is. With the next two,
    /// left out of the fingerprint: they change where things are, not what
    /// a search answers.
    pub store: Store,
    /// Where the crawl's ledger lives.
    pub ledger: Ledger,
    /// The query log.
    pub log: Log,
    /// `agent index watch` (not in the fingerprint either: it changes when
    /// an answer is current, not what it is).
    pub watch: Watch,
    /// `[cross]`: answers that span projects (docs/INDEX-V2.md, "Across
    /// projects"). Every switch off by default; with all of it at its
    /// defaults the fingerprint is what it was before the section existed.
    pub cross: Cross,
}

/// The file watcher (docs/INDEX-V2.md, "Freshness").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Watch {
    /// Its Unix socket, where `agent index touch` (an edit hook) and the
    /// search's stale check name changed paths; "" is `index-watch.sock`
    /// beside the ledger.
    pub socket: String,
    /// A project is flushed once its files have been quiet this long...
    pub settle_ms: u64,
    /// ...or this long after its first change, however busy it stays.
    pub max_wait_ms: u64,
    /// While git holds `.git/index.lock` (a checkout, a rebase), a flush
    /// waits, up to this long.
    pub git_lock_ms: u64,
    /// The tidy (a full crawl and sync: cross-project edges, importance,
    /// word counts, new projects, sessions) runs once everything has been
    /// quiet this long after a flush...
    pub tidy_quiet_s: u64,
    /// ...and at most this often.
    pub tidy_every_s: u64,
    /// The embedder's worker threads in the watcher (a bulk `agent index
    /// embed` takes half the logical CPUs; a background process, fewer).
    pub embed_workers: usize,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            socket: String::new(),
            settle_ms: 1000,
            max_wait_ms: 10_000,
            git_lock_ms: 60_000,
            tidy_quiet_s: 600,
            tidy_every_s: 3600,
            embed_workers: 4,
        }
    }
}

/// The store the commands use when neither `--store` nor
/// `AGENT_INDEX_STORE` says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Store {
    /// `sqlite` or `pg`; "" is sqlite.
    pub kind: String,
    /// The pg store's database URL (`postgresql:///DB?host=SOCKET_DIR`),
    /// when `AGENT_INDEX_PG` isn't set.
    pub url: String,
    /// Its schema, when `AGENT_INDEX_PG_SCHEMA` isn't set; "" is `idx`.
    pub schema: String,
}

/// The crawl's SQLite ledger (index.db).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Ledger {
    /// Its path ("~/" is the home directory); "" is `index.db` beside the
    /// binary's `gomuxai.db`, which ties the ledger to one copy of the
    /// binary. Every sync of a schema should use the one ledger it was built
    /// from.
    pub path: String,
}

/// The query log: one JSON line per search, show, tree and related, for
/// `agent index usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    pub enabled: bool,
    /// The file ("~/" is the home directory); "" is
    /// `$XDG_DATA_HOME/gomuxai/index-queries.jsonl`
    /// (`~/.local/share/gomuxai/index-queries.jsonl`).
    pub path: String,
}

impl Default for Log {
    fn default() -> Self {
        Log {
            enabled: true,
            path: String::new(),
        }
    }
}

/// Answers that span projects: a search with no `--project` or `--under`
/// (docs/INDEX-V2.md, "Across projects"). Each part has its own switch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cross {
    /// Rank the projects for the query before the slots and sub-queries:
    /// project and folder summaries by BM25 (`route_cards` of them, each
    /// project's best `route_summary_best` summed), the query's vector
    /// against the projects' cards, the fused candidates' mass by project
    /// (its best `route_per_project` fusion scores summed), and the
    /// projects the query names. Each list's scores are scaled to its best
    /// (the vector's from its worst to its best), and a project's route
    /// score is their mean weighted by `route_summary`, `route_dense`,
    /// `route_fused` and `route_names`.
    pub route: bool,
    pub route_cards: i64,
    pub route_per_project: usize,
    pub route_summary: f64,
    pub route_dense: f64,
    pub route_fused: f64,
    /// A fourth list: the projects the query names (a word equal to the
    /// name), each scoring 1.
    pub route_names: f64,
    /// How many of a project's summary cards count (its best).
    pub route_summary_best: usize,
    /// The fused candidates read for their projects' mass.
    pub route_fused_depth: usize,
    /// At most this many projects are routed...
    pub route_projects: usize,
    /// ...each with a route score at least this share of the first's. Two
    /// or more routed projects make the query read as spanning projects
    /// when the caller is in no project, or with `route_caller`; from
    /// inside a project it takes a name, a comparison or parts.
    pub route_ratio: f64,
    pub route_caller: bool,
    /// Retrieve again inside each spanned project (the fused statement with
    /// that project as its filter): its candidates join, and its own
    /// ranking picks the project's places in the pool (`slots`). A
    /// candidate scores the better of the whole tree's fusion score and its
    /// project's times `split_raise` (0: a project's candidates reach the
    /// pool only through its places).
    pub split: bool,
    /// How many of each project's own files join.
    pub split_depth: usize,
    pub split_raise: f64,
    /// The reranker's pool across projects: each of the top projects (the
    /// spanned ones, else the `slot_projects` with the most fused mass that
    /// clear `route_ratio`) gets its best `slot_min` files in (by its own
    /// sub-query's ranking with `split`), and no project takes more than
    /// `slot_cap` of the pool. Two or more projects, or nothing changes.
    pub slots: bool,
    pub slot_min: usize,
    pub slot_cap: f64,
    pub slot_projects: usize,
    /// The pool's size when slots apply (0: `[reranker].flat_depth`).
    pub pool_depth: usize,
    /// A `--kind file` answer's first `limit` hold at most this many files
    /// from one project while another of the query's projects has files
    /// further down (0: no cap).
    pub top_cap: usize,
    /// When the query names another project, reads as a comparison, or is
    /// routed to two or more projects, the caller's own project counts
    /// this much (and a dependency at most this), and the lists scoped to
    /// it are left out. 0: the priors stand.
    pub own_project: f64,
    /// Files that sessions touched along with the top hits join the
    /// candidates: from the best `touched_seeds` files, each neighbor
    /// scores by the sessions it shares with them, each session weighted
    /// by its IDF (ln of the touched files over its own) and the neighbor
    /// by its own (ln of the sessions over those that touched it), a
    /// neighbor in another project than its seed by `touched_cross`. The best
    /// `touched_max` neighbors gain `touched_weight` times the pool's last
    /// score, scaled by their share of the strongest.
    pub touched: bool,
    pub touched_seeds: usize,
    pub touched_max: usize,
    pub touched_weight: f64,
    pub touched_cross: f64,
}

impl Default for Cross {
    fn default() -> Self {
        Cross {
            route: false,
            route_cards: 60,
            route_per_project: 3,
            // Tuned on the cross dev set (docs/INDEX-V2.md, "Across
            // projects"): the summaries and the projects' vectors routed
            // worse than the candidates' mass and the names.
            route_summary: 0.0,
            route_dense: 0.0,
            route_fused: 1.0,
            route_names: 1.0,
            route_summary_best: 3,
            route_fused_depth: 100,
            route_projects: 2,
            route_ratio: 0.5,
            route_caller: false,
            split: false,
            split_depth: 30,
            split_raise: 1.0,
            slots: false,
            slot_min: 5,
            slot_cap: 0.6,
            slot_projects: 2,
            pool_depth: 0,
            top_cap: 0,
            own_project: 0.0,
            touched: false,
            touched_seeds: 10,
            touched_max: 10,
            touched_weight: 1.0,
            touched_cross: 1.0,
        }
    }
}

impl Cross {
    /// Whether any part is on.
    pub fn any(&self) -> bool {
        self.route
            || self.split
            || self.slots
            || self.top_cap > 0
            || self.own_project > 0.0
            || self.touched
    }
}

/// A path from the configuration: "~/" is the home directory.
fn home_path(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// The session corpus (docs/INDEX-V2.md, "Sessions as full text").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sessions {
    /// Leave the session corpus out.
    pub off: bool,
    /// A session whose working directory holds any of these is a scripted
    /// batch's, left out.
    pub skip_cwd: Vec<String>,
    /// When set, only sessions whose working directory starts with one of
    /// these (a trial on a few).
    pub only_cwd: Vec<String>,
    /// A subagent's own session too (they are the parent's work, mostly
    /// tool calls).
    pub include_subagents: bool,
    /// A transcript over this many MB is left out (0: none is).
    pub max_mb: usize,
    /// A chunk holds whole turns up to this many characters (about 500
    /// tokens); a longer turn is cut in windows...
    pub chunk_chars: usize,
    /// ...each starting with this many characters of the one before.
    pub overlap_chars: usize,
}

impl Default for Sessions {
    fn default() -> Self {
        Sessions {
            off: false,
            skip_cwd: vec!["style-distill".into()],
            only_cwd: Vec::new(),
            include_subagents: false,
            max_mb: 200,
            chunk_chars: 2000,
            overlap_chars: 200,
        }
    }
}

/// `agent index serve`, the warm query process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Serve {
    /// Searches answered at once (each its own database connection).
    pub workers: usize,
    /// Its Unix socket; "" is `index.sock` in the data directory.
    pub socket: String,
}

impl Default for Serve {
    fn default() -> Self {
        Serve {
            workers: 4,
            socket: String::new(),
        }
    }
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
    /// The card list's kind, project and corpus filters (and `--under`, as
    /// the projects it can hold) go inside pg_search's query, as terms on the
    /// index's filter fields that add nothing to the score, so it scores
    /// only the rows they keep; off, it scores every match and filters
    /// after (the same ranking, slower when the terms are common). Only
    /// where the index has the fields (`schema::bm25_fields`).
    pub pushdown: bool,
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
            pushdown: true,
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
    /// The lexical lists: `bm25` (pg_search), `fts` (native full text),
    /// `both`, or `auto` (""): bm25 where the schema has its indexes, fts
    /// otherwise (docs/INDEX-V2.md, "ParadeDB pg_search"). `--lexical` and
    /// `AGENT_INDEX_LEXICAL` override it for one search.
    pub lexical: String,
}

impl Default for Search {
    fn default() -> Self {
        Search {
            lexical: String::new(),
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
    /// Results its group shows (default: code 8, prose 5, memory 3,
    /// friction 3).
    pub limit: Option<usize>,
    /// Its group's share of `--budget`, in tokens (default: code 700,
    /// prose 450, memory 200, friction 150).
    pub budget: Option<usize>,
}

impl Default for Corpus {
    fn default() -> Self {
        Corpus {
            boost: 1.0,
            limit: None,
            budget: None,
        }
    }
}

/// The corpora a grouped search answers from, in the order shown.
pub const CORPORA: &[&str] = &["code", "prose", "memory", "friction", "session"];

/// What memory notes the sync reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    /// Directories of notes (`*.md`); `~` is the home directory, a `*`
    /// stands for one path component, and `{data}` is this agent's data
    /// directory (beside index.db).
    pub dirs: Vec<String>,
}

impl Default for Memory {
    fn default() -> Self {
        Memory {
            dirs: vec!["~/.claude/projects/*/memory".into(), "{data}/memory".into()],
        }
    }
}

/// Where the friction box is read from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Friction {
    /// Another agent's gomuxai.db, opened read only; "" is this agent's.
    pub store: String,
    /// Leave the friction corpus out.
    pub off: bool,
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
    /// A model's asks (the questions an entity answers) in its card, its
    /// vector and its text search. Off by default: they added nothing
    /// measurable (docs/INDEX-V2.md), so they stay stored, and the
    /// summarizer still writes them, but no list reads them. Switching
    /// rewrites the summarized entities' rows and re-queues their vectors at
    /// the next sync.
    pub asks: bool,
}

impl Default for Cards {
    fn default() -> Self {
        Cards {
            children: 200,
            symbols: 60,
            max_chars: 2000,
            asks: false,
        }
    }
}

/// Which model summaries the live index shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SummaryPolicy {
    /// Entity id prefixes whose model summaries are withheld (their seeds
    /// show): `"project:docs/notes/"`. Empty by default.
    pub withhold: Vec<String>,
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

/// The cross-encoder (docs/INDEX-V2.md, "Reranker"): after fusion and the
/// priors, the top `depth` candidates of a corpus are scored by it and put
/// in its order; on a timeout or an error the fused order stands, and the
/// search says so. Off unless `enabled` or `search --rerank`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reranker {
    pub enabled: bool,
    /// The server's base URL (`/score` on it: vLLM's pooling runner).
    pub endpoint: String,
    pub model: String,
    /// The task instruction sent with each query.
    pub instruction: String,
    pub timeout_ms: u64,
    /// Candidates reranked per corpus.
    pub depth: BTreeMap<String, usize>,
    /// A candidate's text is clipped to this many characters (its card, then
    /// its best chunk's text)...
    pub doc_chars: usize,
    /// ...and the server cuts it at this many tokens.
    pub max_tokens_per_doc: usize,
    /// An identifier's definitions, a whole-query label's entities and a
    /// rare identifier's files stay above what the reranker orders.
    pub keep_pins: bool,
    /// A flat search (one list across corpora: `--kind file`, and the
    /// `hits` of a grouped one) reranks its top this many as one pool.
    pub flat_depth: usize,
}

impl Default for Reranker {
    fn default() -> Self {
        Reranker {
            enabled: false,
            endpoint: "http://127.0.0.1:8100".into(),
            model: "Qwen/Qwen3-Reranker-4B".into(),
            instruction: "Given a code or technical search query, retrieve relevant code entities, SQL statements, or documentation passages that answer the query".into(),
            timeout_ms: 5000,
            doc_chars: 1500,
            max_tokens_per_doc: 512,
            keep_pins: true,
            flat_depth: 50,
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
    pub summaries: Summaries,
    pub query_rewrite: Endpoint,
}

/// `agent index summarize` (docs/INDEX-V2.md, "Summaries and asks, by a
/// local model"): the endpoint, how it is called, how a file's text is
/// clipped and which files are too small to summarize. Every key here is
/// part of each summary's fingerprint, so a change makes them stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Summaries {
    /// An OpenAI-compatible base URL (`http://127.0.0.1:8095`); empty is none.
    pub endpoint: String,
    /// The served model name the requests carry; empty takes the server's
    /// only model. The fingerprint records what the server says it runs.
    pub model: String,
    pub temperature: f64,
    pub max_tokens: u32,
    /// A request's timeout.
    pub timeout_ms: u64,
    /// The sampling seed (attempt n sends seed + n - 1).
    pub seed: u64,
    /// Requests in flight at once.
    pub concurrency: usize,
    /// Further tries of a request after a timeout, a 5xx or a refused
    /// connection, `backoff_ms` doubling each time.
    pub retries: u32,
    pub backoff_ms: u64,
    /// Refused or dropped connections in a row that stop the run (a dead
    /// server), which then exits nonzero.
    pub max_connection_errors: usize,
    /// A file's text in tokens past which it is clipped; code keeps its
    /// head, a doc its heading outline, head and tail.
    pub clip_tokens: usize,
    pub doc_head_tokens: usize,
    pub doc_tail_tokens: usize,
    /// The whole prompt's bound in tokens (under the server's max length,
    /// less `max_tokens`).
    pub max_input_tokens: usize,
    /// Characters read of a file before tokenizing (head, and tail for docs).
    pub preclip_chars: usize,
    /// The symbol list's characters; a longer list, or a clipped file's, is
    /// sampled evenly.
    pub symbols_chars: usize,
    /// Files under either floor are trivial and keep their seed.
    pub min_chars: usize,
    pub min_lines: usize,
    /// A JSON file not named as a manifest or config is summarized only at
    /// or under this size, and only when it is laid out by hand (has lines).
    pub json_max_bytes: u64,
    /// While this file exists no new request starts (a GPU watchdog makes
    /// it when the speech server is busy); empty is none.
    pub pause_file: String,
    /// Words (host names, say) taken out of every input and refused in any
    /// output, besides the built-in key and sign-in patterns.
    pub scrub_words: Vec<String>,
    /// What the server runs, as the person who started it says (the API
    /// doesn't): recorded in the fingerprint.
    pub dtype: String,
    pub quantization: String,
}

impl Default for Summaries {
    fn default() -> Self {
        Summaries {
            endpoint: String::new(),
            model: String::new(),
            temperature: 0.2,
            max_tokens: 500,
            timeout_ms: 180_000,
            seed: 1,
            concurrency: 48,
            retries: 3,
            backoff_ms: 2_000,
            max_connection_errors: 8,
            clip_tokens: 6_000,
            doc_head_tokens: 4_500,
            doc_tail_tokens: 1_500,
            max_input_tokens: 9_000,
            preclip_chars: 80_000,
            symbols_chars: 2_500,
            min_chars: 200,
            min_lines: 5,
            json_max_bytes: 16_384,
            pause_file: String::new(),
            scrub_words: Vec::new(),
            dtype: String::new(),
            quantization: String::new(),
        }
    }
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

/// The sections [`Config::fingerprint`] leaves out.
const NOT_FINGERPRINTED: &[&str] = &["store", "ledger", "log", "watch"];

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
        if !["", "auto", "fts", "bm25", "both"].contains(&s.lexical.as_str()) {
            bail!(
                "search.lexical takes auto, bm25, fts or both, not {:?}",
                s.lexical
            );
        }
        if self.chunks.max_lines < 1 || self.chunks.max_bytes < 256 {
            bail!("chunks: max_lines must be at least 1 and max_bytes at least 256");
        }
        if self.lists.values().any(|w| !w.is_finite() || *w < 0.0) {
            bail!("lists: a list's weight must be a number of 0 or more");
        }
        if !["", "sqlite", "pg", "postgres"].contains(&self.store.kind.as_str()) {
            bail!("store.kind takes sqlite or pg, not {:?}", self.store.kind);
        }
        let x = &self.cross;
        if !(0.0..=1.0).contains(&x.route_ratio) || !(0.0..=1.0).contains(&x.slot_cap) {
            bail!("cross: route_ratio and slot_cap must be between 0 and 1");
        }
        if x.route_cards < 1
            || x.route_per_project < 1
            || [
                x.route_summary,
                x.route_dense,
                x.route_fused,
                x.route_names,
                x.split_raise,
                x.own_project,
                x.touched_weight,
                x.touched_cross,
            ]
            .iter()
            .any(|w| !w.is_finite() || *w < 0.0)
        {
            bail!(
                "cross: the weights must be numbers of 0 or more, route_cards and route_per_project at least 1"
            );
        }
        Ok(())
    }

    /// Whether the pg store is the one to use: `flag` (`--store`), else
    /// `AGENT_INDEX_STORE`, else `[store].kind`; sqlite when none says.
    pub fn wants_pg(&self, flag: &str) -> Result<bool> {
        let env = std::env::var("AGENT_INDEX_STORE").unwrap_or_default();
        let which = [flag, env.as_str(), self.store.kind.as_str()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("");
        match which {
            "" | "sqlite" => Ok(false),
            "pg" | "postgres" => Ok(true),
            other => bail!("--store takes sqlite or pg, not {other:?}"),
        }
    }

    /// The pg store's URL and schema: `AGENT_INDEX_PG`, else
    /// `[store].url`; `AGENT_INDEX_PG_SCHEMA`, else `[store].schema`, else
    /// `idx`. None without a URL.
    pub fn pg_target(&self) -> Option<(String, String)> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let url = env(crate::pg::URL_ENV)
            .or_else(|| (!self.store.url.trim().is_empty()).then(|| self.store.url.clone()))?;
        let schema = env(crate::pg::SCHEMA_ENV)
            .or_else(|| (!self.store.schema.is_empty()).then(|| self.store.schema.clone()))
            .unwrap_or_else(|| crate::pg::DEFAULT_SCHEMA.to_string());
        Some((url, schema))
    }

    /// The ledger's path: `[ledger].path`, else `beside` (index.db beside
    /// the binary's gomuxai.db).
    pub fn ledger_path(&self, beside: PathBuf) -> PathBuf {
        if self.ledger.path.is_empty() {
            beside
        } else {
            home_path(&self.ledger.path)
        }
    }

    /// The watcher's socket: `[watch].socket`, else `index-watch.sock`
    /// beside the ledger.
    pub fn watch_socket(&self, ledger: &Path) -> PathBuf {
        if self.watch.socket.is_empty() {
            ledger
                .parent()
                .unwrap_or(Path::new("."))
                .join("index-watch.sock")
        } else {
            home_path(&self.watch.socket)
        }
    }

    /// The query log's file, when the log is on.
    pub fn log_path(&self) -> Option<PathBuf> {
        if !self.log.enabled {
            return None;
        }
        if !self.log.path.is_empty() {
            return Some(home_path(&self.log.path));
        }
        let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            Some(x) => PathBuf::from(x),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
        };
        Some(base.join("gomuxai").join("index-queries.jsonl"))
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
    /// fingerprints, however their files are written. `[store]`, `[ledger]`
    /// and `[log]` are left out: they change no answer (and a configuration
    /// from before them keeps its fingerprint); so is `[cross]` while it is
    /// at its defaults.
    pub fn fingerprint(&self) -> String {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(m) = v.as_object_mut() {
            for k in NOT_FINGERPRINTED {
                m.remove(*k);
            }
            if self.cross == Cross::default() {
                m.remove("cross");
            }
        }
        let v = serde_json::to_vec(&v).unwrap_or_default();
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

    /// A corpus's group: (results, tokens).
    pub fn corpus_group(&self, corpus: &str) -> (usize, usize) {
        let (l, b) = match corpus {
            "code" => (8, 700),
            "prose" => (5, 450),
            "memory" => (3, 200),
            "session" => (3, 300),
            _ => (3, 150),
        };
        let c = self.corpora.get(corpus);
        (
            c.and_then(|c| c.limit).unwrap_or(l),
            c.and_then(|c| c.budget).unwrap_or(b),
        )
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
        // [cross] at its defaults keeps the fingerprint the defaults had
        // before it existed; a value in it is a new one.
        assert_eq!(d.fingerprint(), "1f3078a48f47");
        let c = Config::parse("[cross]\nslots = true\n").unwrap();
        assert_ne!(c.fingerprint(), d.fingerprint());
        assert!(Config::parse("[cross]\nslot_cap = 2\n").is_err());
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

    /// Where things are ([store], [ledger], [log]) is no part of the
    /// fingerprint; the store comes from the file when nothing else says.
    #[test]
    fn store_ledger_and_log() {
        let d = Config::default();
        let c = Config::parse(
            "[store]\nkind = \"pg\"\nurl = \"postgresql:///db?host=/run/pg\"\nschema = \"idx_t\"\n\
             [ledger]\npath = \"~/ledger/index.db\"\n[log]\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(c.fingerprint(), d.fingerprint());
        assert!(Config::parse("[store]\nkind = \"mysql\"\n").is_err());
        // --store wins; then AGENT_INDEX_STORE (unset in tests unless the
        // runner sets it); then the file.
        assert!(!c.wants_pg("sqlite").unwrap());
        assert!(c.wants_pg("pg").unwrap());
        if std::env::var("AGENT_INDEX_STORE")
            .unwrap_or_default()
            .is_empty()
        {
            assert!(c.wants_pg("").unwrap());
            assert!(!d.wants_pg("").unwrap());
        }
        let (url, schema) = c.pg_target().unwrap();
        match std::env::var(crate::pg::URL_ENV)
            .ok()
            .filter(|u| !u.trim().is_empty())
        {
            Some(env) => assert_eq!(url, env),
            None => assert_eq!(url, "postgresql:///db?host=/run/pg"),
        }
        if std::env::var(crate::pg::SCHEMA_ENV)
            .unwrap_or_default()
            .is_empty()
        {
            assert_eq!(schema, "idx_t");
        }
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                c.ledger_path(PathBuf::from("/x/index.db")),
                PathBuf::from(home).join("ledger/index.db")
            );
        }
        assert_eq!(
            d.ledger_path(PathBuf::from("/x/index.db")),
            PathBuf::from("/x/index.db")
        );
        assert_eq!(c.log_path(), None, "off");
        assert!(
            d.log_path()
                .is_none_or(|p| p.ends_with("gomuxai/index-queries.jsonl"))
        );
    }
}

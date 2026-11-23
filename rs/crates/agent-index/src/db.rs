//! `index.db`: the index's own SQLite file, beside `gomuxai.db` (docs/INDEX.md,
//! "Storage"). WAL, so a query reads while a crawl writes, and a schema version
//! of its own (`PRAGMA user_version`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

/// The schema this build writes. A file at a higher version is from a newer
/// `agent`, and is refused rather than written under rules it doesn't know.
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS roots (
    id TEXT PRIMARY KEY,          -- '@' + its directory's name
    path TEXT NOT NULL UNIQUE,    -- canonical
    depth TEXT NOT NULL,          -- the depth policy its projects start with
    added_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS projects (
    name TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,    -- canonical
    root TEXT NOT NULL DEFAULT '',-- the root id it sits under ('' for project add outside one)
    explicit INTEGER NOT NULL DEFAULT 0, -- added by project add (kept when its root goes)
    depth TEXT NOT NULL,          -- full, files, shallow
    is_git INTEGER NOT NULL DEFAULT 0,
    root_commit TEXT NOT NULL DEFAULT '',
    remote TEXT NOT NULL DEFAULT '',
    last_commit TEXT NOT NULL DEFAULT '',
    last_crawled_at TEXT NOT NULL DEFAULT '',
    cochange_commit TEXT NOT NULL DEFAULT '',
    cochange_at TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS entities (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,           -- root, project, dir, file, symbol, section, concept
    parent TEXT NOT NULL DEFAULT '',
    project TEXT NOT NULL DEFAULT '',
    rel TEXT NOT NULL DEFAULT '', -- its path in its project ('' for a project)
    name TEXT NOT NULL DEFAULT '',
    path TEXT NOT NULL DEFAULT '',-- absolute, canonical
    file_kind TEXT NOT NULL DEFAULT '',
    lang TEXT NOT NULL DEFAULT '',
    size INTEGER NOT NULL DEFAULT 0,
    lines INTEGER NOT NULL DEFAULT 0,
    line INTEGER NOT NULL DEFAULT 0,  -- a symbol's or section's line
    content_key TEXT NOT NULL DEFAULT '',
    mtime INTEGER NOT NULL DEFAULT 0, -- outside git: reuse the key while size and mtime hold
    title TEXT NOT NULL DEFAULT '',
    signature TEXT NOT NULL DEFAULT '',
    untracked INTEGER NOT NULL DEFAULT 0,
    no_content TEXT NOT NULL DEFAULT '', -- why its content wasn't read: large, binary, symlink
    importance REAL NOT NULL DEFAULT 0,
    text_key TEXT NOT NULL DEFAULT '',   -- the hash of the text an embedder would embed
    crawled_at TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS entities_parent ON entities(parent);
CREATE INDEX IF NOT EXISTS entities_project ON entities(project);
CREATE INDEX IF NOT EXISTS entities_path ON entities(path);
CREATE TABLE IF NOT EXISTS aliases (
    old_id TEXT PRIMARY KEY,
    new_id TEXT NOT NULL,
    at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS summaries (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    entity TEXT NOT NULL,
    content_key TEXT NOT NULL DEFAULT '',
    basis TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL DEFAULT '',
    why TEXT NOT NULL DEFAULT '',
    writer TEXT NOT NULL,          -- seed, model:<name>, person
    dream_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,          -- staged, live, dropped
    written_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS summaries_entity ON summaries(entity, seq);
CREATE TABLE IF NOT EXISTS edges (
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    kind TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 1,
    origin TEXT NOT NULL DEFAULT 'code',  -- code, model, person
    evidence TEXT NOT NULL DEFAULT '',
    source_key TEXT NOT NULL DEFAULT '',
    target_key TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (source, target, kind)
);
CREATE INDEX IF NOT EXISTS edges_target ON edges(target, kind);
CREATE TABLE IF NOT EXISTS labels (
    entity TEXT NOT NULL,
    label TEXT NOT NULL,
    norm TEXT NOT NULL,            -- lowercased, words split by single spaces
    compact TEXT NOT NULL,         -- norm without spaces
    kind TEXT NOT NULL,            -- pref, alt, hidden, ask, old
    origin TEXT NOT NULL,          -- code, model, person, lexicon, learned
    weight REAL NOT NULL DEFAULT 1,
    evidence TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (entity, label, kind)
);
CREATE INDEX IF NOT EXISTS labels_norm ON labels(norm);
CREATE INDEX IF NOT EXISTS labels_compact ON labels(compact);
-- A Markdown doc's backticked references, kept so documented-by edges can be
-- resolved again across every project without re-reading the doc.
CREATE TABLE IF NOT EXISTS refs (
    doc TEXT NOT NULL,
    ref TEXT NOT NULL,
    line INTEGER NOT NULL,
    PRIMARY KEY (doc, ref)
);
-- Each package manifest's parse, so depends-on can be resolved across every
-- project without re-reading them.
CREATE TABLE IF NOT EXISTS manifests (
    file TEXT PRIMARY KEY,         -- the manifest's entity id
    dir TEXT NOT NULL,             -- its directory's entity id
    project TEXT NOT NULL,
    ecosystem TEXT NOT NULL,
    name TEXT NOT NULL,
    data TEXT NOT NULL             -- JSON: deps, workspace paths
);
-- References that resolved nowhere: doc drift, listed by stale.
CREATE TABLE IF NOT EXISTS dangling (
    doc TEXT NOT NULL,
    ref TEXT NOT NULL,
    line INTEGER NOT NULL,
    PRIMARY KEY (doc, ref)
);
CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
    id UNINDEXED, title, name, path, summary, symbols, why, labels,
    tokenize = "porter unicode61 tokenchars '_'"
);
CREATE VIRTUAL TABLE IF NOT EXISTS search_tri USING fts5(
    id UNINDEXED, text,
    tokenize = 'trigram'
);
-- The embedding lane's side of the contract (docs/INDEX.md, "Embeddings"):
-- vectors per (entity, chunk, model), half floats; the crawl fills the queue.
CREATE TABLE IF NOT EXISTS vectors (
    entity_id TEXT NOT NULL,
    chunk INTEGER NOT NULL DEFAULT 0,
    model TEXT NOT NULL,
    text_key TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY (entity_id, chunk, model)
);
CREATE TABLE IF NOT EXISTS embed_queue (
    entity_id TEXT PRIMARY KEY,
    text_key TEXT NOT NULL,
    reason TEXT NOT NULL,          -- new, changed, removed
    queued_at TEXT NOT NULL
);
"#;

/// An open `index.db`.
pub struct Index {
    pub conn: Connection,
    pub path: PathBuf,
}

/// Where `index.db` lives: beside `gomuxai.db`, in the data dir next to the binary.
pub fn path_beside(database_path: &str) -> PathBuf {
    let db = Path::new(database_path);
    db.parent().unwrap_or(Path::new(".")).join("index.db")
}

impl Index {
    /// Opens (creating when missing) the index at `path`.
    pub fn open(path: &Path) -> Result<Index> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "{} is at schema version {version}, newer than this agent's {SCHEMA_VERSION}: use the newer agent, or move the file aside to rebuild it",
                path.display()
            );
        }
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("creating the index schema in {}", path.display()))?;
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Index {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// An index in memory, for tests.
    pub fn open_in_memory() -> Result<Index> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Index {
            conn,
            path: PathBuf::from(":memory:"),
        })
    }

    /// The id an old (renamed) id now answers to, if any; follows chains.
    pub fn resolve_alias(&self, id: &str) -> Result<Option<String>> {
        let mut cur = id.to_string();
        let mut seen = 0;
        while let Some(next) = self
            .conn
            .query_row(
                "SELECT new_id FROM aliases WHERE old_id = ?",
                params![cur],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            cur = next;
            seen += 1;
            if seen > 20 {
                break;
            }
        }
        Ok(if seen == 0 { None } else { Some(cur) })
    }
}

/// Now, as stored: RFC 3339 in UTC, seconds.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

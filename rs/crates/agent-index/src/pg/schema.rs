//! The Postgres schema, as versioned migrations embedded in the crate
//! (docs/INDEX-V2.md, "Schema sketch"). Each runs once, in order, in a
//! transaction, recorded in `migrations`. A schema at a version this build
//! doesn't know is refused rather than written under rules it doesn't know.

use anyhow::{Context, Result, bail};

use super::Pg;

/// The migrations, oldest first; a migration's version is its place (from 1).
pub const MIGRATIONS: &[&str] = &[MIGRATION_1, BM25_INDEXES];

/// 1: projects, roots, entities (weighted full text, trigram text, card
/// vector), chunks (english and simple full text), edges, labels, aliases,
/// dangling references, the embed queue, runs, queries and a key-value
/// `meta` for the sync's digests.
const MIGRATION_1: &str = r#"
-- An OR and an AND of tsqueries, to build a query's any-word and every-word
-- forms from its terms inside the statement.
CREATE AGGREGATE tsq_or(tsquery) (SFUNC = tsquery_or, STYPE = tsquery);
CREATE AGGREGATE tsq_and(tsquery) (SFUNC = tsquery_and, STYPE = tsquery);
-- array_to_string is only stable; a generated column needs immutable.
CREATE FUNCTION words_of(text[]) RETURNS text LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT array_to_string($1, ' ') $$;

CREATE TABLE meta (
    key text PRIMARY KEY,
    value text NOT NULL
);

CREATE TABLE roots (
    id text PRIMARY KEY,
    path text NOT NULL,
    depth text NOT NULL,
    added_at text NOT NULL DEFAULT ''
);

CREATE TABLE projects (
    name text PRIMARY KEY,
    path text NOT NULL,
    root text NOT NULL DEFAULT '',
    explicit boolean NOT NULL DEFAULT false,
    depth text NOT NULL DEFAULT '',
    is_git boolean NOT NULL DEFAULT false,
    identity text NOT NULL DEFAULT '',        -- the root commit
    remote text NOT NULL DEFAULT '',
    crawled_commit text NOT NULL DEFAULT '',
    crawled_at text NOT NULL DEFAULT '',
    crawled_depth text NOT NULL DEFAULT ''
);

CREATE TABLE entities (
    id text PRIMARY KEY,
    corpus text NOT NULL DEFAULT 'code',      -- code, prose, session, memory, friction
    kind text NOT NULL,                       -- root, project, dir, file, symbol, section, concept
    project text NOT NULL DEFAULT '',
    parent text NOT NULL DEFAULT '',
    rel text NOT NULL DEFAULT '',
    name text NOT NULL DEFAULT '',
    path text NOT NULL DEFAULT '',
    file_kind text NOT NULL DEFAULT '',
    lang text NOT NULL DEFAULT '',
    size bigint NOT NULL DEFAULT 0,
    lines bigint NOT NULL DEFAULT 0,
    line bigint NOT NULL DEFAULT 0,
    content_key text NOT NULL DEFAULT '',
    title text NOT NULL DEFAULT '',
    signature text NOT NULL DEFAULT '',
    untracked boolean NOT NULL DEFAULT false,
    no_content text NOT NULL DEFAULT '',
    importance double precision NOT NULL DEFAULT 0,
    seed text NOT NULL DEFAULT '',
    summary text NOT NULL DEFAULT '',         -- a model's or person's; empty in iteration 1
    why text NOT NULL DEFAULT '',
    asks text[] NOT NULL DEFAULT '{}',
    summary_key text NOT NULL DEFAULT '',     -- the content key the shown summary was written against
    summary_writer text NOT NULL DEFAULT '',  -- seed, model:<name>, person ('' for none)
    summary_at text NOT NULL DEFAULT '',
    meta jsonb NOT NULL DEFAULT '{}',
    -- Identifiers whole and split (camelCase and snake_case), the path's
    -- words, for the simple configuration.
    terms text NOT NULL DEFAULT '',
    labels_text text NOT NULL DEFAULT '',
    -- A file's symbols or a directory's children, whole and split.
    symbols text NOT NULL DEFAULT '',
    tsv tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('simple'::regconfig, terms || ' ' || labels_text), 'A') ||
        setweight(to_tsvector('english'::regconfig, title || ' ' || summary || ' ' || words_of(asks)), 'B') ||
        setweight(to_tsvector('english'::regconfig, seed || ' ' || why), 'C') ||
        setweight(to_tsvector('simple'::regconfig, symbols), 'D')
    ) STORED,
    -- Names, paths and labels for trigram matching; symbols have none (there
    -- are ten times as many, and full text finds their names whole).
    tri text GENERATED ALWAYS AS (
        CASE WHEN kind <> 'symbol' THEN lower(name || ' ' || rel || ' ' || labels_text) END
    ) STORED,
    embed boolean NOT NULL DEFAULT false,     -- whether it gets a card vector
    text_key text NOT NULL DEFAULT '',        -- the hash of its card text
    chunk_key text NOT NULL DEFAULT '',       -- the content key its chunks were cut from ('' for none)
    sync_key text NOT NULL DEFAULT '',        -- the hash of everything the sync wrote
    card_vec halfvec(384),
    vec_key text NOT NULL DEFAULT '',         -- the text key card_vec was made from
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX entities_tsv ON entities USING gin (tsv);
CREATE INDEX entities_tri ON entities USING gin (tri gin_trgm_ops);
CREATE INDEX entities_parent ON entities (parent);
CREATE INDEX entities_project ON entities (project);
CREATE INDEX entities_path ON entities (path);
CREATE INDEX entities_name ON entities (name);
CREATE INDEX entities_lname ON entities (lower(name));
CREATE INDEX entities_importance ON entities (importance);
CREATE INDEX entities_card_vec ON entities USING hnsw (card_vec halfvec_cosine_ops);

-- Long text, cut by section or in windows of lines, so no tsvector passes
-- its limits (256 positions a lexeme, 1 MB).
CREATE TABLE chunks (
    entity_id text NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    ord integer NOT NULL,
    line_start integer NOT NULL,
    line_end integer NOT NULL,
    heading text NOT NULL DEFAULT '',
    text text NOT NULL,
    terms text NOT NULL DEFAULT '',           -- camelCase identifiers' words
    tsv_en tsvector GENERATED ALWAYS AS (to_tsvector('english'::regconfig, text)) STORED,
    tsv_simple tsvector GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, text || ' ' || terms)) STORED,
    vec halfvec(384),
    meta jsonb NOT NULL DEFAULT '{}',
    PRIMARY KEY (entity_id, ord)
);
CREATE INDEX chunks_tsv_en ON chunks USING gin (tsv_en);
CREATE INDEX chunks_tsv_simple ON chunks USING gin (tsv_simple);

CREATE TABLE edges (
    src text NOT NULL,
    dst text NOT NULL,
    kind text NOT NULL,
    weight double precision NOT NULL DEFAULT 1,
    origin text NOT NULL DEFAULT 'code',
    evidence text NOT NULL DEFAULT '',
    src_key text NOT NULL DEFAULT '',
    dst_key text NOT NULL DEFAULT '',
    PRIMARY KEY (src, dst, kind)
);
CREATE INDEX edges_dst ON edges (dst, kind);
CREATE INDEX edges_kind ON edges (kind);

CREATE TABLE labels (
    entity_id text NOT NULL,
    label text NOT NULL,
    norm text NOT NULL,
    compact text NOT NULL,
    kind text NOT NULL,                       -- pref, alt, hidden, ask, old
    origin text NOT NULL,                     -- code, model, person, lexicon, learned
    weight double precision NOT NULL DEFAULT 1,
    evidence text NOT NULL DEFAULT '',
    PRIMARY KEY (entity_id, label, kind)
);
CREATE INDEX labels_norm ON labels (norm);
CREATE INDEX labels_compact ON labels (compact);

CREATE TABLE aliases (
    old_id text PRIMARY KEY,
    new_id text NOT NULL
);

CREATE TABLE dangling (
    doc text NOT NULL,
    ref text NOT NULL,
    line bigint NOT NULL,
    PRIMARY KEY (doc, ref)
);

CREATE TABLE embed_queue (
    entity_id text PRIMARY KEY,
    text_key text NOT NULL,
    reason text NOT NULL,                     -- new, changed
    queued_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE runs (
    id bigserial PRIMARY KEY,
    kind text NOT NULL,                       -- sync, embed
    config jsonb NOT NULL DEFAULT '{}',
    fingerprint text NOT NULL DEFAULT '',
    started timestamptz NOT NULL,
    finished timestamptz,
    coverage jsonb NOT NULL DEFAULT '{}'
);

CREATE TABLE queries (
    id bigserial PRIMARY KEY,
    at timestamptz NOT NULL DEFAULT now(),
    caller text NOT NULL DEFAULT '',
    text text NOT NULL,
    results jsonb NOT NULL DEFAULT '[]',
    ms jsonb NOT NULL DEFAULT '{}'
);
"#;

/// 2: the BM25 indexes of the A/B (docs/INDEX-V2.md, "ParadeDB pg_search: an
/// A/B"), over the same card text and chunks as the native lists. Only where
/// the database has the `pg_search` extension: elsewhere this does nothing,
/// and [`ensure_bm25`] runs it again once the extension is there. The
/// `source_code` tokenizer splits on punctuation, camelCase and snake_case
/// and lowercases, so `terms`' split words come from the text itself; prose
/// fields are stemmed too, as `to_tsvector('english', ...)` stems them.
/// Chunks index `text` alone: `chunks.terms` holds what the tokenizer
/// already yields. Filter columns aren't indexed: [`super::search::bm25_ctes`]
/// filters on the heap, as the native lists do.
const BM25_INDEXES: &str = r#"
DO $do$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search') THEN
        RETURN;
    END IF;
    CREATE INDEX IF NOT EXISTS entities_bm25 ON entities USING paradedb (
        id,
        (terms::pdb.source_code),
        (labels_text::pdb.source_code),
        (title::pdb.source_code('stemmer=english')),
        (summary::pdb.source_code('stemmer=english')),
        ((words_of(asks))::pdb.source_code('stemmer=english', 'alias=asks')),
        (seed::pdb.source_code('stemmer=english')),
        (why::pdb.source_code('stemmer=english')),
        (symbols::pdb.source_code));
    CREATE INDEX IF NOT EXISTS chunks_bm25 ON chunks USING paradedb (
        entity_id,
        ord,
        (text::pdb.source_code('stemmer=english')));
END
$do$;
"#;

/// Whether this schema has both BM25 indexes.
pub fn has_bm25(pg: &Pg) -> Result<bool> {
    let n: i64 = pg
        .query(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = $1
               AND indexname IN ('entities_bm25', 'chunks_bm25')",
            &[&pg.schema],
        )?
        .first()
        .map(|r| r.get(0))
        .unwrap_or(0);
    Ok(n == 2)
}

/// Makes sure the BM25 indexes are there, for a schema migrated before the
/// database had `pg_search`; says what's missing when it still hasn't.
pub fn ensure_bm25(pg: &Pg) -> Result<()> {
    if has_bm25(pg)? {
        return Ok(());
    }
    pg.batch(BM25_INDEXES)
        .with_context(|| format!("building the BM25 indexes in schema {}", pg.schema))?;
    if !has_bm25(pg)? {
        bail!(
            "BM25 needs the pg_search extension in this database: CREATE EXTENSION pg_search (the server must have it in shared_preload_libraries); --lexical fts uses native full text"
        );
    }
    Ok(())
}

/// Brings the schema to the latest version.
pub fn migrate(pg: &Pg) -> Result<()> {
    pg.batch(
        "CREATE TABLE IF NOT EXISTS migrations (
             version integer PRIMARY KEY,
             applied_at timestamptz NOT NULL DEFAULT now())",
    )?;
    let have: i32 = pg
        .query("SELECT coalesce(max(version), 0) FROM migrations", &[])?
        .first()
        .map(|r| r.get(0))
        .unwrap_or(0);
    let latest = MIGRATIONS.len() as i32;
    if have > latest {
        bail!(
            "schema {} is at version {have}, newer than this agent's {latest}: use the newer agent, or drop the schema to rebuild it",
            pg.schema
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(have as usize) {
        let v = i as i32 + 1;
        let mut c = pg.client();
        let mut tx = c.transaction()?;
        tx.batch_execute(sql)
            .with_context(|| format!("migrating schema {} to version {v}", pg.schema))?;
        tx.execute("INSERT INTO migrations (version) VALUES ($1)", &[&v])?;
        tx.commit()?;
    }
    Ok(())
}

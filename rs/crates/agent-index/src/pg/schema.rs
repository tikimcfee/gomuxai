//! The Postgres schema, as versioned migrations embedded in the crate
//! (docs/INDEX-V2.md, "Schema sketch"). Each runs once, in order, in a
//! transaction, recorded in `migrations`. A schema at a version this build
//! doesn't know is refused rather than written under rules it doesn't know.

use anyhow::{Context, Result, bail};

use super::Pg;

/// The migrations, oldest first; a migration's version is its place (from 1).
pub const MIGRATIONS: &[&str] = &[MIGRATION_1, BM25_INDEXES, LEXEMES, SUMMARIES];

/// 4: every summary a model wrote (`agent index summarize`, `pg::summ`),
/// kept: the live one per entity is shown over its seed while its content
/// key is the entity's (the sync's overlay); a newer one supersedes it; a
/// failed attempt is kept with its reason, so a resume skips it. Done is
/// (content key, fingerprint), never the id alone.
const SUMMARIES: &str = r#"
CREATE TABLE summaries (
    id bigserial PRIMARY KEY,
    entity_id text NOT NULL,
    content_key text NOT NULL,
    fingerprint text NOT NULL,
    run_id bigint NOT NULL DEFAULT 0,
    status text NOT NULL,                     -- live, superseded, failed
    reason text NOT NULL DEFAULT '',          -- a failure's, by the run's names
    summary text NOT NULL DEFAULT '',
    why text NOT NULL DEFAULT '',
    asks text[] NOT NULL DEFAULT '{}',
    dropped_asks text[] NOT NULL DEFAULT '{}', -- by the ask rule or as repeated across entries
    writer text NOT NULL DEFAULT '',          -- model:<id>@<snapshot>
    input_sha text NOT NULL DEFAULT '',       -- sha256 of the rendered request
    attempt integer NOT NULL DEFAULT 1,
    clipped boolean NOT NULL DEFAULT false,
    in_tokens integer NOT NULL DEFAULT 0,
    out_tokens integer NOT NULL DEFAULT 0,
    ms integer NOT NULL DEFAULT 0,
    meta jsonb NOT NULL DEFAULT '{}',
    at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX summaries_key ON summaries (content_key, fingerprint);
CREATE INDEX summaries_entity ON summaries (entity_id, id);
CREATE INDEX summaries_live ON summaries (entity_id) WHERE status = 'live';
"#;

/// 3: how many chunks hold each lexeme (of those in at least
/// [`super::sync::LEXEME_FLOOR`]; a lexeme not listed is rarer), refreshed
/// by the sync when chunks change. Search takes a query's rarest terms for
/// the chunk list, a document frequency that native full text lacks.
const LEXEMES: &str = r#"
CREATE TABLE lexemes (
    config text NOT NULL,                     -- simple, english
    word text NOT NULL,
    ndoc integer NOT NULL,
    PRIMARY KEY (config, word)
);
"#;

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

/// The card index's fields: what the card list matches (`terms` and the
/// card's text) and, since the filters went inside the query (`[bm25]
/// pushdown`), the filter fields, one term per value (`pdb.literal`).
macro_rules! entities_bm25_fields {
    () => {
        "id,
        (terms::pdb.source_code),
        (labels_text::pdb.source_code),
        (title::pdb.source_code('stemmer=english')),
        (summary::pdb.source_code('stemmer=english')),
        ((words_of(asks))::pdb.source_code('stemmer=english', 'alias=asks')),
        (seed::pdb.source_code('stemmer=english')),
        (why::pdb.source_code('stemmer=english')),
        (symbols::pdb.source_code),
        (project::pdb.literal),
        (kind::pdb.literal),
        (file_kind::pdb.literal),
        (corpus::pdb.literal)"
    };
}

/// 2: the BM25 indexes of the A/B (docs/INDEX-V2.md, "ParadeDB pg_search: an
/// A/B"), over the same card text and chunks as the native lists. Only where
/// the database has the `pg_search` extension: elsewhere this does nothing,
/// and [`ensure_bm25`] runs it again once the extension is there. The
/// `source_code` tokenizer splits on punctuation, camelCase and snake_case
/// and lowercases, so `terms`' split words come from the text itself; prose
/// fields are stemmed too, as `to_tsvector('english', ...)` stems them.
/// Chunks index `text` alone: `chunks.terms` holds what the tokenizer
/// already yields, and their filters stay on the heap (a chunk holds no
/// project of its own). A schema whose card index predates the filter
/// fields gets them at connect ([`BM25_FIELDS`]).
const BM25_INDEXES: &str = concat!(
    r#"
DO $do$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_search') THEN
        RETURN;
    END IF;
    CREATE INDEX IF NOT EXISTS entities_bm25 ON entities USING paradedb (
        "#,
    entities_bm25_fields!(),
    r#");
    CREATE INDEX IF NOT EXISTS chunks_bm25 ON chunks USING paradedb (
        entity_id,
        ord,
        (text::pdb.source_code('stemmer=english')));
END
$do$;
"#
);

/// The card index built again with the filter fields, for a schema whose
/// index predates them (a table takes one ParadeDB index, so it is dropped
/// first: searches wait for the build, about 2 s for 460,000 cards). Not a
/// migration: an older agent still reads the schema, and its queries never
/// touch the new fields.
const BM25_FIELDS: &str = concat!(
    "DROP INDEX entities_bm25;
     CREATE INDEX entities_bm25 ON entities USING paradedb (",
    entities_bm25_fields!(),
    ");"
);

/// The card index's definition in this schema, if it has one. Found by
/// name in `pg_class` first and only then read: `pg_get_indexdef` opens
/// the table, and a filter on `pg_indexes.indexdef` may read every
/// schema's indexes, failing on one another process is dropping.
const CARD_INDEX_DEF: &str = "SELECT pg_get_indexdef(c.oid) FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1 AND c.relname = 'entities_bm25' AND c.relkind = 'i'";

/// Whether a card index definition has the filter fields.
fn has_fields(def: &str) -> bool {
    def.contains("(project)::pdb.literal")
}

/// Whether the card index has the filter fields, so a search may filter
/// inside the query ([`super::search::bm25_ctes`]).
pub fn bm25_fields(pg: &Pg) -> Result<bool> {
    Ok(pg
        .query_opt(CARD_INDEX_DEF, &[&pg.schema])?
        .is_some_and(|r| has_fields(&r.get::<_, String>(0))))
}

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
///
/// It runs in one transaction under an advisory lock, so two processes that
/// connect at once to a new schema don't both migrate it: the second waits,
/// then finds it current.
pub fn migrate(pg: &Pg) -> Result<()> {
    let mut c = pg.client();
    let mut tx = c.transaction()?;
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext('agent-index migrate ' || $1))",
        &[&pg.schema],
    )?;
    tx.batch_execute(
        "CREATE TABLE IF NOT EXISTS migrations (
             version integer PRIMARY KEY,
             applied_at timestamptz NOT NULL DEFAULT now())",
    )?;
    let have: i32 = tx
        .query_one("SELECT coalesce(max(version), 0) FROM migrations", &[])?
        .get(0);
    let latest = MIGRATIONS.len() as i32;
    if have > latest {
        bail!(
            "schema {} is at version {have}, newer than this agent's {latest}: use the newer agent, or drop the schema to rebuild it",
            pg.schema
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(have as usize) {
        let v = i as i32 + 1;
        tx.batch_execute(sql)
            .with_context(|| format!("migrating schema {} to version {v}", pg.schema))?;
        tx.execute("INSERT INTO migrations (version) VALUES ($1)", &[&v])?;
    }
    // A card index from before the filter fields gets them, once, under
    // the same lock.
    let stale = tx
        .query_opt(CARD_INDEX_DEF, &[&pg.schema])?
        .is_some_and(|r| !has_fields(&r.get::<_, String>(0)));
    if stale {
        tx.batch_execute(BM25_FIELDS).with_context(|| {
            format!(
                "adding the filter fields to schema {}'s card index",
                pg.schema
            )
        })?;
    }
    tx.commit()?;
    drop(c);
    ensure_support_indexes(pg)
}

/// Indexes that only make queries faster, created when missing: unlike a
/// migration they don't raise the schema's version, so an older agent still
/// reads the schema. (Checked first: CREATE INDEX IF NOT EXISTS would take
/// the table's lock on every connect.)
const SUPPORT_INDEXES: &[(&str, &str)] = &[
    // The small corpora (memory, friction) searched alone start from their
    // few rows, not from every match of a big index.
    (
        "entities_corpus",
        "CREATE INDEX IF NOT EXISTS entities_corpus ON entities (corpus)",
    ),
];

fn ensure_support_indexes(pg: &Pg) -> Result<()> {
    for (name, sql) in SUPPORT_INDEXES {
        let have = !pg
            .query(
                "SELECT 1 FROM pg_indexes WHERE schemaname = $1 AND indexname = $2",
                &[&pg.schema, name],
            )?
            .is_empty();
        if !have {
            pg.batch(sql)
                .with_context(|| format!("creating index {name} in schema {}", pg.schema))?;
        }
    }
    Ok(())
}

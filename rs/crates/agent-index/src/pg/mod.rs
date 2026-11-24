//! The index on Postgres (docs/INDEX-V2.md, "Store: Postgres"): one schema
//! (`idx` by default) holding entities, chunks, edges and labels, with the
//! ranking, fuzzy matching, vectors and graph walks done by the database.
//!
//! Iteration 1 keeps v1's crawl and its SQLite file as the crawl's ledger
//! (renames, aliases, references, manifests, co-change bookkeeping): `sync`
//! mirrors it into Postgres incrementally, and every query reads Postgres
//! only. Moving that bookkeeping into Postgres is a later step.
//!
//! The connection is a URL (`postgresql:///devindex?host=/run/pg`, a Unix
//! socket with peer authentication, say) given by the caller: from
//! `AGENT_INDEX_PG` in the CLI, never a path in this crate.

use std::cell::RefCell;

use anyhow::{Context, Result, bail};
use postgres::{Client, NoTls};

use crate::config::Config;

pub mod cross;
pub mod dirsum;
#[cfg(feature = "embed")]
pub mod embed_pg;
pub mod fresh;
pub mod rerank;
pub mod schema;
pub mod search;
pub mod serve;
pub mod sessions;
pub mod side;
pub mod summ;
pub mod sync;
pub mod view;

#[cfg(test)]
mod pg_test;
#[cfg(test)]
mod summ_test;

/// The environment variable that holds the database URL.
pub const URL_ENV: &str = "AGENT_INDEX_PG";
/// The environment variable that names the schema (default [`DEFAULT_SCHEMA`]).
pub const SCHEMA_ENV: &str = "AGENT_INDEX_PG_SCHEMA";
pub const DEFAULT_SCHEMA: &str = "idx";

/// The vectors' dimension (bge-small-en-v1.5).
pub const DIM: usize = 384;

/// An open connection, its search path set to the index's schema (then
/// `public`, where the extensions' types and operators live). Queries take
/// `&self`: the client sits in a RefCell, and nothing here holds a borrow
/// across a call that takes it again.
pub struct Pg {
    client: RefCell<Client>,
    pub schema: String,
    pub url_shown: String,
    /// The tunables every step reads (crate::config).
    pub cfg: Config,
}

/// Whether a schema name is a plain lowercase identifier (it goes into SQL
/// unquoted).
pub fn check_schema(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        bail!("schema {s:?} must be a lowercase identifier (letters, digits, _)");
    }
    Ok(())
}

/// A URL with any password left out, to print.
fn shown(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(a), Some(b)) if b > a => format!("{}://…{}", &url[..a], &url[b..]),
        _ => url.to_string(),
    }
}

impl Pg {
    /// Connects, creates the schema when missing and brings it to the
    /// current version.
    pub fn connect(url: &str, schema: &str) -> Result<Pg> {
        Pg::connect_with(url, schema, Config::default())
    }

    /// Connects with a configuration (see [`Pg::connect`]).
    pub fn connect_with(url: &str, schema: &str, cfg: Config) -> Result<Pg> {
        check_schema(schema)?;
        let client =
            Client::connect(url, NoTls).with_context(|| format!("connecting to {}", shown(url)))?;
        let (ef, trgm) = (cfg.search.hnsw_ef_search, cfg.search.trigram_threshold);
        let pg = Pg {
            client: RefCell::new(client),
            schema: schema.to_string(),
            url_shown: shown(url),
            cfg,
        };
        pg.batch(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))?;
        // The vector library must be loaded before its settings mean anything
        // (a SET before then makes a placeholder that does nothing).
        pg.batch(&format!(
            "SET search_path TO {schema}, public; LOAD 'vector'; LOAD 'pg_trgm'; \
             SET hnsw.iterative_scan = relaxed_order; SET hnsw.ef_search = {ef}; SET jit = off; \
             SET pg_trgm.word_similarity_threshold = {trgm}; SET paradedb.planner_warnings = off;"
        ))?;
        schema::migrate(&pg)?;
        Ok(pg)
    }

    /// The URL and schema from the environment: `AGENT_INDEX_PG`, and
    /// `AGENT_INDEX_PG_SCHEMA` or `idx`. None when the URL isn't set.
    pub fn from_env() -> Option<(String, String)> {
        let url = std::env::var(URL_ENV)
            .ok()
            .filter(|u| !u.trim().is_empty())?;
        let schema = std::env::var(SCHEMA_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SCHEMA.to_string());
        Some((url, schema))
    }

    /// The client, for one call at a time.
    pub fn client(&self) -> std::cell::RefMut<'_, Client> {
        self.client.borrow_mut()
    }

    pub fn batch(&self, sql: &str) -> Result<()> {
        self.client().batch_execute(sql)?;
        Ok(())
    }

    pub fn query(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<postgres::Row>> {
        Ok(self.client().query(sql, params)?)
    }

    pub fn query_opt(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Option<postgres::Row>> {
        Ok(self.client().query_opt(sql, params)?)
    }

    pub fn execute(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<u64> {
        Ok(self.client().execute(sql, params)?)
    }

    /// Drops the schema and everything in it (tests' throwaway schemas).
    pub fn drop_schema(&self) -> Result<()> {
        self.batch(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
    }

    /// Each table's size on disk, with its indexes and TOAST, largest first.
    pub fn table_sizes(&self) -> Result<Vec<(String, i64)>> {
        let rows = self.query(
            "SELECT c.relname::text, pg_total_relation_size(c.oid)
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relkind = 'r' ORDER BY 2 DESC",
            &[&self.schema],
        )?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }
}

/// One field for COPY's text format: backslash, tab, newline and carriage
/// return escaped; NUL (which a Postgres text can't hold) dropped.
pub fn copy_field(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => {}
            c => out.push(c),
        }
    }
}

/// One COPY row of text fields.
pub fn copy_row(out: &mut String, fields: &[&str]) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        copy_field(out, f);
    }
    out.push('\n');
}

/// A vector as pgvector's text form, `[0.1,0.2,...]`.
pub fn vec_text(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

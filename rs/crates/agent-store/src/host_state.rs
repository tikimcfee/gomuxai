//! Values that belong to the host rather than to an agent (focus, say). The
//! table is the Rust build's own; the Go binary opens a store that has it as
//! before.

use anyhow::{Context, Result};
use rusqlite::params;

use crate::gotime::Time;
use crate::jobs::has_table;
use crate::sqlite::{Store, query_one, text};
use crate::tree::time_nanos;

/// HOST_STATE_SCHEMA is a key/value store for this host.
pub(crate) const HOST_STATE_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS host_state (
		key TEXT PRIMARY KEY,
		value TEXT NOT NULL,
		updated_at TEXT NOT NULL
	);
"#;

impl Store {
    /// get_host_state returns key's value, or None when it isn't set (or the
    /// store, opened read-only, predates host state).
    pub fn get_host_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        if !has_table(self, &conn, "host_state")? {
            return Ok(None);
        }
        query_one(
            &conn,
            "SELECT value FROM host_state WHERE key = ?",
            [key],
            |r| text(r, 0),
        )
        .context("querying host state")
    }

    /// set_host_state sets key to value.
    pub fn set_host_state(&self, key: &str, value: &str) -> Result<()> {
        self.exec(
            "setting host state",
            "INSERT OR REPLACE INTO host_state (key, value, updated_at) VALUES (?, ?, ?)",
            params![key, value, time_nanos(&Time::now())],
        )?;
        Ok(())
    }

    /// delete_host_state removes key; removing a key that isn't set is not an
    /// error.
    pub fn delete_host_state(&self, key: &str) -> Result<()> {
        self.exec(
            "deleting host state",
            "DELETE FROM host_state WHERE key = ?",
            [key],
        )?;
        Ok(())
    }
}

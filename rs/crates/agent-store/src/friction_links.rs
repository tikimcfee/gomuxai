use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Row, params};

use crate::commands::time_key;
use crate::friction::optional_time;
use crate::models::FrictionLink;
use crate::sqlite::{Store, int, parse_time, query_all, text};

/// FRICTION_LINKS_SCHEMA holds links between friction entries on different
/// hosts: the same problem, filed twice. Each host keeps its own half of a
/// link, keyed by its own entry, and caches what it last saw of the other
/// end. Host names match ignoring case.
pub(crate) const FRICTION_LINKS_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS friction_links (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		friction_id INTEGER NOT NULL REFERENCES friction(id) ON DELETE CASCADE,
		peer_host TEXT NOT NULL COLLATE NOCASE,
		peer_id INTEGER NOT NULL,
		peer_uid TEXT NOT NULL DEFAULT '',
		linked_by TEXT NOT NULL,
		linked_at TEXT NOT NULL,
		note TEXT NOT NULL DEFAULT '',
		confirmed_at TEXT NOT NULL DEFAULT '',
		peer_status TEXT NOT NULL DEFAULT '',
		peer_kind TEXT NOT NULL DEFAULT '',
		peer_summary TEXT NOT NULL DEFAULT '',
		peer_votes INTEGER NOT NULL DEFAULT 0,
		peer_resolved_by TEXT NOT NULL DEFAULT '',
		peer_resolution TEXT NOT NULL DEFAULT '',
		peer_checked_at TEXT NOT NULL DEFAULT '',
		UNIQUE (friction_id, peer_host, peer_id)
	);
"#;

const FRICTION_LINK_COLUMNS: &str = "id, friction_id, peer_host, peer_id, peer_uid, linked_by, linked_at, note,
	confirmed_at, peer_status, peer_kind, peer_summary, peer_votes, peer_resolved_by, peer_resolution, peer_checked_at";

fn scan_friction_link(row: &Row<'_>) -> rusqlite::Result<FrictionLink> {
    Ok(FrictionLink {
        id: int(row, 0)?,
        friction_id: int(row, 1)?,
        peer_host: text(row, 2)?,
        peer_id: int(row, 3)?,
        peer_uid: text(row, 4)?,
        linked_by: text(row, 5)?,
        linked_at: parse_time(&text(row, 6)?),
        note: text(row, 7)?,
        confirmed_at: parse_time(&text(row, 8)?),
        peer_status: text(row, 9)?,
        peer_kind: text(row, 10)?,
        peer_summary: text(row, 11)?,
        peer_votes: int(row, 12)?,
        peer_resolved_by: text(row, 13)?,
        peer_resolution: text(row, 14)?,
        peer_checked_at: parse_time(&text(row, 15)?),
    })
}

impl Store {
    /// add_friction_link records a link, or, if l.friction_id already has one
    /// to that entry, updates it: the uid and confirmation once known, and
    /// what was seen of the other end. Who linked it, when, and its note stay
    /// as first recorded. It sets l.id.
    pub fn add_friction_link(&self, l: &mut FrictionLink) -> Result<()> {
        l.id = self
            .conn()
            .query_row(
                "
		INSERT INTO friction_links (friction_id, peer_host, peer_id, peer_uid, linked_by, linked_at, note,
			confirmed_at, peer_status, peer_kind, peer_summary, peer_votes, peer_resolved_by, peer_resolution, peer_checked_at)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		ON CONFLICT (friction_id, peer_host, peer_id) DO UPDATE SET
			peer_uid = CASE WHEN excluded.peer_uid <> '' THEN excluded.peer_uid ELSE peer_uid END,
			confirmed_at = CASE WHEN excluded.confirmed_at <> '' THEN excluded.confirmed_at ELSE confirmed_at END,
			note = CASE WHEN note = '' THEN excluded.note ELSE note END,
			peer_status = CASE WHEN excluded.peer_checked_at <> '' THEN excluded.peer_status ELSE peer_status END,
			peer_kind = CASE WHEN excluded.peer_checked_at <> '' THEN excluded.peer_kind ELSE peer_kind END,
			peer_summary = CASE WHEN excluded.peer_checked_at <> '' THEN excluded.peer_summary ELSE peer_summary END,
			peer_votes = CASE WHEN excluded.peer_checked_at <> '' THEN excluded.peer_votes ELSE peer_votes END,
			peer_resolved_by = CASE WHEN excluded.peer_checked_at <> '' THEN excluded.peer_resolved_by ELSE peer_resolved_by END,
			peer_resolution = CASE WHEN excluded.peer_checked_at <> '' THEN excluded.peer_resolution ELSE peer_resolution END,
			peer_checked_at = CASE WHEN excluded.peer_checked_at <> '' THEN excluded.peer_checked_at ELSE peer_checked_at END
		RETURNING id
	",
                params![
                    l.friction_id,
                    l.peer_host,
                    l.peer_id,
                    l.peer_uid,
                    l.linked_by,
                    time_key(&l.linked_at),
                    l.note,
                    optional_time(&l.confirmed_at),
                    l.peer_status,
                    l.peer_kind,
                    l.peer_summary,
                    l.peer_votes,
                    l.peer_resolved_by,
                    l.peer_resolution,
                    optional_time(&l.peer_checked_at)
                ],
                |r| int(r, 0),
            )
            .context("linking friction")?;
        Ok(())
    }

    /// list_friction_links returns the links of entry friction_id, or with 0,
    /// every link in the store, by entry, then oldest first.
    pub fn list_friction_links(&self, friction_id: i64) -> Result<Vec<FrictionLink>> {
        let mut query = format!("SELECT {FRICTION_LINK_COLUMNS} FROM friction_links");
        let mut args: Vec<Value> = Vec::new();
        if friction_id != 0 {
            query += " WHERE friction_id = ?";
            args.push(Value::Integer(friction_id));
        }
        query += " ORDER BY friction_id, id";
        query_all(
            &self.conn(),
            &query,
            rusqlite::params_from_iter(args),
            scan_friction_link,
        )
        .context("querying friction links")
    }

    /// update_friction_link saves what was last seen of a link's other end:
    /// its id (an entry closed there as a duplicate hands its links to its
    /// original), uid, confirmation, and cached state. If the link's entry
    /// already links to the new id, this one is left as it was.
    pub fn update_friction_link(&self, l: &FrictionLink) -> Result<()> {
        self.conn()
            .execute(
                "
		UPDATE OR IGNORE friction_links SET peer_id = ?, peer_uid = ?, confirmed_at = ?, peer_status = ?, peer_kind = ?,
			peer_summary = ?, peer_votes = ?, peer_resolved_by = ?, peer_resolution = ?, peer_checked_at = ?
		WHERE id = ?
	",
                params![
                    l.peer_id,
                    l.peer_uid,
                    optional_time(&l.confirmed_at),
                    l.peer_status,
                    l.peer_kind,
                    l.peer_summary,
                    l.peer_votes,
                    l.peer_resolved_by,
                    l.peer_resolution,
                    optional_time(&l.peer_checked_at),
                    l.id
                ],
            )
            .context("updating friction link")?;
        Ok(())
    }

    /// delete_friction_link removes entry friction_id's link to peer_id on
    /// peer_host (matched ignoring case), saying whether there was one.
    pub fn delete_friction_link(
        &self,
        friction_id: i64,
        peer_host: &str,
        peer_id: i64,
    ) -> Result<bool> {
        let n = self
            .conn()
            .execute(
                "DELETE FROM friction_links WHERE friction_id = ? AND peer_host = ? AND peer_id = ?",
                params![friction_id, peer_host, peer_id],
            )
            .context("unlinking friction")?;
        Ok(n > 0)
    }
}

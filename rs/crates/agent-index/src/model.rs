//! Entities as the read side sees them: a row, its current summary and its
//! freshness (docs/INDEX.md, "Freshness keys").

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// One entity row.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub parent: String,
    pub project: String,
    pub rel: String,
    pub name: String,
    pub path: String,
    pub file_kind: String,
    pub lang: String,
    pub size: i64,
    pub lines: i64,
    pub line: i64,
    pub content_key: String,
    pub title: String,
    pub signature: String,
    pub untracked: bool,
    pub no_content: String,
    pub importance: f64,
}

pub const ENTITY_COLS: &str = "id, kind, parent, project, rel, name, path, file_kind, lang, size, lines, line, content_key, title, signature, untracked, no_content, importance";

impl Entity {
    pub fn from_row(r: &Row) -> rusqlite::Result<Entity> {
        Ok(Entity {
            id: r.get(0)?,
            kind: r.get(1)?,
            parent: r.get(2)?,
            project: r.get(3)?,
            rel: r.get(4)?,
            name: r.get(5)?,
            path: r.get(6)?,
            file_kind: r.get(7)?,
            lang: r.get(8)?,
            size: r.get(9)?,
            lines: r.get(10)?,
            line: r.get(11)?,
            content_key: r.get(12)?,
            title: r.get(13)?,
            signature: r.get(14)?,
            untracked: r.get::<_, i64>(15)? != 0,
            no_content: r.get(16)?,
            importance: r.get(17)?,
        })
    }

    /// A one-line title: its own, else its name, else its id.
    pub fn display_title(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.name.is_empty() {
            &self.name
        } else {
            &self.id
        }
    }
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Entity>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ENTITY_COLS} FROM entities WHERE id = ?"),
            params![id],
            Entity::from_row,
        )
        .optional()?)
}

/// The summary an entity shows, and how current it is.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Summary {
    pub text: String,
    pub why: String,
    pub writer: String,
    /// The content key it was written against.
    pub content_key: String,
    pub written_at: String,
}

/// fresh, drifted, stale, seed or none.
pub fn freshness(entity_key: &str, s: Option<&Summary>) -> &'static str {
    match s {
        None => "none",
        Some(s) if s.writer == "seed" => "seed",
        Some(s) if s.content_key == entity_key => "fresh",
        // v1 doesn't measure how far the code moved (git diff --numstat between
        // the summary's commit and now): a changed key reads stale.
        Some(_) => "stale",
    }
}

/// The summary an entity shows: its latest live summary a model or person
/// wrote, else its latest live seed.
pub fn summary(conn: &Connection, id: &str) -> Result<Option<Summary>> {
    let q = "SELECT text, why, writer, content_key, written_at FROM summaries
             WHERE entity = ? AND status = 'live' AND text != ''
             ORDER BY (writer = 'seed'), seq DESC LIMIT 1";
    Ok(conn
        .query_row(q, params![id], |r| {
            Ok(Summary {
                text: r.get(0)?,
                why: r.get(1)?,
                writer: r.get(2)?,
                content_key: r.get(3)?,
                written_at: r.get(4)?,
            })
        })
        .optional()?)
}

/// The project the id names, or "" (`alpha:src/x.rs` → `alpha`).
pub fn project_of_id(id: &str) -> &str {
    if id.starts_with('@') || id.starts_with("concept:") {
        return "";
    }
    id.split(':').next().unwrap_or("")
}

/// The file entity a symbol or section belongs to (its id up to `#`).
pub fn file_of(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
}

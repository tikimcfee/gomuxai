use anyhow::{Context, Result};

use crate::sqlite::Store;

/// HostRenamed counts what rename_host rewrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostRenamed {
    /// agents whose spawned_by named the old host
    pub agents: i64,
    /// friction links to entries on the old host
    pub friction_links: i64,
}

/// `UPDATE <table> SET <col> = ...`: rewrites "name@host" values whose host
/// (after the last @; names have none) is old, ignoring case.
fn rewrite(column: &str, table: &str) -> String {
    let host = format!("substr({column}, instr({column}, '@') + 1)");
    format!(
        "UPDATE {table} SET {column} = substr({column}, 1, instr({column}, '@')) || ? WHERE instr({column}, '@') > 0 AND lower({host}) = lower(?)"
    )
}

impl Store {
    /// rename_host rewrites this store's references to another host that was
    /// renamed from old to new (agent bus trust --renames): agents spawned
    /// from there ("lead@old" in spawned_by, and in their runs, so the tree
    /// shows the same parent) and friction links to its entries. Host names
    /// match ignoring case. Text that names the old host (held messages,
    /// briefings) is left as written.
    pub fn rename_host(&self, old: &str, new: &str) -> Result<HostRenamed> {
        self.in_tx(|tx| {
            let mut n = HostRenamed::default();
            let agents = tx
                .execute(&rewrite("spawned_by", "agents"), [new, old])
                .context("renaming spawned_by")?;
            n.agents = agents as i64;
            tx.execute(&rewrite("parent", "runs"), [new, old])
                .context("renaming runs' parent")?;

            // peer_host matches ignoring case (COLLATE NOCASE). A link the
            // entry already has under the new name wins; the old one goes.
            let links = tx
                .execute(
                    "UPDATE OR IGNORE friction_links SET peer_host = ? WHERE peer_host = ?",
                    [new, old],
                )
                .context("renaming friction links")?;
            n.friction_links = links as i64;
            let dropped = tx
                .execute(
                    "DELETE FROM friction_links WHERE peer_host = ? AND peer_host <> ?",
                    [old, new],
                )
                .context("dropping duplicate friction links")?;
            n.friction_links += dropped as i64;
            Ok(n)
        })
    }
}

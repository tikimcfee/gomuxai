package storage

import (
	"context"
	"database/sql"
	"fmt"
)

// HostRenamed counts what RenameHost rewrote.
type HostRenamed struct {
	Agents        int // agents whose spawned_by named the old host
	FrictionLinks int // friction links to entries on the old host
}

// RenameHost rewrites this store's references to another host that was
// renamed from old to new (agent bus trust --renames): agents spawned from
// there ("lead@old" in spawned_by, and in their runs, so the tree shows
// the same parent) and friction links to its entries. Host names match
// ignoring case. Text that names the old host (held messages, briefings)
// is left as written.
func (s *SQLiteStorage) RenameHost(ctx context.Context, old, new string) (HostRenamed, error) {
	var n HostRenamed
	err := s.inTx(ctx, func(tx *sql.Tx) error {
		// "name@host": the host is after the last @ (names have none).
		const host = `substr(%[1]s, instr(%[1]s, '@') + 1)`
		const rewrite = `UPDATE %[2]s SET %[1]s = substr(%[1]s, 1, instr(%[1]s, '@')) || ? WHERE instr(%[1]s, '@') > 0 AND lower(` + host + `) = lower(?)`
		res, err := tx.ExecContext(ctx, fmt.Sprintf(rewrite, "spawned_by", "agents"), new, old)
		if err != nil {
			return fmt.Errorf("renaming spawned_by: %w", err)
		}
		agents, _ := res.RowsAffected()
		n.Agents = int(agents)
		if _, err := tx.ExecContext(ctx, fmt.Sprintf(rewrite, "parent", "runs"), new, old); err != nil {
			return fmt.Errorf("renaming runs' parent: %w", err)
		}

		// peer_host matches ignoring case (COLLATE NOCASE). A link the
		// entry already has under the new name wins; the old one goes.
		res, err = tx.ExecContext(ctx, `UPDATE OR IGNORE friction_links SET peer_host = ? WHERE peer_host = ?`, new, old)
		if err != nil {
			return fmt.Errorf("renaming friction links: %w", err)
		}
		links, _ := res.RowsAffected()
		n.FrictionLinks = int(links)
		if res, err = tx.ExecContext(ctx, `DELETE FROM friction_links WHERE peer_host = ? AND peer_host <> ?`, old, new); err != nil {
			return fmt.Errorf("dropping duplicate friction links: %w", err)
		}
		dropped, _ := res.RowsAffected()
		n.FrictionLinks += int(dropped)
		return nil
	})
	return n, err
}

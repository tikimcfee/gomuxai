package storage

import (
	"context"
	"fmt"
)

// frictionLinksSchema holds links between friction entries on different
// hosts: the same problem, filed twice. Each host keeps its own half of a
// link, keyed by its own entry, and caches what it last saw of the other
// end. Host names match ignoring case.
const frictionLinksSchema = `
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
`

const frictionLinkColumns = `id, friction_id, peer_host, peer_id, peer_uid, linked_by, linked_at, note,
	confirmed_at, peer_status, peer_kind, peer_summary, peer_votes, peer_resolved_by, peer_resolution, peer_checked_at`

func scanFrictionLink(row scanner) (*FrictionLink, error) {
	var l FrictionLink
	var linkedAt, confirmedAt, checkedAt string
	if err := row.Scan(&l.ID, &l.FrictionID, &l.PeerHost, &l.PeerID, &l.PeerUID, &l.LinkedBy, &linkedAt, &l.Note,
		&confirmedAt, &l.PeerStatus, &l.PeerKind, &l.PeerSummary, &l.PeerVotes, &l.PeerResolvedBy, &l.PeerResolution,
		&checkedAt); err != nil {
		return nil, err
	}
	l.LinkedAt = parseTime(linkedAt)
	l.ConfirmedAt = parseTime(confirmedAt)
	l.PeerCheckedAt = parseTime(checkedAt)
	return &l, nil
}

// AddFrictionLink records a link, or, if l.FrictionID already has one to
// that entry, updates it: the uid and confirmation once known, and what was
// seen of the other end. Who linked it, when, and its note stay as first
// recorded. It sets l.ID.
func (s *SQLiteStorage) AddFrictionLink(ctx context.Context, l *FrictionLink) error {
	err := s.db.QueryRowContext(ctx, `
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
	`, l.FrictionID, l.PeerHost, l.PeerID, l.PeerUID, l.LinkedBy, timeKey(l.LinkedAt), l.Note,
		optionalTime(l.ConfirmedAt), l.PeerStatus, l.PeerKind, l.PeerSummary, l.PeerVotes, l.PeerResolvedBy,
		l.PeerResolution, optionalTime(l.PeerCheckedAt)).Scan(&l.ID)
	if err != nil {
		return fmt.Errorf("linking friction: %w", err)
	}
	return nil
}

// ListFrictionLinks returns the links of entry frictionID, or with 0, every
// link in the store, by entry, then oldest first.
func (s *SQLiteStorage) ListFrictionLinks(ctx context.Context, frictionID int64) ([]*FrictionLink, error) {
	query := `SELECT ` + frictionLinkColumns + ` FROM friction_links`
	var args []any
	if frictionID != 0 {
		query, args = query+` WHERE friction_id = ?`, append(args, frictionID)
	}
	links, err := queryAll(ctx, s.db, scanFrictionLink, query+` ORDER BY friction_id, id`, args...)
	if err != nil {
		return nil, fmt.Errorf("querying friction links: %w", err)
	}
	return links, nil
}

// UpdateFrictionLink saves what was last seen of a link's other end: its
// id (an entry closed there as a duplicate hands its links to its
// original), uid, confirmation, and cached state. If the link's entry
// already links to the new id, this one is left as it was.
func (s *SQLiteStorage) UpdateFrictionLink(ctx context.Context, l *FrictionLink) error {
	if _, err := s.db.ExecContext(ctx, `
		UPDATE OR IGNORE friction_links SET peer_id = ?, peer_uid = ?, confirmed_at = ?, peer_status = ?, peer_kind = ?,
			peer_summary = ?, peer_votes = ?, peer_resolved_by = ?, peer_resolution = ?, peer_checked_at = ?
		WHERE id = ?
	`, l.PeerID, l.PeerUID, optionalTime(l.ConfirmedAt), l.PeerStatus, l.PeerKind, l.PeerSummary, l.PeerVotes,
		l.PeerResolvedBy, l.PeerResolution, optionalTime(l.PeerCheckedAt), l.ID); err != nil {
		return fmt.Errorf("updating friction link: %w", err)
	}
	return nil
}

// DeleteFrictionLink removes entry frictionID's link to peerID on
// peerHost (matched ignoring case), saying whether there was one.
func (s *SQLiteStorage) DeleteFrictionLink(ctx context.Context, frictionID int64, peerHost string, peerID int64) (bool, error) {
	res, err := s.db.ExecContext(ctx, `DELETE FROM friction_links WHERE friction_id = ? AND peer_host = ? AND peer_id = ?`,
		frictionID, peerHost, peerID)
	if err != nil {
		return false, fmt.Errorf("unlinking friction: %w", err)
	}
	n, err := res.RowsAffected()
	return n > 0, err
}

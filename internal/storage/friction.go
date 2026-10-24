package storage

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"strings"
	"time"
)

// frictionSchema holds the friction box (agent friction): entries, and the
// +1 votes on them. An attached command is stored as JSON.
const frictionSchema = `
	CREATE TABLE IF NOT EXISTS friction (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		uid TEXT NOT NULL UNIQUE,
		host TEXT NOT NULL,
		filed_by TEXT NOT NULL,
		parent TEXT NOT NULL,
		cwd TEXT NOT NULL,
		build TEXT NOT NULL,
		created_at TEXT NOT NULL,
		updated_at TEXT NOT NULL,
		kind TEXT NOT NULL,
		summary TEXT NOT NULL,
		detail TEXT NOT NULL,
		command TEXT NOT NULL,
		status TEXT NOT NULL,
		accepted_by TEXT NOT NULL DEFAULT '',
		accepted_at TEXT NOT NULL DEFAULT '',
		accept_note TEXT NOT NULL DEFAULT '',
		resolved_by TEXT NOT NULL DEFAULT '',
		resolved_at TEXT NOT NULL DEFAULT '',
		resolution TEXT NOT NULL DEFAULT '',
		duplicate_of INTEGER
	);

	CREATE TABLE IF NOT EXISTS friction_votes (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		friction_id INTEGER NOT NULL REFERENCES friction(id) ON DELETE CASCADE,
		uid TEXT NOT NULL UNIQUE,
		host TEXT NOT NULL,
		voter TEXT NOT NULL,
		parent TEXT NOT NULL,
		build TEXT NOT NULL,
		created_at TEXT NOT NULL,
		note TEXT NOT NULL,
		command TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_friction_votes_friction
		ON friction_votes(friction_id);
`

// frictionColumns ends with the vote count: the entry's own votes, its
// duplicates' votes, and one for each duplicate's filing.
const frictionColumns = `id, uid, host, filed_by, parent, cwd, build, created_at, updated_at, kind,
	summary, detail, command, status, accepted_by, accepted_at, accept_note,
	resolved_by, resolved_at, resolution, COALESCE(duplicate_of, 0),
	(SELECT COUNT(*) FROM friction_votes v WHERE v.friction_id = friction.id
		OR v.friction_id IN (SELECT d.id FROM friction d WHERE d.duplicate_of = friction.id))
	+ (SELECT COUNT(*) FROM friction d WHERE d.duplicate_of = friction.id)`

func scanFriction(row scanner) (*Friction, error) {
	var f Friction
	var createdAt, updatedAt, acceptedAt, resolvedAt, command string
	if err := row.Scan(&f.ID, &f.UID, &f.Host, &f.FiledBy, &f.Parent, &f.Cwd, &f.Build, &createdAt, &updatedAt,
		&f.Kind, &f.Summary, &f.Detail, &command, &f.Status, &f.AcceptedBy, &acceptedAt, &f.AcceptNote,
		&f.ResolvedBy, &resolvedAt, &f.Resolution, &f.DuplicateOf, &f.Votes); err != nil {
		return nil, err
	}
	f.CreatedAt = parseTime(createdAt)
	f.UpdatedAt = parseTime(updatedAt)
	f.AcceptedAt = parseTime(acceptedAt)
	f.ResolvedAt = parseTime(resolvedAt)
	f.Command = decodeCommand(command)
	return &f, nil
}

// encodeCommand stores an attached command as JSON, "" for none.
func encodeCommand(c *AttachedCommand) string {
	if c == nil {
		return ""
	}
	data, _ := json.Marshal(c)
	return string(data)
}

func decodeCommand(s string) *AttachedCommand {
	if s == "" {
		return nil
	}
	var c AttachedCommand
	if json.Unmarshal([]byte(s), &c) != nil {
		return nil
	}
	return &c
}

// optionalTime formats t for storage, "" for the zero time.
func optionalTime(t time.Time) string {
	if t.IsZero() {
		return ""
	}
	return timeKey(t)
}

// AddFriction files a friction entry and sets f.ID.
func (s *SQLiteStorage) AddFriction(ctx context.Context, f *Friction) error {
	id, err := s.insert(ctx, "filing friction", `
		INSERT INTO friction (uid, host, filed_by, parent, cwd, build, created_at, updated_at,
			kind, summary, detail, command, status)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
	`, f.UID, f.Host, f.FiledBy, f.Parent, f.Cwd, f.Build, timeKey(f.CreatedAt), timeKey(f.UpdatedAt),
		f.Kind, f.Summary, f.Detail, encodeCommand(f.Command), f.Status)
	if err != nil {
		return err
	}
	f.ID = id
	return nil
}

// GetFriction returns friction entry id, or nil if there is none.
func (s *SQLiteStorage) GetFriction(ctx context.Context, id int64) (*Friction, error) {
	f, err := queryOne(ctx, s.db, scanFriction, `SELECT `+frictionColumns+` FROM friction WHERE id = ?`, id)
	if err != nil {
		return nil, fmt.Errorf("querying friction: %w", err)
	}
	return f, nil
}

// ListFriction returns the friction entries f selects, most votes first,
// then newest.
func (s *SQLiteStorage) ListFriction(ctx context.Context, f FrictionFilter) ([]*Friction, error) {
	var where []string
	var args []any
	if len(f.Statuses) > 0 {
		where = append(where, "status IN (?"+strings.Repeat(", ?", len(f.Statuses)-1)+")")
		for _, st := range f.Statuses {
			args = append(args, st)
		}
	}
	if f.Kind != "" {
		where, args = append(where, "kind = ?"), append(args, f.Kind)
	}
	if f.FiledBy != "" {
		where, args = append(where, "filed_by = ?"), append(args, f.FiledBy)
	}
	if f.DuplicateOf != 0 {
		where, args = append(where, "duplicate_of = ?"), append(args, f.DuplicateOf)
	}
	query := `SELECT ` + frictionColumns + ` AS votes FROM friction`
	if len(where) > 0 {
		query += ` WHERE ` + strings.Join(where, " AND ")
	}
	query += ` ORDER BY votes DESC, id DESC`
	entries, err := queryAll(ctx, s.db, scanFriction, query, args...)
	if err != nil {
		return nil, fmt.Errorf("querying friction: %w", err)
	}
	return entries, nil
}

// LastFrictionID returns the newest friction entry's id, or 0 if there are
// none.
func (s *SQLiteStorage) LastFrictionID(ctx context.Context) (int64, error) {
	var id int64
	if err := s.db.QueryRowContext(ctx, `SELECT COALESCE(MAX(id), 0) FROM friction`).Scan(&id); err != nil {
		return 0, fmt.Errorf("querying friction: %w", err)
	}
	return id, nil
}

// UpdateFriction saves f's status and the accept and resolve fields. Marking
// it a duplicate also moves entries that were duplicates of f onto its
// original, so duplicates stay one level deep.
func (s *SQLiteStorage) UpdateFriction(ctx context.Context, f *Friction) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("beginning transaction: %w", err)
	}
	defer tx.Rollback()

	dup := sql.NullInt64{Int64: f.DuplicateOf, Valid: f.DuplicateOf != 0}
	if _, err := tx.ExecContext(ctx, `
		UPDATE friction SET status = ?, updated_at = ?, accepted_by = ?, accepted_at = ?, accept_note = ?,
			resolved_by = ?, resolved_at = ?, resolution = ?, duplicate_of = ?
		WHERE id = ?
	`, f.Status, timeKey(f.UpdatedAt), f.AcceptedBy, optionalTime(f.AcceptedAt), f.AcceptNote,
		f.ResolvedBy, optionalTime(f.ResolvedAt), f.Resolution, dup, f.ID); err != nil {
		return fmt.Errorf("updating friction: %w", err)
	}
	if dup.Valid {
		if _, err := tx.ExecContext(ctx, `UPDATE friction SET duplicate_of = ? WHERE duplicate_of = ?`, dup, f.ID); err != nil {
			return fmt.Errorf("moving duplicates: %w", err)
		}
	}
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("committing friction: %w", err)
	}
	return nil
}

const frictionVoteColumns = `id, friction_id, uid, host, voter, parent, build, created_at, note, command`

func scanFrictionVote(row scanner) (*FrictionVote, error) {
	var v FrictionVote
	var createdAt, command string
	if err := row.Scan(&v.ID, &v.FrictionID, &v.UID, &v.Host, &v.Voter, &v.Parent, &v.Build, &createdAt, &v.Note, &command); err != nil {
		return nil, err
	}
	v.CreatedAt = parseTime(createdAt)
	v.Command = decodeCommand(command)
	return &v, nil
}

// AddFrictionVote records a +1 and sets v.ID.
func (s *SQLiteStorage) AddFrictionVote(ctx context.Context, v *FrictionVote) error {
	id, err := s.insert(ctx, "voting on friction", `
		INSERT INTO friction_votes (friction_id, uid, host, voter, parent, build, created_at, note, command)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
	`, v.FrictionID, v.UID, v.Host, v.Voter, v.Parent, v.Build, timeKey(v.CreatedAt), v.Note, encodeCommand(v.Command))
	if err != nil {
		return err
	}
	v.ID = id
	return nil
}

// ListFrictionVotes returns the votes on a friction entry, oldest first.
func (s *SQLiteStorage) ListFrictionVotes(ctx context.Context, frictionID int64) ([]*FrictionVote, error) {
	votes, err := queryAll(ctx, s.db, scanFrictionVote,
		`SELECT `+frictionVoteColumns+` FROM friction_votes WHERE friction_id = ? ORDER BY id ASC`, frictionID)
	if err != nil {
		return nil, fmt.Errorf("querying friction votes: %w", err)
	}
	return votes, nil
}

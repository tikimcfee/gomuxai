package storage

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"
)

// commandLogSchema records every agent invocation (agent history). Rows are
// written once, when a command ends, and pruned to the newest
// commandLogKeep.
const commandLogSchema = `
	CREATE TABLE IF NOT EXISTS command_log (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		started_at TEXT NOT NULL,
		duration_ms INTEGER NOT NULL,
		host TEXT NOT NULL,
		agent TEXT NOT NULL,
		parent TEXT NOT NULL,
		cwd TEXT NOT NULL,
		args TEXT NOT NULL,
		exit_code INTEGER NOT NULL,
		error TEXT NOT NULL,
		build TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_command_log_agent
		ON command_log(agent, id);
`

// commandLogKeep is how many command records the log keeps. Every
// commandLogPruneEvery-th insert deletes the older ones, so most inserts
// don't pay for it. Variables for tests.
var (
	commandLogKeep       int64 = 20000
	commandLogPruneEvery int64 = 100
)

// timeKey formats t so that stored times sort as text: UTC, with a fixed
// number of fractional digits.
func timeKey(t time.Time) string {
	return t.UTC().Format("2006-01-02T15:04:05.000Z07:00")
}

const commandColumns = `id, started_at, duration_ms, host, agent, parent, cwd, args, exit_code, error, build`

func scanCommand(row scanner) (*CommandRecord, error) {
	var rec CommandRecord
	var startedAt, args string
	if err := row.Scan(&rec.ID, &startedAt, &rec.DurationMS, &rec.Host, &rec.Agent, &rec.Parent,
		&rec.Cwd, &args, &rec.ExitCode, &rec.Error, &rec.Build); err != nil {
		return nil, err
	}
	rec.StartedAt = parseTime(startedAt)
	if err := json.Unmarshal([]byte(args), &rec.Args); err != nil {
		rec.Args = []string{args}
	}
	return &rec, nil
}

// LogCommand records a command run, with its agent's recorded parent, and
// sets rec.ID and rec.Parent.
func (s *SQLiteStorage) LogCommand(ctx context.Context, rec *CommandRecord) error {
	args, err := json.Marshal(rec.Args)
	if err != nil {
		return fmt.Errorf("encoding command args: %w", err)
	}
	if rec.Args == nil {
		args = []byte("[]")
	}
	// The parent is looked up here, in the insert, rather than at read
	// time: by then the agent may have been cleaned up.
	var parent string
	err = s.db.QueryRowContext(ctx, `
		INSERT INTO command_log (started_at, duration_ms, host, agent, parent, cwd, args, exit_code, error, build)
		VALUES (?, ?, ?, ?, COALESCE((SELECT spawned_by FROM agents WHERE name = ? AND name != ''), ''), ?, ?, ?, ?, ?)
		RETURNING id, parent
	`, timeKey(rec.StartedAt), rec.DurationMS, rec.Host, rec.Agent, rec.Agent, rec.Cwd, string(args),
		rec.ExitCode, rec.Error, rec.Build).Scan(&rec.ID, &parent)
	if err != nil {
		return fmt.Errorf("logging command: %w", err)
	}
	rec.Parent = parent
	if rec.ID%commandLogPruneEvery == 0 {
		return s.exec(ctx, "pruning command log", `DELETE FROM command_log WHERE id <= ?`, rec.ID-commandLogKeep)
	}
	return nil
}

// ListCommands returns the command records f selects, newest first.
func (s *SQLiteStorage) ListCommands(ctx context.Context, f CommandFilter) ([]*CommandRecord, error) {
	var where []string
	var args []any
	if f.ByAgent {
		where, args = append(where, "agent = ?"), append(args, f.Agent)
	}
	if f.Cwd != "" {
		where, args = append(where, "cwd = ?"), append(args, f.Cwd)
	}
	if f.Failed {
		where = append(where, "exit_code != 0")
	}
	if !f.Since.IsZero() {
		where, args = append(where, "started_at >= ?"), append(args, timeKey(f.Since))
	}
	query := `SELECT ` + commandColumns + ` FROM command_log`
	if len(where) > 0 {
		query += ` WHERE ` + strings.Join(where, " AND ")
	}
	query += ` ORDER BY id DESC`
	if f.Limit > 0 {
		query += fmt.Sprintf(` LIMIT %d`, f.Limit)
	}
	recs, err := queryAll(ctx, s.db, scanCommand, query, args...)
	if err != nil {
		return nil, fmt.Errorf("querying command log: %w", err)
	}
	return recs, nil
}

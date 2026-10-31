package storage

import (
	"context"
	"fmt"
	"time"
)

// sessionReportsSchema keeps the last hook report of every session the
// global hook heard from, keyed by the harness's session id (a Claude
// subagent's is agent-<agent id>, as agent sessions names it). Workgroup
// agents' reports also go to their own state; these serve every session,
// including the ones a person started, so agent sessions can say how one
// ended.
const sessionReportsSchema = `
	CREATE TABLE IF NOT EXISTS session_reports (
		session_id TEXT PRIMARY KEY,
		harness TEXT NOT NULL,
		status TEXT NOT NULL,
		end_reason TEXT NOT NULL DEFAULT '',
		event TEXT NOT NULL DEFAULT '',
		detail TEXT NOT NULL DEFAULT '',
		transcript TEXT NOT NULL DEFAULT '',
		at TEXT NOT NULL
	);
`

// SessionReport is a session's last hook report.
type SessionReport struct {
	SessionID  string
	Harness    string
	Status     string
	EndReason  string
	Event      string
	Detail     string
	Transcript string // "" leaves the stored one
	At         time.Time
}

// RecordSessionReport stores r as its session's last report, unless the
// stored one is newer (hooks run as separate processes, so two can finish
// out of order).
func (s *SQLiteStorage) RecordSessionReport(ctx context.Context, r *SessionReport) error {
	return s.exec(ctx, "recording a session report", `
		INSERT INTO session_reports (session_id, harness, status, end_reason, event, detail, transcript, at)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?)
		ON CONFLICT (session_id) DO UPDATE SET
			harness = excluded.harness, status = excluded.status, end_reason = excluded.end_reason,
			event = excluded.event, detail = excluded.detail, at = excluded.at,
			transcript = CASE WHEN excluded.transcript = '' THEN session_reports.transcript ELSE excluded.transcript END
		WHERE excluded.at >= session_reports.at`,
		r.SessionID, r.Harness, r.Status, r.EndReason, r.Event, r.Detail, r.Transcript, timeNanos(r.At))
}

// SessionReports returns every stored session report, by session id. A
// store from before session reports has none.
func (s *SQLiteStorage) SessionReports(ctx context.Context) (map[string]*SessionReport, error) {
	if s.schema != nil {
		var n int
		if err := s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'session_reports'`).Scan(&n); err != nil || n == 0 {
			return nil, err
		}
	}
	rows, err := queryAll(ctx, s.db, func(row scanner) (*SessionReport, error) {
		var r SessionReport
		var at string
		if err := row.Scan(&r.SessionID, &r.Harness, &r.Status, &r.EndReason, &r.Event, &r.Detail, &r.Transcript, &at); err != nil {
			return nil, err
		}
		r.At = parseTime(at)
		return &r, nil
	}, `SELECT session_id, harness, status, end_reason, event, detail, transcript, at FROM session_reports`)
	if err != nil {
		return nil, fmt.Errorf("reading session reports: %w", err)
	}
	out := make(map[string]*SessionReport, len(rows))
	for _, r := range rows {
		out[r.SessionID] = r
	}
	return out, nil
}

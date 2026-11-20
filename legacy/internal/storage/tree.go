package storage

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"sort"
	"strings"
	"time"
)

// treeSchema records the agent tree's history for agent export. Spawn
// opens a run and cleanup closes it, so a departed agent stays readable;
// spawn, cleanup, and every status report append an agent event, which
// followers tail by id.
const treeSchema = `
	CREATE TABLE IF NOT EXISTS runs (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		name TEXT NOT NULL,
		parent TEXT NOT NULL,
		backend TEXT NOT NULL,
		session TEXT NOT NULL,
		cwd TEXT NOT NULL,
		created_at TEXT NOT NULL,
		ended_at TEXT NOT NULL DEFAULT '',
		status TEXT NOT NULL DEFAULT '',
		transcript TEXT NOT NULL DEFAULT '',
		end_reason TEXT NOT NULL DEFAULT ''
	);

	CREATE INDEX IF NOT EXISTS idx_runs_name ON runs(name, id);

	CREATE TABLE IF NOT EXISTS agent_events (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		at TEXT NOT NULL,
		kind TEXT NOT NULL,
		agent TEXT NOT NULL,
		run INTEGER NOT NULL,
		status TEXT NOT NULL DEFAULT '',
		event TEXT NOT NULL DEFAULT '',
		detail TEXT NOT NULL DEFAULT '',
		tool TEXT NOT NULL DEFAULT '',
		tool_input TEXT NOT NULL DEFAULT '',
		session_id TEXT NOT NULL DEFAULT '',
		transcript TEXT NOT NULL DEFAULT '',
		end_reason TEXT NOT NULL DEFAULT ''
	);
`

// agentEventsKeep is how many agent events the table keeps; every
// agentEventsPruneEvery-th insert deletes older ones. Variables for tests.
var (
	agentEventsKeep       int64 = 20000
	agentEventsPruneEvery int64 = 100
)

// Agent event kinds.
const (
	AgentSpawned = "spawned" // spawn recorded the agent
	AgentStatus  = "status"  // a status report was recorded
	AgentRemoved = "removed" // cleanup (or a failed spawn) deleted the record
)

// Run is one spawn of an agent name, from spawn until cleanup and after.
type Run struct {
	ID        int64
	Name      string
	Parent    string // spawned_by; "" for a person
	Backend   string
	Session   string
	Cwd       string
	CreatedAt time.Time
	EndedAt   time.Time // zero while the agent is live
	// Status, EndReason and Transcript are the agent's last reported
	// status, end reason and transcript path, copied when the run ends.
	Status     string
	EndReason  string
	Transcript string
}

// AgentEvent is one change to the agent tree. Status events carry the
// report; spawned and removed events name the run, which holds the rest.
type AgentEvent struct {
	ID    int64
	At    time.Time
	Kind  string // AgentSpawned, AgentStatus, or AgentRemoved
	Agent string
	Run   int64 // the agent's run; 0 for an agent spawned before runs were recorded

	Status    string
	Event     string
	Detail    string
	Tool      string
	ToolInput string // only for a waiting status: inputs can be 64 KiB
	SessionID string // only when the report set it
	// Transcript is the CLI's transcript path, when the report named one.
	Transcript string
	// EndReason says how the session ended, with a status of ended.
	EndReason string
}

// Tree is a store's agents, read in one transaction.
type Tree struct {
	// Cursor is the newest agent event's id (0 if none): the tree includes
	// every event up to it, and none after.
	Cursor int64
	// Events reports whether the store records agent events: one written by
	// an agent built before agent export has no events to follow.
	Events bool
	Agents []*Agent        // live agents, oldest first
	Runs   map[string]*Run // live agents' runs, by name
	State  []*StateEntry   // live agents' reserved status keys
	Ended  []*Run          // departed runs, oldest first, if asked for
}

// treeStateKeys are the state keys a tree reads: the reserved status keys
// (see service.StateKey*).
var treeStateKeys = []string{"_status", "_event", "_detail", "_tool", "_tool_input", "_session_id", "_transcript", "_end_reason"}

// schema says which parts of the current schema a store has. A store
// opened read-only may predate them.
type schema struct {
	spawnedBy, cwd, adopted, runs, events, endReason bool
}

// currentSchema is a migrated store's.
var currentSchema = &schema{spawnedBy: true, cwd: true, adopted: true, runs: true, events: true, endReason: true}

// OpenReadOnly opens another binary's store without writing to it: no
// migrations, and SQLite's read-only mode. Only the tree readers work on it.
func OpenReadOnly(dbPath string) (*SQLiteStorage, error) {
	dsn := "file:" + (&url.URL{Path: dbPath}).EscapedPath() + "?mode=ro&_pragma=busy_timeout(5000)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("opening database: %w", err)
	}
	// One connection: data_version is per connection, so it must be
	// compared on the same one.
	db.SetMaxOpenConns(1)
	s := &SQLiteStorage{db: db}
	if s.schema, err = readSchema(db); err != nil {
		db.Close()
		return nil, fmt.Errorf("reading %s: %w", dbPath, err)
	}
	return s, nil
}

func readSchema(db *sql.DB) (*schema, error) {
	has := func(query string, args ...any) (bool, error) {
		var n int
		err := db.QueryRow(query, args...).Scan(&n)
		return n > 0, err
	}
	column := `SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?`
	table := `SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?`
	var sc schema
	var err error
	if ok, err := has(table, "agents"); err != nil || !ok {
		if err == nil {
			err = errors.New("no agents table: not an agent store")
		}
		return nil, err
	}
	for _, c := range []struct {
		ok    *bool
		query string
		args  []any
	}{
		{&sc.spawnedBy, column, []any{"agents", "spawned_by"}},
		{&sc.cwd, column, []any{"agents", "cwd"}},
		{&sc.adopted, column, []any{"agents", "adopted"}},
		{&sc.runs, table, []any{"runs"}},
		{&sc.events, table, []any{"agent_events"}},
		{&sc.endReason, column, []any{"agent_events", "end_reason"}},
	} {
		if *c.ok, err = has(c.query, c.args...); err != nil {
			return nil, err
		}
	}
	return &sc, nil
}

func (s *SQLiteStorage) schemaOf() *schema {
	if s.schema == nil {
		return currentSchema
	}
	return s.schema
}

func boolToInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

// createAgentTx records a new agent, opens its run, and appends its spawned
// event.
func createAgentTx(ctx context.Context, tx *sql.Tx, agent *Agent) error {
	created := agent.CreatedAt.Format(time.RFC3339)
	if _, err := tx.ExecContext(ctx, `INSERT INTO agents (`+agentColumns+`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		agent.Name, agent.SessionName, agent.Backend, agent.TaskFile, agent.LogFile,
		created, agent.SpawnedBy, agent.Cwd, boolToInt(agent.Adopted)); err != nil {
		return fmt.Errorf("inserting agent: %w", err)
	}
	return openRunTx(ctx, tx, agent)
}

// replaceAgentTx closes the run of the agent named agent.Name, rewrites its
// record from agent, and opens a new run: a resume. The record is updated,
// not deleted, since deleting it would cascade to the agent's state.
func replaceAgentTx(ctx context.Context, tx *sql.Tx, agent *Agent) error {
	found, err := closeRunTx(ctx, tx, agent.Name)
	if err != nil {
		return err
	}
	if !found {
		return fmt.Errorf("no agent %s to replace", agent.Name)
	}
	if _, err := tx.ExecContext(ctx, `UPDATE agents SET session_name = ?, backend = ?, task_file = ?, log_file = ?, created_at = ?, spawned_by = ?, cwd = ?, adopted = ? WHERE name = ?`,
		agent.SessionName, agent.Backend, agent.TaskFile, agent.LogFile, agent.CreatedAt.Format(time.RFC3339),
		agent.SpawnedBy, agent.Cwd, boolToInt(agent.Adopted), agent.Name); err != nil {
		return fmt.Errorf("updating agent: %w", err)
	}
	return openRunTx(ctx, tx, agent)
}

// openRunTx opens a run for agent and appends its spawned event.
func openRunTx(ctx context.Context, tx *sql.Tx, agent *Agent) error {
	res, err := tx.ExecContext(ctx, `INSERT INTO runs (name, parent, backend, session, cwd, created_at) VALUES (?, ?, ?, ?, ?, ?)`,
		agent.Name, agent.SpawnedBy, agent.Backend, agent.SessionName, agent.Cwd, timeNanos(agent.CreatedAt))
	if err != nil {
		return fmt.Errorf("opening run: %w", err)
	}
	run, _ := res.LastInsertId()
	return insertAgentEvent(ctx, tx, &AgentEvent{At: agent.CreatedAt, Kind: AgentSpawned, Agent: agent.Name, Run: run})
}

// deleteAgentTx deletes an agent's record, closes its run (closeRunTx),
// and appends its removed event.
func deleteAgentTx(ctx context.Context, tx *sql.Tx, name string) error {
	if found, err := closeRunTx(ctx, tx, name); err != nil || !found {
		return err // !found: no such agent
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM agents WHERE name = ?`, name); err != nil {
		return fmt.Errorf("deleting agent: %w", err)
	}
	return nil
}

// closeRunTx closes an agent's run with its last status and transcript,
// and appends its removed event; found is false if there is no such agent.
// An agent spawned before runs were recorded gets a closed run from its
// record.
//
// Every statement that reads also writes: a transaction that starts with a
// plain read can't take the write lock later while another process writes
// (SQLITE_BUSY, which busy_timeout doesn't wait out).
func closeRunTx(ctx context.Context, tx *sql.Tx, name string) (found bool, err error) {
	const last = `COALESCE((SELECT value FROM state WHERE agent_name = ? AND key = ?), '')`
	now := timeNanos(time.Now())
	var run int64
	var status, transcript, endReason string
	err = tx.QueryRowContext(ctx, `UPDATE runs SET ended_at = ?, status = `+last+`, transcript = `+last+`, end_reason = `+last+`
		WHERE id = (SELECT MAX(id) FROM runs WHERE name = ? AND ended_at = '') AND EXISTS (SELECT 1 FROM agents WHERE name = ?)
		RETURNING id, status, transcript, end_reason`,
		now, name, "_status", name, "_transcript", name, "_end_reason", name, name).Scan(&run, &status, &transcript, &endReason)
	if errors.Is(err, sql.ErrNoRows) {
		err = tx.QueryRowContext(ctx, `INSERT INTO runs (name, parent, backend, session, cwd, created_at, ended_at, status, transcript, end_reason)
			SELECT name, COALESCE(spawned_by, ''), backend, session_name, COALESCE(cwd, ''), created_at, ?, `+last+`, `+last+`, `+last+`
			FROM agents WHERE name = ? RETURNING id, status, transcript, end_reason`,
			now, name, "_status", name, "_transcript", name, "_end_reason", name, name).Scan(&run, &status, &transcript, &endReason)
	}
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("closing run: %w", err)
	}
	return true, insertAgentEvent(ctx, tx, &AgentEvent{At: parseTime(now), Kind: AgentRemoved, Agent: name, Run: run, Status: status, Transcript: transcript, EndReason: endReason})
}

// insertAgentEvent appends ev. A run of 0 means the agent's open run.
func insertAgentEvent(ctx context.Context, tx *sql.Tx, ev *AgentEvent) error {
	run := any(ev.Run)
	if ev.Run == 0 {
		run = sql.Null[int64]{}
	}
	var id int64
	err := tx.QueryRowContext(ctx, `
		INSERT INTO agent_events (at, kind, agent, run, status, event, detail, tool, tool_input, session_id, transcript, end_reason)
		VALUES (?, ?, ?, COALESCE(?, (SELECT MAX(id) FROM runs WHERE name = ? AND ended_at = ''), 0), ?, ?, ?, ?, ?, ?, ?, ?)
		RETURNING id`,
		timeNanos(ev.At), ev.Kind, ev.Agent, run, ev.Agent, ev.Status, ev.Event, ev.Detail, ev.Tool, ev.ToolInput,
		ev.SessionID, ev.Transcript, ev.EndReason).Scan(&id)
	if err != nil {
		return fmt.Errorf("recording agent event: %w", err)
	}
	ev.ID = id
	if id%agentEventsPruneEvery == 0 {
		if _, err := tx.ExecContext(ctx, `DELETE FROM agent_events WHERE id <= ?`, id-agentEventsKeep); err != nil {
			return fmt.Errorf("pruning agent events: %w", err)
		}
	}
	return nil
}

// timeNanos formats t for the tree's tables: UTC with nanoseconds, since
// a whole agent turn can fit in one second.
func timeNanos(t time.Time) string {
	return t.UTC().Format(time.RFC3339Nano)
}

// runColumns and eventColumns are the columns scanRun and scanAgentEvent
// read; a store from before end reasons were kept reads them as ”.
func (s *SQLiteStorage) runColumns() string {
	return `id, name, parent, backend, session, cwd, created_at, ended_at, status, transcript, ` + s.endReasonColumn()
}

func (s *SQLiteStorage) eventColumns() string {
	return `id, at, kind, agent, run, status, event, detail, tool, tool_input, session_id, transcript, ` + s.endReasonColumn()
}

func (s *SQLiteStorage) endReasonColumn() string {
	if s.schemaOf().endReason {
		return "end_reason"
	}
	return "'' AS end_reason"
}

func scanRun(row scanner) (*Run, error) {
	var r Run
	var created, ended string
	if err := row.Scan(&r.ID, &r.Name, &r.Parent, &r.Backend, &r.Session, &r.Cwd, &created, &ended, &r.Status, &r.Transcript, &r.EndReason); err != nil {
		return nil, err
	}
	r.CreatedAt, r.EndedAt = parseTime(created), parseTime(ended)
	return &r, nil
}

func scanAgentEvent(row scanner) (*AgentEvent, error) {
	var ev AgentEvent
	var at string
	if err := row.Scan(&ev.ID, &at, &ev.Kind, &ev.Agent, &ev.Run, &ev.Status, &ev.Event, &ev.Detail,
		&ev.Tool, &ev.ToolInput, &ev.SessionID, &ev.Transcript, &ev.EndReason); err != nil {
		return nil, err
	}
	ev.At = parseTime(at)
	return &ev, nil
}

// GetRun returns a run by id, or nil if there is none.
func (s *SQLiteStorage) GetRun(ctx context.Context, id int64) (*Run, error) {
	if !s.schemaOf().runs {
		return nil, nil
	}
	run, err := queryOne(ctx, s.db, scanRun, `SELECT `+s.runColumns()+` FROM runs WHERE id = ?`, id)
	if err != nil {
		return nil, fmt.Errorf("querying run: %w", err)
	}
	return run, nil
}

// AgentEventsAfter returns up to limit agent events with ids above after,
// oldest first, and the oldest id the store still keeps (0 if none), so a
// caller can tell whether events after `after` were pruned.
func (s *SQLiteStorage) AgentEventsAfter(ctx context.Context, after int64, limit int) ([]*AgentEvent, int64, error) {
	if !s.schemaOf().events {
		return nil, 0, nil
	}
	var oldest int64
	if err := s.db.QueryRowContext(ctx, `SELECT COALESCE(MIN(id), 0) FROM agent_events`).Scan(&oldest); err != nil {
		return nil, 0, fmt.Errorf("querying agent events: %w", err)
	}
	events, err := queryAll(ctx, s.db, scanAgentEvent,
		`SELECT `+s.eventColumns()+` FROM agent_events WHERE id > ? ORDER BY id LIMIT ?`, after, limit)
	if err != nil {
		return nil, 0, fmt.Errorf("querying agent events: %w", err)
	}
	return events, oldest, nil
}

// DataVersion is SQLite's data_version: it changes when another connection
// commits, so a caller can skip re-reading a quiet store.
func (s *SQLiteStorage) DataVersion(ctx context.Context) (int64, error) {
	conn, err := s.db.Conn(ctx)
	if err != nil {
		return 0, err
	}
	defer conn.Close()
	var v int64
	err = conn.QueryRowContext(ctx, `PRAGMA data_version`).Scan(&v)
	return v, err
}

// ReadTree reads the live agents with their runs and status keys, and with
// ended, the departed runs, all in one read transaction so they agree with
// each other and with Cursor. Departed agents of a store that predates runs
// come from its command log's spawns, newest per name, with no ID.
func (s *SQLiteStorage) ReadTree(ctx context.Context, ended bool) (*Tree, error) {
	sc := s.schemaOf()
	tx, err := s.db.BeginTx(ctx, &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return nil, fmt.Errorf("reading agent tree: %w", err)
	}
	defer tx.Rollback()

	t := &Tree{Events: sc.events, Runs: map[string]*Run{}}
	if sc.events {
		if err := tx.QueryRowContext(ctx, `SELECT COALESCE(MAX(id), 0) FROM agent_events`).Scan(&t.Cursor); err != nil {
			return nil, fmt.Errorf("reading agent events: %w", err)
		}
	}

	cols := `name, session_name, backend, task_file, log_file, created_at, ` +
		map[bool]string{true: `spawned_by`, false: `NULL`}[sc.spawnedBy] + `, ` +
		map[bool]string{true: `cwd`, false: `NULL`}[sc.cwd] + `, ` +
		map[bool]string{true: `adopted`, false: `0`}[sc.adopted]
	if t.Agents, err = queryAllTx(ctx, tx, scanAgent, `SELECT `+cols+` FROM agents ORDER BY created_at, name`); err != nil {
		return nil, fmt.Errorf("reading agents: %w", err)
	}
	live := make(map[string]bool, len(t.Agents))
	for _, a := range t.Agents {
		live[a.Name] = true
	}

	keys := "'" + strings.Join(treeStateKeys, "','") + "'"
	if t.State, err = queryAllTx(ctx, tx, scanState, `SELECT `+stateColumns+` FROM state WHERE key IN (`+keys+`) ORDER BY agent_name, key`); err != nil {
		return nil, fmt.Errorf("reading agent state: %w", err)
	}

	hasRun := map[string]bool{}
	if sc.runs {
		runs, err := queryAllTx(ctx, tx, scanRun, `SELECT `+s.runColumns()+` FROM runs ORDER BY id`)
		if err != nil {
			return nil, fmt.Errorf("reading runs: %w", err)
		}
		for _, r := range runs {
			hasRun[r.Name] = true
			switch {
			case r.EndedAt.IsZero() && live[r.Name]:
				t.Runs[r.Name] = r
			case !r.EndedAt.IsZero() && ended:
				t.Ended = append(t.Ended, r)
			}
		}
	}

	// Stores and agents from before runs: the command log's spawns give
	// cwd for live agents and the whole record for departed ones.
	needSpawns := ended
	for _, a := range t.Agents {
		if a.Cwd == "" && t.Runs[a.Name] == nil {
			needSpawns = true
		}
	}
	if !needSpawns {
		return t, nil
	}
	spawns, err := loggedSpawns(ctx, tx)
	if err != nil {
		return nil, err
	}
	for _, a := range t.Agents {
		if sp := spawns[a.Name]; sp != nil && a.Cwd == "" && t.Runs[a.Name] == nil {
			a.Cwd = sp.Cwd
		}
	}
	if ended {
		var legacy []*Run
		for name, sp := range spawns {
			if !live[name] && !hasRun[name] {
				legacy = append(legacy, sp)
			}
		}
		sortRuns(legacy)
		t.Ended = append(legacy, t.Ended...)
	}
	return t, nil
}

// loggedSpawns returns each name's newest successful spawn in the command
// log, as a run with no ID or end time.
func loggedSpawns(ctx context.Context, tx *sql.Tx) (map[string]*Run, error) {
	rows, err := tx.QueryContext(ctx, `SELECT started_at, agent, cwd, args FROM command_log
		WHERE args LIKE '["spawn",%' AND exit_code = 0 ORDER BY id`)
	if err != nil {
		return nil, fmt.Errorf("reading logged spawns: %w", err)
	}
	defer rows.Close()
	out := map[string]*Run{}
	for rows.Next() {
		var at, parent, cwd, args string
		if err := rows.Scan(&at, &parent, &cwd, &args); err != nil {
			return nil, fmt.Errorf("reading logged spawns: %w", err)
		}
		name, backend := spawnArgs(args)
		if name == "" {
			continue
		}
		out[name] = &Run{Name: name, Parent: parent, Backend: backend, Session: "agent-" + name, Cwd: cwd, CreatedAt: parseTime(at)}
	}
	return out, rows.Err()
}

// spawnArgs reads the agent name and --backend from a logged spawn's
// arguments. The name is spawn's first argument that isn't a flag; only
// --backend takes a value before it in practice.
func spawnArgs(raw string) (name, backend string) {
	var argv []string
	if json.Unmarshal([]byte(raw), &argv) != nil || len(argv) < 2 || argv[0] != "spawn" {
		return "", ""
	}
	for i := 1; i < len(argv); i++ {
		switch a := argv[i]; {
		case (a == "--backend" || a == "--backend-config" || a == "--task") && i+1 < len(argv):
			if a == "--backend" {
				backend = argv[i+1]
			}
			i++
		case strings.HasPrefix(a, "--backend="):
			backend = strings.TrimPrefix(a, "--backend=")
		case strings.HasPrefix(a, "-"):
		case name == "":
			name = a
		}
	}
	return name, backend
}

func sortRuns(runs []*Run) {
	sort.SliceStable(runs, func(i, j int) bool { return runs[i].CreatedAt.Before(runs[j].CreatedAt) })
}

// queryAllTx is queryAll inside a transaction.
func queryAllTx[T any](ctx context.Context, tx *sql.Tx, scan func(scanner) (*T, error), query string, args ...any) ([]*T, error) {
	rows, err := tx.QueryContext(ctx, query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*T
	for rows.Next() {
		v, err := scan(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, rows.Err()
}

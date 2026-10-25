package storage

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"sort"
	"time"

	_ "modernc.org/sqlite"
)

// SQLiteStorage implements Storage using SQLite.
type SQLiteStorage struct {
	db *sql.DB
	// schema is what a store opened read-only has; nil for a migrated
	// store, which has everything.
	schema *schema
}

// NewSQLiteStorage creates a new SQLite storage instance.
func NewSQLiteStorage(dbPath string) (*SQLiteStorage, error) {
	// Per-connection pragmas go in the DSN so every pooled connection gets
	// them; a one-off db.Exec only reaches whichever connection runs it.
	// busy_timeout: backend hooks write status from several short-lived
	// processes at once. foreign_keys: cleanup relies on the agents → state
	// cascade.
	db, err := sql.Open("sqlite", dbPath+"?_pragma=busy_timeout(5000)&_pragma=foreign_keys(1)&_pragma=synchronous(NORMAL)")
	if err != nil {
		return nil, fmt.Errorf("opening database: %w", err)
	}

	// journal_mode is stored in the database file, so once is enough.
	if _, err := db.Exec("PRAGMA journal_mode = WAL"); err != nil {
		db.Close()
		return nil, fmt.Errorf("executing PRAGMA journal_mode = WAL: %w", err)
	}

	s := &SQLiteStorage{db: db}
	if err := s.migrate(); err != nil {
		db.Close()
		return nil, fmt.Errorf("running migrations: %w", err)
	}

	return s, nil
}

// migrate creates the database schema.
func (s *SQLiteStorage) migrate() error {
	schema := `
	CREATE TABLE IF NOT EXISTS agents (
		name TEXT PRIMARY KEY,
		session_name TEXT NOT NULL,
		backend TEXT NOT NULL,
		task_file TEXT,
		log_file TEXT,
		created_at TEXT NOT NULL,
		spawned_by TEXT,
		cwd TEXT
	);

	CREATE TABLE IF NOT EXISTS messages (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		sender_id TEXT NOT NULL,
		receiver_id TEXT NOT NULL,
		content TEXT NOT NULL,
		status TEXT NOT NULL DEFAULT 'pending',
		created_at TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_messages_receiver_status
		ON messages(receiver_id, status);

	-- Tasks as standalone repository (not tied to agents)
	CREATE TABLE IF NOT EXISTS tasks (
		name TEXT PRIMARY KEY,
		content TEXT NOT NULL,
		source_file TEXT,
		created_at TEXT NOT NULL
	);

	-- State as key-value store
	CREATE TABLE IF NOT EXISTS state (
		agent_name TEXT NOT NULL,
		key TEXT NOT NULL,
		value TEXT NOT NULL,
		updated_at TEXT NOT NULL,
		PRIMARY KEY (agent_name, key),
		FOREIGN KEY (agent_name) REFERENCES agents(name) ON DELETE CASCADE
	);
	` + commandLogSchema + frictionSchema + treeSchema

	if _, err := s.db.Exec(schema); err != nil {
		return err
	}
	if err := s.addColumn("agents", "spawned_by", "TEXT"); err != nil {
		return err
	}
	return s.addColumn("agents", "cwd", "TEXT")
}

// addColumn adds a column that databases created before it lack.
func (s *SQLiteStorage) addColumn(table, column, def string) error {
	has := func() (bool, error) {
		var n int
		err := s.db.QueryRow(`SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?`, table, column).Scan(&n)
		return n > 0, err
	}
	if ok, err := has(); err != nil || ok {
		return err
	}
	_, err := s.db.Exec(fmt.Sprintf(`ALTER TABLE %s ADD COLUMN %s %s`, table, column, def))
	if err != nil {
		// Another process may have added it first.
		if ok, _ := has(); ok {
			return nil
		}
	}
	return err
}

// scanner is a *sql.Row or *sql.Rows.
type scanner interface {
	Scan(dest ...any) error
}

// queryOne runs a single-row query, returning nil (and no error) when no row
// matches.
func queryOne[T any](ctx context.Context, db *sql.DB, scan func(scanner) (*T, error), query string, args ...any) (*T, error) {
	v, err := scan(db.QueryRowContext(ctx, query, args...))
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	return v, err
}

// queryAll runs a query and scans every row.
func queryAll[T any](ctx context.Context, db *sql.DB, scan func(scanner) (*T, error), query string, args ...any) ([]*T, error) {
	rows, err := db.QueryContext(ctx, query, args...)
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

// exec runs a statement, wrapping any error with what it was doing.
func (s *SQLiteStorage) exec(ctx context.Context, what, query string, args ...any) error {
	if _, err := s.db.ExecContext(ctx, query, args...); err != nil {
		return fmt.Errorf("%s: %w", what, err)
	}
	return nil
}

// insert runs an INSERT and returns the new row's id.
func (s *SQLiteStorage) insert(ctx context.Context, what, query string, args ...any) (int64, error) {
	result, err := s.db.ExecContext(ctx, query, args...)
	if err != nil {
		return 0, fmt.Errorf("%s: %w", what, err)
	}
	id, _ := result.LastInsertId()
	return id, nil
}

// parseTime reads a stored timestamp. RFC3339 parsing also accepts the
// fractional seconds that state timestamps carry.
func parseTime(s string) time.Time {
	t, _ := time.Parse(time.RFC3339, s)
	return t
}

const agentColumns = `name, session_name, backend, task_file, log_file, created_at, spawned_by, cwd`

func scanAgent(row scanner) (*Agent, error) {
	var agent Agent
	var createdAt string
	var taskFile, logFile, spawnedBy, cwd sql.NullString
	if err := row.Scan(&agent.Name, &agent.SessionName, &agent.Backend, &taskFile, &logFile, &createdAt, &spawnedBy, &cwd); err != nil {
		return nil, err
	}
	agent.TaskFile = taskFile.String
	agent.LogFile = logFile.String
	agent.SpawnedBy = spawnedBy.String
	agent.Cwd = cwd.String
	agent.CreatedAt = parseTime(createdAt)
	return &agent, nil
}

// CreateAgent creates a new agent record, opens its run, and records its
// spawned event, in one transaction.
func (s *SQLiteStorage) CreateAgent(ctx context.Context, agent *Agent) error {
	return s.inTx(ctx, func(tx *sql.Tx) error { return createAgentTx(ctx, tx, agent) })
}

// inTx runs fn in a transaction, committing if it succeeds.
func (s *SQLiteStorage) inTx(ctx context.Context, fn func(*sql.Tx) error) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("beginning transaction: %w", err)
	}
	defer tx.Rollback()
	if err := fn(tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("committing: %w", err)
	}
	return nil
}

// GetAgent retrieves an agent by name.
func (s *SQLiteStorage) GetAgent(ctx context.Context, name string) (*Agent, error) {
	agent, err := queryOne(ctx, s.db, scanAgent,
		`SELECT `+agentColumns+` FROM agents WHERE name = ?`, name)
	if err != nil {
		return nil, fmt.Errorf("querying agent: %w", err)
	}
	return agent, nil
}

// ListAgents returns all agents.
func (s *SQLiteStorage) ListAgents(ctx context.Context) ([]*Agent, error) {
	agents, err := queryAll(ctx, s.db, scanAgent,
		`SELECT `+agentColumns+` FROM agents ORDER BY created_at DESC`)
	if err != nil {
		return nil, fmt.Errorf("querying agents: %w", err)
	}
	return agents, nil
}

// DeleteAgent removes an agent record, closes its run, and records its
// removed event, in one transaction.
func (s *SQLiteStorage) DeleteAgent(ctx context.Context, name string) error {
	return s.inTx(ctx, func(tx *sql.Tx) error { return deleteAgentTx(ctx, tx, name) })
}

const messageColumns = `id, sender_id, receiver_id, content, status, created_at`

func scanMessage(row scanner) (*Message, error) {
	var msg Message
	var createdAt string
	if err := row.Scan(&msg.ID, &msg.SenderID, &msg.ReceiverID, &msg.Content, &msg.Status, &createdAt); err != nil {
		return nil, err
	}
	msg.CreatedAt = parseTime(createdAt)
	return &msg, nil
}

// CreateMessage creates a new message record.
func (s *SQLiteStorage) CreateMessage(ctx context.Context, msg *Message) error {
	id, err := s.insert(ctx, "inserting message",
		`INSERT INTO messages (sender_id, receiver_id, content, status, created_at) VALUES (?, ?, ?, ?, ?)`,
		msg.SenderID, msg.ReceiverID, msg.Content, msg.Status, msg.CreatedAt.Format(time.RFC3339))
	if err != nil {
		return err
	}
	msg.ID = id
	return nil
}

// GetPendingMessages retrieves pending messages for a receiver, oldest first.
func (s *SQLiteStorage) GetPendingMessages(ctx context.Context, receiverID string, limit int) ([]*Message, error) {
	// created_at has whole seconds; id keeps messages queued within one
	// second in the order they were queued.
	messages, err := queryAll(ctx, s.db, scanMessage, `
		SELECT `+messageColumns+`
		FROM messages
		WHERE receiver_id = ? AND status = 'pending'
		ORDER BY created_at ASC, id ASC
		LIMIT ?
	`, receiverID, limit)
	if err != nil {
		return nil, fmt.Errorf("querying messages: %w", err)
	}
	return messages, nil
}

// UpdateMessageStatus updates the status of a message.
func (s *SQLiteStorage) UpdateMessageStatus(ctx context.Context, id int64, status MessageStatus) error {
	return s.exec(ctx, "updating message status", `UPDATE messages SET status = ? WHERE id = ?`, status, id)
}

// ClaimMessage marks a pending message delivered, reporting whether this
// call did: of two callers racing to deliver it, only one gets true.
func (s *SQLiteStorage) ClaimMessage(ctx context.Context, id int64) (bool, error) {
	result, err := s.db.ExecContext(ctx,
		`UPDATE messages SET status = 'delivered' WHERE id = ? AND status = 'pending'`, id)
	if err != nil {
		return false, fmt.Errorf("claiming message: %w", err)
	}
	n, err := result.RowsAffected()
	return n == 1, err
}

// ClaimPendingMessages marks every pending message for receiverID
// delivered and returns them, oldest first. If update is non-nil, the state
// change it returns for them is applied in the same transaction. The claim
// is the transaction's first statement, so it takes the write lock at once:
// a message stored concurrently is either claimed here or committed after
// the state change, where whoever stored it can see that change.
func (s *SQLiteStorage) ClaimPendingMessages(ctx context.Context, receiverID string, update func(claimed []*Message) StateUpdate) ([]*Message, error) {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return nil, fmt.Errorf("beginning transaction: %w", err)
	}
	defer tx.Rollback()

	rows, err := tx.QueryContext(ctx, `
		UPDATE messages SET status = 'delivered'
		WHERE receiver_id = ? AND status = 'pending'
		RETURNING `+messageColumns, receiverID)
	if err != nil {
		return nil, fmt.Errorf("claiming messages: %w", err)
	}
	var claimed []*Message
	for rows.Next() {
		msg, err := scanMessage(rows)
		if err != nil {
			rows.Close()
			return nil, fmt.Errorf("claiming messages: %w", err)
		}
		claimed = append(claimed, msg)
	}
	if err := rows.Close(); err != nil {
		return nil, fmt.Errorf("claiming messages: %w", err)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("claiming messages: %w", err)
	}
	// RETURNING has no order; GetPendingMessages' order is the one queued.
	sort.Slice(claimed, func(i, j int) bool {
		a, b := claimed[i], claimed[j]
		if !a.CreatedAt.Equal(b.CreatedAt) {
			return a.CreatedAt.Before(b.CreatedAt)
		}
		return a.ID < b.ID
	})

	if update != nil {
		u := update(claimed)
		if err := applyStateUpdate(ctx, tx, u.Entries, u.AgentName, u.Delete); err != nil {
			return nil, err
		}
		if u.Event != nil {
			if err := insertAgentEvent(ctx, tx, u.Event); err != nil {
				return nil, err
			}
		}
	}
	if err := tx.Commit(); err != nil {
		return nil, fmt.Errorf("committing claim: %w", err)
	}
	return claimed, nil
}

// DeleteMessagesByAgent removes all messages involving an agent.
func (s *SQLiteStorage) DeleteMessagesByAgent(ctx context.Context, agentName string) error {
	return s.exec(ctx, "deleting messages",
		`DELETE FROM messages WHERE sender_id = ? OR receiver_id = ?`, agentName, agentName)
}

// Close closes the database connection.
func (s *SQLiteStorage) Close() error {
	return s.db.Close()
}

const taskColumns = `name, content, source_file, created_at`

func scanTask(row scanner) (*Task, error) {
	var task Task
	var createdAt string
	var sourceFile sql.NullString
	if err := row.Scan(&task.Name, &task.Content, &sourceFile, &createdAt); err != nil {
		return nil, err
	}
	task.SourceFile = sourceFile.String
	task.CreatedAt = parseTime(createdAt)
	return &task, nil
}

// SetTask creates or updates a task in the repository.
func (s *SQLiteStorage) SetTask(ctx context.Context, task *Task) error {
	return s.exec(ctx, "setting task", `
		INSERT INTO tasks (`+taskColumns+`)
		VALUES (?, ?, ?, ?)
		ON CONFLICT(name) DO UPDATE SET
			content = excluded.content,
			source_file = excluded.source_file,
			created_at = excluded.created_at
	`, task.Name, task.Content, task.SourceFile, task.CreatedAt.Format(time.RFC3339))
}

// GetTask retrieves a task by name.
func (s *SQLiteStorage) GetTask(ctx context.Context, name string) (*Task, error) {
	task, err := queryOne(ctx, s.db, scanTask,
		`SELECT `+taskColumns+` FROM tasks WHERE name = ?`, name)
	if err != nil {
		return nil, fmt.Errorf("querying task: %w", err)
	}
	return task, nil
}

// ListTasks retrieves all tasks.
func (s *SQLiteStorage) ListTasks(ctx context.Context) ([]*Task, error) {
	tasks, err := queryAll(ctx, s.db, scanTask,
		`SELECT `+taskColumns+` FROM tasks ORDER BY name ASC`)
	if err != nil {
		return nil, fmt.Errorf("querying tasks: %w", err)
	}
	return tasks, nil
}

// DeleteTask removes a task by name.
func (s *SQLiteStorage) DeleteTask(ctx context.Context, name string) error {
	return s.exec(ctx, "deleting task", `DELETE FROM tasks WHERE name = ?`, name)
}

const stateColumns = `agent_name, key, value, updated_at`

func scanState(row scanner) (*StateEntry, error) {
	var entry StateEntry
	var updatedAt string
	if err := row.Scan(&entry.AgentName, &entry.Key, &entry.Value, &updatedAt); err != nil {
		return nil, err
	}
	entry.UpdatedAt = parseTime(updatedAt)
	return &entry, nil
}

// SetState creates or updates a state entry for an agent.
func (s *SQLiteStorage) SetState(ctx context.Context, entry *StateEntry) error {
	return s.exec(ctx, "setting state", setStateQuery, stateArgs(entry)...)
}

// SetStates creates or updates several state entries atomically, so readers
// never see a mix of two concurrent writes.
func (s *SQLiteStorage) SetStates(ctx context.Context, entries []*StateEntry) error {
	return s.UpdateStates(ctx, entries, "", nil)
}

// UpdateStates sets entries and deletes agentName's keys in del, in one
// transaction.
func (s *SQLiteStorage) UpdateStates(ctx context.Context, entries []*StateEntry, agentName string, del []string) error {
	return s.RecordReport(ctx, entries, agentName, del, nil)
}

// RecordReport is UpdateStates that also appends ev (if not nil) to the
// agent events, in the same transaction.
func (s *SQLiteStorage) RecordReport(ctx context.Context, entries []*StateEntry, agentName string, del []string, ev *AgentEvent) error {
	return s.inTx(ctx, func(tx *sql.Tx) error {
		if err := applyStateUpdate(ctx, tx, entries, agentName, del); err != nil {
			return err
		}
		if ev == nil {
			return nil
		}
		return insertAgentEvent(ctx, tx, ev)
	})
}

// applyStateUpdate sets entries and deletes agentName's keys in del, in tx.
func applyStateUpdate(ctx context.Context, tx *sql.Tx, entries []*StateEntry, agentName string, del []string) error {
	for _, entry := range entries {
		if _, err := tx.ExecContext(ctx, setStateQuery, stateArgs(entry)...); err != nil {
			return fmt.Errorf("setting state %s: %w", entry.Key, err)
		}
	}
	for _, key := range del {
		if _, err := tx.ExecContext(ctx, `DELETE FROM state WHERE agent_name = ? AND key = ?`, agentName, key); err != nil {
			return fmt.Errorf("deleting state %s: %w", key, err)
		}
	}
	return nil
}

const setStateQuery = `
	INSERT INTO state (` + stateColumns + `)
	VALUES (?, ?, ?, ?)
	ON CONFLICT(agent_name, key) DO UPDATE SET
		value = excluded.value,
		updated_at = excluded.updated_at
`

func stateArgs(entry *StateEntry) []any {
	// Nanoseconds: `agent wait --next` tells reports apart by timestamp, and
	// a whole agent turn can fit inside one second.
	return []any{entry.AgentName, entry.Key, entry.Value, entry.UpdatedAt.Format(time.RFC3339Nano)}
}

// GetState retrieves a single state entry.
func (s *SQLiteStorage) GetState(ctx context.Context, agentName, key string) (*StateEntry, error) {
	entry, err := queryOne(ctx, s.db, scanState,
		`SELECT `+stateColumns+` FROM state WHERE agent_name = ? AND key = ?`, agentName, key)
	if err != nil {
		return nil, fmt.Errorf("querying state: %w", err)
	}
	return entry, nil
}

// ListState retrieves all state entries for an agent.
func (s *SQLiteStorage) ListState(ctx context.Context, agentName string) ([]*StateEntry, error) {
	entries, err := queryAll(ctx, s.db, scanState,
		`SELECT `+stateColumns+` FROM state WHERE agent_name = ? ORDER BY key ASC`, agentName)
	if err != nil {
		return nil, fmt.Errorf("querying state: %w", err)
	}
	return entries, nil
}

// DeleteState removes all state entries for an agent.
func (s *SQLiteStorage) DeleteState(ctx context.Context, agentName string) error {
	return s.exec(ctx, "deleting state", `DELETE FROM state WHERE agent_name = ?`, agentName)
}

// DeleteStateKey removes one state entry for an agent.
func (s *SQLiteStorage) DeleteStateKey(ctx context.Context, agentName, key string) error {
	return s.exec(ctx, "deleting state", `DELETE FROM state WHERE agent_name = ? AND key = ?`, agentName, key)
}

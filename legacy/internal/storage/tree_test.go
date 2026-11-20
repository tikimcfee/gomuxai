package storage

import (
	"context"
	"database/sql"
	"path/filepath"
	"slices"
	"testing"
	"time"
)

func newTestStore(t *testing.T) (*SQLiteStorage, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "gomuxai.db")
	s, err := NewSQLiteStorage(path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	return s, path
}

func eventKinds(t *testing.T, s *SQLiteStorage, after int64) []string {
	t.Helper()
	events, _, err := s.AgentEventsAfter(context.Background(), after, 100)
	if err != nil {
		t.Fatal(err)
	}
	var out []string
	for _, ev := range events {
		out = append(out, ev.Kind+":"+ev.Agent+":"+ev.Status)
	}
	return out
}

func TestTree_SpawnReportCleanup(t *testing.T) {
	s, _ := newTestStore(t)
	ctx := context.Background()
	created := time.Date(2026, 10, 24, 14, 0, 0, 0, time.UTC)
	if err := s.CreateAgent(ctx, &Agent{Name: "lead", SessionName: "agent-lead", Backend: "claude", Cwd: "/w", CreatedAt: created}); err != nil {
		t.Fatal(err)
	}
	if err := s.CreateAgent(ctx, &Agent{Name: "lead-t1", SessionName: "agent-lead-t1", Backend: "agy", SpawnedBy: "lead", Cwd: "/w/sub", CreatedAt: created.Add(time.Second)}); err != nil {
		t.Fatal(err)
	}
	report := func(name, status, transcript string) {
		t.Helper()
		at := time.Now()
		entries := []*StateEntry{{AgentName: name, Key: "_status", Value: status, UpdatedAt: at}}
		if transcript != "" {
			entries = append(entries, &StateEntry{AgentName: name, Key: "_transcript", Value: transcript, UpdatedAt: at})
		}
		if err := s.RecordReport(ctx, entries, name, nil, &AgentEvent{At: at, Kind: AgentStatus, Agent: name, Status: status, Transcript: transcript}); err != nil {
			t.Fatal(err)
		}
	}
	report("lead-t1", "processing", "/t/one.jsonl")
	report("lead-t1", "idle", "")

	tree, err := s.ReadTree(ctx, false)
	if err != nil {
		t.Fatal(err)
	}
	if len(tree.Agents) != 2 || tree.Agents[0].Name != "lead" || tree.Agents[1].Cwd != "/w/sub" {
		t.Fatalf("live agents = %+v, want lead then lead-t1 (cwd /w/sub)", tree.Agents)
	}
	if r := tree.Runs["lead-t1"]; r == nil || r.Parent != "lead" || r.Backend != "agy" || !r.EndedAt.IsZero() {
		t.Errorf("lead-t1's run = %+v, want an open run with parent lead", r)
	}
	if tree.Cursor != 4 || !tree.Events {
		t.Errorf("cursor = %d (events %v), want 4: two spawns and two reports", tree.Cursor, tree.Events)
	}

	if err := s.DeleteAgent(ctx, "lead-t1"); err != nil {
		t.Fatal(err)
	}
	if err := s.DeleteAgent(ctx, "nobody"); err != nil {
		t.Fatalf("deleting an unknown agent: %v, want no error and no event", err)
	}
	want := []string{"spawned:lead:", "spawned:lead-t1:", "status:lead-t1:processing", "status:lead-t1:idle", "removed:lead-t1:idle"}
	if got := eventKinds(t, s, 0); !slices.Equal(got, want) {
		t.Errorf("events = %v, want %v", got, want)
	}
	if got := eventKinds(t, s, 3); !slices.Equal(got, want[3:]) {
		t.Errorf("events after 3 = %v, want %v", got, want[3:])
	}

	tree, err = s.ReadTree(ctx, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(tree.Agents) != 1 || len(tree.Ended) != 1 {
		t.Fatalf("after cleanup: %d live, %d ended, want 1 and 1", len(tree.Agents), len(tree.Ended))
	}
	r := tree.Ended[0]
	if r.Name != "lead-t1" || r.EndedAt.IsZero() || r.Status != "idle" || r.Transcript != "/t/one.jsonl" || r.Cwd != "/w/sub" {
		t.Errorf("ended run = %+v, want lead-t1, ended, idle, with its transcript and cwd", r)
	}
	if !r.CreatedAt.Equal(created.Add(time.Second)) {
		t.Errorf("ended run created %v, want %v", r.CreatedAt, created.Add(time.Second))
	}
	events, _, _ := s.AgentEventsAfter(ctx, 4, 10)
	if len(events) != 1 || events[0].Run != r.ID {
		t.Errorf("removed event = %+v, want one naming run %d", events, r.ID)
	}
}

func TestTree_EventsPruned(t *testing.T) {
	oldKeep, oldEvery := agentEventsKeep, agentEventsPruneEvery
	agentEventsKeep, agentEventsPruneEvery = 3, 2
	t.Cleanup(func() { agentEventsKeep, agentEventsPruneEvery = oldKeep, oldEvery })

	s, _ := newTestStore(t)
	ctx := context.Background()
	if err := s.CreateAgent(ctx, &Agent{Name: "a", SessionName: "agent-a", Backend: "claude", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 5; i++ {
		ev := &AgentEvent{At: time.Now(), Kind: AgentStatus, Agent: "a", Status: "idle"}
		if err := s.RecordReport(ctx, nil, "a", nil, ev); err != nil {
			t.Fatal(err)
		}
	}
	_, oldest, err := s.AgentEventsAfter(ctx, 0, 10)
	if err != nil {
		t.Fatal(err)
	}
	if oldest != 4 {
		t.Errorf("oldest kept = %d, want 4 (event 6 pruned up to 6-3)", oldest)
	}
}

// A store from before runs and agent events (and agents.cwd): its live
// agents still read, with cwd from the command log, and its departed agents
// come from the logged spawns.
func TestTree_OldStoreReadOnly(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gomuxai.db")
	db, err := sql.Open("sqlite", path)
	if err != nil {
		t.Fatal(err)
	}
	for _, stmt := range []string{
		`CREATE TABLE agents (name TEXT PRIMARY KEY, session_name TEXT NOT NULL, backend TEXT NOT NULL, task_file TEXT, log_file TEXT, created_at TEXT NOT NULL, spawned_by TEXT)`,
		`CREATE TABLE state (agent_name TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (agent_name, key))`,
		commandLogSchema,
		`INSERT INTO agents VALUES ('live', 'agent-live', 'claude', '', '', '2026-10-24T14:00:00Z', 'boss')`,
		`INSERT INTO state VALUES ('live', '_status', 'idle', '2026-10-24T14:01:00Z')`,
		`INSERT INTO command_log (started_at, duration_ms, host, agent, parent, cwd, args, exit_code, error, build)
			VALUES ('2026-10-24T13:00:00.000Z', 1, 'h', 'boss', '', '/old', '["spawn","gone","--backend","agy"]', 0, '', 'b'),
			       ('2026-10-24T13:30:00.000Z', 1, 'h', 'boss', '', '/w', '["spawn","--backend","claude","live","task.md"]', 0, '', 'b'),
			       ('2026-10-24T13:40:00.000Z', 1, 'h', 'boss', '', '/w', '["spawn","failed"]', 1, 'no', 'b')`,
	} {
		if _, err := db.Exec(stmt); err != nil {
			t.Fatalf("%s: %v", stmt, err)
		}
	}
	db.Close()

	s, err := OpenReadOnly(path)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	tree, err := s.ReadTree(context.Background(), true)
	if err != nil {
		t.Fatal(err)
	}
	if tree.Events || tree.Cursor != 0 {
		t.Errorf("events = %v, cursor %d; want no events in an old store", tree.Events, tree.Cursor)
	}
	if len(tree.Agents) != 1 || tree.Agents[0].SpawnedBy != "boss" || tree.Agents[0].Cwd != "/w" {
		t.Errorf("live = %+v, want live, parent boss, cwd /w from its logged spawn", tree.Agents)
	}
	if len(tree.State) != 1 {
		t.Errorf("state = %d entries, want 1", len(tree.State))
	}
	if len(tree.Ended) != 1 || tree.Ended[0].Name != "gone" || tree.Ended[0].Backend != "agy" || tree.Ended[0].Cwd != "/old" || tree.Ended[0].ID != 0 {
		t.Errorf("ended = %+v, want only gone (agy, /old, no run id); the failed spawn isn't one", tree.Ended)
	}
	if _, _, err := s.AgentEventsAfter(context.Background(), 0, 10); err != nil {
		t.Errorf("events of an old store: %v, want none and no error", err)
	}
	if err := s.CreateAgent(context.Background(), &Agent{Name: "x", SessionName: "agent-x", Backend: "claude", CreatedAt: time.Now()}); err == nil {
		t.Error("a read-only store took a write")
	}
}

func TestSpawnArgs(t *testing.T) {
	for _, tc := range []struct{ raw, name, backend string }{
		{`["spawn","helper","--backend","claude"]`, "helper", "claude"},
		{`["spawn","--backend=agy","--task","review","helper"]`, "helper", "agy"},
		{`["spawn","--skip-permissions","helper","task.md"]`, "helper", ""},
		{`["list"]`, "", ""},
		{`not json`, "", ""},
	} {
		if name, backend := spawnArgs(tc.raw); name != tc.name || backend != tc.backend {
			t.Errorf("spawnArgs(%s) = %q, %q; want %q, %q", tc.raw, name, backend, tc.name, tc.backend)
		}
	}
}

// A store from before end reasons: opening it renames "exited" to "ended"
// once, with "gone" as the reason (nothing recorded how those ended), and
// leaves later rows alone.
func TestMigrateExitedToEnded(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gomuxai.db")
	s, err := NewSQLiteStorage(path)
	if err != nil {
		t.Fatal(err)
	}
	s.Close()
	db, err := sql.Open("sqlite", path)
	if err != nil {
		t.Fatal(err)
	}
	for _, stmt := range []string{
		// As the binary before end reasons left it.
		`ALTER TABLE agent_events DROP COLUMN end_reason`,
		`ALTER TABLE runs DROP COLUMN end_reason`,
		`INSERT INTO agents (name, session_name, backend, created_at) VALUES ('w', 'agent-w', 'claude', '2026-10-24T14:00:00Z')`,
		`INSERT INTO state VALUES ('w', '_status', 'exited', '2026-10-24T14:01:00Z')`,
		`INSERT INTO runs (name, parent, backend, session, cwd, created_at, ended_at, status) VALUES ('old', '', 'claude', 's', '/', 'x', 'y', 'exited')`,
		`INSERT INTO agent_events (at, kind, agent, run, status) VALUES ('2026-10-24T14:01:00Z', 'status', 'w', 0, 'exited')`,
	} {
		if _, err := db.Exec(stmt); err != nil {
			t.Fatalf("%s: %v", stmt, err)
		}
	}
	db.Close()

	s, err = NewSQLiteStorage(path)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	ctx := context.Background()
	st, _ := s.GetState(ctx, "w", "_status")
	why, _ := s.GetState(ctx, "w", "_end_reason")
	if st == nil || st.Value != "ended" || why == nil || why.Value != "gone" {
		t.Errorf("state: %+v %+v", st, why)
	}
	var runStatus, runReason, evStatus, evReason string
	s.db.QueryRow(`SELECT status, end_reason FROM runs WHERE name = 'old'`).Scan(&runStatus, &runReason)
	s.db.QueryRow(`SELECT status, end_reason FROM agent_events WHERE agent = 'w'`).Scan(&evStatus, &evReason)
	if runStatus != "ended" || runReason != "gone" || evStatus != "ended" || evReason != "gone" {
		t.Errorf("run %s/%s, event %s/%s", runStatus, runReason, evStatus, evReason)
	}
	// Once: a later "ended" row with its own reason is not touched again.
	s.Close()
	s2, _ := NewSQLiteStorage(path)
	defer s2.Close()
	if why, _ := s2.GetState(ctx, "w", "_end_reason"); why == nil || why.Value != "gone" {
		t.Errorf("reopen: %+v", why)
	}
}

// A resume closes the agent's run with its last status and opens a new one
// under the same name, keeping its state.
func TestTree_ReplaceAgent(t *testing.T) {
	s, _ := newTestStore(t)
	ctx := context.Background()
	created := time.Date(2026, 10, 31, 9, 0, 0, 0, time.UTC)
	if err := s.CreateAgent(ctx, &Agent{Name: "lead", SessionName: "agent-lead", Backend: "agy", SpawnedBy: "boss", Cwd: "/w", CreatedAt: created}); err != nil {
		t.Fatal(err)
	}
	at := time.Now()
	if err := s.SetStates(ctx, []*StateEntry{
		{AgentName: "lead", Key: "_status", Value: "ended", UpdatedAt: at},
		{AgentName: "lead", Key: "_end_reason", Value: "gone", UpdatedAt: at},
		{AgentName: "lead", Key: "notes", Value: "keep me", UpdatedAt: at},
	}); err != nil {
		t.Fatal(err)
	}

	resumed := created.Add(time.Hour)
	if err := s.ReplaceAgent(ctx, &Agent{Name: "lead", SessionName: "agent-lead", Backend: "agy", SpawnedBy: "boss", Cwd: "/w", CreatedAt: resumed}); err != nil {
		t.Fatal(err)
	}

	tree, err := s.ReadTree(ctx, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(tree.Agents) != 1 || !tree.Agents[0].CreatedAt.Equal(resumed) || tree.Agents[0].SpawnedBy != "boss" {
		t.Fatalf("live agents = %+v, want lead, resumed at %v, still boss's", tree.Agents, resumed)
	}
	if r := tree.Runs["lead"]; r == nil || !r.EndedAt.IsZero() || !r.CreatedAt.Equal(resumed) {
		t.Errorf("lead's live run = %+v, want an open one from the resume", r)
	}
	if len(tree.Ended) != 1 || tree.Ended[0].Status != "ended" || tree.Ended[0].EndReason != "gone" || !tree.Ended[0].CreatedAt.Equal(created) {
		t.Errorf("ended runs = %+v, want the first run, ended (gone)", tree.Ended)
	}
	want := []string{"spawned:lead:", "removed:lead:ended", "spawned:lead:"}
	if got := eventKinds(t, s, 0); !slices.Equal(got, want) {
		t.Errorf("events = %v, want %v", got, want)
	}
	if e, err := s.GetState(ctx, "lead", "notes"); err != nil || e == nil || e.Value != "keep me" {
		t.Errorf("notes after replace = %+v, %v; want kept", e, err)
	}
}

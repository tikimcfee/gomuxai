package export

import (
	"context"
	"database/sql"
	"errors"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// fixture is a store being written as agent writes it, and the sessions
// a fake tmux server has.
type fixture struct {
	t        *testing.T
	dir      string
	w        *storage.SQLiteStorage
	sessions []string
	pathOf   map[string]string // session -> data dir its PATH points at
}

func newFixture(t *testing.T) *fixture {
	t.Helper()
	dir := filepath.Join(t.TempDir(), ".gomuxai")
	return &fixture{t: t, dir: dir, w: openStore(t, dir), pathOf: map[string]string{}}
}

func openStore(t *testing.T, dir string) *storage.SQLiteStorage {
	t.Helper()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	w, err := storage.NewSQLiteStorage(filepath.Join(dir, "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { w.Close() })
	return w
}

func (f *fixture) deps() Deps {
	return Deps{
		Host:          "box",
		SessionPrefix: "agent-",
		Sessions:      func() ([]string, error) { return f.sessions, nil },
		SessionStore:  func(s string) string { return f.pathOf[s] },
	}
}

func (f *fixture) tree(all bool, dirs ...string) *Tree {
	tr := New(f.deps(), append([]string{f.dir}, dirs...), nil, all)
	f.t.Cleanup(tr.Close)
	return tr
}

func (f *fixture) spawn(w *storage.SQLiteStorage, name, parent string) {
	f.t.Helper()
	f.sessions = append(f.sessions, "agent-"+name)
	if err := w.CreateAgent(context.Background(), &storage.Agent{Name: name, SessionName: "agent-" + name, Backend: "claude",
		SpawnedBy: parent, Cwd: "/w", CreatedAt: time.Now()}); err != nil {
		f.t.Fatal(err)
	}
}

func (f *fixture) report(name, status, detail, transcript string) {
	f.t.Helper()
	now := time.Now()
	entries := []*storage.StateEntry{
		{AgentName: name, Key: "_status", Value: status, UpdatedAt: now},
		{AgentName: name, Key: "_detail", Value: detail, UpdatedAt: now},
	}
	if transcript != "" {
		entries = append(entries, &storage.StateEntry{AgentName: name, Key: "_transcript", Value: transcript, UpdatedAt: now})
	}
	ev := &storage.AgentEvent{At: now, Kind: storage.AgentStatus, Agent: name, Status: status, Detail: detail, Transcript: transcript}
	if err := f.w.RecordReport(context.Background(), entries, name, nil, ev); err != nil {
		f.t.Fatal(err)
	}
}

func (f *fixture) cleanup(name string) {
	f.t.Helper()
	f.sessions = slices.DeleteFunc(f.sessions, func(s string) bool { return s == "agent-"+name })
	if err := f.w.DeleteAgent(context.Background(), name); err != nil {
		f.t.Fatal(err)
	}
}

func poll(t *testing.T, tr *Tree) []any {
	t.Helper()
	lines, err := tr.Poll(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	return lines
}

// summary renders lines as "type:name[:reason][:status]" for comparing.
func summary(lines []any) []string {
	var out []string
	for _, l := range lines {
		switch l := l.(type) {
		case AgentLine:
			s := l.Type + ":" + l.Agent.Name
			if l.Reason != "" {
				s += ":" + l.Reason
			}
			if l.Agent.Status != "" {
				s += ":" + l.Agent.Status
			}
			out = append(out, s)
		case StoreLine:
			out = append(out, "store:"+filepath.Base(filepath.Dir(l.Store.Dir)))
		case HostLine:
			out = append(out, "host:"+l.Host.Host)
		case UnrecordedLine:
			out = append(out, "unrecorded")
		}
	}
	return out
}

func TestSnapshot(t *testing.T) {
	f := newFixture(t)
	transcript := filepath.Join(t.TempDir(), "s.jsonl")
	if err := os.WriteFile(transcript, []byte("{}\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	f.spawn(f.w, "lead", "")
	f.spawn(f.w, "lead-t1", "lead")
	f.report("lead-t1", "waiting", "permission: Bash: rm x", transcript)
	f.spawn(f.w, "gone", "lead")
	f.cleanup("gone")
	f.sessions = append(f.sessions, "agent-stray", "other-session")

	snap, err := f.tree(false).Snapshot(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Stores) != 1 || !snap.Stores[0].This || !snap.Stores[0].Events || snap.Stores[0].Cursor != 5 {
		t.Errorf("stores = %+v, want this store with events up to 5", snap.Stores)
	}
	var names []string
	for _, a := range snap.Agents {
		names = append(names, a.Name)
	}
	if !slices.Equal(names, []string{"lead", "lead-t1"}) {
		t.Fatalf("agents = %v, want lead, lead-t1 (gone was cleaned up)", names)
	}
	a := snap.Agents[1]
	if a.Parent != "lead" || a.Host != "box" || a.Kind != KindTmux || !a.Live || a.Attached == nil || !*a.Attached ||
		a.Status != "waiting" || a.Run == 0 || a.Store != f.dir || a.Cwd != "/w" {
		t.Errorf("lead-t1 = %+v", a)
	}
	if a.Transcript != transcript || a.Dialect != "claude" || a.TranscriptSize != 3 || a.TranscriptMtime.IsZero() {
		t.Errorf("lead-t1 transcript = %q %q %d %v, want the file, claude, 3 bytes", a.Transcript, a.Dialect, a.TranscriptSize, a.TranscriptMtime)
	}
	if len(snap.Unrecorded) != 1 || snap.Unrecorded[0].Session != "agent-stray" {
		t.Errorf("unrecorded = %+v, want agent-stray only (other-session isn't an agent's)", snap.Unrecorded)
	}

	snap, err = f.tree(true).Snapshot(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Agents) != 3 {
		t.Fatalf("with all: %d agents, want 3", len(snap.Agents))
	}
	g := snap.Agents[2]
	if g.Name != "gone" || g.Live || g.EndedAt.IsZero() || g.Attached == nil || *g.Attached || g.Parent != "lead" {
		t.Errorf("departed = %+v, want gone, not live, ended, detached", g)
	}
}

// Runs don't record the session id, so a departed agent's comes from its
// transcript's path: in a snapshot, and in the removed line of a follower
// that never saw a _session_id for it.
func TestDepartedSessionID(t *testing.T) {
	f := newFixture(t)
	claude := "/home/user/.claude/projects/-w/11111111-2222-3333-4444-555555555555.jsonl"
	agy := "/home/user/.gemini/antigravity-cli/brain/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/.system_generated/logs/transcript_full.jsonl"
	f.spawn(f.w, "cl", "")
	f.report("cl", "idle", "", claude)
	f.spawn(f.w, "ag", "")
	f.report("ag", "idle", "", agy)
	f.spawn(f.w, "none", "")

	tr := f.tree(true)
	if _, err := tr.Snapshot(context.Background()); err != nil {
		t.Fatal(err)
	}
	f.cleanup("cl")
	lines := poll(t, tr)
	if len(lines) != 1 || lines[0].(AgentLine).Agent.SessionID != "11111111-2222-3333-4444-555555555555" {
		t.Errorf("removed line = %+v, want cl with its transcript's session id", lines)
	}

	f.cleanup("ag")
	f.cleanup("none")
	snap, err := f.tree(true).Snapshot(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	got := map[string]string{}
	for _, a := range snap.Agents {
		got[a.Name] = a.SessionID
	}
	want := map[string]string{"cl": "11111111-2222-3333-4444-555555555555", "ag": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "none": ""}
	if !maps.Equal(got, want) {
		t.Errorf("departed session ids = %v, want %v", got, want)
	}
}

func TestFollow(t *testing.T) {
	f := newFixture(t)
	f.spawn(f.w, "lead", "")
	tr := f.tree(true)
	if _, err := tr.Snapshot(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := poll(t, tr); len(got) != 0 {
		t.Fatalf("a quiet store: %v, want no lines", summary(got))
	}

	f.spawn(f.w, "lead-t1", "lead")
	f.report("lead-t1", "processing", "Bash: ls", "")
	f.report("lead-t1", "waiting", "permission: Bash: rm x", "")
	f.report("lead-t1", "idle", "", "")
	lines := poll(t, tr)
	want := []string{"spawned:lead-t1", "changed:lead-t1:status:processing", "changed:lead-t1:status:waiting", "changed:lead-t1:status:idle"}
	if got := summary(lines); !slices.Equal(got, want) {
		t.Fatalf("lines = %v, want %v: every report, even one already superseded", got, want)
	}
	sp := lines[0].(AgentLine)
	if sp.ID != 2 || sp.Store != f.dir || sp.Agent.Parent != "lead" || !sp.Agent.Live || sp.Agent.Attached == nil || !*sp.Agent.Attached {
		t.Errorf("spawned = %+v", sp)
	}
	if w := lines[2].(AgentLine); w.Agent.Detail != "permission: Bash: rm x" || w.ID != 4 {
		t.Errorf("waiting line = %+v", w)
	}

	f.cleanup("lead-t1")
	lines = poll(t, tr)
	if got := summary(lines); !slices.Equal(got, []string{"removed:lead-t1:idle"}) {
		t.Fatalf("after cleanup: %v, want removed:lead-t1:idle", got)
	}
	rm := lines[0].(AgentLine).Agent
	if rm.Live || rm.EndedAt.IsZero() || rm.Attached == nil || *rm.Attached {
		t.Errorf("removed agent = %+v, want departed", rm)
	}
	if got := len(tr.snapshot().Agents); got != 2 {
		t.Errorf("tree with all after cleanup has %d agents, want lead and the departed lead-t1", got)
	}

	// The session going away without a cleanup shows on the next session check.
	f.sessions = nil
	if got := summary(tr.PollSessions(context.Background())); !slices.Equal(got, []string{"changed:lead:session"}) {
		t.Errorf("session gone: %v, want changed:lead:session", got)
	}
	if got := tr.PollSessions(context.Background()); len(got) != 0 {
		t.Errorf("unchanged sessions: %v, want no lines", summary(got))
	}
}

func TestFollow_TranscriptGrows(t *testing.T) {
	f := newFixture(t)
	f.spawn(f.w, "a", "")
	transcript := filepath.Join(t.TempDir(), "a.jsonl")
	os.WriteFile(transcript, []byte("{}\n"), 0o600)
	f.report("a", "processing", "", transcript)
	tr := f.tree(false)
	if _, err := tr.Snapshot(context.Background()); err != nil {
		t.Fatal(err)
	}
	os.WriteFile(transcript, []byte("{}\n{}\n"), 0o600)
	lines := tr.PollSessions(context.Background())
	if got := summary(lines); !slices.Equal(got, []string{"changed:a:transcript:processing"}) {
		t.Fatalf("lines = %v, want one transcript change", got)
	}
	if a := lines[0].(AgentLine).Agent; a.TranscriptSize != 6 {
		t.Errorf("size = %d, want 6", a.TranscriptSize)
	}
}

func TestResume(t *testing.T) {
	f := newFixture(t)
	f.spawn(f.w, "a", "")
	f.report("a", "processing", "", "")
	f.report("a", "idle", "", "")

	tr := f.tree(false)
	ok, err := tr.Resume(context.Background(), 1)
	if err != nil || !ok {
		t.Fatalf("Resume(1) = %v, %v; want a resume", ok, err)
	}
	if got := summary(poll(t, tr)); !slices.Equal(got, []string{"changed:a:status:processing", "changed:a:status:idle"}) {
		t.Errorf("after resuming from 1: %v, want the two reports", got)
	}
	if ok, _ := f.tree(false).Resume(context.Background(), 99); ok {
		t.Error("Resume past the newest event resumed")
	}
}

// A second store, found through an unrecorded session's PATH, is read and
// followed too; one from an older binary (no events) is re-read whole when
// it changes.
func TestFollow_OtherStores(t *testing.T) {
	f := newFixture(t)
	f.spawn(f.w, "coord", "")
	tr := f.tree(false)
	if _, err := tr.Snapshot(context.Background()); err != nil {
		t.Fatal(err)
	}

	otherDir := filepath.Join(t.TempDir(), ".gomuxai")
	other := openStore(t, otherDir)
	f.spawn(other, "coord-t1", "coord")
	f.pathOf["agent-coord-t1"] = otherDir
	lines := tr.PollSessions(context.Background())
	if got := summary(lines); !slices.Equal(got, []string{"store:" + filepath.Base(filepath.Dir(otherDir))}) {
		t.Fatalf("new session in another store: %v, want a store line", got)
	}
	sl := lines[0].(StoreLine)
	if len(sl.Agents) != 1 || sl.Agents[0].Parent != "coord" || sl.Agents[0].Store != otherDir {
		t.Errorf("store line = %+v", sl)
	}
	f.spawn(other, "coord-t2", "coord")
	if got := summary(poll(t, tr)); !slices.Equal(got, []string{"spawned:coord-t2"}) {
		t.Errorf("spawn in the other store: %v, want spawned:coord-t2", got)
	}

	// No store found: the session is listed as unrecorded, once.
	f.sessions = append(f.sessions, "agent-stray")
	if got := summary(tr.PollSessions(context.Background())); !slices.Equal(got, []string{"unrecorded"}) {
		t.Errorf("stray session: %v, want an unrecorded line", got)
	}
	if got := tr.PollSessions(context.Background()); len(got) != 0 {
		t.Errorf("same stray session: %v, want no lines", summary(got))
	}
}

func TestFollow_StoreWithoutEvents(t *testing.T) {
	f := newFixture(t)
	oldDir := filepath.Join(t.TempDir(), ".gomuxai")
	os.MkdirAll(oldDir, 0o755)
	db, err := sql.Open("sqlite", filepath.Join(oldDir, "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	for _, stmt := range []string{
		`CREATE TABLE agents (name TEXT PRIMARY KEY, session_name TEXT NOT NULL, backend TEXT NOT NULL, task_file TEXT, log_file TEXT, created_at TEXT NOT NULL, spawned_by TEXT)`,
		`CREATE TABLE state (agent_name TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (agent_name, key))`,
		`CREATE TABLE command_log (id INTEGER PRIMARY KEY, started_at TEXT, agent TEXT, cwd TEXT, args TEXT, exit_code INTEGER)`,
		`INSERT INTO agents VALUES ('old', 'agent-old', 'claude', '', '', '2026-10-24T14:00:00Z', '')`,
	} {
		if _, err := db.Exec(stmt); err != nil {
			t.Fatal(err)
		}
	}
	tr := f.tree(false, oldDir)
	snap, err := tr.Snapshot(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Stores) != 2 || snap.Stores[1].Events || len(snap.Agents) != 1 {
		t.Fatalf("snapshot = %+v, want the old store without events and its agent", snap)
	}
	if got := poll(t, tr); len(got) != 0 {
		t.Errorf("quiet old store: %v, want no lines", summary(got))
	}
	if _, err := db.Exec(`INSERT INTO state VALUES ('old', '_status', 'processing', '2026-10-24T14:01:00Z')`); err != nil {
		t.Fatal(err)
	}
	lines := poll(t, tr)
	if got := summary(lines); !slices.Equal(got, []string{"changed:old:status:processing"}) || lines[0].(AgentLine).ID != 0 {
		t.Errorf("old store's agent changed: %v, want a changed line with no event id", got)
	}
	if _, err := db.Exec(`INSERT INTO agents VALUES ('new', 'agent-new', 'claude', '', '', '2026-10-24T14:02:00Z', '')`); err != nil {
		t.Fatal(err)
	}
	lines = poll(t, tr)
	if got := summary(lines); len(got) != 1 || len(lines[0].(StoreLine).Agents) != 2 {
		t.Errorf("an agent appeared in the old store: %v, want a store line with both (no event says it was spawned)", got)
	}
}

func TestHosts(t *testing.T) {
	f := newFixture(t)
	d := f.deps()
	var listed []natsbus.AgentInfo
	var listErr error
	d.ListHost = func(_ context.Context, host string) ([]natsbus.AgentInfo, error) { return listed, listErr }
	tr := New(d, []string{f.dir}, []string{"hub"}, false)
	defer tr.Close()

	listed = []natsbus.AgentInfo{{Name: "far", Backend: "claude", Session: "agent-far", SpawnedBy: "lead", Status: "idle", CreatedAt: time.Now()}}
	snap, err := tr.Snapshot(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Hosts) != 1 || snap.Hosts[0].Agents != 1 || len(snap.Agents) != 1 {
		t.Fatalf("snapshot = %+v, want hub's one agent", snap)
	}
	if a := snap.Agents[0]; a.Host != "hub" || a.Store != "" || a.Attached != nil || a.Status != "idle" || a.Parent != "lead" {
		t.Errorf("remote agent = %+v, want host hub, no store, attached unknown", a)
	}
	if got := tr.PollHosts(context.Background()); len(got) != 0 {
		t.Errorf("unchanged host: %v, want no lines", summary(got))
	}
	listErr = errors.New("no bus")
	lines := tr.PollHosts(context.Background())
	if got := summary(lines); !slices.Equal(got, []string{"host:hub"}) || lines[0].(HostLine).Host.Error != "no bus" {
		t.Errorf("host unreachable: %+v, want a host line with the error", lines)
	}
}

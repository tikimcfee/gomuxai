// Package export builds the agent tree that agent export prints: a
// snapshot of every agent in one or more stores, and the changes after it,
// read from the stores' agent events. It only reads: stores open read-only,
// and nothing runs a backend CLI.
package export

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// KindTmux is an agent running in a tmux session that agent spawned. It is
// the only kind so far; plain CLI sessions a person started could be
// another.
const KindTmux = "tmux"

// Agent is one agent as agent export prints it. Status, Event, Detail,
// Tool, and ToolInput are the last recorded report (hooks or agent report),
// not the resolved status agent status shows: no backend CLI runs.
type Agent struct {
	Name      string    `json:"name"`
	Host      string    `json:"host"`
	Kind      string    `json:"kind"`
	Run       int64     `json:"run,omitempty"`    // the spawn's id in its store; 0 if spawned before runs were recorded
	Parent    string    `json:"parent,omitempty"` // the agent that spawned it; absent for a person
	Backend   string    `json:"backend"`
	Session   string    `json:"session"`         // tmux session on Host
	Store     string    `json:"store,omitempty"` // the data dir that records it; absent for other hosts
	Cwd       string    `json:"cwd,omitempty"`
	CreatedAt time.Time `json:"created_at"`
	EndedAt   time.Time `json:"ended_at,omitzero"` // when cleanup removed it
	Live      bool      `json:"live"`              // it has a record (not cleaned up)
	// Attached reports whether its tmux session exists; absent for other
	// hosts, where it can't be checked.
	Attached   *bool     `json:"attached,omitempty"`
	Status     string    `json:"status,omitempty"`
	Event      string    `json:"event,omitempty"`
	Detail     string    `json:"detail,omitempty"`
	Tool       string    `json:"tool,omitempty"`
	ToolInput  string    `json:"tool_input,omitempty"` // a pending permission prompt's full call
	StatusAt   time.Time `json:"status_at,omitzero"`
	SessionID  string    `json:"session_id,omitempty"`
	Transcript string    `json:"transcript,omitempty"`
	Dialect    string    `json:"dialect,omitempty"` // how to read Transcript: claude or agy
	// TranscriptSize and TranscriptMtime are the transcript file's, when it
	// exists on this host.
	TranscriptSize  int64     `json:"transcript_size,omitempty"`
	TranscriptMtime time.Time `json:"transcript_mtime,omitzero"`
}

// Store is a data dir export reads.
type Store struct {
	Dir  string `json:"dir"`
	This bool   `json:"this,omitempty"` // the running binary's own store
	// Cursor is the newest agent event the output includes: follow lines
	// for this store carry larger ids.
	Cursor int64 `json:"cursor"`
	// Events is false for a store written by an agent built before export:
	// its changes arrive as whole store lines instead of events.
	Events bool   `json:"events"`
	Error  string `json:"error,omitempty"`
}

// Session is a tmux agent session that no store export read records.
type Session struct {
	Session string `json:"session"`
	// Store is the data dir of the binary that started it, if found (it
	// may be unreadable, or not record the session).
	Store string `json:"store,omitempty"`
}

// Host is another host whose agents the output includes.
type Host struct {
	Host   string `json:"host"`
	Agents int    `json:"agents"`
	Error  string `json:"error,omitempty"`
}

// Snapshot is the whole tree.
type Snapshot struct {
	Type       string    `json:"type"` // "snapshot"
	Host       string    `json:"host"`
	Stores     []Store   `json:"stores"`
	Hosts      []Host    `json:"hosts,omitempty"`
	Agents     []Agent   `json:"agents"`
	Unrecorded []Session `json:"unrecorded"`
}

// Line types after the snapshot.
const (
	LineSpawned    = "spawned"    // spawn recorded an agent
	LineChanged    = "changed"    // an agent changed; Reason says what
	LineRemoved    = "removed"    // cleanup removed an agent (Agent is its departed form)
	LineStore      = "store"      // a store's agents, replacing what was known of it
	LineHost       = "host"       // another host's agents, replacing what was known of it
	LineUnrecorded = "unrecorded" // the sessions no store records, replacing the last list
)

// Reasons for a changed line.
const (
	ReasonStatus     = "status"     // a status report (an agent event)
	ReasonSession    = "session"    // its tmux session appeared or went away
	ReasonTranscript = "transcript" // its transcript grew or moved
)

// AgentLine is a spawned, changed, or removed line.
type AgentLine struct {
	Type   string    `json:"type"`
	ID     int64     `json:"id,omitempty"`    // the agent event's id in Store; absent for session and transcript changes
	Store  string    `json:"store,omitempty"` // absent for other hosts
	At     time.Time `json:"at"`
	Reason string    `json:"reason,omitempty"`
	Agent  Agent     `json:"agent"`
}

// StoreLine replaces what was known of a store: one found while
// following, or one without events that changed.
type StoreLine struct {
	Type   string  `json:"type"` // "store"
	Store  Store   `json:"store"`
	Agents []Agent `json:"agents"`
}

// HostLine replaces what was known of another host's agents.
type HostLine struct {
	Type   string  `json:"type"` // "host"
	Host   Host    `json:"host"`
	Agents []Agent `json:"agents"`
}

// UnrecordedLine replaces the list of sessions no store records.
type UnrecordedLine struct {
	Type     string    `json:"type"` // "unrecorded"
	Sessions []Session `json:"sessions"`
}

// Deps is what a Tree needs from outside: tmux, the bus, the clock.
type Deps struct {
	Host          string // this host's name
	SessionPrefix string // agent sessions' prefix, "agent-"
	// Sessions lists the tmux server's session names.
	Sessions func() ([]string, error)
	// SessionStore returns the data dir of the binary that started a
	// session (from the PATH it was given), or "".
	SessionStore func(session string) string
	// ListHost lists another host's agents over the bus.
	ListHost func(ctx context.Context, host string) ([]natsbus.AgentInfo, error)
	Now      func() time.Time
}

// Tree is the agent tree across stores and hosts, kept current by its
// Poll methods. It isn't safe for concurrent use.
type Tree struct {
	d      Deps
	all    bool // include departed agents
	stores []*store
	hosts  []string

	sessions   map[string]bool   // live tmux sessions
	sessStore  map[string]string // session -> SessionStore's answer, asked once
	unrecorded []Session
	remote     map[string][]Agent // by host
	hostErrs   map[string]string  // why a host's last list failed
}

type store struct {
	dir     string
	this    bool
	db      *storage.SQLiteStorage
	err     string
	cursor  int64
	events  bool
	version int64 // data_version, for stores without events
	live    map[string]*Agent
	ended   []Agent
}

// pollBatch bounds the events one Poll reads per store.
const pollBatch = 500

// New returns a tree over the stores in dirs (data dirs; the first is this
// binary's own) and the hosts named. With all, departed agents are
// included.
func New(d Deps, dirs []string, hosts []string, all bool) *Tree {
	if d.Now == nil {
		d.Now = time.Now
	}
	t := &Tree{d: d, all: all, hosts: hosts, sessStore: map[string]string{}, remote: map[string][]Agent{}, hostErrs: map[string]string{}}
	for i, dir := range dirs {
		t.addStore(dir, i == 0)
	}
	return t
}

// Close closes the stores.
func (t *Tree) Close() {
	for _, s := range t.stores {
		if s.db != nil {
			s.db.Close()
		}
	}
}

// addStore adds a store unless it is known, and reports whether it did.
func (t *Tree) addStore(dir string, this bool) (*store, bool) {
	dir = filepath.Clean(dir)
	for _, s := range t.stores {
		if s.dir == dir {
			return s, false
		}
	}
	s := &store{dir: dir, this: this, live: map[string]*Agent{}}
	db, err := storage.OpenReadOnly(filepath.Join(dir, "gomuxai.db"))
	if err != nil {
		s.err = err.Error()
	} else {
		s.db = db
	}
	t.stores = append(t.stores, s)
	return s, true
}

// Snapshot reads every store and host and returns the whole tree. Stores
// that sessions point at are found and read too.
func (t *Tree) Snapshot(ctx context.Context) (*Snapshot, error) {
	t.refreshSessions()
	for _, s := range t.stores {
		if err := t.read(ctx, s); err != nil && s.this {
			return nil, err
		}
	}
	t.findStores(ctx)
	for _, h := range t.hosts {
		t.readHost(ctx, h)
	}
	return t.snapshot(), nil
}

func (t *Tree) snapshot() *Snapshot {
	snap := &Snapshot{Type: "snapshot", Host: t.d.Host, Agents: []Agent{}, Unrecorded: t.unrecorded}
	for _, s := range t.stores {
		snap.Stores = append(snap.Stores, s.info())
		snap.Agents = append(snap.Agents, s.agents()...)
	}
	for _, h := range t.hosts {
		snap.Hosts = append(snap.Hosts, t.hostInfo(h))
		snap.Agents = append(snap.Agents, t.remote[h]...)
	}
	sortAgents(snap.Agents)
	if snap.Unrecorded == nil {
		snap.Unrecorded = []Session{}
	}
	return snap
}

// Resume fills the tree as Snapshot does, but sets this store's cursor to
// since, so the next Poll returns the events after it. It returns false,
// changing nothing, when those events are no longer all kept (or since is
// ahead of the store): the caller starts from a snapshot instead.
func (t *Tree) Resume(ctx context.Context, since int64) (bool, error) {
	if _, err := t.Snapshot(ctx); err != nil {
		return false, err
	}
	own := t.stores[0]
	if own.db == nil || !own.events || since > own.cursor {
		return false, nil
	}
	_, oldest, err := own.db.AgentEventsAfter(ctx, since, 1)
	if err != nil {
		return false, err
	}
	if since < own.cursor && oldest > since+1 {
		return false, nil
	}
	own.cursor = since
	return true, nil
}

// Poll returns the lines for each store's new agent events, and a store
// line for each store without events whose contents changed.
func (t *Tree) Poll(ctx context.Context) ([]any, error) {
	var lines []any
	for _, s := range t.stores {
		if s.db == nil {
			continue
		}
		if !s.events {
			v, err := s.db.DataVersion(ctx)
			if err != nil || v == s.version {
				continue
			}
			lines = append(lines, t.reread(ctx, s)...)
			continue
		}
		for {
			events, _, err := s.db.AgentEventsAfter(ctx, s.cursor, pollBatch)
			if err != nil {
				return lines, fmt.Errorf("reading agent events of %s: %w", s.dir, err)
			}
			for _, ev := range events {
				if line, ok := t.apply(ctx, s, ev); ok {
					lines = append(lines, line)
				}
				s.cursor = ev.ID
			}
			if len(events) < pollBatch {
				break
			}
		}
	}
	return lines, nil
}

// PollSessions checks the tmux sessions and transcripts: a session that
// appeared or went away, a transcript that grew, a store found through a
// new session, and a change in the sessions no store records.
func (t *Tree) PollSessions(ctx context.Context) []any {
	var lines []any
	t.refreshSessions()
	now := t.d.Now().UTC()
	for _, s := range t.stores {
		for _, name := range sortedNames(s.live) {
			a := s.live[name]
			attached := t.sessions[a.Session]
			if a.Attached == nil || *a.Attached != attached {
				a.Attached = &attached
				lines = append(lines, AgentLine{Type: LineChanged, Store: s.dir, At: now, Reason: ReasonSession, Agent: *a})
			}
			size, mtime := a.TranscriptSize, a.TranscriptMtime
			statTranscript(a)
			if a.TranscriptSize != size || !a.TranscriptMtime.Equal(mtime) {
				lines = append(lines, AgentLine{Type: LineChanged, Store: s.dir, At: now, Reason: ReasonTranscript, Agent: *a})
			}
		}
	}
	before := t.unrecorded
	for _, s := range t.findStores(ctx) {
		lines = append(lines, StoreLine{Type: LineStore, Store: s.info(), Agents: s.agents()})
	}
	if !slices.Equal(before, t.unrecorded) {
		lines = append(lines, UnrecordedLine{Type: LineUnrecorded, Sessions: nonNil(t.unrecorded)})
	}
	return lines
}

// PollHosts lists the other hosts again and returns a host line for each
// whose agents changed. Each list makes that host resolve its agents'
// status, so call it rarely.
func (t *Tree) PollHosts(ctx context.Context) []any {
	var lines []any
	for _, h := range t.hosts {
		before, hadErr := t.remote[h], t.hostErr(h)
		t.readHost(ctx, h)
		if !sameAgents(before, t.remote[h]) || hadErr != t.hostErr(h) {
			lines = append(lines, HostLine{Type: LineHost, Host: t.hostInfo(h), Agents: nonNil(t.remote[h])})
		}
	}
	return lines
}

// reread reads a store without events again. With the same agents, each
// one that changed gets a changed line; if agents came or went, a store
// line replaces them all, since without events nothing says whether they
// were spawned or cleaned up.
func (t *Tree) reread(ctx context.Context, s *store) []any {
	before := s.agents()
	if err := t.read(ctx, s); err != nil {
		return nil
	}
	after := s.agents()
	key := func(a Agent) string {
		return a.Name + "\x00" + strconv.FormatInt(a.Run, 10) + "\x00" + strconv.FormatBool(a.Live)
	}
	if !slices.Equal(keysOf(before, key), keysOf(after, key)) {
		return []any{StoreLine{Type: LineStore, Store: s.info(), Agents: after}}
	}
	var lines []any
	now := t.d.Now().UTC()
	for i := range after {
		if !sameAgents(before[i:i+1], after[i:i+1]) {
			lines = append(lines, AgentLine{Type: LineChanged, Store: s.dir, At: now, Reason: ReasonStatus, Agent: after[i]})
		}
	}
	return lines
}

func keysOf(agents []Agent, key func(Agent) string) []string {
	out := make([]string, len(agents))
	for i, a := range agents {
		out[i] = key(a)
	}
	return out
}

// apply updates s for ev and returns its line.
func (t *Tree) apply(ctx context.Context, s *store, ev *storage.AgentEvent) (any, bool) {
	line := AgentLine{ID: ev.ID, Store: s.dir, At: ev.At}
	switch ev.Kind {
	case storage.AgentSpawned:
		run, err := s.db.GetRun(ctx, ev.Run)
		if err != nil || run == nil {
			return nil, false
		}
		a := t.fromRun(s, run)
		a.Live = true
		t.refreshSessions() // the session was created just before the record
		attached := t.sessions[a.Session]
		a.Attached = &attached
		s.live[a.Name] = &a
		line.Type, line.Agent = LineSpawned, a
	case storage.AgentStatus:
		a := s.live[ev.Agent]
		if a == nil {
			// Removed since (a resumed follower replays events from before
			// the tree it read): describe the run as it was.
			run, err := s.db.GetRun(ctx, ev.Run)
			if err != nil || run == nil {
				return nil, false
			}
			r := t.fromRun(s, run)
			r.Live, r.EndedAt = true, time.Time{} // as it was then
			a = &r
		}
		a.Status, a.Event, a.Detail, a.Tool, a.ToolInput, a.StatusAt = ev.Status, ev.Event, ev.Detail, ev.Tool, ev.ToolInput, ev.At
		if ev.SessionID != "" {
			a.SessionID = ev.SessionID
		}
		if ev.Transcript != "" {
			a.Transcript = ev.Transcript
			a.Dialect = dialect(a.Backend, a.Transcript)
		}
		statTranscript(a)
		line.Type, line.Reason, line.Agent = LineChanged, ReasonStatus, *a
	case storage.AgentRemoved:
		run, err := s.db.GetRun(ctx, ev.Run)
		if err != nil || run == nil {
			return nil, false
		}
		a := t.fromRun(s, run)
		if prev := s.live[ev.Agent]; prev != nil {
			a.Event, a.Detail, a.Tool, a.StatusAt = prev.Event, prev.Detail, prev.Tool, prev.StatusAt
			if prev.SessionID != "" {
				a.SessionID = prev.SessionID
			}
		}
		delete(s.live, ev.Agent)
		if t.all {
			s.ended = append(s.ended, a)
		}
		line.Type, line.Agent = LineRemoved, a
	default:
		return nil, false
	}
	return line, true
}

// read reads s whole.
func (t *Tree) read(ctx context.Context, s *store) error {
	if s.db == nil {
		return fmt.Errorf("opening %s: %s", s.dir, s.err)
	}
	if v, err := s.db.DataVersion(ctx); err == nil {
		s.version = v
	}
	tree, err := s.db.ReadTree(ctx, t.all)
	if err != nil {
		s.err = err.Error()
		return fmt.Errorf("reading %s: %w", s.dir, err)
	}
	s.err, s.cursor, s.events = "", tree.Cursor, tree.Events
	s.live, s.ended = map[string]*Agent{}, nil

	state := map[string]map[string]*storage.StateEntry{}
	for _, e := range tree.State {
		if state[e.AgentName] == nil {
			state[e.AgentName] = map[string]*storage.StateEntry{}
		}
		state[e.AgentName][e.Key] = e
	}
	for _, rec := range tree.Agents {
		a := Agent{Name: rec.Name, Host: t.d.Host, Kind: KindTmux, Parent: rec.SpawnedBy, Backend: rec.Backend,
			Session: rec.SessionName, Store: s.dir, Cwd: rec.Cwd, CreatedAt: rec.CreatedAt.UTC(), Live: true}
		if run := tree.Runs[rec.Name]; run != nil {
			a.Run = run.ID
			if a.Cwd == "" {
				a.Cwd = run.Cwd
			}
		}
		attached := t.sessions[a.Session]
		a.Attached = &attached
		keys := state[rec.Name]
		value := func(k string) string {
			if e := keys[k]; e != nil {
				return e.Value
			}
			return ""
		}
		if e := keys["_status"]; e != nil {
			a.Status, a.StatusAt = e.Value, e.UpdatedAt.UTC()
		}
		a.Event, a.Detail, a.Tool, a.ToolInput = value("_event"), value("_detail"), value("_tool"), value("_tool_input")
		if a.Status != "waiting" {
			a.ToolInput = "" // as in events: only a pending prompt's
		}
		a.SessionID, a.Transcript = value("_session_id"), value("_transcript")
		a.Dialect = dialect(a.Backend, a.Transcript)
		statTranscript(&a)
		s.live[a.Name] = &a
	}
	for _, run := range tree.Ended {
		s.ended = append(s.ended, t.fromRun(s, run))
	}
	return nil
}

// fromRun describes a run: departed if it ended, otherwise without the
// live agent's status.
func (t *Tree) fromRun(s *store, run *storage.Run) Agent {
	a := Agent{Name: run.Name, Host: t.d.Host, Kind: KindTmux, Run: run.ID, Parent: run.Parent, Backend: run.Backend,
		Session: run.Session, Store: s.dir, Cwd: run.Cwd, CreatedAt: run.CreatedAt.UTC(), EndedAt: run.EndedAt.UTC(),
		Status: run.Status, Transcript: run.Transcript}
	if !run.EndedAt.IsZero() {
		attached := false
		a.Attached = &attached
	}
	if a.Transcript == "" && run.ID == 0 {
		// Spawned before runs: the transcripts list beside its log has
		// the newest one.
		a.Transcript = lastTranscript(filepath.Join(s.dir, "logs", run.Name+".transcripts"))
	}
	// Runs don't record the session id; the transcript's path names it.
	a.SessionID = transcript.SessionIDFromPath(a.Transcript)
	a.Dialect = dialect(a.Backend, a.Transcript)
	statTranscript(&a)
	return a
}

// refreshSessions re-lists the tmux sessions. A failure (no tmux server)
// is no sessions.
func (t *Tree) refreshSessions() {
	t.sessions = map[string]bool{}
	if t.d.Sessions == nil {
		return
	}
	names, err := t.d.Sessions()
	if err != nil {
		return
	}
	for _, n := range names {
		t.sessions[n] = true
	}
}

// findStores looks at each agent session no store records, adds and reads
// the store its binary uses, if that is new, and updates the unrecorded
// list. It returns the stores it added.
func (t *Tree) findStores(ctx context.Context) []*store {
	var added []*store
	var unrecorded []Session
	for _, sess := range sortedKeys(t.sessions) {
		if !strings.HasPrefix(sess, t.d.SessionPrefix) || t.recorded(sess) {
			continue
		}
		dir, asked := t.sessStore[sess]
		if !asked && t.d.SessionStore != nil {
			dir = t.d.SessionStore(sess)
			t.sessStore[sess] = dir
		}
		if dir != "" {
			if s, isNew := t.addStore(dir, false); isNew {
				if t.read(ctx, s) == nil {
					added = append(added, s)
				}
				if t.recorded(sess) {
					continue
				}
			}
		}
		unrecorded = append(unrecorded, Session{Session: sess, Store: dir})
	}
	for sess := range t.sessStore {
		if !t.sessions[sess] {
			delete(t.sessStore, sess) // a later session of that name may be another binary's
		}
	}
	t.unrecorded = unrecorded
	return added
}

func (t *Tree) recorded(session string) bool {
	for _, s := range t.stores {
		for _, a := range s.live {
			if a.Session == session {
				return true
			}
		}
	}
	return false
}

// readHost lists host's agents over the bus.
func (t *Tree) readHost(ctx context.Context, host string) {
	if t.d.ListHost == nil {
		t.remote[host] = nil
		return
	}
	infos, err := t.d.ListHost(ctx, host)
	if err != nil {
		t.remote[host] = nil
		t.setHostErr(host, err.Error())
		return
	}
	t.setHostErr(host, "")
	agents := make([]Agent, 0, len(infos))
	for _, in := range infos {
		agents = append(agents, Agent{Name: in.Name, Host: host, Kind: KindTmux, Parent: in.SpawnedBy, Backend: in.Backend,
			Session: in.Session, CreatedAt: in.CreatedAt.UTC(), Live: true, Status: in.Status})
	}
	sortAgents(agents)
	t.remote[host] = agents
}

func (t *Tree) setHostErr(host, msg string) { t.hostErrs[host] = msg }

func (t *Tree) hostErr(host string) string { return t.hostErrs[host] }

func (t *Tree) hostInfo(host string) Host {
	return Host{Host: host, Agents: len(t.remote[host]), Error: t.hostErr(host)}
}

func (s *store) info() Store {
	return Store{Dir: s.dir, This: s.this, Cursor: s.cursor, Events: s.events, Error: s.err}
}

// agents returns s's agents, live and departed, in tree order.
func (s *store) agents() []Agent {
	out := make([]Agent, 0, len(s.live)+len(s.ended))
	for _, a := range s.live {
		out = append(out, *a)
	}
	out = append(out, s.ended...)
	sortAgents(out)
	return out
}

// sortAgents orders agents by creation, then name, host, and run.
func sortAgents(agents []Agent) {
	sort.SliceStable(agents, func(i, j int) bool {
		a, b := agents[i], agents[j]
		switch {
		case !a.CreatedAt.Equal(b.CreatedAt):
			return a.CreatedAt.Before(b.CreatedAt)
		case a.Name != b.Name:
			return a.Name < b.Name
		case a.Host != b.Host:
			return a.Host < b.Host
		}
		return a.Run < b.Run
	})
}

func sameAgents(a, b []Agent) bool {
	return slices.EqualFunc(a, b, func(x, y Agent) bool {
		xa, ya := x.Attached, y.Attached
		x.Attached, y.Attached = nil, nil
		return x == y && (xa == nil) == (ya == nil) && (xa == nil || *xa == *ya)
	})
}

// statTranscript fills in the transcript file's size and time, or clears
// them when there is none.
func statTranscript(a *Agent) {
	a.TranscriptSize, a.TranscriptMtime = 0, time.Time{}
	if a.Transcript == "" {
		return
	}
	if info, err := os.Stat(a.Transcript); err == nil {
		a.TranscriptSize, a.TranscriptMtime = info.Size(), info.ModTime().UTC()
	}
}

// dialect names the format a transcript is in.
func dialect(backend, transcript string) string {
	switch {
	case transcript == "":
		return ""
	case backend == "claude" || strings.Contains(transcript, "/.claude/projects/"):
		return "claude"
	case backend == "agy" || strings.Contains(transcript, "/antigravity-cli/brain/"):
		return "agy"
	}
	return ""
}

// lastTranscript returns the newest path in a transcripts list
// ("<backend>\t<path>" lines), or "".
func lastTranscript(listPath string) string {
	f, err := os.Open(listPath)
	if err != nil {
		return ""
	}
	defer f.Close()
	var last string
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		if _, p, ok := strings.Cut(sc.Text(), "\t"); ok && p != "" {
			last = p
		}
	}
	return last
}

func sortedNames(m map[string]*Agent) []string {
	names := make([]string, 0, len(m))
	for n := range m {
		names = append(names, n)
	}
	sort.Strings(names)
	return names
}

func sortedKeys(m map[string]bool) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func nonNil[T any](s []T) []T {
	if s == nil {
		return []T{}
	}
	return s
}

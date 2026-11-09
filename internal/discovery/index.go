package discovery

import (
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode"

	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// Index is every session the scanned dirs hold and the workgroup agents
// name, as directory listings and stat say: no transcript is read until a
// caller asks for the sessions it picked (Resolve), or a filter needs what
// only a transcript says (a Claude session's cwd, for a project). With
// thousands of transcripts, picking the newest few, or one by id or path,
// costs a listing and a read of each one picked.
type Index struct {
	j joined // sessions newest first (sortSessions); stubs until filled
	// byKey maps a transcript's path (pathKey) to its session; byID a
	// harness and session id to its newest session with a transcript.
	byKey map[string]int
	byID  map[string]int
}

// NewIndex lists the sessions in dirs and joins them to agents, as Resolve
// does, reading no transcript.
func NewIndex(dirs []Dir, agents []Agent) *Index {
	x := &Index{j: join(list(dirs), slices.Clone(agents))}
	sortSessions(x.j.sessions)
	x.reindex()
	return x
}

func (x *Index) reindex() {
	x.byKey, x.byID = map[string]int{}, map[string]int{}
	for i := range x.j.sessions {
		s := &x.j.sessions[i]
		if s.Path == "" {
			continue
		}
		x.byKey[pathKey(s)] = i
		if _, ok := x.byID[idKey(s.Harness, s.SessionID)]; !ok {
			x.byID[idKey(s.Harness, s.SessionID)] = i
		}
	}
}

func idKey(harness, id string) string { return harness + "\x00" + id }

// Sessions returns every session, newest first, as listed: the id,
// harness, path, size and mtime are set, a subagent's parent where its path
// or index names it, and the rest only once read. Resolve reads and
// resolves the ones a caller picks.
func (x *Index) Sessions() []Session {
	return slices.Clone(x.j.sessions)
}

// Harnesses names every harness a session has: Harnesses, and the
// backend of a workgroup agent listed with no transcript (aider, say).
func (x *Index) Harnesses() []string {
	out := slices.Clone(Harnesses)
	for _, s := range x.j.sessions {
		if !slices.Contains(out, s.Harness) {
			out = append(out, s.Harness)
		}
	}
	return out
}

// Lookup returns the session of the transcript at path: the listed one,
// or, for a transcript outside the scanned dirs, one added to the index.
// ok is false if no session lists path and there is no file there.
func (x *Index) Lookup(path string) (Session, bool) {
	if i, ok := x.byKey[canonical(path)]; ok {
		return x.j.sessions[i], true
	}
	s, ok := stubAt(path)
	if !ok {
		return Session{}, false
	}
	x.j.sessions = append(x.j.sessions, s)
	sortSessions(x.j.sessions)
	x.reindex()
	return s, true
}

// Resolve reads the picked sessions (from Sessions, Select, Lookup or
// Continue) and the sessions above each subagent among them, and sets
// their workgroup and status by Resolve's rules. It returns them newest
// first, ancestors included.
func (x *Index) Resolve(picked []Session, live Live) []Session {
	var idx []int
	seen := map[int]bool{}
	var out []Session
	for _, p := range picked {
		if p.Path == "" {
			out = append(out, p) // a live agent with no transcript: resolved when joined
			continue
		}
		i, ok := x.byKey[pathKey(&p)]
		for ok && !seen[i] {
			seen[i] = true
			idx = append(idx, i)
			parent := x.j.sessions[i].ParentSession
			if parent == nil {
				break
			}
			i, ok = x.byID[idKey(x.j.sessions[i].Harness, parent.SessionID)]
		}
	}
	slices.Sort(idx)
	for _, i := range idx {
		x.j.sessions[i].fill()
		out = append(out, x.j.sessions[i])
	}
	x.j.status(out, live)
	sortSessions(out)
	return out
}

// Query is what Select picks: sessions that match every filter set, by
// their group (a session and the subagents under it, which go with it and
// are matched as it is). Groups are ordered by their top session's last
// write, newest first; a live agent's session with no transcript yet counts
// as written now.
type Query struct {
	Harness string
	// Since keeps sessions written at or after it.
	Since time.Time
	// Project keeps sessions whose cwd matches it: an absolute directory
	// matches a cwd that is it or inside it (symlinks resolved); anything
	// else, a project name (the cwd's last part, as listed) containing it,
	// ignoring case. A session that recorded no cwd matches neither.
	Project string
	// Workgroup keeps the sessions a workgroup agent owns; Local the rest.
	Workgroup, Local bool
	// Limit keeps the newest Limit groups; 0 keeps every one.
	Limit int
}

// Select returns the sessions q picks, newest group first, unread (pass
// them to Resolve), and how many more groups matched but were left out
// under q.Limit. It reads only the transcripts a project filter needs: a
// Claude session's whose project folder (a slug of the cwd it started in)
// could match.
func (x *Index) Select(q Query) (picked []Session, left int) {
	groups := map[int][]int{} // top session -> its members, newest first
	var tops []int
	for i := range x.j.sessions {
		t := x.top(i)
		if _, ok := groups[t]; !ok {
			tops = append(tops, t)
		}
		groups[t] = append(groups[t], i)
	}
	// By the top session's last write; one with no transcript first.
	slices.SortStableFunc(tops, func(a, b int) int {
		ua, ub := x.j.sessions[a].Path == "", x.j.sessions[b].Path == ""
		switch {
		case ua && !ub:
			return -1
		case ub && !ua:
			return 1
		}
		return a - b
	})
	project := newProjectMatch(q.Project)
	shown := 0
	for _, t := range tops {
		if !x.keep(t, q, project) {
			continue
		}
		if q.Limit > 0 && shown >= q.Limit {
			left++
			continue
		}
		shown++
		for _, i := range groups[t] {
			picked = append(picked, x.j.sessions[i])
		}
	}
	return picked, left
}

// top returns the session at the top of i's chain of parents, among the
// index's sessions.
func (x *Index) top(i int) int {
	seen := map[int]bool{i: true}
	for {
		s := &x.j.sessions[i]
		if s.ParentSession == nil {
			return i
		}
		p, ok := x.byID[idKey(s.Harness, s.ParentSession.SessionID)]
		if !ok || seen[p] {
			return i
		}
		seen[p] = true
		i = p
	}
}

// keep reports whether the group under session i matches q.
func (x *Index) keep(i int, q Query, project projectMatch) bool {
	s := &x.j.sessions[i]
	switch {
	case q.Harness != "" && s.Harness != q.Harness:
		return false
	case !q.Since.IsZero() && s.Path != "" && s.Mtime.Before(q.Since):
		return false
	case q.Workgroup && !x.owned(i), q.Local && x.owned(i):
		return false
	case project.set():
		if !s.probed && !project.mayMatch(s.folder) {
			return false
		}
		s.fill()
		return project.cwd(s.Cwd)
	}
	return true
}

// owned reports whether a workgroup agent owns session i, as status would
// set it: the run its transcript belongs to.
func (x *Index) owned(i int) bool {
	s := &x.j.sessions[i]
	if s.Path == "" {
		return s.Workgroup != nil
	}
	return x.j.owner[pathKey(s)] != nil
}

// Continue returns the session `<harness> --continue` would pick in dir,
// by Continue's rules, reading only the Claude transcripts whose project
// folder could be dir's, newest first, until one ran in dir.
func (x *Index) Continue(harness, dir string) (Session, bool) {
	project := newProjectMatch(dir)
	exact := canonical(dir)
	var candidates []Session
	for i := range x.j.sessions {
		s := &x.j.sessions[i]
		if s.Harness != harness || s.ParentSession != nil || s.Path == "" {
			continue
		}
		if !s.probed {
			if !project.mayMatch(s.folder) {
				continue
			}
			s.fill()
		}
		candidates = append(candidates, *s)
		// Newest first, so the first that ran in dir is Claude's pick.
		if harness == HarnessClaude && s.Cwd != "" && canonical(s.Cwd) == exact {
			break
		}
	}
	return Continue(candidates, harness, dir)
}

// projectMatch is a Query's Project.
type projectMatch struct {
	dirs []string // an absolute directory, as given and canonical
	name string   // else a part of the project name, lower case
	// runs are the letter and digit runs of each of dirs, or of name: the
	// characters a Claude project folder's name keeps from the cwd.
	runs [][]string
}

func newProjectMatch(q string) projectMatch {
	var m projectMatch
	switch {
	case q == "":
		return m
	case strings.HasPrefix(q, "/"):
		m.dirs = []string{filepath.Clean(q)}
		if c := canonical(q); c != m.dirs[0] {
			m.dirs = append(m.dirs, c)
		}
		for _, d := range m.dirs {
			m.runs = append(m.runs, alnumRuns(d))
		}
	default:
		m.name = strings.ToLower(q)
		m.runs = [][]string{alnumRuns(m.name)}
	}
	return m
}

func (m projectMatch) set() bool { return m.dirs != nil || m.name != "" }

// cwd reports whether a session that recorded cwd matches.
func (m projectMatch) cwd(cwd string) bool {
	cwd = strings.TrimPrefix(cwd, "file://")
	if cwd == "" {
		return false
	}
	if m.name != "" {
		return strings.Contains(strings.ToLower(transcript.ProjectOf(cwd)), m.name)
	}
	for _, c := range []string{filepath.Clean(cwd), canonical(cwd)} {
		for _, d := range m.dirs {
			if c == d || within(c, d) {
				return true
			}
		}
	}
	return false
}

// mayMatch reports whether a Claude session listed in the project folder
// named folder could match, before its transcript is read. Claude Code
// names the folder for the cwd the session started in, every character but
// a letter or digit replaced by '-', so the letters and digits of a match
// are all in the name. A name that isn't such a slug (or is long enough
// to have been shortened) could be anything, and so could a transcript
// listed outside a project folder ("").
func (m projectMatch) mayMatch(folder string) bool {
	if folder == "" || len(folder) >= 200 || strings.ContainsFunc(folder, func(r rune) bool {
		return r != '-' && !isAlnum(r)
	}) {
		return true
	}
	lower := strings.ToLower(folder)
	for _, runs := range m.runs {
		all := true
		for _, r := range runs {
			if !strings.Contains(lower, strings.ToLower(r)) {
				all = false
				break
			}
		}
		if all {
			return true
		}
	}
	return false
}

// alnumRuns splits s into its runs of ASCII letters and digits.
func alnumRuns(s string) []string {
	return strings.FieldsFunc(s, func(r rune) bool { return !isAlnum(r) })
}

func isAlnum(r rune) bool {
	return r < unicode.MaxASCII && (unicode.IsLetter(r) || unicode.IsDigit(r))
}

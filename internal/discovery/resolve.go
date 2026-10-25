package discovery

import (
	"path/filepath"
	"sort"
	"time"
)

// Statuses a session can have: an agent's (internal/backend/types), and
// exited for one that is over.
const (
	StatusIdle       = "idle"
	StatusProcessing = "processing"
	StatusWaiting    = "waiting"
	StatusError      = "error"
	StatusExited     = "exited"
	StatusUnknown    = "unknown"
)

// RecentWindow is how recently a session with no other sign of life must
// have written its transcript to count as possibly running (unknown)
// rather than exited.
const RecentWindow = 10 * time.Minute

// Agent is one run of a gomuxai workgroup agent, live or departed, as the
// join needs it.
type Agent struct {
	Name      string
	Parent    string
	Store     string
	Host      string
	Backend   string
	Live      bool // it has a record (not cleaned up)
	CreatedAt time.Time
	EndedAt   time.Time
	// Status and StatusAt are its last report.
	Status   string
	StatusAt time.Time
	// SessionID and Transcript are its current session (live), or its last
	// transcript (departed).
	SessionID  string
	Transcript string
	// History is every transcript its name recorded, oldest first: the
	// list beside its log, which spans /clear, respawns and cleanup.
	History []string
}

// Live is what the harnesses say is running on this host now.
type Live struct {
	// Claude maps the id of every Claude Code session running on this host
	// to its status, from `claude agents --json`; nil if that couldn't be
	// asked.
	Claude map[string]string
	Now    time.Time
}

// Resolve joins sessions to the workgroup agents whose transcripts they
// are, adds those agents' transcripts that sessions lack (outside the
// scanned dirs) and live agents with no transcript yet, and sets every
// session's status. It returns the sessions newest first.
//
// A transcript belongs to the run that records it as its current or last
// one; one only an agent's history names belongs to that name's latest run
// created before the transcript was last written. A subagent's session
// belongs to its parent session's agent.
//
// Status:
//   - a live agent's current session: its last report, corrected by
//     `claude agents --json` for Claude (its status when it lists the
//     session; exited when it doesn't), and by agy's presence lock;
//   - any other session of a workgroup agent: exited;
//   - Claude, no workgroup: `claude agents --json`'s status, or exited if
//     it doesn't list the session (unknown if it couldn't be asked);
//   - agy, no workgroup: unknown while agy holds the conversation's presence
//     lock (running; idle or busy can't be told), exited when not;
//   - Kimi (and agy without presence files): unknown if the transcript
//     changed within RecentWindow, else exited;
//   - a subagent: exited if it recorded finishing, agy says it isn't open,
//     or its parent session is exited; else unknown.
func Resolve(sessions []Session, agents []Agent, live Live) []Session {
	if live.Now.IsZero() {
		live.Now = time.Now()
	}
	byPath := map[string]int{}
	for i, s := range sessions {
		byPath[filepath.Clean(s.Path)] = i
	}
	owner := owners(agents, func(path string) (time.Time, bool) {
		if i, ok := byPath[path]; ok {
			return sessions[i].Mtime, true
		}
		return time.Time{}, false
	})
	for _, path := range sortedPaths(owner) {
		if _, ok := byPath[path]; ok {
			continue
		}
		if s, ok := Probe(path); ok {
			byPath[path] = len(sessions)
			sessions = append(sessions, s)
		}
	}
	for i := range agents {
		a := &agents[i]
		if !a.Live {
			continue
		}
		if _, ok := byPath[filepath.Clean(a.Transcript)]; ok && a.Transcript != "" {
			continue
		}
		// No transcript yet (a Claude session before its first prompt),
		// or a backend that keeps none.
		s := Session{SessionID: a.SessionID, Harness: a.Backend, Path: a.Transcript}
		if a.Transcript != "" {
			s.SessionID = firstNonEmpty(a.SessionID, SessionIDFromPath(a.Transcript))
		} else {
			s.Workgroup, s.Status, s.StatusAt = workgroupOf(a), reported(a), a.StatusAt
		}
		byPath[filepath.Clean(a.Transcript)] = len(sessions)
		sessions = append(sessions, s)
	}

	// Main sessions first: a subagent's status and workgroup follow its
	// parent's.
	byID := map[string]*Session{}
	for i := range sessions {
		s := &sessions[i]
		if s.ParentSession != nil || (s.Path == "" && s.Workgroup != nil) {
			continue
		}
		if a := owner[filepath.Clean(s.Path)]; a != nil {
			s.Workgroup = workgroupOf(a)
			ownedStatus(s, a, live)
		} else {
			localStatus(s, live)
		}
		byID[s.Harness+"\x00"+s.SessionID] = s
	}
	for i := range sessions {
		s := &sessions[i]
		if s.ParentSession == nil {
			continue
		}
		parent := byID[s.Harness+"\x00"+s.ParentSession.SessionID]
		if a := owner[filepath.Clean(s.Path)]; a != nil {
			s.Workgroup = workgroupOf(a)
		} else if parent != nil {
			s.Workgroup = parent.Workgroup
		}
		subagentStatus(s, parent, live)
	}
	sortSessions(sessions)
	return sessions
}

// owners maps each transcript path the agents name to the run it belongs
// to. mtime gives a transcript's last write, if it is known.
func owners(agents []Agent, mtime func(path string) (time.Time, bool)) map[string]*Agent {
	owner := map[string]*Agent{}
	runs := map[string][]*Agent{} // by store and name, oldest first
	for i := range agents {
		a := &agents[i]
		key := a.Store + "\x00" + a.Name
		runs[key] = append(runs[key], a)
	}
	for _, rs := range runs {
		sort.SliceStable(rs, func(i, j int) bool { return rs[i].CreatedAt.Before(rs[j].CreatedAt) })
		for _, a := range rs {
			for _, p := range a.History {
				p = filepath.Clean(p)
				if owner[p] != nil {
					continue
				}
				owner[p] = rs[len(rs)-1]
				if at, ok := mtime(p); ok {
					owner[p] = rs[0]
					for _, r := range rs {
						if !r.CreatedAt.After(at) {
							owner[p] = r
						}
					}
				}
			}
		}
		for _, a := range rs {
			if a.Transcript != "" {
				owner[filepath.Clean(a.Transcript)] = a
			}
		}
	}
	return owner
}

func workgroupOf(a *Agent) *Workgroup {
	return &Workgroup{Name: a.Name, Parent: a.Parent, Store: a.Store, Host: a.Host}
}

// reported is a live agent's last report, or unknown before its first.
func reported(a *Agent) string {
	return firstNonEmpty(a.Status, StatusUnknown)
}

// ownedStatus sets the status of a session a workgroup agent owns.
func ownedStatus(s *Session, a *Agent, live Live) {
	if !a.Live || filepath.Clean(a.Transcript) != filepath.Clean(s.Path) {
		s.Status, s.StatusAt = StatusExited, s.Mtime
		if !a.Live && filepath.Clean(a.Transcript) == filepath.Clean(s.Path) && !a.EndedAt.IsZero() {
			s.StatusAt = a.EndedAt
		}
		return
	}
	s.Status, s.StatusAt = reported(a), a.StatusAt
	switch {
	case s.Harness == HarnessClaude && live.Claude != nil:
		if st, ok := live.Claude[s.SessionID]; ok {
			if st != s.Status {
				s.Status, s.StatusAt = st, s.Mtime
			}
		} else if s.Status != StatusExited {
			s.Status, s.StatusAt = StatusExited, s.Mtime
		}
	case s.live == liveNo && s.Status != StatusExited:
		s.Status, s.StatusAt = StatusExited, s.Mtime
	}
}

// localStatus sets the status of a session no workgroup owns, from what the
// harness says.
func localStatus(s *Session, live Live) {
	s.StatusAt = s.Mtime
	switch {
	case s.Harness == HarnessClaude && live.Claude == nil:
		s.Status = StatusUnknown
	case s.Harness == HarnessClaude:
		s.Status = StatusExited
		if st, ok := live.Claude[s.SessionID]; ok {
			s.Status = st
		}
	case s.live == liveYes:
		s.Status = StatusUnknown
	case s.live == liveNo:
		s.Status = StatusExited
	default:
		s.Status = recent(s, live)
	}
}

// subagentStatus sets a subagent session's status from its own record and
// its parent's.
func subagentStatus(s *Session, parent *Session, live Live) {
	s.StatusAt = s.Mtime
	switch {
	case s.state == subFinished, s.live == liveNo:
		s.Status = StatusExited
	case parent != nil && parent.Status == StatusExited:
		s.Status = StatusExited
	case s.live == liveYes, parent != nil:
		s.Status = StatusUnknown
	default:
		s.Status = recent(s, live)
	}
}

// recent is unknown for a transcript written within RecentWindow, else
// exited.
func recent(s *Session, live Live) string {
	if live.Now.Sub(s.Mtime) < RecentWindow {
		return StatusUnknown
	}
	return StatusExited
}

func sortedPaths(m map[string]*Agent) []string {
	paths := make([]string, 0, len(m))
	for p := range m {
		paths = append(paths, p)
	}
	sort.Strings(paths)
	return paths
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

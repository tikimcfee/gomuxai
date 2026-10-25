package discovery

import (
	"path/filepath"
	"slices"
	"sort"
	"time"

	"github.com/tikimcfee/gomuxai/internal/transcript"
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

// Native is what a backend CLI's own session list (claude agents --json)
// says about one session.
type Native struct {
	Queried bool   // the CLI answered
	Listed  bool   // it lists the session
	Status  string // its status there; StatusUnknown for a state with no mapping
}

// How ReconcileNative decides.
const (
	NativeWins   = iota // the CLI's status
	NativeGone          // the CLI doesn't list a session the report says is alive: it exited without a SessionEnd hook
	ReportStands        // the report: the CLI wasn't asked or didn't answer, or lists the session in a state with no mapping (alive, so not a crash)
	ReportDenied        // the report, over the CLI's waiting just after a deny (which fires no hook): the CLI catching up
)

// DeniedLag bounds how long after a deny native status may still say
// waiting before it is believed again. Observed: under a second.
const DeniedLag = 5 * time.Second

// ReconcileNative is the one rule for a session that has both a hook report
// and a place in its CLI's own session list, used by agent status and agent
// sessions alike. report is the last report's status ("" if none); denied
// says whether that report is a deny within DeniedLag with no prompt left
// (asked only when the CLI says waiting).
func ReconcileNative(n Native, report string, denied func() bool) int {
	switch {
	case n.Listed && n.Status == StatusUnknown:
		return ReportStands
	case n.Listed && n.Status == StatusWaiting && denied():
		return ReportDenied
	case n.Listed:
		return NativeWins
	case n.Queried && report != "" && report != StatusExited:
		return NativeGone
	}
	return ReportStands
}

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
	// Status, Event and StatusAt are its last report.
	Status   string
	Event    string
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
//   - a live agent's current session: its last report, reconciled with
//     `claude agents --json` for Claude by ReconcileNative (the rule agent
//     status uses), and exited when agy no longer holds it open;
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
	// Paths are compared resolved, so a symlinked ~/.claude (or two
	// spellings of one dir) is one session, not an owned and a local one.
	byPath := map[string]int{}
	for i, s := range sessions {
		byPath[canonical(s.Path)] = i
	}
	// Every transcript an agent names is read first, so the join below
	// knows each one's last write wherever it lives.
	for i := range agents {
		for _, p := range append(slices.Clone(agents[i].History), agents[i].Transcript) {
			key := canonical(p)
			if _, ok := byPath[key]; ok || p == "" {
				continue
			}
			if s, ok := Probe(p); ok {
				byPath[key] = len(sessions)
				sessions = append(sessions, s)
			}
		}
	}
	owner := owners(agents, func(key string) (time.Time, bool) {
		if i, ok := byPath[key]; ok {
			return sessions[i].Mtime, true
		}
		return time.Time{}, false
	})
	for i := range agents {
		a := &agents[i]
		if !a.Live {
			continue
		}
		if _, ok := byPath[canonical(a.Transcript)]; ok && a.Transcript != "" {
			continue
		}
		// No transcript yet (a Claude session before its first prompt),
		// or a backend that keeps none.
		s := Session{SessionID: a.SessionID, Harness: a.Backend, Path: a.Transcript}
		if a.Transcript != "" {
			s.SessionID = firstNonEmpty(a.SessionID, transcript.SessionIDFromPath(a.Transcript))
		} else {
			s.Workgroup, s.Status, s.StatusAt = workgroupOf(a), reported(a), a.StatusAt
		}
		sessions = append(sessions, s)
	}

	// Main sessions first, then each subagent once its parent is done: a
	// subagent's status and workgroup follow its parent's, at any depth.
	byID := map[string]*Session{}
	done := make([]bool, len(sessions))
	for i := range sessions {
		s := &sessions[i]
		if s.ParentSession != nil {
			continue
		}
		done[i] = true
		if s.Path == "" && s.Workgroup != nil {
			continue
		}
		if a := owner[canonical(s.Path)]; a != nil {
			s.Workgroup = workgroupOf(a)
			ownedStatus(s, a, live)
		} else {
			localStatus(s, live)
		}
		byID[s.Harness+"\x00"+s.SessionID] = s
	}
	for progress, waitForParent := true, true; ; {
		if !progress {
			if !waitForParent {
				break
			}
			waitForParent = false // a cycle, or a parent that isn't here: go on without it
		}
		progress = false
		for i := range sessions {
			s := &sessions[i]
			if done[i] {
				continue
			}
			key := s.Harness + "\x00" + s.ParentSession.SessionID
			parent := byID[key]
			if parent == nil && waitForParent && hasSession(sessions, done, key) {
				continue
			}
			if a := owner[canonical(s.Path)]; a != nil {
				s.Workgroup = workgroupOf(a)
			} else if parent != nil {
				s.Workgroup = parent.Workgroup
			}
			subagentStatus(s, parent, live)
			byID[s.Harness+"\x00"+s.SessionID] = s
			done[i], progress, waitForParent = true, true, true
		}
	}
	sortSessions(sessions)
	return sessions
}

// hasSession reports whether a session not yet done has the harness and id
// in key.
func hasSession(sessions []Session, done []bool, key string) bool {
	for i, s := range sessions {
		if !done[i] && s.Harness+"\x00"+s.SessionID == key {
			return true
		}
	}
	return false
}

// canonical is the key a path is joined on: with symlinks resolved, or
// just cleaned if it can't be (it doesn't exist).
func canonical(path string) string {
	if path == "" {
		return ""
	}
	if resolved, err := filepath.EvalSymlinks(path); err == nil {
		return resolved
	}
	return filepath.Clean(path)
}

// owners maps each transcript path the agents name (canonical) to the run
// it belongs to. mtime gives a transcript's last write, if it is known.
func owners(agents []Agent, mtime func(key string) (time.Time, bool)) map[string]*Agent {
	owner := map[string]*Agent{}
	runs := map[string][]*Agent{} // by store and name, oldest first
	for i := range agents {
		a := &agents[i]
		key := canonical(a.Store) + "\x00" + a.Name
		runs[key] = append(runs[key], a)
	}
	keys := make([]string, 0, len(runs))
	for k := range runs {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		rs := runs[k]
		sort.SliceStable(rs, func(i, j int) bool { return rs[i].CreatedAt.Before(rs[j].CreatedAt) })
		for _, a := range rs {
			for _, p := range a.History {
				p = canonical(p)
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
				owner[canonical(a.Transcript)] = a
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
	current := canonical(a.Transcript) == canonical(s.Path)
	if !a.Live || !current {
		s.Status, s.StatusAt = StatusExited, s.Mtime
		if !a.Live && current && !a.EndedAt.IsZero() {
			s.StatusAt = a.EndedAt
		}
		return
	}
	s.Status, s.StatusAt = reported(a), a.StatusAt
	switch {
	case s.Harness == HarnessClaude:
		st, listed := live.Claude[s.SessionID]
		n := Native{Queried: live.Claude != nil, Listed: listed, Status: st}
		denied := func() bool { return a.Event == "denied" && live.Now.Sub(a.StatusAt) < DeniedLag }
		switch ReconcileNative(n, a.Status, denied) {
		case NativeWins:
			if st != s.Status {
				s.Status, s.StatusAt = st, s.Mtime
			}
		case NativeGone:
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

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

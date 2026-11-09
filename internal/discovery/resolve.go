package discovery

import (
	"path/filepath"
	"slices"
	"sort"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// Statuses a session can have: an agent's (internal/backend/types, which
// also lists what each means), ended for one that is over, beside an end
// reason.
const (
	StatusIdle       = "idle"
	StatusProcessing = "processing"
	StatusWaiting    = "waiting"
	StatusError      = "error"
	StatusEnded      = "ended"
	StatusUnknown    = "unknown"
)

// Native is what a backend CLI's own record of its sessions (Claude Code's session registry)
// says about one session.
type Native struct {
	Queried bool   // the CLI answered
	Listed  bool   // it lists the session
	Status  string // its status there; StatusUnknown for a state with no mapping
	// PaneErr says why the agent's pane couldn't be read, when the session
	// isn't listed and the pane was asked what it runs instead.
	PaneErr error
	// Err says why the CLI couldn't be asked (Queried false).
	Err error
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
	case n.Queried && report != "" && report != StatusEnded:
		return NativeGone
	}
	return ReportStands
}

// RecentWindow is how recently a session with no other sign of life must
// have written its transcript to count as possibly running (unknown)
// rather than ended (gone).
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
	// Status, EndReason, Event and StatusAt are its last report.
	Status    string
	EndReason string
	Event     string
	StatusAt  time.Time
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
	// to its status, from Claude Code's session registry; nil if that couldn't be
	// asked.
	Claude map[string]string
	// Reports maps a session id to its last report from the global hook
	// (agent hook), which hears every Claude Code session on the host.
	Reports map[string]Report
	Now     time.Time
}

// Report is a session's last report from the global hook.
type Report struct {
	Status, EndReason string
	At                time.Time
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
//     the session registry for Claude by ReconcileNative (the rule agent
//     status uses), and ended (gone) when agy no longer holds it open;
//   - any other session of a workgroup agent: ended (replaced), or (closed)
//     for the last one of a departed agent;
//   - Claude, no workgroup: the session registry's status, or ended (gone)
//     if no running process holds the session (unknown if it couldn't be
//     read);
//   - agy, no workgroup: unknown while agy holds the conversation's presence
//     lock (running; idle or busy can't be told), ended (gone) when not;
//   - Kimi (and agy without presence files): unknown if the transcript
//     changed within RecentWindow, else ended (gone);
//   - a subagent: ended (finished) if it recorded finishing, (gone) if agy
//     says it isn't open, (parent) if its parent session ended; else unknown.
func Resolve(sessions []Session, agents []Agent, live Live) []Session {
	j := join(sessions, agents)
	j.status(j.sessions, live)
	sortSessions(j.sessions)
	return j.sessions
}

// joined is sessions joined to the workgroup agents: the transcripts the
// agents name added, and which run owns each transcript.
type joined struct {
	sessions []Session
	// owner maps a transcript's path (pathKey) to the run it belongs to.
	owner map[string]*Agent
}

// join adds to sessions the transcripts agents name that sessions lack
// (outside the scanned dirs; as stubs, stubAt) and live agents with no
// transcript yet, and works out which run owns each transcript. It reads
// no transcript: what it needs is each one's path and last write. It
// changes agents (a live agent's current transcript, where only its
// session id named it), which owner points into.
func join(sessions []Session, agents []Agent) joined {
	// Paths are compared resolved, so a symlinked ~/.claude (or two
	// spellings of one dir) is one session, not an owned and a local one.
	byPath := map[string]int{}
	for i := range sessions {
		byPath[pathKey(&sessions[i])] = i
	}
	// Every transcript an agent names is listed first, so the join below
	// knows each one's last write wherever it lives.
	for i := range agents {
		for _, p := range append(slices.Clone(agents[i].History), agents[i].Transcript) {
			key := canonical(p)
			if _, ok := byPath[key]; ok || p == "" {
				continue
			}
			if s, ok := stubAt(p); ok {
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
	// A live agent whose current session no hook has named a transcript for
	// (one its pane runs, adopted from the CLI's registry) owns the session
	// the scan found under that id.
	for i := range agents {
		a := &agents[i]
		if !a.Live || a.Transcript != "" || a.SessionID == "" {
			continue
		}
		for j := range sessions {
			if s := &sessions[j]; s.SessionID == a.SessionID && s.Harness == a.Backend && s.ParentSession == nil {
				a.Transcript = s.Path
				owner[pathKey(s)] = a
				break
			}
		}
	}
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
			if s.Status == StatusEnded {
				s.EndReason = firstNonEmpty(a.EndReason, string(types.EndGone))
			}
		}
		sessions = append(sessions, s)
	}
	return joined{sessions: sessions, owner: owner}
}

// status sets the workgroup and status of sessions, which it fills first
// should any be a stub. A subagent's follow its parent's, when its parent
// is among sessions.
func (j joined) status(sessions []Session, live Live) {
	if live.Now.IsZero() {
		live.Now = time.Now()
	}
	owner := j.owner
	// Main sessions first, then each subagent once its parent is done: a
	// subagent's status and workgroup follow its parent's, at any depth.
	byID := map[string]*Session{}
	done := make([]bool, len(sessions))
	for i := range sessions {
		s := &sessions[i]
		s.fill()
		if s.ParentSession != nil {
			continue
		}
		done[i] = true
		if s.Path == "" && s.Workgroup != nil {
			continue
		}
		if a := owner[pathKey(s)]; a != nil {
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
			if a := owner[pathKey(s)]; a != nil {
				s.Workgroup = workgroupOf(a)
			} else if parent != nil {
				s.Workgroup = parent.Workgroup
			}
			subagentStatus(s, parent, live)
			byID[s.Harness+"\x00"+s.SessionID] = s
			done[i], progress, waitForParent = true, true, true
		}
	}
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

// pathKey is the key s's transcript is joined on: its path, canonical.
func pathKey(s *Session) string {
	if s.key != "" {
		return s.key
	}
	return canonical(s.Path)
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
	switch {
	case !a.Live && current:
		ended(s, types.EndClosed, s.Mtime)
		if !a.EndedAt.IsZero() {
			s.StatusAt = a.EndedAt
		}
		// What its last report said, if it ended before cleanup did.
		if a.Status == StatusEnded && a.EndReason != "" {
			s.EndReason = a.EndReason
		}
		return
	case !current:
		ended(s, types.EndReplaced, s.Mtime)
		return
	}
	s.Status, s.StatusAt = reported(a), a.StatusAt
	if s.Status == StatusEnded {
		s.EndReason = firstNonEmpty(a.EndReason, string(types.EndGone))
	}
	switch {
	case s.Harness == HarnessClaude:
		st, listed := live.Claude[s.SessionID]
		n := Native{Queried: live.Claude != nil, Listed: listed, Status: st}
		denied := func() bool { return a.Event == "denied" && live.Now.Sub(a.StatusAt) < DeniedLag }
		switch ReconcileNative(n, a.Status, denied) {
		case NativeWins:
			if st != s.Status {
				s.Status, s.StatusAt, s.EndReason = st, s.Mtime, ""
			}
		case NativeGone:
			ended(s, types.EndGone, s.Mtime)
		}
	case s.live == liveNo && s.Status != StatusEnded:
		ended(s, types.EndGone, s.Mtime)
	}
}

// ended sets s ended, for reason, at at.
func ended(s *Session, reason types.EndReason, at time.Time) {
	s.Status, s.EndReason, s.StatusAt = StatusEnded, string(reason), at
}

// localStatus sets the status of a session no workgroup owns, from what the
// harness says.
func localStatus(s *Session, live Live) {
	s.StatusAt = s.Mtime
	switch {
	case s.Harness == HarnessClaude && live.Claude == nil:
		s.Status = StatusUnknown
	case s.Harness == HarnessClaude:
		if st, ok := live.Claude[s.SessionID]; ok {
			s.Status = st
		} else if r, ok := live.Reports[s.SessionID]; ok && r.Status == StatusEnded && r.EndReason != "" {
			ended(s, types.EndReason(r.EndReason), r.At) // the hook heard how it ended
		} else {
			ended(s, types.EndGone, s.Mtime)
		}
	case s.live == liveYes:
		s.Status = StatusUnknown
	case s.live == liveNo:
		ended(s, types.EndGone, s.Mtime)
	default:
		recent(s, live)
	}
}

// subagentStatus sets a subagent session's status from its own record and
// its parent's.
func subagentStatus(s *Session, parent *Session, live Live) {
	s.StatusAt = s.Mtime
	r, reported := live.Reports[s.SessionID]
	switch {
	case s.state == subFinished, reported && r.Status == StatusEnded && r.EndReason == string(types.EndFinished):
		ended(s, types.EndFinished, s.Mtime)
	case s.live == liveNo:
		ended(s, types.EndGone, s.Mtime)
	case parent != nil && parent.Status == StatusEnded:
		ended(s, types.EndParent, s.Mtime)
	case reported && r.Status == StatusProcessing:
		// The hook saw it start (SubagentStart, its tool calls) and not stop.
		s.Status, s.StatusAt = StatusProcessing, r.At
	case s.live == liveYes, parent != nil:
		s.Status = StatusUnknown
	default:
		recent(s, live)
	}
}

// recent sets unknown for a transcript written within RecentWindow, else
// ended (gone): nothing says it runs, nor how it ended.
func recent(s *Session, live Live) {
	if live.Now.Sub(s.Mtime) < RecentWindow {
		s.Status = StatusUnknown
		return
	}
	ended(s, types.EndGone, s.Mtime)
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

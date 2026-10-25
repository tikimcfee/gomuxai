package discovery

import (
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// Session is one agent session, as docs/EVENTS.md describes it: a
// transcript and what it says about itself, the workgroup agent that owns
// it (nil if none), and its status.
type Session struct {
	SessionID string    `json:"session_id"`
	Harness   string    `json:"harness"`
	Path      string    `json:"path"`
	Size      int64     `json:"size"`
	Mtime     time.Time `json:"mtime,omitzero"`
	// Cwd is the working directory the session recorded; Project is its
	// last component, never the storage folder's name.
	Cwd     string `json:"cwd,omitempty"`
	Project string `json:"project,omitempty"`
	// Title is the latest title the harness recorded.
	Title     string     `json:"title,omitempty"`
	Model     string     `json:"model,omitempty"`
	Version   string     `json:"version,omitempty"`
	GitBranch string     `json:"git_branch,omitempty"`
	Workgroup *Workgroup `json:"workgroup"`
	Status    string     `json:"status"`
	StatusAt  time.Time  `json:"status_at,omitzero"`
	// ParentSession is set on a subagent's session: the session that
	// started it, and the tool call that did, where the harness records it.
	ParentSession *ParentSession `json:"parent_session,omitempty"`
	// ContinuedIn is the session this one continued in (Claude Code's
	// continued-in record).
	ContinuedIn string `json:"continued_in,omitempty"`

	// live is what the harness's own files say about whether the session
	// is still running, for Resolve; liveUnknown if they say nothing.
	live liveness
	// state is a subagent's own record of whether it finished (Claude's
	// meta.json finished, agy's SUBAGENT_STATE_*): finished, running or
	// unrecorded.
	state subState
}

// Workgroup is the gomuxai agent that owns a session.
type Workgroup struct {
	Name   string `json:"name"`
	Parent string `json:"parent,omitempty"` // the agent that spawned it; absent for a person
	Store  string `json:"store"`            // the data dir that records it
	Host   string `json:"host"`
}

// ParentSession links a subagent's session to the one that started it.
type ParentSession struct {
	SessionID string `json:"session_id"`
	ToolID    string `json:"tool_id,omitempty"`
}

type liveness int

const (
	liveUnknown liveness = iota
	liveYes
	liveNo
)

type subState int

const (
	subUnrecorded subState = iota
	subFinished
	subRunning
)

// Scan lists the sessions in dirs, newest first, with what their files say
// about them. Workgroup and Status are left for Resolve. A directory that
// doesn't exist adds nothing; a file that can't be read is left out.
func Scan(dirs []Dir) []Session {
	var out []Session
	for _, d := range dirs {
		if !isDir(d.Path) {
			continue
		}
		switch d.Harness {
		case HarnessClaude:
			out = append(out, scanClaude(d.Path)...)
		case HarnessAgy:
			out = append(out, scanAgy(d.Path)...)
		case HarnessKimi:
			out = append(out, scanKimi(d.Path)...)
		}
	}
	return dedupe(out)
}

// Probe reads the one transcript at path, as Scan would have, for a
// workgroup agent's transcript outside the scanned directories. ok is false
// if there is no such file.
func Probe(path string) (Session, bool) {
	info, err := os.Stat(path)
	if err != nil || info.IsDir() {
		return Session{}, false
	}
	switch HarnessOf(path) {
	case HarnessKimi:
		return probeKimi(path, info), true
	case HarnessAgy:
		// brain/<id>/.system_generated/logs/<transcript>
		brain := filepath.Dir(filepath.Dir(filepath.Dir(filepath.Dir(path))))
		return probeAgy(newAgyIndex(filepath.Dir(brain)), SessionIDFromPath(path), path, info), true
	default:
		return probeClaude(path, info), true
	}
}

// dedupe drops repeated paths (overlapping configured dirs) and sorts the
// rest newest first.
func dedupe(sessions []Session) []Session {
	seen := map[string]bool{}
	out := sessions[:0]
	for _, s := range sessions {
		key := filepath.Clean(s.Path)
		if seen[key] {
			continue
		}
		seen[key] = true
		out = append(out, s)
	}
	sortSessions(out)
	return out
}

// sortSessions orders sessions newest transcript first, then by id.
func sortSessions(sessions []Session) {
	sort.SliceStable(sessions, func(i, j int) bool {
		a, b := sessions[i], sessions[j]
		if !a.Mtime.Equal(b.Mtime) {
			return a.Mtime.After(b.Mtime)
		}
		return a.SessionID < b.SessionID
	})
}

// projectOf is a session's project: the last component of the workspace it
// recorded (a file:// URI too), by one rule for every harness.
func projectOf(cwd string) string {
	p := strings.TrimRight(strings.TrimPrefix(cwd, "file://"), "/")
	if p == "" {
		return ""
	}
	return filepath.Base(p)
}

// fromFile starts a session for the transcript at path.
func fromFile(harness, id, path string, info os.FileInfo) Session {
	return Session{SessionID: id, Harness: harness, Path: path, Size: info.Size(), Mtime: info.ModTime().UTC()}
}

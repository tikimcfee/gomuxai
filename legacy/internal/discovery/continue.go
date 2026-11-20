package discovery

import (
	"path/filepath"
	"strings"
)

// Continue returns the session `<harness> --continue` would pick in dir,
// from sessions (Scan's, or Resolve's), and whether there is one. It goes
// by each CLI's own rule:
//
//   - claude: "the most recent conversation in the current directory": the
//     session whose recorded working directory is dir (Claude Code keeps a
//     project's sessions in one folder named for its directory) whose
//     transcript was written last.
//   - agy: the most recently active conversation whose workspace is dir;
//     if none, agy falls back to the most recent one in a parent or child
//     directory of dir, and so does this.
//
// Both skip subagents' sessions, which aren't conversations of their own,
// and transcripts with nothing in them (Scan never lists those). Whether
// the session can be continued (it may still be running) is the caller's
// to check: the CLI would pick it all the same.
func Continue(sessions []Session, harness, dir string) (Session, bool) {
	dir = canonical(dir)
	var exact, near *Session
	for i := range sessions {
		s := &sessions[i]
		if s.Harness != harness || s.ParentSession != nil || s.Cwd == "" || s.Path == "" {
			continue
		}
		cwd := canonical(s.Cwd)
		switch {
		case cwd == dir:
			if exact == nil || newer(s, exact) {
				exact = s
			}
		case harness == HarnessAgy && (within(cwd, dir) || within(dir, cwd)):
			if near == nil || newer(s, near) {
				near = s
			}
		}
	}
	switch {
	case exact != nil:
		return *exact, true
	case near != nil:
		return *near, true
	}
	return Session{}, false
}

// newer reports whether a was written after b (by id, for a tie, as
// sortSessions orders them).
func newer(a, b *Session) bool {
	if !a.Mtime.Equal(b.Mtime) {
		return a.Mtime.After(b.Mtime)
	}
	return a.SessionID < b.SessionID
}

// within reports whether path is inside dir (not dir itself).
func within(path, dir string) bool {
	rel, err := filepath.Rel(dir, path)
	return err == nil && rel != "." && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) && !filepath.IsAbs(rel)
}

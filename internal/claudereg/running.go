package claudereg

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strconv"
)

// Proven is an entry with its verdict.
type Proven struct {
	Entry
	Verdict Verdict
}

// Snapshot is the registry read at one moment, each entry checked against
// the running processes.
type Snapshot struct {
	Entries  []Proven
	Problems []Problem
}

// Read reads the registry in dir and proves each entry against h.
func Read(dir string, h Host) (*Snapshot, error) {
	entries, problems, err := ReadDir(dir)
	if err != nil {
		return nil, err
	}
	s := &Snapshot{Problems: problems}
	for _, e := range entries {
		s.Entries = append(s.Entries, Proven{Entry: e, Verdict: Proof(e, h)})
	}
	return s, nil
}

// ReadSystem reads this machine's registry.
func ReadSystem() (*Snapshot, error) {
	dir, err := Dir()
	if err != nil {
		return nil, err
	}
	return Read(dir, System{})
}

// Err says which registry files couldn't be read as entries at all (not
// JSON, no pid or session id), or nil. Such a file may be a running
// session's, so a reader that would conclude a session isn't running (it
// isn't listed, or a pane runs nothing) says so instead while there are any.
// (A half-written file reads this way for a moment.)
func (s *Snapshot) Err() error {
	if len(s.Problems) == 0 {
		return nil
	}
	errs := make([]error, len(s.Problems))
	for i, p := range s.Problems {
		errs[i] = p
	}
	return fmt.Errorf("Claude Code's session registry has entries gomuxai can't read, so it won't say what runs: %w", errors.Join(errs...))
}

// running reports whether an entry's process is (or may be) running.
func running(v Verdict) bool { return v == Live || v == Unproven }

// Sessions returns, for each session a running process holds, the entry
// that holds it. When two processes claim one session (a CLI stopped with
// Ctrl-Z and the same session resumed in another), the one updated last
// wins: the stopped one stops updating.
func (s *Snapshot) Sessions() map[string]Proven {
	out := map[string]Proven{}
	for _, e := range s.Entries {
		if !running(e.Verdict) {
			continue
		}
		if cur, ok := out[e.SessionID]; ok && !newer(e, cur) {
			continue
		}
		out[e.SessionID] = e
	}
	return out
}

// newer reports whether a should win over b for the same session: proven
// over unproven, then the later update.
func newer(a, b Proven) bool {
	if (a.Verdict == Live) != (b.Verdict == Live) {
		return a.Verdict == Live
	}
	return a.UpdatedAt.After(b.UpdatedAt)
}

// ByPID returns the proven-live entry for pid, if any.
func (s *Snapshot) ByPID(pid int) (Proven, bool) {
	for _, e := range s.Entries {
		if e.PID == pid && e.Verdict == Live {
			return e, true
		}
	}
	return Proven{}, false
}

// Tree is the process tree, as UnderPane needs it.
type Tree interface {
	// Depth is how many parent steps lead from pid up to root (0 for root
	// itself); ok is false when root isn't among pid's ancestors.
	Depth(pid, root int) (int, bool)
}

// UnderPane returns the session a pane runs: of the proven-live entries
// whose process descends from panePID, the nearest to it. A Claude started
// from inside that session (a `claude -p` a tool ran) is deeper, so it is
// never mistaken for the pane's own. found is false when no Claude Code
// process runs in the pane. Two at the same depth (two CLIs side by side
// under one shell, one of them stopped with Ctrl-Z) are an error that names
// both, unless only one is the session most recently updated.
func (s *Snapshot) UnderPane(t Tree, panePID int) (e Proven, found bool, err error) {
	type hit struct {
		Proven
		depth int
	}
	var hits []hit
	for _, e := range s.Entries {
		if e.Verdict != Live {
			continue
		}
		if d, ok := t.Depth(e.PID, panePID); ok {
			hits = append(hits, hit{e, d})
		}
	}
	if len(hits) == 0 {
		return Proven{}, false, nil
	}
	sort.Slice(hits, func(i, j int) bool {
		if hits[i].depth != hits[j].depth {
			return hits[i].depth < hits[j].depth
		}
		return hits[i].UpdatedAt.After(hits[j].UpdatedAt)
	})
	if len(hits) > 1 && hits[1].depth == hits[0].depth && hits[1].UpdatedAt.Equal(hits[0].UpdatedAt) {
		return Proven{}, false, fmt.Errorf("two Claude Code processes run at the same depth under pane process %d (pids %d and %d, sessions %s and %s), and neither updated last",
			panePID, hits[0].PID, hits[1].PID, hits[0].SessionID, hits[1].SessionID)
	}
	return hits[0].Proven, true, nil
}

// ReadPID reads and proves the entry of one process, if it has one: found
// is false when pid has no registry file (it isn't a Claude Code CLI).
func ReadPID(dir string, pid int, h Host) (e Proven, found bool, err error) {
	data, err := os.ReadFile(filepath.Join(dir, strconv.Itoa(pid)+".json"))
	if errors.Is(err, fs.ErrNotExist) {
		return Proven{}, false, nil
	}
	if err != nil {
		return Proven{}, false, err
	}
	entry, err := ParseEntry(data)
	if err != nil {
		return Proven{}, false, fmt.Errorf("%s: %w", filepath.Join(dir, strconv.Itoa(pid)+".json"), err)
	}
	if entry.PID != pid {
		return Proven{}, false, fmt.Errorf("%d.json names pid %d", pid, entry.PID)
	}
	entry.Path = filepath.Join(dir, strconv.Itoa(pid)+".json")
	return Proven{Entry: entry, Verdict: Proof(entry, h)}, true, nil
}

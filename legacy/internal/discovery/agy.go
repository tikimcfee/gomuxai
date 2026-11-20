package discovery

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"syscall"

	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// agy (the Antigravity CLI, and its desktop app) keeps each conversation
// in <brain>/<conversation id>/, its transcript at
// .system_generated/logs/transcript_full.jsonl. The transcript names no
// workspace, title or model, so the rest comes from the index files beside
// brain/, as glyph3d-js cli/sessions.go reads them:
//   - history.jsonl (transcript.ReadAgyHistory): a conversation's
//     workspace;
//   - brain/<parent>/.system_generated/subagents/<child>.json
//     (transcript.ReadAgySubagent): a subagent conversation, its parent,
//     workspace and state;
//   - annotations/<id>.pbtxt (title:"..."): the conversation's title;
//   - presence/<id>.lock, which agy holds an exclusive flock on while the
//     conversation is open.
//
// transcript.HarvestAgy reads what they say about a conversation.

// agyTranscripts are the transcript names a conversation may have, the one
// agy's hooks report first.
var agyTranscripts = []string{"transcript_full.jsonl", "transcript.jsonl"}

// agyIndex is what the files beside brain/ say about its conversations.
type agyIndex struct {
	home      string            // the dir holding brain/
	workspace map[string]string // conversation id -> workspace, from history.jsonl
	subagents map[string]transcript.AgySubagent
	locked    func(path string) bool // whether a process holds a lock on path
}

// scanAgy lists the conversations under a brain dir.
func scanAgy(brain string) []Session {
	entries, err := os.ReadDir(brain)
	if err != nil {
		return nil
	}
	idx := newAgyIndex(filepath.Dir(brain))
	var out []Session
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		path, info := agyTranscript(filepath.Join(brain, e.Name()))
		if info == nil {
			continue
		}
		out = append(out, probeAgy(idx, e.Name(), path, info))
	}
	// A subagent with no presence lock of its own is open while it says it
	// is alive and its parent is open.
	live := map[string]liveness{}
	for _, s := range out {
		live[s.SessionID] = s.live
	}
	for i, s := range out {
		if s.ParentSession != nil && s.live != liveYes && s.state == subRunning && live[s.ParentSession.SessionID] == liveYes {
			out[i].live = liveYes
		}
	}
	return out
}

// agyTranscript finds a conversation dir's transcript, or returns a nil
// info if it has none (a conversation that never got a step).
func agyTranscript(conv string) (string, os.FileInfo) {
	for _, name := range agyTranscripts {
		for _, path := range []string{filepath.Join(conv, ".system_generated", "logs", name), filepath.Join(conv, name)} {
			if info, err := os.Stat(path); err == nil && !info.IsDir() && info.Size() > 0 {
				return path, info
			}
		}
	}
	return "", nil
}

// probeAgy describes one conversation from idx and its presence lock.
func probeAgy(idx *agyIndex, id, path string, info os.FileInfo) Session {
	m := transcript.Session{SessionID: id}
	files := transcript.AgyFiles{Home: idx.home, Workspace: idx.workspace[id]}
	if sub, ok := idx.subagents[id]; ok {
		files.Subagent = &sub
	}
	m.HarvestAgy(files)
	s := fromMeta(HarnessAgy, path, info, m)
	if sub := files.Subagent; sub != nil {
		switch sub.State {
		case "SUBAGENT_STATE_ALIVE":
			s.state = subRunning
		case "":
		default:
			s.state = subFinished
		}
	}
	presence := filepath.Join(idx.home, "presence")
	if isDir(presence) {
		s.live = liveNo
		if idx.locked(filepath.Join(presence, id+".lock")) {
			s.live = liveYes
		}
	}
	return s
}

// newAgyIndex reads the index files in home (the dir holding brain/).
// Missing or malformed files and lines contribute nothing.
func newAgyIndex(home string) *agyIndex {
	idx := &agyIndex{home: home, workspace: transcript.ReadAgyHistory(home), subagents: map[string]transcript.AgySubagent{}, locked: newLockProbe()}
	brain := filepath.Join(home, "brain")
	parents, _ := os.ReadDir(brain)
	for _, p := range parents {
		dir := filepath.Join(brain, p.Name(), ".system_generated", "subagents")
		subs, err := os.ReadDir(dir)
		if !p.IsDir() || err != nil {
			continue
		}
		for _, f := range subs {
			if f.IsDir() || !strings.HasSuffix(f.Name(), ".json") {
				continue
			}
			if sub, ok := transcript.ReadAgySubagent(filepath.Join(dir, f.Name())); ok {
				idx.subagents[sub.ConversationID] = sub
			}
		}
	}
	return idx
}

// flockHeld reports whether a process holds an exclusive flock on path. It
// asks for a shared lock without waiting, and gives it back at once, so for
// that moment agy's own non-blocking exclusive lock would fail: only where
// there is no passive way (see newLockProbe).
func flockHeld(path string) bool {
	f, err := os.Open(path)
	if err != nil {
		return false
	}
	defer f.Close()
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_SH|syscall.LOCK_NB); err != nil {
		return errors.Is(err, syscall.EWOULDBLOCK)
	}
	syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	return false
}

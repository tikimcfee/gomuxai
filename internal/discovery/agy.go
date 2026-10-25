package discovery

import (
	"bytes"
	"encoding/json"
	"errors"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

// agy (the Antigravity CLI, and its desktop app) keeps each conversation
// in <brain>/<conversation id>/, its transcript at
// .system_generated/logs/transcript_full.jsonl. The transcript names no
// workspace, title or model, so the rest comes from the index files beside
// brain/, as glyph3d-js cli/sessions.go reads them:
//   - history.jsonl, one {"workspace","conversationId"?} line per typed
//     prompt: a conversation's workspace;
//   - brain/<parent>/.system_generated/subagents/<child>.json
//     ({"conversationId","subagentDescriptor":{"role"},"state",
//     "workspaceUris"}): a subagent conversation, its parent, workspace and
//     state;
//   - annotations/<id>.pbtxt (title:"..."): the conversation's title;
//   - presence/<id>.lock, which agy holds an exclusive flock on while the
//     conversation is open.

// agyTranscripts are the transcript names a conversation may have, the one
// agy's hooks report first.
var agyTranscripts = []string{"transcript_full.jsonl", "transcript.jsonl"}

// agyIndex is what the files beside brain/ say about its conversations.
type agyIndex struct {
	home      string            // the dir holding brain/
	workspace map[string]string // conversation id -> workspace, from history.jsonl
	subagents map[string]agySubagent
	locked    func(path string) bool // whether a process holds a lock on path
}

type agySubagent struct {
	parent    string
	role      string
	state     string
	workspace string
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
	s := fromFile(HarnessAgy, id, path, info)
	s.Cwd = idx.workspace[id]
	if sub, ok := idx.subagents[id]; ok {
		s.ParentSession = &ParentSession{SessionID: sub.parent}
		s.Title = sub.role
		if s.Cwd == "" {
			s.Cwd = sub.workspace
		}
		switch sub.state {
		case "SUBAGENT_STATE_ALIVE":
			s.state = subRunning
		case "":
		default:
			s.state = subFinished
		}
	}
	s.Project = projectOf(s.Cwd)
	if data, err := os.ReadFile(filepath.Join(idx.home, "annotations", id+".pbtxt")); err == nil {
		if t := agyAnnotationTitle(data); t != "" {
			s.Title = t
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
	idx := &agyIndex{home: home, workspace: map[string]string{}, subagents: map[string]agySubagent{}, locked: newLockProbe()}
	if data, err := os.ReadFile(filepath.Join(home, "history.jsonl")); err == nil {
		for line := range bytes.SplitSeq(data, []byte{'\n'}) {
			var e struct {
				ConversationID string `json:"conversationId"`
				Workspace      string `json:"workspace"`
			}
			if len(bytes.TrimSpace(line)) == 0 || json.Unmarshal(line, &e) != nil || e.ConversationID == "" || e.Workspace == "" {
				continue
			}
			if _, ok := idx.workspace[e.ConversationID]; !ok {
				idx.workspace[e.ConversationID] = filepath.Clean(e.Workspace)
			}
		}
	}
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
			data, err := os.ReadFile(filepath.Join(dir, f.Name()))
			if err != nil {
				continue
			}
			var d struct {
				ConversationID string `json:"conversationId"`
				Descriptor     struct {
					Role string `json:"role"`
				} `json:"subagentDescriptor"`
				State         string   `json:"state"`
				WorkspaceURIs []string `json:"workspaceUris"`
			}
			if json.Unmarshal(data, &d) != nil || d.ConversationID == "" {
				continue
			}
			sub := agySubagent{parent: p.Name(), role: d.Descriptor.Role, state: d.State}
			for _, u := range d.WorkspaceURIs {
				if sub.workspace = fileURIPath(u); sub.workspace != "" {
					break
				}
			}
			idx.subagents[d.ConversationID] = sub
		}
	}
	return idx
}

// agyAnnotationTitle pulls the title:"..." field out of an annotation file
// (protobuf text format, whose string escapes are a subset of Go's), or "".
func agyAnnotationTitle(data []byte) string {
	s := string(data)
	i := strings.Index(s, `title:"`)
	if i < 0 {
		return ""
	}
	q, err := strconv.QuotedPrefix(s[i+len("title:"):])
	if err != nil {
		return ""
	}
	t, err := strconv.Unquote(q)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(t)
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

// fileURIPath maps a file:// URI to a clean local path, or "".
func fileURIPath(uri string) string {
	u, err := url.Parse(uri)
	if err != nil || u.Scheme != "file" || u.Path == "" {
		return ""
	}
	return filepath.Clean(filepath.FromSlash(u.Path))
}

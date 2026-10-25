package discovery

import (
	"bytes"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// Claude Code keeps a session at <projects>/<project folder>/<session
// id>.jsonl, and each subagent the session started at <session
// id>/subagents/agent-<agent id>.jsonl beside a meta.json naming the tool
// call that started it. The project folder's name is a slug of the cwd and
// is never read: the records carry the cwd itself.

// scanClaude lists the sessions under a projects dir: <dir>/*.jsonl,
// <dir>/<project>/*.jsonl, and their subagents.
func scanClaude(dir string) []Session {
	var out []Session
	add := func(folder string) {
		entries, err := os.ReadDir(folder)
		if err != nil {
			return
		}
		for _, e := range entries {
			path := filepath.Join(folder, e.Name())
			if e.IsDir() {
				out = append(out, scanClaudeSubagents(filepath.Join(path, "subagents"))...)
				continue
			}
			if !strings.HasSuffix(e.Name(), ".jsonl") {
				continue
			}
			if info, err := e.Info(); err == nil && info.Size() > 0 {
				out = append(out, probeClaude(path, info, false))
			}
		}
	}
	add(dir)
	if entries, err := os.ReadDir(dir); err == nil {
		for _, e := range entries {
			if e.IsDir() {
				add(filepath.Join(dir, e.Name()))
			}
		}
	}
	return out
}

// scanClaudeSubagents lists the subagent sessions in a session's
// subagents dir.
func scanClaudeSubagents(dir string) []Session {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil
	}
	var out []Session
	for _, e := range entries {
		name := e.Name()
		if e.IsDir() || !strings.HasPrefix(name, "agent-") || !strings.HasSuffix(name, ".jsonl") {
			continue
		}
		if info, err := e.Info(); err == nil && info.Size() > 0 {
			out = append(out, probeClaudeSubagent(filepath.Join(dir, name), info))
		}
	}
	return out
}

// isClaudeSubagent reports whether path is a subagent's transcript,
// <session id>/subagents/agent-<id>.jsonl.
func isClaudeSubagent(path string) bool {
	return filepath.Base(filepath.Dir(path)) == "subagents" && strings.HasPrefix(filepath.Base(path), "agent-")
}

// probeClaudeSubagent reads a subagent's transcript and its meta.json.
func probeClaudeSubagent(path string, info os.FileInfo) Session {
	s := probeClaude(path, info, true)
	s.ParentSession = &ParentSession{SessionID: filepath.Base(filepath.Dir(filepath.Dir(path)))}
	readClaudeMeta(&s, strings.TrimSuffix(path, ".jsonl")+".meta.json")
	return s
}

// readClaudeMeta fills a subagent's session from its meta.json: the tool
// call that started it, whether it finished, and its description as the
// title when the transcript records none.
func readClaudeMeta(s *Session, path string) {
	data, err := os.ReadFile(path)
	if err != nil {
		return
	}
	var meta struct {
		AgentType   string `json:"agentType"`
		Description string `json:"description"`
		ToolUseID   string `json:"toolUseId"`
		Finished    *bool  `json:"finished"`
	}
	if json.Unmarshal(data, &meta) != nil {
		return
	}
	s.ParentSession.ToolID = meta.ToolUseID
	if meta.Finished != nil {
		s.state = subRunning
		if *meta.Finished {
			s.state = subFinished
		}
	}
	if s.Title == "" {
		s.Title = meta.Description
	}
}

// claudeLine is the part of a transcript line discovery reads.
type claudeLine struct {
	Type        string `json:"type"`
	SessionID   string `json:"sessionId"`
	CWD         string `json:"cwd"`
	Version     string `json:"version"`
	GitBranch   string `json:"gitBranch"`
	AITitle     string `json:"aiTitle"`
	CustomTitle string `json:"customTitle"`
	ContinuedIn string `json:"continuedInSessionId"`
	Message     *struct {
		Model string `json:"model"`
	} `json:"message"`
}

// Read sizes for a Claude transcript: the head read for the cwd; the tail
// chunk read backwards for the newest metadata; how far from the end to
// look for anything at all (claudeTail); and how far past the newest AI
// title to keep looking for a custom one.
const (
	claudeHead       = 256 << 10
	claudeTailChunk  = 64 << 10
	claudeTail       = 1 << 20
	claudeTitleSlack = 64 << 10
)

// probeClaude reads a Claude transcript's metadata (see readClaude).
// A subagent's title comes from its meta.json, so its transcript isn't
// searched for one.
func probeClaude(path string, info os.FileInfo, subagent bool) Session {
	s := fromFile(HarnessClaude, SessionIDFromPath(path), path, info)
	f, err := os.Open(path)
	if err != nil {
		return s
	}
	defer f.Close()
	readClaude(&s, f, info.Size(), subagent)
	return s
}

// readClaude fills s from a Claude transcript of size bytes: the cwd from
// its first record that has one, and from the end backwards the newest
// title, model, version, git branch and continued-in. It reads at most
// claudeHead bytes of the head and claudeTail of the end, never the whole
// of a large transcript; a title older than that is not found.
//
// The title is the newest custom title (a person's /rename) if there is
// one, else the newest AI title. Claude Code re-appends the custom title
// beside each AI title it writes, so the search stops claudeTitleSlack
// past the newest AI title. A subagent's transcript holds no title, so only
// its metadata is looked for.
func readClaude(s *Session, f io.ReaderAt, size int64, subagent bool) {
	if size <= 2*claudeTailChunk {
		// Small enough to read once for both ends.
		data := make([]byte, size)
		n, _ := f.ReadAt(data, 0)
		f, size = bytes.NewReader(data[:n]), int64(n)
	}
	// The cwd is in one of the first records; the head read grows from
	// 16 KiB only until one has it.
	var head []byte
	for want := int64(16 << 10); s.Cwd == "" && int64(len(head)) < min(claudeHead, size); want *= 2 {
		more := make([]byte, min(want, claudeHead, size)-int64(len(head)))
		n, _ := f.ReadAt(more, int64(len(head)))
		if n == 0 {
			break
		}
		head = append(head, more[:n]...)
		for line := range bytes.SplitSeq(head, []byte{'\n'}) {
			var r claudeLine
			if bytes.Contains(line, []byte(`"cwd"`)) && json.Unmarshal(line, &r) == nil && r.CWD != "" {
				s.Cwd = r.CWD
				break
			}
		}
	}

	var aiTitle, customTitle, cwd string
	var aiTitleAt int64 = -1
	versionSeen := false // the newest record with a version: its branch, even "", is the branch
	eachLineBackward(f, size, claudeTailChunk, claudeTail, func(line []byte, at int64) bool {
		switch {
		case !subagent && bytes.Contains(line, []byte(`"custom-title"`)):
			var r claudeLine
			if customTitle == "" && json.Unmarshal(line, &r) == nil && r.Type == "custom-title" && r.CustomTitle != "" {
				customTitle = r.CustomTitle
			}
		case !subagent && bytes.Contains(line, []byte(`"ai-title"`)):
			var r claudeLine
			if aiTitle == "" && json.Unmarshal(line, &r) == nil && r.Type == "ai-title" && r.AITitle != "" {
				aiTitle, aiTitleAt = r.AITitle, at
			}
		case bytes.Contains(line, []byte(`"continued-in"`)):
			var r claudeLine
			if s.ContinuedIn == "" && json.Unmarshal(line, &r) == nil && r.Type == "continued-in" {
				s.ContinuedIn = r.ContinuedIn
			}
		case s.Model == "" || !versionSeen || cwd == "":
			var r claudeLine
			if !bytes.Contains(line, []byte(`"sessionId"`)) || json.Unmarshal(line, &r) != nil {
				break
			}
			if s.Model == "" && r.Type == "assistant" && r.Message != nil && r.Message.Model != "" && r.Message.Model != "<synthetic>" {
				s.Model = r.Message.Model
			}
			if !versionSeen && r.Version != "" {
				s.Version, s.GitBranch, versionSeen = r.Version, r.GitBranch, true
			}
			if cwd == "" {
				cwd = r.CWD
			}
		}
		metaDone := s.Model != "" && versionSeen
		titleDone := subagent || customTitle != "" || (aiTitleAt >= 0 && aiTitleAt-at > claudeTitleSlack)
		return !(metaDone && titleDone)
	})
	s.Title = customTitle
	if s.Title == "" {
		s.Title = aiTitle
	}
	if s.Cwd == "" {
		s.Cwd = cwd
	}
	s.Project = projectOf(s.Cwd)
}

// eachLineBackward calls fn with each line of f's first size bytes, last
// line first, with the byte offset it starts at, until fn returns false.
// It reads chunk bytes at a time from the end, so stopping early reads only
// the tail, and it never reads more than the last limit bytes: a line that
// starts before them is left out. A last line without a newline is passed
// too (it may be one being written; fn's decode decides).
func eachLineBackward(f io.ReaderAt, size, chunk, limit int64, fn func(line []byte, at int64) bool) {
	floor := max(size-limit, 0)
	var carry []byte // the start of the line the previous chunk began inside
	for end := size; end > floor; {
		start := max(end-chunk, floor)
		buf := make([]byte, end-start, end-start+int64(len(carry)))
		if _, err := f.ReadAt(buf, start); err != nil && err != io.EOF {
			return
		}
		buf = append(buf, carry...)
		// Every line after the chunk's first newline is whole; the part
		// before it continues into the next chunk back, unless this chunk
		// starts the file.
		for {
			nl := bytes.LastIndexByte(buf, '\n')
			if nl < 0 {
				break
			}
			if line := bytes.TrimSpace(buf[nl+1:]); len(line) > 0 && !fn(line, start+int64(nl)+1) {
				return
			}
			buf = buf[:nl]
		}
		carry = buf
		end = start
	}
	if line := bytes.TrimSpace(carry); len(line) > 0 && floor == 0 {
		fn(line, 0)
	}
}

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
				out = append(out, scanClaudeSubagents(filepath.Join(path, "subagents"), e.Name())...)
				continue
			}
			if !strings.HasSuffix(e.Name(), ".jsonl") {
				continue
			}
			if info, err := e.Info(); err == nil && info.Size() > 0 {
				out = append(out, probeClaude(path, info))
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

// scanClaudeSubagents lists the subagent sessions of the session named
// parent.
func scanClaudeSubagents(dir, parent string) []Session {
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
			s := probeClaude(filepath.Join(dir, name), info)
			s.ParentSession = &ParentSession{SessionID: parent}
			readClaudeMeta(&s, filepath.Join(dir, strings.TrimSuffix(name, ".jsonl")+".meta.json"))
			out = append(out, s)
		}
	}
	return out
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

// Read sizes for a Claude transcript: the head read for the cwd, the tail
// chunk read backwards for the newest metadata, and how far past the
// newest AI title to keep looking for a custom one.
const (
	claudeHead       = 256 << 10
	claudeTailChunk  = 256 << 10
	claudeTitleSlack = 1 << 20
)

// probeClaude reads a Claude transcript's metadata: the cwd from its first
// record that has one, and from the end backwards the newest title, model,
// version, git branch and continued-in.
//
// The title is the newest custom title (a person's /rename) if there is
// one, else the newest AI title: Claude Code re-appends the custom title
// beside each AI title it writes, so the custom one is never far from the
// end. The search stops claudeTitleSlack past the newest AI title; a
// transcript with no title at all is read whole.
func probeClaude(path string, info os.FileInfo) Session {
	s := fromFile(HarnessClaude, SessionIDFromPath(path), path, info)
	f, err := os.Open(path)
	if err != nil {
		return s
	}
	defer f.Close()
	size := info.Size()

	head := make([]byte, min(claudeHead, size))
	n, _ := f.ReadAt(head, 0)
	for line := range bytes.SplitSeq(head[:n], []byte{'\n'}) {
		var r claudeLine
		if !bytes.Contains(line, []byte(`"cwd"`)) || json.Unmarshal(line, &r) != nil || r.CWD == "" {
			continue
		}
		s.Cwd = r.CWD
		break
	}

	var aiTitle, customTitle, cwd string
	var aiTitleAt int64 = -1
	eachLineBackward(f, size, claudeTailChunk, func(line []byte, at int64) bool {
		switch {
		case bytes.Contains(line, []byte(`"custom-title"`)):
			var r claudeLine
			if json.Unmarshal(line, &r) == nil && r.Type == "custom-title" && r.CustomTitle != "" {
				customTitle = r.CustomTitle
			}
		case bytes.Contains(line, []byte(`"ai-title"`)):
			var r claudeLine
			if aiTitle == "" && json.Unmarshal(line, &r) == nil && r.Type == "ai-title" && r.AITitle != "" {
				aiTitle, aiTitleAt = r.AITitle, at
			}
		case bytes.Contains(line, []byte(`"continued-in"`)):
			var r claudeLine
			if s.ContinuedIn == "" && json.Unmarshal(line, &r) == nil && r.Type == "continued-in" {
				s.ContinuedIn = r.ContinuedIn
			}
		case s.Model == "" || s.Version == "" || s.GitBranch == "" || cwd == "":
			var r claudeLine
			if !bytes.Contains(line, []byte(`"sessionId"`)) || json.Unmarshal(line, &r) != nil {
				break
			}
			if s.Model == "" && r.Type == "assistant" && r.Message != nil && r.Message.Model != "" && r.Message.Model != "<synthetic>" {
				s.Model = r.Message.Model
			}
			if s.Version == "" {
				s.Version = r.Version
			}
			if s.GitBranch == "" {
				s.GitBranch = r.GitBranch
			}
			if cwd == "" {
				cwd = r.CWD
			}
		}
		metaDone := s.Model != "" && s.Version != "" && s.GitBranch != ""
		titleDone := customTitle != "" || (aiTitleAt >= 0 && aiTitleAt-at > claudeTitleSlack)
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
	return s
}

// eachLineBackward calls fn with each line of f's first size bytes, last
// line first, with the byte offset it starts at, until fn returns false.
// It reads chunk bytes at a time from the end, so stopping early reads only
// the tail. A last line without a newline is passed too (it may be one
// being written; fn's decode decides).
func eachLineBackward(f io.ReaderAt, size, chunk int64, fn func(line []byte, at int64) bool) {
	var carry []byte // the start of the line the previous chunk began inside
	for end := size; end > 0; {
		start := max(end-chunk, 0)
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
	if line := bytes.TrimSpace(carry); len(line) > 0 {
		fn(line, 0)
	}
}

package discovery

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/transcript"
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

// probeClaudeSubagent reads a subagent's transcript and its meta.json: the
// tool call that started it, whether it finished, and its description as
// the title (transcript.HarvestClaudeSubagent).
func probeClaudeSubagent(path string, info os.FileInfo) Session {
	m := readClaudeFile(path, info, true)
	m.ParentSession = &transcript.ParentSession{SessionID: filepath.Base(filepath.Dir(filepath.Dir(path)))}
	meta, ok := transcript.ReadClaudeSubagentMeta(path)
	if ok {
		m.HarvestClaudeSubagent(meta)
	}
	s := fromMeta(HarnessClaude, path, info, m)
	if ok && meta.Finished != nil {
		s.state = subRunning
		if *meta.Finished {
			s.state = subFinished
		}
	}
	return s
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

// probeClaude reads a Claude session's transcript for its metadata (see
// readClaude).
func probeClaude(path string, info os.FileInfo) Session {
	return fromMeta(HarnessClaude, path, info, readClaudeFile(path, info, false))
}

// readClaudeFile reads the metadata of the Claude transcript at path (see
// readClaude); none if it can't be opened.
func readClaudeFile(path string, info os.FileInfo, subagent bool) transcript.Session {
	f, err := os.Open(path)
	if err != nil {
		return transcript.Session{}
	}
	defer f.Close()
	return readClaude(f, info.Size(), subagent)
}

// readClaude reads a Claude transcript of size bytes for its metadata, by
// transcript.HarvestClaude's rules: from the end backwards, the newest
// title, agent name, model, version, git branch and continued-in, and the
// cwd from its first record that has one. It reads at most claudeHead
// bytes of the head and claudeTail of the end, never the whole of a large
// transcript; a value older than that is not found.
//
// The title is the newest custom title (a person's /rename) if there is
// one, else the newest AI title. Claude Code re-appends the custom title
// beside each AI title it writes, so the search stops claudeTitleSlack
// past the newest AI title. A subagent's transcript holds no title, so only
// its metadata is looked for.
func readClaude(f io.ReaderAt, size int64, subagent bool) transcript.Session {
	if size <= 2*claudeTailChunk {
		// Small enough to read once for both ends.
		data := make([]byte, size)
		n, _ := f.ReadAt(data, 0)
		f, size = bytes.NewReader(data[:n]), int64(n)
	}
	var m transcript.Session
	aiTitleAt := int64(-1) // where the newest title is, while it isn't a custom one
	eachLineBackward(f, size, claudeTailChunk, claudeTail, func(line []byte, at int64) bool {
		// Once the model, version and a cwd are known, only the records
		// that carry nothing but metadata can add anything.
		if m.Model != "" && m.Version != "" && m.CWD != "" && !transcript.ClaudeMetaRecord(line) {
			return true
		}
		transcript.HarvestClaude(&m, line, at)
		if aiTitleAt < 0 && m.Title != "" {
			aiTitleAt = at
		}
		metaDone := m.Model != "" && m.Version != ""
		titleDone := subagent || m.TitleIsCustom() || (aiTitleAt >= 0 && aiTitleAt-at > claudeTitleSlack)
		return !(metaDone && titleDone)
	})

	// The cwd is the first record's that has one, in the head; the head
	// read grows from 16 KiB only until a record has it. Only that record
	// of the head is harvested.
	var head []byte
	var first transcript.Session
	for want := int64(16 << 10); first.CWD == "" && int64(len(head)) < min(claudeHead, size); want *= 2 {
		more := make([]byte, min(want, claudeHead, size)-int64(len(head)))
		n, _ := f.ReadAt(more, int64(len(head)))
		if n == 0 {
			break
		}
		head = append(head, more[:n]...)
		for at := int64(0); at < int64(len(head)); {
			line := head[at:]
			if nl := bytes.IndexByte(line, '\n'); nl >= 0 {
				line = line[:nl]
			}
			if bytes.Contains(line, []byte(`"cwd"`)) {
				if transcript.HarvestClaude(&first, line, at); first.CWD != "" {
					transcript.HarvestClaude(&m, line, at)
					break
				}
			}
			at += int64(len(line)) + 1
		}
	}
	return m
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

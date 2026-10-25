package transcript

import (
	"bytes"
	"encoding/json"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// What a session says about itself: its working directory, title, agent
// name, model, version and git branch, and the session it continued in,
// from its transcript's records and the files the harness keeps beside
// it. These are the one set of rules for them. The readers apply them to
// every line as they parse; agent sessions (internal/discovery) applies
// them to the head and tail of a transcript it reads, never the whole, its
// tail backwards.
//
// So a value is placed by its line's byte offset, not by when it was
// harvested: each Harvest function takes the line's offset, and the
// newest line's value wins (the oldest's, for the working directory),
// whatever order the lines come in. A value from the files beside a
// transcript counts as from offset 0.

// metaAt holds the byte offset of the line each of a Session's harvested
// fields came from.
type metaAt struct {
	cwd, title, agentName, model, version, continuedIn int64
}

// newer reports whether a value from the line at at replaces the one a
// field holds from the line at was: it does if the field has none yet
// (have is false) or the line is newer.
func newer(have bool, at, was int64) bool { return !have || at > was }

// setCWD records the working directory a line at at names: the session's
// is the oldest one recorded, the directory it started in. Project is its
// last element.
func (s *Session) setCWD(cwd string, at int64) {
	if cwd == "" || (s.CWD != "" && at >= s.at.cwd) {
		return
	}
	s.CWD, s.Project, s.at.cwd = cwd, ProjectOf(cwd), at
}

// ProjectOf is a session's project: the last element of the working
// directory it recorded (a file:// URI too), by one rule for every
// harness. It is never the name of the folder the harness stores the
// session in.
func ProjectOf(cwd string) string {
	p := strings.TrimRight(strings.TrimPrefix(cwd, "file://"), "/")
	if p == "" {
		return ""
	}
	return filepath.Base(p)
}

// setTitle records a title: a custom one (a person's rename) wins over
// every other, and among titles of a kind the newest wins.
func (s *Session) setTitle(title string, custom bool, at int64) {
	if title == "" || (s.customTitle && !custom) {
		return
	}
	if custom != s.customTitle || newer(s.Title != "", at, s.at.title) {
		s.Title, s.customTitle, s.at.title = title, custom, at
	}
}

// TitleIsCustom reports whether Title is a custom title, which no other
// kind replaces: a reader of a transcript's end need look no further for
// one.
func (s *Session) TitleIsCustom() bool { return s.customTitle }

func (s *Session) setModel(model string, at int64) {
	if model != "" && newer(s.Model != "", at, s.at.model) {
		s.Model, s.at.model = model, at
	}
}

// setVersion records the harness version a line at at names, and the git
// branch the same line names: the newest line with a version sets both,
// even an empty branch (a session that left its repository).
func (s *Session) setVersion(version, branch string, at int64) {
	if version != "" && newer(s.Version != "", at, s.at.version) {
		s.Version, s.GitBranch, s.at.version = version, branch, at
	}
}

func (s *Session) setAgentName(name string, at int64) {
	if name != "" && newer(s.AgentName != "", at, s.at.agentName) {
		s.AgentName, s.at.agentName = name, at
	}
}

func (s *Session) setContinuedIn(id string, at int64) {
	if id != "" && newer(s.ContinuedIn != "", at, s.at.continuedIn) {
		s.ContinuedIn, s.at.continuedIn = id, at
	}
}

// claudeMeta is the part of a Claude Code record that says something
// about its session.
type claudeMeta struct {
	Type        string `json:"type"`
	SessionID   string `json:"sessionId"`
	AgentID     string `json:"agentId"`
	CWD         string `json:"cwd"`
	Version     string `json:"version"`
	GitBranch   string `json:"gitBranch"`
	AITitle     string `json:"aiTitle"`
	CustomTitle string `json:"customTitle"`
	AgentName   string `json:"agentName"`
	ContinuedIn string `json:"continuedInSessionId"`
	IsSidechain bool   `json:"isSidechain"`
}

// claudeMetaRecords are the types of the Claude Code records that carry
// session metadata and nothing else.
var claudeMetaRecords = [][]byte{[]byte(`"ai-title"`), []byte(`"custom-title"`), []byte(`"agent-name"`), []byte(`"continued-in"`)}

// ClaudeMetaRecord reports, from a line's bytes alone, whether it may be
// one of the records of a Claude Code transcript that carry only session
// metadata: a title, the agent name, the session it continued in. Every
// other record says at most the working directory, model, version and git
// branch, so a reader that has those need decode no other line.
func ClaudeMetaRecord(line []byte) bool {
	for _, t := range claudeMetaRecords {
		if bytes.Contains(line, t) {
			return true
		}
	}
	return false
}

// HarvestClaude records in s what the line of a Claude Code transcript at
// byte offset at says about its session (see harvestClaude). Lines may
// come in any order; one that isn't a JSON object says nothing.
func HarvestClaude(s *Session, line []byte, at int64) {
	var r struct {
		claudeMeta
		Message *struct {
			Model string `json:"model"`
		} `json:"message"`
	}
	if json.Unmarshal(line, &r) != nil {
		return
	}
	model := ""
	if r.Message != nil {
		model = r.Message.Model
	}
	harvestClaude(s, &r.claudeMeta, model, at)
}

// harvestClaude records in s what a Claude Code record at byte offset at
// says about its session, model being its message's: the oldest working
// directory; the newest title, a custom one (a person's /rename) winning
// over AI titles; the newest agent name, continued-in and model, but not
// an API error's (<synthetic>); and the newest record with a version sets
// the version and the git branch, even an empty branch. A sidechain record
// with no agent id (before subagents had their own files, they were
// sidechains of the main transcript) is another session's and says
// nothing about this one.
func harvestClaude(s *Session, r *claudeMeta, model string, at int64) {
	if r.IsSidechain && r.AgentID == "" {
		return
	}
	switch r.Type {
	case "ai-title":
		s.setTitle(r.AITitle, false, at)
	case "custom-title":
		s.setTitle(r.CustomTitle, true, at)
	case "agent-name":
		s.setAgentName(r.AgentName, at)
	case "continued-in":
		s.setContinuedIn(r.ContinuedIn, at)
	case "assistant":
		if model != "<synthetic>" {
			s.setModel(model, at)
		}
	}
	switch {
	case r.AgentID != "":
		// A subagent's own transcript: its session is named after it, and
		// its records name the session that started it.
		s.SessionID = "agent-" + r.AgentID
		if r.SessionID != "" {
			if s.ParentSession == nil {
				s.ParentSession = &ParentSession{}
			}
			s.ParentSession.SessionID = r.SessionID
		}
	case r.SessionID != "":
		s.SessionID = r.SessionID
	}
	s.setCWD(r.CWD, at)
	s.setVersion(r.Version, r.GitBranch, at)
}

// ClaudeSubagentMeta is the meta.json Claude Code keeps beside a
// subagent's transcript (<session id>/subagents/agent-<id>.meta.json).
type ClaudeSubagentMeta struct {
	AgentType   string `json:"agentType"`
	Description string `json:"description"`
	// ToolUseID is the call that started the subagent.
	ToolUseID string `json:"toolUseId"`
	// Finished is whether it finished; nil if the file doesn't say.
	Finished *bool `json:"finished"`
}

// ReadClaudeSubagentMeta reads the meta.json beside the Claude subagent
// transcript at path. ok is false if there is none, or it isn't JSON.
func ReadClaudeSubagentMeta(path string) (meta ClaudeSubagentMeta, ok bool) {
	data, err := os.ReadFile(strings.TrimSuffix(path, ".jsonl") + ".meta.json")
	if err != nil || json.Unmarshal(data, &meta) != nil {
		return ClaudeSubagentMeta{}, false
	}
	return meta, true
}

// HarvestClaudeSubagent records in s what a Claude subagent's meta.json
// says about it: the call that started it, and its description as the
// title, since a subagent's transcript records none.
func (s *Session) HarvestClaudeSubagent(meta ClaudeSubagentMeta) {
	if s.ParentSession != nil && s.ParentSession.ToolID == "" {
		s.ParentSession.ToolID = meta.ToolUseID
	}
	if s.Title == "" {
		s.Title = meta.Description
	}
}

// HarvestKimi records in s what the line of a Kimi Code wire log at byte
// offset at says about its session: the newest llm.request's model. Lines
// may come in any order; a line that isn't an llm.request is passed over
// without being decoded. (The title and working directory are in
// state.json: FileOptions.)
func HarvestKimi(s *Session, line []byte, at int64) {
	if !bytes.Contains(line, []byte(`"llm.request"`)) {
		return
	}
	var r kimiFields
	if json.Unmarshal(line, &r) == nil && r.str("type") == "llm.request" {
		s.setModel(r.str("model"), at)
	}
}

// AgySubagent is agy's record of a subagent conversation,
// brain/<parent>/.system_generated/subagents/<conversation id>.json.
type AgySubagent struct {
	ConversationID string
	// Parent is the conversation that started it.
	Parent string
	Role   string
	// State is agy's SUBAGENT_STATE_*.
	State string
	// Workspace is the first of its workspace URIs that is a file://
	// one, as a path.
	Workspace string
}

// ReadAgySubagent reads the subagent descriptor at path,
// brain/<parent>/.system_generated/subagents/<id>.json. ok is false if it
// isn't one.
func ReadAgySubagent(path string) (sub AgySubagent, ok bool) {
	data, err := os.ReadFile(path)
	if err != nil {
		return AgySubagent{}, false
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
		return AgySubagent{}, false
	}
	sub = AgySubagent{ConversationID: d.ConversationID, Role: d.Descriptor.Role, State: d.State,
		Parent: filepath.Base(filepath.Dir(filepath.Dir(filepath.Dir(path))))}
	for _, u := range d.WorkspaceURIs {
		if sub.Workspace = fileURIPath(u); sub.Workspace != "" {
			break
		}
	}
	return sub, true
}

// fileURIPath maps a file:// URI to a clean local path, or "".
func fileURIPath(uri string) string {
	u, err := url.Parse(uri)
	if err != nil || u.Scheme != "file" || u.Path == "" {
		return ""
	}
	return filepath.Clean(filepath.FromSlash(u.Path))
}

// ReadAgyHistory reads agy's history.jsonl in home (the dir holding
// brain/), one {"workspace","conversationId"?} line per typed prompt: each
// conversation's workspace, the first one recorded. Missing or malformed
// files and lines contribute nothing.
func ReadAgyHistory(home string) map[string]string {
	out := map[string]string{}
	data, err := os.ReadFile(filepath.Join(home, "history.jsonl"))
	if err != nil {
		return out
	}
	for line := range bytes.SplitSeq(data, []byte{'\n'}) {
		var e struct {
			ConversationID string `json:"conversationId"`
			Workspace      string `json:"workspace"`
		}
		if len(bytes.TrimSpace(line)) == 0 || json.Unmarshal(line, &e) != nil || e.ConversationID == "" || e.Workspace == "" {
			continue
		}
		if _, ok := out[e.ConversationID]; !ok {
			out[e.ConversationID] = filepath.Clean(e.Workspace)
		}
	}
	return out
}

// AgyFiles is what agy keeps beside a conversation's transcript.
type AgyFiles struct {
	// Home is the dir holding brain/, and annotations/ beside it.
	Home string
	// Workspace is the conversation's workspace as history.jsonl records
	// it (ReadAgyHistory).
	Workspace string
	// Subagent is the conversation's descriptor, if it is a subagent.
	Subagent *AgySubagent
}

// HarvestAgy records in s, the conversation s.SessionID, what agy keeps
// beside its transcript: its workspace (history.jsonl's, else a
// subagent's own), the conversation that started a subagent, and its
// title: the one annotations/<id>.pbtxt holds, else a subagent's role.
func (s *Session) HarvestAgy(f AgyFiles) {
	s.setCWD(f.Workspace, 0)
	if sub := f.Subagent; sub != nil {
		s.ParentSession = &ParentSession{SessionID: sub.Parent}
		s.setCWD(sub.Workspace, 0)
		s.setTitle(sub.Role, false, 0)
	}
	if data, err := os.ReadFile(filepath.Join(f.Home, "annotations", s.SessionID+".pbtxt")); err == nil {
		if t := agyAnnotationTitle(data); t != "" {
			s.Title = t
		}
	}
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

package discovery

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
)

// Kimi Code keeps a session as a directory,
// <sessions>/wd_<project>_<hash>/session_<uuid>/: state.json (title,
// workDir), the main agent's event log agents/main/wire.jsonl, and each
// subagent's at agents/<agent>/wire.jsonl. session_index.jsonl beside
// sessions/ maps each session to its workDir too. Kimi has no file that
// says a session is open.

// scanKimi lists the sessions, main agents and subagents, under a sessions
// dir.
func scanKimi(dir string) []Session {
	workspaces, err := os.ReadDir(dir)
	if err != nil {
		return nil
	}
	index := kimiIndex(filepath.Dir(dir))
	var out []Session
	for _, ws := range workspaces {
		sessions, err := os.ReadDir(filepath.Join(dir, ws.Name()))
		if !ws.IsDir() || err != nil {
			continue
		}
		for _, sd := range sessions {
			agents, err := os.ReadDir(filepath.Join(dir, ws.Name(), sd.Name(), "agents"))
			if !sd.IsDir() || err != nil {
				continue
			}
			for _, a := range agents {
				wire := filepath.Join(dir, ws.Name(), sd.Name(), "agents", a.Name(), "wire.jsonl")
				info, err := os.Stat(wire)
				if !a.IsDir() || err != nil || info.IsDir() || info.Size() == 0 {
					continue
				}
				s := probeKimi(wire, info)
				if s.Cwd == "" {
					s.Cwd = index[strings.TrimPrefix(sd.Name(), "session_")]
					s.Project = projectOf(s.Cwd)
				}
				out = append(out, s)
			}
		}
	}
	return out
}

// probeKimi reads a Kimi wire log's session: the title and cwd from the
// session's state.json, the model from the newest llm.request line, and for
// a subagent, its parent session.
func probeKimi(wire string, info os.FileInfo) Session {
	s := fromFile(HarnessKimi, SessionIDFromPath(wire), wire, info)
	agentDir := filepath.Dir(wire)
	sessionDir := filepath.Dir(filepath.Dir(agentDir))
	if filepath.Base(agentDir) != "main" {
		s.ParentSession = &ParentSession{SessionID: strings.TrimPrefix(filepath.Base(sessionDir), "session_")}
	}
	if data, err := os.ReadFile(filepath.Join(sessionDir, "state.json")); err == nil {
		var state struct {
			Title   string `json:"title"`
			WorkDir string `json:"workDir"`
			CWD     string `json:"cwd"`
		}
		if json.Unmarshal(data, &state) == nil {
			s.Title = strings.TrimSpace(state.Title)
			s.Cwd = strings.TrimSpace(state.WorkDir)
			if s.Cwd == "" {
				s.Cwd = strings.TrimSpace(state.CWD)
			}
		}
	}
	s.Project = projectOf(s.Cwd)
	if f, err := os.Open(wire); err == nil {
		eachLineBackward(f, info.Size(), claudeTailChunk, func(line []byte, _ int64) bool {
			var r struct {
				Type  string `json:"type"`
				Model string `json:"model"`
			}
			if bytes.Contains(line, []byte(`"llm.request"`)) && json.Unmarshal(line, &r) == nil && r.Type == "llm.request" && r.Model != "" {
				s.Model = r.Model
				return false
			}
			return true
		})
		f.Close()
	}
	return s
}

// kimiIndex reads session_index.jsonl in home (the dir holding sessions/):
// session uuid -> workDir.
func kimiIndex(home string) map[string]string {
	out := map[string]string{}
	data, err := os.ReadFile(filepath.Join(home, "session_index.jsonl"))
	if err != nil {
		return out
	}
	for line := range bytes.SplitSeq(data, []byte{'\n'}) {
		var e struct {
			SessionID string `json:"sessionId"`
			WorkDir   string `json:"workDir"`
		}
		if len(bytes.TrimSpace(line)) == 0 || json.Unmarshal(line, &e) != nil || e.SessionID == "" || e.WorkDir == "" {
			continue
		}
		out[strings.TrimPrefix(e.SessionID, "session_")] = filepath.Clean(e.WorkDir)
	}
	return out
}

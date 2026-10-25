package discovery

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/transcript"
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
					s.Project = transcript.ProjectOf(s.Cwd)
				}
				out = append(out, s)
			}
		}
	}
	return out
}

// probeKimi reads a Kimi wire log's session: what the files beside it say
// (transcript.FileOptions: the title and cwd from the session's
// state.json, and for a subagent, its parent session), and the model from
// the newest llm.request line, read from the end backwards.
func probeKimi(wire string, info os.FileInfo) Session {
	m := transcript.FileOptions(HarnessKimi, wire).Session
	if f, err := os.Open(wire); err == nil {
		eachLineBackward(f, info.Size(), claudeTailChunk, claudeTail, func(line []byte, at int64) bool {
			transcript.HarvestKimi(&m, line, at)
			return m.Model == ""
		})
		f.Close()
	}
	return fromMeta(HarnessKimi, wire, info, m)
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

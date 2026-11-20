use std::collections::HashMap;

use agent_base::gostd::{filepath, json};
use agent_transcript::{ParentSession, file_options, harvest_kimi, project_of, trim_space};
use serde::Deserialize;

use crate::claude::{CLAUDE_TAIL, CLAUDE_TAIL_CHUNK, each_line_backward};
use crate::dirs::{HARNESS_KIMI, entry_name, os_path, read_dir_sorted};
use crate::sessions::{FileStat, Session, from_meta, stub};

pub(crate) fn list_kimi(dir: &str) -> Vec<Session> {
    let Ok(workspaces) = read_dir_sorted(dir) else {
        return Vec::new();
    };
    let index = kimi_index(&filepath::dir(dir));
    let mut out = Vec::new();
    for ws in workspaces {
        if !ws.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let ws_name = entry_name(dir, &ws);
        let ws_dir = filepath::join(&[dir, &ws_name]);
        let Ok(sessions) = read_dir_sorted(&ws_dir) else {
            continue;
        };
        for sd in sessions {
            if !sd.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let sd_name = entry_name(&ws_dir, &sd);
            let agents_dir = filepath::join(&[dir, &ws_name, &sd_name, "agents"]);
            let Ok(agents) = read_dir_sorted(&agents_dir) else {
                continue;
            };
            let sess_id = sd_name.strip_prefix("session_").unwrap_or(&sd_name);
            for a in agents {
                if !a.file_type().is_ok_and(|ft| ft.is_dir()) {
                    continue;
                }
                let a_name = entry_name(&agents_dir, &a);
                let wire = filepath::join(&[&agents_dir, &a_name, "wire.jsonl"]);
                let Ok(info) = std::fs::metadata(os_path(&wire)) else {
                    continue;
                };
                if info.is_dir() || info.len() == 0 {
                    continue;
                }
                let mut s = stub(HARNESS_KIMI, &wire, FileStat::from_metadata(&info));
                if a_name != "main" {
                    s.parent_session = Some(ParentSession {
                        session_id: sess_id.to_string(),
                        tool_id: String::new(),
                    });
                }
                if let Some(cwd) = index.get(sess_id) {
                    s.cwd = cwd.clone();
                    s.project = project_of(&s.cwd);
                }
                out.push(s);
            }
        }
    }
    out
}

pub(crate) fn probe_kimi(wire: &str, stat: FileStat) -> Session {
    let mut m = file_options(HARNESS_KIMI, wire).session;
    if let Ok(f) = std::fs::File::open(os_path(wire)) {
        each_line_backward(&f, stat.size, CLAUDE_TAIL_CHUNK, CLAUDE_TAIL, |line, at| {
            harvest_kimi(&mut m, line, at);
            m.model.is_empty()
        });
    }
    from_meta(HARNESS_KIMI, wire, stat, m)
}

fn kimi_index(home: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(data) = std::fs::read(os_path(&filepath::join(&[home, "session_index.jsonl"]))) else {
        return out;
    };
    #[derive(Deserialize)]
    struct Entry {
        #[serde(rename = "sessionId", default)]
        session_id: String,
        #[serde(rename = "workDir", default)]
        work_dir: String,
    }
    for line in data.split(|&b| b == b'\n') {
        // The line is decoded whole, as Go's json.Unmarshal reads it: a form
        // feed or vertical tab around it isn't JSON whitespace.
        if trim_space(line).is_empty() {
            continue;
        }
        let Ok(e) = json::from_slice::<Entry>(line) else {
            continue;
        };
        if e.session_id.is_empty() || e.work_dir.is_empty() {
            continue;
        }
        let id = e
            .session_id
            .strip_prefix("session_")
            .unwrap_or(&e.session_id)
            .to_string();
        out.insert(id, filepath::clean(&e.work_dir));
    }
    out
}

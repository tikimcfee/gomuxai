//! Reads back the session name or conversation title a backend CLI recorded
//! in its own registry or transcript metadata (`docs/FLEET.md`, "Name at
//! launch").
//!
//! - **Claude Code (`"claude"`)**: at launch (`claude --name <name>`) and on
//!   `/rename`, Claude Code writes `"name"` with `"nameSource": "user"` (or
//!   `"collision"` if another live session already held that name) to its
//!   running session registry (`<claude config dir>/sessions/<pid>.json`), and
//!   appends `{"type":"custom-title","customTitle":"..."}` and
//!   `{"type":"agent-name","agentName":"..."}` to the session JSONL once the
//!   transcript is written. An unnamed interactive session initially gets a
//!   placeholder `"nameSource": "derived"` slug (`<project>-<hex>`) in the
//!   registry, then after its first turn gets an AI-generated title
//!   (`"nameSource": "auto"`, `"ai-title"`, `"agent-name"`). The `"derived"`
//!   placeholder is ignored so a freshly launched session isn't reported under
//!   its pre-`--name` slug.
//! - **Antigravity CLI (`"agy"`)**: stores a conversation's title in
//!   `<agy home>/annotations/<conversation id>.pbtxt` (`title:"..."`).

use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use agent_base::gostd::filepath;
use agent_claudereg as claudereg;
use agent_transcript::{
    AgyFiles, Session, agy_conversation_dir, claude_meta_record, claude_subagent_session,
    file_options, harvest_claude, read_claude_subagent_meta,
};

const TAIL_CHUNK: i64 = 64 << 10;

/// Returns the session name or conversation title that `backend`'s CLI
/// recorded for `session_id` / `transcript_path`, or `""` when none is
/// recorded.
pub fn session_name(backend: &str, session_id: &str, transcript_path: &str) -> String {
    match backend {
        "claude" => {
            let snap = claudereg::read_system().ok();
            session_name_with(
                backend,
                session_id,
                transcript_path,
                snap.as_ref(),
                claudereg::find_transcript,
                &default_agy_homes(),
            )
        }
        "agy" => session_name_with(
            backend,
            session_id,
            transcript_path,
            None,
            |_| None,
            &default_agy_homes(),
        ),
        "kimi" if !transcript_path.is_empty() => {
            file_options("kimi", transcript_path).session.title
        }
        _ => String::new(),
    }
}

fn default_agy_homes() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    if home.is_empty() {
        return Vec::new();
    }
    vec![
        filepath::join(&[home.as_str(), ".gemini", "antigravity-cli"]),
        filepath::join(&[home.as_str(), ".gemini", "antigravity"]),
    ]
}

pub(crate) fn session_name_with(
    backend: &str,
    session_id: &str,
    transcript_path: &str,
    claude_snap: Option<&claudereg::Snapshot>,
    find_claude_transcript: impl FnOnce(&str) -> Option<PathBuf>,
    agy_homes: &[String],
) -> String {
    match backend {
        "claude" => {
            if !session_id.is_empty()
                && let Some(snap) = claude_snap
                && let Some(proven) = snap.sessions().get(session_id)
                && !proven.entry.name.is_empty()
                && proven.entry.name_source != "derived"
            {
                return proven.entry.name.clone();
            }
            let resolved: Option<String> = if !transcript_path.is_empty() {
                Some(transcript_path.to_string())
            } else if !session_id.is_empty() {
                find_claude_transcript(session_id).map(|p| p.to_string_lossy().into_owned())
            } else {
                None
            };
            let Some(path) = resolved else {
                return String::new();
            };
            read_claude_transcript_name(&path)
        }
        "agy" => read_agy_title(session_id, transcript_path, agy_homes),
        "kimi" if !transcript_path.is_empty() => {
            file_options("kimi", transcript_path).session.title
        }
        _ => String::new(),
    }
}

fn read_agy_title(session_id: &str, transcript_path: &str, agy_homes: &[String]) -> String {
    if !transcript_path.is_empty() {
        let conv = agy_conversation_dir(transcript_path);
        if !conv.is_empty() {
            let id = if !session_id.is_empty() {
                session_id.to_string()
            } else {
                filepath::base(&conv)
            };
            let home = filepath::dir(&filepath::dir(&conv));
            let mut s = Session {
                session_id: id,
                ..Default::default()
            };
            s.harvest_agy(&AgyFiles {
                home,
                ..Default::default()
            });
            if !s.title.is_empty() {
                return s.title;
            }
        }
    }
    if session_id.is_empty() {
        return String::new();
    }
    for home in agy_homes {
        let mut s = Session {
            session_id: session_id.to_string(),
            ..Default::default()
        };
        s.harvest_agy(&AgyFiles {
            home: home.clone(),
            ..Default::default()
        });
        if !s.title.is_empty() {
            return s.title;
        }
    }
    String::new()
}

fn read_claude_transcript_name(path: &str) -> String {
    let Ok(f) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(info) = f.metadata() else {
        return String::new();
    };
    if info.is_dir() {
        return String::new();
    }
    let size = info.len() as i64;
    let mut m = Session::default();
    each_line_backward(&f, size, |line, at| {
        if !claude_meta_record(line) {
            return true;
        }
        harvest_claude(&mut m, line, at);
        !m.title_is_custom()
    });
    if !claude_subagent_session(path).is_empty()
        && let Some(meta) = read_claude_subagent_meta(path)
    {
        m.harvest_claude_subagent(&meta);
    }
    if m.title_is_custom() {
        m.title
    } else if !m.agent_name.is_empty() {
        m.agent_name
    } else {
        m.title
    }
}

fn read_at_full(f: &std::fs::File, buf: &mut [u8], mut offset: u64) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match f.read_at(&mut buf[total..], offset) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn trim_ascii(mut b: &[u8]) -> &[u8] {
    while let Some(&c) = b.first() {
        if c.is_ascii_whitespace() {
            b = &b[1..];
        } else {
            break;
        }
    }
    while let Some(&c) = b.last() {
        if c.is_ascii_whitespace() {
            b = &b[..b.len() - 1];
        } else {
            break;
        }
    }
    b
}

fn each_line_backward(f: &std::fs::File, size: i64, mut cb: impl FnMut(&[u8], i64) -> bool) {
    let mut carry: Vec<u8> = Vec::new();
    let mut end = size;
    while end > 0 {
        let start = (end - TAIL_CHUNK).max(0);
        let len = (end - start) as usize;
        let mut buf = vec![0u8; len];
        let Ok(n) = read_at_full(f, &mut buf, start as u64) else {
            return;
        };
        buf.truncate(n);
        buf.extend_from_slice(&carry);
        while let Some(nl) = buf.iter().rposition(|&b| b == b'\n') {
            let line = trim_ascii(&buf[nl + 1..]);
            if !line.is_empty() && !cb(line, start + nl as i64 + 1) {
                return;
            }
            buf.truncate(nl);
        }
        carry = buf;
        end = start;
    }
    let line = trim_ascii(&carry);
    if !line.is_empty() {
        cb(line, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudereg::{Entry, Proven, Snapshot, Verdict};

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("agent-backend-sname-")
            .tempdir_in("/var/tmp")
            .or_else(|_| tempfile::tempdir())
            .expect("tempdir")
    }

    fn snap_with(entries: Vec<Entry>) -> Snapshot {
        Snapshot {
            entries: entries
                .into_iter()
                .map(|entry| Proven {
                    entry,
                    verdict: Verdict::Live,
                })
                .collect(),
            problems: Vec::new(),
        }
    }

    #[test]
    fn test_claude_session_name_from_registry_and_transcript() {
        let dir = temp_dir();
        let transcript = dir.path().join("s1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","sessionId":"s1","cwd":"/srv/app","message":{"role":"user","content":"hi"}}"#,
                "\n",
                r#"{"type":"custom-title","sessionId":"s1","customTitle":"worker-one"}"#,
                "\n",
                r#"{"type":"agent-name","sessionId":"s1","agentName":"worker-one"}"#,
                "\n",
                r#"{"type":"ai-title","sessionId":"s1","aiTitle":"Say Hi To User"}"#,
                "\n",
            ),
        )
        .unwrap();
        let tpath = transcript.to_str().unwrap();

        // Live registry user/collision name takes precedence (e.g. right after
        // claude --resume <id> --name new-worker, before a new turn writes to JSONL).
        let snap = snap_with(vec![Entry {
            pid: 100,
            session_id: "s1".into(),
            name: "new-worker".into(),
            name_source: "user".into(),
            ..Default::default()
        }]);
        assert_eq!(
            session_name_with("claude", "s1", tpath, Some(&snap), |_| None, &[]),
            "new-worker"
        );

        // Claude's initial "derived" placeholder (<project>-<hex>) is ignored:
        // falls back to the transcript's custom-title, or "" before any turn.
        let derived = snap_with(vec![
            Entry {
                pid: 101,
                session_id: "s1".into(),
                name: "app-9a".into(),
                name_source: "derived".into(),
                ..Default::default()
            },
            Entry {
                pid: 102,
                session_id: "fresh".into(),
                name: "app-ff".into(),
                name_source: "derived".into(),
                ..Default::default()
            },
        ]);
        assert_eq!(
            session_name_with("claude", "s1", tpath, Some(&derived), |_| None, &[]),
            "worker-one"
        );
        assert_eq!(
            session_name_with("claude", "fresh", "", Some(&derived), |_| None, &[]),
            ""
        );

        // Ended session (not in registry): custom-title wins over later ai-title,
        // even across multi-chunk transcripts (> 64 KiB) or via find_transcript.
        let filler = format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"s2\",\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}}}}\n",
            "x".repeat(150_000)
        );
        let long_transcript = dir.path().join("s2.jsonl");
        std::fs::write(
            &long_transcript,
            format!(
                "{{\"type\":\"custom-title\",\"sessionId\":\"s2\",\"customTitle\":\"early-name\"}}\n{filler}{{\"type\":\"ai-title\",\"sessionId\":\"s2\",\"aiTitle\":\"Later AI Title\"}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            session_name_with(
                "claude",
                "s2",
                "",
                None,
                |_| Some(long_transcript.clone()),
                &[]
            ),
            "early-name"
        );

        // Unnamed Claude session after a turn: returns the AI-generated title.
        let unnamed = dir.path().join("s3.jsonl");
        std::fs::write(
            &unnamed,
            "{\"type\":\"ai-title\",\"sessionId\":\"s3\",\"aiTitle\":\"Fix Parser Bug\"}\n",
        )
        .unwrap();
        assert_eq!(
            session_name_with(
                "claude",
                "s3",
                unnamed.to_str().unwrap(),
                None,
                |_| None,
                &[]
            ),
            "Fix Parser Bug"
        );
    }

    #[test]
    fn test_agy_session_name_from_annotations() {
        let dir = temp_dir();
        let home = dir.path().join("agy-home");
        let conv_id = "11111111-2222-3333-4444-555555555555";
        let logs = home
            .join("brain")
            .join(conv_id)
            .join(".system_generated")
            .join("logs");
        let ann = home.join("annotations");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::create_dir_all(&ann).unwrap();
        let tpath = logs.join("transcript_full.jsonl");
        std::fs::write(
            &tpath,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hi\"}\n",
        )
        .unwrap();

        // No annotation yet -> "".
        let homes = vec![home.to_str().unwrap().to_string()];
        assert_eq!(
            session_name_with(
                "agy",
                conv_id,
                tpath.to_str().unwrap(),
                None,
                |_| None,
                &homes
            ),
            ""
        );

        // Annotation written by agy -> reads title, both via transcript_path and via session_id alone.
        std::fs::write(
            ann.join(format!("{conv_id}.pbtxt")),
            "last_user_view_time:{seconds:1700000000}\ntitle:\"Investigate Fleet Mismatch\"\n",
        )
        .unwrap();
        assert_eq!(
            session_name_with("agy", "", tpath.to_str().unwrap(), None, |_| None, &[]),
            "Investigate Fleet Mismatch"
        );
        assert_eq!(
            session_name_with("agy", conv_id, "", None, |_| None, &homes),
            "Investigate Fleet Mismatch"
        );

        // Unknown / non-transcript backends return "".
        assert_eq!(session_name_with("shell", "s", "", None, |_| None, &[]), "");
    }
}

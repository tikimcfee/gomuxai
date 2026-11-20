//! A Claude Code session can move to another process mid-conversation:
//! Claude Code (2.1.319 on) hands an interactive session over to a background
//! session its daemon runs (`"kind": "bg"`). The pane's CLI goes on drawing the
//! conversation, and takes what is typed into it, but the new process has a
//! session id of its own and runs every hook from then on, while the old
//! process's registry entry keeps the old id and its last status.
//!
//! Where the new process runs says nothing about whose conversation it carries:
//! the daemon is one per user, a child of whichever CLI started it first, and
//! from 2.1.325 it hands a moved session to a spare host it started ahead of
//! time (`"bg claimed-spare"` in `~/.claude/daemon.log`). So a moved session
//! can run under another agent's pane, with that agent's environment (friction
//! #48-#50). What does say it is the old transcript: the move appends a
//! `{"type":"continued-in","continuedInSessionId":"<new>"}` line to it. A
//! `claude --resume <id> --fork-session` a person runs starts a separate
//! conversation and writes no such line, so the line alone proves the move, and
//! [`Snapshot::continuation`] follows it.

use crate::proof::Verdict;
use crate::registry::{Entry, KIND_BACKGROUND, dir};
use crate::running::{Proven, Snapshot, is_running, newer};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How much of the end of a transcript [`continued_in`] reads for the
/// `continued-in` line: the move appends it, and the old process, whose
/// conversation has gone elsewhere, writes little after it. Status reads it on
/// every check of a Claude agent, so it is kept small.
pub const CONTINUED_TAIL: u64 = 64 << 10;

/// Bounds how many moves [`Snapshot::continuation`] follows.
const MAX_MOVES: usize = 16;

#[derive(Deserialize)]
struct ContinuedRecord {
    #[serde(default, rename = "type")]
    ty: Option<String>,
    #[serde(default, rename = "continuedInSessionId")]
    continued_in: Option<String>,
}

/// Returns the session the transcript at `path` says it continued in (its last
/// `continued-in` line), or `None` if it says none.
pub fn continued_in(path: impl AsRef<Path>) -> Option<String> {
    let mut f = File::open(path).ok()?;
    if let Ok(meta) = f.metadata()
        && meta.len() > CONTINUED_TAIL
    {
        f.seek(SeekFrom::End(-(CONTINUED_TAIL as i64))).ok()?;
    }

    let marker = br#""continued-in""#;
    let mut reader = BufReader::with_capacity(64 << 10, f);
    let mut line = Vec::new();
    let mut last: Option<String> = None;

    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !line.windows(marker.len()).any(|w| w == marker) {
            continue;
        }
        if let Ok(r) = serde_json::from_slice::<ContinuedRecord>(&line)
            && r.ty.as_deref() == Some("continued-in")
            && let Some(id) = r.continued_in.filter(|s| !s.is_empty())
        {
            last = Some(id);
        }
    }

    last
}

/// Returns the path of Claude Code's transcript for `session_id`
/// (`<config dir>/projects/<project>/<id>.jsonl`, the config dir being
/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`), or `None` if there is none.
pub fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let sessions_dir = dir().ok()?;
    if session_id.is_empty() || session_id.contains(['/', '\\', '*', '?', '[']) {
        return None;
    }
    let projects_dir = sessions_dir.parent()?.join("projects");
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(projects_dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    subdirs.sort();

    let file_name = format!("{session_id}.jsonl");
    for sub in subdirs {
        let candidate = sub.join(&file_name);
        if std::fs::symlink_metadata(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
}

impl Snapshot {
    /// Follows `outer`'s conversation through the moves Claude Code made (its
    /// transcript's `continued-in` line, then that session's, and so on) to the
    /// session it goes on in now, wherever the process holding it runs.
    /// `transcript_of` finds a session's transcript ([`find_transcript`]).
    /// Returns `None` when `outer`'s session hasn't moved. When it has, returns
    /// the running entry that holds the last session, or, if no running process
    /// holds it (the daemon retired the background session), an entry with only
    /// its `session_id` and the verdict [`Verdict::Stale`]: the conversation is
    /// still that session, which the pane's CLI starts again on its next input.
    pub fn continuation<P: AsRef<Path>>(
        &self,
        outer: &Proven,
        mut transcript_of: impl FnMut(&str) -> Option<P>,
    ) -> Option<Proven> {
        let mut seen = HashSet::from([outer.session_id.clone()]);
        let mut cur = outer.session_id.clone();
        let mut moved = false;

        for _ in 0..MAX_MOVES {
            let Some(next) = transcript_of(&cur).and_then(continued_in) else {
                break;
            };
            if !seen.insert(next.clone()) {
                break;
            }
            cur = next;
            moved = true;
        }

        if !moved {
            return None;
        }
        if let Some(h) = self.holder(&cur) {
            return Some(h);
        }
        Some(Proven {
            entry: Entry {
                session_id: cur,
                ..Default::default()
            },
            verdict: Verdict::Stale,
        })
    }

    /// Returns the running entry that holds session `id`: a background
    /// session's own process first (a person may have resumed the same session
    /// elsewhere too), then as [`Self::sessions`] picks.
    fn holder(&self, id: &str) -> Option<Proven> {
        let mut best: Option<&Proven> = None;
        for e in &self.entries {
            if e.session_id != id || !is_running(e.verdict) {
                continue;
            }
            let replace = match best {
                None => true,
                Some(cur) => {
                    let bg = e.kind == KIND_BACKGROUND;
                    let best_bg = cur.kind == KIND_BACKGROUND;
                    (bg && !best_bg) || (bg == best_bg && newer(e, cur))
                }
            };
            if replace {
                best = Some(e);
            }
        }
        best.cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::KIND_INTERACTIVE;
    use crate::registry::tests::temp_dir;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_transcript(dir: &Path, id: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    // The tail Claude Code 2.1.319 wrote when it moved a session into the
    // background (ids shortened).
    const COST_LINE: &str = r#"{"type":"cost-state","sessionId":"old","totalCostUSD":30.4}"#;
    const MOVED_LINE: &str = r#"{"type":"continued-in","timestamp":"2026-11-09T03:14:51.363Z","sessionId":"old","continuedInSessionId":"new"}"#;

    #[test]
    fn test_continued_in() {
        let tmp = temp_dir();
        let dir = tmp.path();

        let twice = write_transcript(
            dir,
            "a",
            &[
                MOVED_LINE,
                r#"{"type":"continued-in","sessionId":"a","continuedInSessionId":"newer"}"#,
            ],
        );
        assert_eq!(continued_in(&twice).as_deref(), Some("newer"));

        // The line comes after more than the tail holds of earlier history.
        let big_user = format!(
            r#"{{"type":"user","text":"{}"}}"#,
            "x".repeat(2 * CONTINUED_TAIL as usize)
        );
        let long = write_transcript(dir, "b", &[&big_user, MOVED_LINE]);
        assert_eq!(continued_in(&long).as_deref(), Some("new"));

        // A mention in a message isn't the record.
        let quoted = write_transcript(
            dir,
            "c",
            &[r#"{"type":"user","text":"the \"continued-in\" line"}"#],
        );
        assert_eq!(continued_in(&quoted), None);

        assert_eq!(continued_in(dir.join("none.jsonl")), None);
    }

    #[test]
    fn test_continuation() {
        let tmp = temp_dir();
        let dir = tmp.path();
        write_transcript(dir, "old", &[MOVED_LINE]);
        let transcript_of = |id: &str| Some(dir.join(format!("{id}.jsonl")));
        let outer = Proven {
            entry: Entry {
                pid: 102,
                session_id: "old".into(),
                ..Default::default()
            },
            verdict: Verdict::Live,
        };

        let mut s = Snapshot {
            entries: vec![
                outer.clone(),
                Proven {
                    entry: Entry {
                        pid: 200,
                        session_id: "new".into(),
                        kind: KIND_INTERACTIVE.into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
                Proven {
                    entry: Entry {
                        pid: 905,
                        session_id: "new".into(),
                        kind: KIND_BACKGROUND.into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
                Proven {
                    entry: Entry {
                        pid: 106,
                        session_id: "job".into(),
                        kind: KIND_BACKGROUND.into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
            ],
            problems: Vec::new(),
        };

        let e = s.continuation(&outer, transcript_of).expect("moved");
        assert_eq!(e.pid, 905);

        // Not moved: no continuation.
        let job = Proven {
            entry: Entry {
                pid: 106,
                session_id: "job".into(),
                ..Default::default()
            },
            verdict: Verdict::Live,
        };
        assert!(s.continuation(&job, transcript_of).is_none());

        // Moved twice: the conversation goes on in the last.
        write_transcript(
            dir,
            "new",
            &[r#"{"type":"continued-in","sessionId":"new","continuedInSessionId":"newer"}"#],
        );
        s.entries.push(Proven {
            entry: Entry {
                pid: 907,
                session_id: "newer".into(),
                kind: KIND_BACKGROUND.into(),
                ..Default::default()
            },
            verdict: Verdict::Live,
        });
        let e = s.continuation(&outer, transcript_of).expect("moved twice");
        assert_eq!(e.pid, 907);

        // The daemon retired the background session: no process holds it, but
        // the conversation is still that session, not the old one.
        s.entries.truncate(4);
        s.entries[2].verdict = Verdict::Stale;
        s.entries[1].verdict = Verdict::Stale;
        let e = s.continuation(&outer, transcript_of).expect("retired");
        assert_eq!(e.session_id, "newer");
        assert_eq!(e.pid, 0);
        assert_eq!(e.verdict, Verdict::Stale);

        // A loop of continued-in lines ends.
        write_transcript(
            dir,
            "newer",
            &[r#"{"type":"continued-in","sessionId":"newer","continuedInSessionId":"old"}"#],
        );
        assert!(s.continuation(&outer, transcript_of).is_some());
    }

    #[test]
    fn test_find_transcript() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = temp_dir();
        let cfg = tmp.path();
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        // SAFETY: serialized by ENV_LOCK inside this test module.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", cfg);
        }

        let proj = cfg.join("projects").join("-home-user-dev");
        std::fs::create_dir_all(&proj).unwrap();
        let path = write_transcript(&proj, "abc-123", &[COST_LINE]);

        let got = find_transcript("abc-123");
        let missing_cases: Vec<_> = ["", "missing", "*", "../x"]
            .into_iter()
            .map(|id| (id, find_transcript(id)))
            .collect();

        // Restore env before assertions.
        // SAFETY: serialized by ENV_LOCK inside this test module.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
        }

        assert_eq!(got.as_deref(), Some(path.as_path()));
        for (id, found) in missing_cases {
            assert_eq!(found, None, "{id:?}");
        }
    }
}

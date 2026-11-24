//! The session corpus (docs/INDEX-V2.md, "Sessions as full text"): every
//! Claude and agy session, read through the transcript event model
//! (`agent_transcript`), scrubbed at the point of reading
//! (`agent_dream::scrub`: pasted and harness-inserted blocks, code blocks,
//! quotes, private keys, and lines with a credential, an email address or a
//! sign-in URL dropped), and cut by turn into chunks of about 500 tokens with
//! a small overlap. A session is an entity `session:<harness>:<id>`, with
//! `touched` edges to the files it read or changed.
//!
//! This corpus is personal: it lives in the local database only.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use agent_dream::scrub;
use serde::Serialize;

use super::Pg;
use super::sync::{SideChunk, SideNote};
use crate::config::Sessions as Cfg;
use crate::crawl::hash;
use crate::extract::{SEED_MAX, clip};

/// What the reader's way of making a session's text is; a change reads
/// every session again.
const READER_VERSION: &str = "s2";

/// A session as the caller listed it (`agent sessions`' discovery).
#[derive(Debug, Clone, Default)]
pub struct SessionRef {
    pub harness: String,
    pub session_id: String,
    /// The transcript.
    pub path: String,
    pub cwd: String,
    pub title: String,
    pub size: i64,
    /// Its last write (RFC 3339), part of its key.
    pub mtime: String,
    /// The session that started it, for a subagent's ("" otherwise).
    pub parent: String,
}

/// What reading the sessions did, scrub counts included.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionsReport {
    pub listed: usize,
    /// Left out: outside `[sessions].only_cwd`, when that is set.
    pub not_asked: usize,
    /// Left out: run from a scripted batch's directory (`[sessions].skip_cwd`).
    pub scripted: usize,
    /// Left out: a subagent's session (`[sessions].include_subagents` off).
    pub subagents: usize,
    /// Left out: over `[sessions].max_mb`.
    pub too_large: usize,
    /// Unchanged since the last sync: not read again.
    pub kept: usize,
    pub read: usize,
    /// Transcripts that couldn't be read (said, and left out).
    pub failed: usize,
    pub turns: usize,
    pub chunks: usize,
    pub prompts: usize,
    pub replies: usize,
    pub tool_lines: usize,
    /// Lines of prompts and replies before and after the scrubber.
    pub lines_in: usize,
    pub lines_kept: usize,
    /// Of those dropped: lines with a credential, an email address or a
    /// sign-in URL (tool lines included).
    pub credential_lines: usize,
    /// Pasted blocks and harness-inserted blocks dropped whole.
    pub pasted_blocks: usize,
    pub inserted_blocks: usize,
    /// Characters of text kept.
    pub chars: usize,
}

/// The key a session's row was written from (its file's size and last write,
/// and how it was read).
fn key_of(s: &SessionRef, cfg: &Cfg) -> String {
    hash(&[
        s.path.as_bytes(),
        s.size.to_string().as_bytes(),
        s.mtime.as_bytes(),
        READER_VERSION.as_bytes(),
        cfg.chunk_chars.to_string().as_bytes(),
        cfg.overlap_chars.to_string().as_bytes(),
    ])
}

/// The sessions' keys as the store holds them, by id.
pub fn known_keys(pg: &Pg) -> anyhow::Result<HashMap<String, String>> {
    Ok(pg
        .query(
            "SELECT id, content_key FROM entities WHERE corpus = 'session'",
            &[],
        )?
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect())
}

/// The id a session gets.
pub fn session_id(s: &SessionRef) -> String {
    format!("session:{}:{}", s.harness, s.session_id)
}

/// Scrubs one text and counts what went.
fn scrubbed(text: &str, rep: &mut SessionsReport) -> String {
    rep.pasted_blocks += text.matches("<pasted_content").count();
    rep.inserted_blocks += text.matches("<system-reminder").count()
        + text.matches("<command-").count() / 2
        + text.matches("<local-command-").count() / 2;
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    rep.credential_lines += text.lines().filter(|l| scrub::has_credential(l)).count();
    let out = scrub::excerpt(text, usize::MAX >> 1);
    rep.lines_in += lines;
    rep.lines_kept += out.lines().filter(|l| !l.trim().is_empty()).count();
    out
}

/// One turn's text and what it touched.
#[derive(Default)]
struct Turn {
    index: usize,
    ts: String,
    lines: Vec<String>,
    touched: Vec<(String, String)>,
}

/// A path a tool named, absolute.
fn absolute(target: &str, cwd: &str) -> String {
    let p = Path::new(target);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    };
    crate::crawl::normalize(&p)
}

/// Reads one session into a side note.
fn read_one(
    s: &SessionRef,
    projects: &[(String, String)],
    cfg: &Cfg,
    rep: &mut SessionsReport,
) -> anyhow::Result<SideNote> {
    // Every event (read_file keeps the last N; this N is past any session).
    let f = agent_transcript::read_file(&s.path, None, 1 << 24)?;
    let cwd = if s.cwd.is_empty() {
        f.session.cwd.clone()
    } else {
        s.cwd.clone()
    };
    let mut turns: Vec<Turn> = Vec::new();
    let mut first_prompt = String::new();
    // One event per id, in its latest state (a tool with its result), where
    // it was added: the stream also holds each update.
    let events = agent_transcript::latest(&f.events);
    for ev in &events {
        if ev.abandoned || ev.kind == "request" {
            continue;
        }
        let index = ev.turn.as_ref().map(|t| t.index).unwrap_or(0);
        if turns.last().is_none_or(|t| t.index != index) {
            turns.push(Turn {
                index,
                ts: ev.ts.clone(),
                ..Default::default()
            });
        }
        let t = turns.last_mut().expect("a turn");
        if let Some(p) = &ev.prompt {
            let text = scrubbed(&p.text, rep);
            if !text.trim().is_empty() {
                rep.prompts += 1;
                if first_prompt.is_empty() {
                    first_prompt = text.clone();
                }
                t.lines.push(format!("Person: {text}"));
            }
        } else if let Some(say) = &ev.say {
            if say.provisional {
                continue;
            }
            let text = scrubbed(&say.text, rep);
            if !text.trim().is_empty() {
                rep.replies += 1;
                t.lines.push(format!("Agent: {text}"));
            }
        } else if let Some(tool) = &ev.tool {
            if tool.action == agent_transcript::ACTION_NOISE {
                continue;
            }
            let what = if tool.target.is_empty() {
                clip(&tool.detail.replace('\n', " "), 120)
            } else {
                tool.target.clone()
            };
            let status = tool
                .result
                .as_ref()
                .map(|r| r.status.as_str())
                .filter(|s| !s.is_empty() && *s != "ok" && *s != "success")
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            let line = format!("- {} {what}{status}", tool.action);
            if scrub::has_credential(&line) {
                rep.credential_lines += 1;
                continue;
            }
            rep.tool_lines += 1;
            t.lines.push(line);
            if matches!(tool.action.as_str(), "read" | "edit" | "write") && !tool.target.is_empty()
            {
                t.touched
                    .push((absolute(&tool.target, &cwd), tool.action.clone()));
            }
        }
    }
    turns.retain(|t| !t.lines.is_empty());
    rep.turns += turns.len();
    // Chunks: whole turns gathered up to chunk_chars; a longer turn cut in
    // windows of lines, each starting with the last overlap_chars of the one
    // before.
    let project = projects
        .iter()
        .filter(|(_, p)| cwd == *p || cwd.starts_with(&format!("{p}/")))
        .max_by_key(|(_, p)| p.len())
        .map(|(n, _)| n.clone())
        .unwrap_or_default();
    let mut chunks: Vec<SideChunk> = Vec::new();
    let mut touched_all: Vec<(String, String)> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut cur_len = 0;
    let mut cur_turns: (usize, usize) = (0, 0);
    let mut cur_ts = String::new();
    let mut cur_files: BTreeSet<String> = BTreeSet::new();
    let flush = |cur: &mut Vec<String>,
                 cur_len: &mut usize,
                 turns: (usize, usize),
                 ts: &str,
                 files: &mut BTreeSet<String>,
                 chunks: &mut Vec<SideChunk>| {
        if cur.is_empty() {
            return;
        }
        let text = cur.join("\n");
        chunks.push(SideChunk {
            line_start: turns.0,
            line_end: turns.1,
            heading: format!("turns {}..{}", turns.0, turns.1),
            text,
            meta: serde_json::json!({
                "session": s.session_id,
                "harness": s.harness,
                "project": project,
                "time": ts,
                "turns": [turns.0, turns.1],
                "files": files.iter().collect::<Vec<_>>(),
            }),
        });
        cur.clear();
        *cur_len = 0;
        files.clear();
    };
    for t in &turns {
        touched_all.extend(t.touched.iter().cloned());
        let head = format!("## Turn {} {}", t.index, t.ts);
        let turn_len: usize = t.lines.iter().map(|l| l.len() + 1).sum::<usize>() + head.len();
        if !cur.is_empty() && cur_len + turn_len > cfg.chunk_chars {
            flush(
                &mut cur,
                &mut cur_len,
                cur_turns,
                &cur_ts,
                &mut cur_files,
                &mut chunks,
            );
        }
        if cur.is_empty() {
            cur_turns = (t.index, t.index);
            cur_ts = t.ts.clone();
        }
        cur_turns.1 = t.index;
        cur_files.extend(t.touched.iter().map(|(p, _)| p.clone()));
        cur.push(head);
        cur_len += t.index.to_string().len() + 10;
        for l in &t.lines {
            // A long line is cut; a full window goes, keeping its tail.
            let mut rest = l.as_str();
            while !rest.is_empty() {
                let room = cfg.chunk_chars.saturating_sub(cur_len).max(200);
                let mut cut = rest.len().min(room);
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                cur.push(rest[..cut].to_string());
                cur_len += cut + 1;
                rest = &rest[cut..];
                if cur_len >= cfg.chunk_chars {
                    let tail: String = {
                        let all = cur.join("\n");
                        let start = all.len().saturating_sub(cfg.overlap_chars);
                        let mut st = start;
                        while !all.is_char_boundary(st) {
                            st += 1;
                        }
                        all[st..].to_string()
                    };
                    flush(
                        &mut cur,
                        &mut cur_len,
                        cur_turns,
                        &cur_ts,
                        &mut cur_files,
                        &mut chunks,
                    );
                    cur_turns = (t.index, t.index);
                    cur_ts = t.ts.clone();
                    cur.push(format!("…{tail}"));
                    cur_len = tail.len() + 1;
                }
            }
        }
    }
    flush(
        &mut cur,
        &mut cur_len,
        cur_turns,
        &cur_ts,
        &mut cur_files,
        &mut chunks,
    );
    rep.chunks += chunks.len();
    rep.chars += chunks.iter().map(|c| c.text.len()).sum::<usize>();
    let title = if !s.title.is_empty() {
        s.title.clone()
    } else if !f.session.title.is_empty() {
        f.session.title.clone()
    } else {
        clip(first_prompt.lines().next().unwrap_or(""), 80)
    };
    let (first_ts, last_ts) = (
        turns.first().map(|t| t.ts.clone()).unwrap_or_default(),
        turns.last().map(|t| t.ts.clone()).unwrap_or_default(),
    );
    Ok(SideNote {
        id: session_id(s),
        corpus: "session".into(),
        kind: "session".into(),
        key: key_of(s, cfg),
        project,
        name: s.session_id.clone(),
        path: s.path.clone(),
        title: scrub::excerpt(&title, 200),
        seed: clip(&first_prompt.replace('\n', " "), SEED_MAX),
        chunks,
        touched: touched_all,
        status: format!(
            "{}, {} turns, {first_ts} to {last_ts}",
            s.harness,
            turns.len()
        ),
        ..Default::default()
    })
}

/// The sessions as side notes: each one listed and not left out, read
/// (or kept, when its key is what `known` holds for it).
pub fn session_notes(
    refs: &[SessionRef],
    known: &HashMap<String, String>,
    projects: &[(String, String)],
    cfg: &Cfg,
) -> (Vec<SideNote>, SessionsReport) {
    let mut rep = SessionsReport {
        listed: refs.len(),
        ..Default::default()
    };
    let mut out = Vec::new();
    for s in refs {
        if !cfg.only_cwd.is_empty() && !cfg.only_cwd.iter().any(|p| s.cwd.starts_with(p.as_str())) {
            rep.not_asked += 1;
            continue;
        }
        if cfg
            .skip_cwd
            .iter()
            .any(|p| !p.is_empty() && s.cwd.contains(p.as_str()))
        {
            rep.scripted += 1;
            continue;
        }
        if !s.parent.is_empty() && !cfg.include_subagents {
            rep.subagents += 1;
            continue;
        }
        if cfg.max_mb > 0 && s.size > (cfg.max_mb as i64) << 20 {
            rep.too_large += 1;
            continue;
        }
        let id = session_id(s);
        if known.get(&id) == Some(&key_of(s, cfg)) {
            rep.kept += 1;
            out.push(SideNote {
                id,
                corpus: "session".into(),
                keep: true,
                ..Default::default()
            });
            continue;
        }
        match read_one(s, projects, cfg, &mut rep) {
            Ok(n) => {
                rep.read += 1;
                out.push(n);
            }
            Err(_) => rep.failed += 1,
        }
    }
    (out, rep)
}

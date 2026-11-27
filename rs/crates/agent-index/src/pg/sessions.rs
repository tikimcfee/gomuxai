//! The session corpus (docs/INDEX-V2.md, "Sessions as full text"): every
//! Claude and agy session, read through the transcript event model
//! (`agent_transcript`) and cut by turn into chunks of about 500 tokens with
//! a small overlap. A session is an entity `session:<harness>:<id>`, with
//! `touched` edges to the files it read or changed.
//!
//! Nothing is dropped (2026-11-27: a reader that sees text cut out
//! assumes, and writes down something not quite true). Secrets are redacted
//! in place everywhere, tool lines and titles too (`agent_dream::scrub::redact`),
//! and harness-inserted blocks are labelled as data (`label_blocks`).
//! Pasted blocks are kept as well, apart: a session's pasted blocks are one
//! more entity, `session:<harness>:<id>#pasted`, in the corpus `pasted`, so
//! a search can leave them out (`--corpus` without it). The session's own
//! text says where each one was.
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
const READER_VERSION: &str = "s3";

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
    /// Secrets redacted in place (prompts, replies, tool lines, titles).
    pub redacted: usize,
    /// Pasted blocks, kept apart (in the sessions' `#pasted` entities).
    pub pasted_blocks: usize,
    /// Sessions with pasted blocks: a `#pasted` entity each.
    pub pasted_notes: usize,
    /// Harness-inserted blocks, labelled as data.
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
            "SELECT id, content_key FROM entities WHERE corpus IN ('session', 'pasted')",
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

/// The id of a session's pasted blocks.
pub fn pasted_id(s: &SessionRef) -> String {
    format!("{}#pasted", session_id(s))
}

/// `text` with its secrets redacted in place, counted.
fn redacted(text: &str, rep: &mut SessionsReport) -> String {
    let out = scrub::redact(text);
    rep.redacted += out
        .matches("[redacted: ")
        .count()
        .saturating_sub(text.matches("[redacted: ").count());
    out
}

/// One prompt's or reply's text as the index reads it: harness blocks
/// labelled as data and secrets redacted, every word kept. Each pasted block
/// is taken out to the session's `#pasted` entity (labelled, redacted, as
/// `pasted N: ...`, numbered across the session from `*n`), and a marker
/// naming it is left where it was. Returns the text and the blocks.
fn read_text(
    text: &str,
    rep: &mut SessionsReport,
    n: &mut usize,
    pasted_at: &str,
) -> (String, Vec<String>) {
    const OPEN: &str = "<pasted_content";
    const CLOSE: &str = "</pasted_content>";
    rep.inserted_blocks += text.matches("<system-reminder").count()
        + text.matches("<command-").count() / 2
        + text.matches("<local-command-").count() / 2;
    let mut own = String::with_capacity(text.len());
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(OPEN) {
        let after = &rest[i + OPEN.len()..];
        // `<pasted_contents>` or the like isn't the tag.
        if !after.starts_with(|c: char| c == '>' || c.is_whitespace()) {
            own.push_str(&rest[..i + OPEN.len()]);
            rest = after;
            continue;
        }
        own.push_str(&rest[..i]);
        let (block, next) = match after.find(CLOSE) {
            Some(j) => (
                &rest[i..i + OPEN.len() + j + CLOSE.len()],
                &after[j + CLOSE.len()..],
            ),
            // Unclosed: the rest is the block (label_blocks says so).
            None => (&rest[i..], ""),
        };
        *n += 1;
        rep.pasted_blocks += 1;
        let b = redacted(&scrub::label_blocks(block), rep);
        blocks.push(format!("pasted {n}: {b}"));
        own.push_str(&format!("⟦pasted content {n}: kept apart, in {pasted_at}⟧"));
        rest = next;
    }
    own.push_str(rest);
    (redacted(&scrub::label_blocks(&own), rep), blocks)
}

/// One turn's text, its pasted blocks, and what it touched.
#[derive(Default)]
struct Turn {
    index: usize,
    ts: String,
    lines: Vec<String>,
    pasted: Vec<String>,
    touched: Vec<(String, String)>,
}

/// A chunk's worth of turns.
struct Window {
    turns: (usize, usize),
    ts: String,
    text: String,
    files: BTreeSet<String>,
}

/// The windows of some of the turns' lines (`pick`: their own, or their
/// pasted blocks): whole turns gathered up to `chunk_chars`; a longer turn
/// cut in windows of lines, each starting with the last `overlap_chars` of
/// the one before. A turn with none of those lines is passed over.
fn windows(
    turns: &[Turn],
    pick: fn(&Turn) -> &[String],
    with_files: bool,
    cfg: &Cfg,
) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
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
                 out: &mut Vec<Window>| {
        if cur.is_empty() {
            return;
        }
        out.push(Window {
            turns,
            ts: ts.to_string(),
            text: cur.join("\n"),
            files: std::mem::take(files),
        });
        cur.clear();
        *cur_len = 0;
    };
    for t in turns {
        let lines = pick(t);
        if lines.is_empty() {
            continue;
        }
        let head = format!("## Turn {} {}", t.index, t.ts);
        let turn_len: usize = lines.iter().map(|l| l.len() + 1).sum::<usize>() + head.len();
        if !cur.is_empty() && cur_len + turn_len > cfg.chunk_chars {
            flush(
                &mut cur,
                &mut cur_len,
                cur_turns,
                &cur_ts,
                &mut cur_files,
                &mut out,
            );
        }
        if cur.is_empty() {
            cur_turns = (t.index, t.index);
            cur_ts = t.ts.clone();
        }
        cur_turns.1 = t.index;
        if with_files {
            cur_files.extend(t.touched.iter().map(|(p, _)| p.clone()));
        }
        cur.push(head);
        cur_len += t.index.to_string().len() + 10;
        for l in lines {
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
                        &mut out,
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
        &mut out,
    );
    out
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

/// Reads one session into its side note, and a second for its pasted
/// blocks when it has any.
fn read_one(
    s: &SessionRef,
    projects: &[(String, String)],
    cfg: &Cfg,
    rep: &mut SessionsReport,
) -> anyhow::Result<Vec<SideNote>> {
    let pasted_at = pasted_id(s);
    let mut n_pasted = 0;
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
            let (text, blocks) = read_text(&p.text, rep, &mut n_pasted, &pasted_at);
            if !text.trim().is_empty() {
                rep.prompts += 1;
                if first_prompt.is_empty() {
                    first_prompt = text.clone();
                }
                t.lines.push(format!("Person: {text}"));
            }
            t.pasted
                .extend(blocks.into_iter().map(|b| format!("Person, {b}")));
        } else if let Some(say) = &ev.say {
            if say.provisional {
                continue;
            }
            let (text, blocks) = read_text(&say.text, rep, &mut n_pasted, &pasted_at);
            if !text.trim().is_empty() {
                rep.replies += 1;
                t.lines.push(format!("Agent: {text}"));
            }
            t.pasted
                .extend(blocks.into_iter().map(|b| format!("Agent, {b}")));
        } else if let Some(tool) = &ev.tool {
            if tool.action == agent_transcript::ACTION_NOISE {
                continue;
            }
            // Redacted before it is clipped, so a cut can't leave part of a
            // secret too short to be seen as one.
            let what = if tool.target.is_empty() {
                clip(&redacted(&tool.detail, rep).replace('\n', " "), 120)
            } else {
                redacted(&tool.target, rep)
            };
            let status = tool
                .result
                .as_ref()
                .map(|r| r.status.as_str())
                .filter(|s| !s.is_empty() && *s != "ok" && *s != "success")
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            let line = format!("- {} {what}{status}", tool.action);
            rep.tool_lines += 1;
            t.lines.push(line);
            if matches!(tool.action.as_str(), "read" | "edit" | "write") && !tool.target.is_empty()
            {
                t.touched
                    .push((absolute(&tool.target, &cwd), tool.action.clone()));
            }
        }
    }
    turns.retain(|t| !t.lines.is_empty() || !t.pasted.is_empty());
    rep.turns += turns.len();
    let project = projects
        .iter()
        .filter(|(_, p)| cwd == *p || cwd.starts_with(&format!("{p}/")))
        .max_by_key(|(_, p)| p.len())
        .map(|(n, _)| n.clone())
        .unwrap_or_default();
    let chunk = |w: Window, pasted: bool| {
        let mut meta = serde_json::json!({
            "session": s.session_id,
            "harness": s.harness,
            "project": project,
            "time": w.ts,
            "turns": [w.turns.0, w.turns.1],
            "files": w.files.iter().collect::<Vec<_>>(),
        });
        if pasted {
            meta["pasted"] = serde_json::Value::Bool(true);
        }
        SideChunk {
            line_start: w.turns.0,
            line_end: w.turns.1,
            heading: format!("turns {}..{}", w.turns.0, w.turns.1),
            text: w.text,
            meta,
        }
    };
    let chunks: Vec<SideChunk> = windows(&turns, |t| t.lines.as_slice(), true, cfg)
        .into_iter()
        .map(|w| chunk(w, false))
        .collect();
    let pasted_chunks: Vec<SideChunk> = windows(&turns, |t| t.pasted.as_slice(), false, cfg)
        .into_iter()
        .map(|w| chunk(w, true))
        .collect();
    let touched_all: Vec<(String, String)> = turns
        .iter()
        .flat_map(|t| t.touched.iter().cloned())
        .collect();
    rep.chunks += chunks.len() + pasted_chunks.len();
    rep.chars += chunks
        .iter()
        .chain(&pasted_chunks)
        .map(|c| c.text.len())
        .sum::<usize>();
    // Redacted before it is clipped (first_prompt is already).
    let title = if !s.title.is_empty() {
        clip(&redacted(&s.title.replace('\n', " "), rep), 200)
    } else if !f.session.title.is_empty() {
        clip(&redacted(&f.session.title.replace('\n', " "), rep), 200)
    } else {
        clip(first_prompt.lines().next().unwrap_or(""), 80)
    };
    let (first_ts, last_ts) = (
        turns.first().map(|t| t.ts.clone()).unwrap_or_default(),
        turns.last().map(|t| t.ts.clone()).unwrap_or_default(),
    );
    let mut out = Vec::new();
    if !pasted_chunks.is_empty() {
        rep.pasted_notes += 1;
        let first = turns
            .iter()
            .find_map(|t| t.pasted.first())
            .map(|b| b.replace('\n', " "))
            .unwrap_or_default();
        out.push(SideNote {
            id: pasted_at.clone(),
            corpus: "pasted".into(),
            kind: "pasted".into(),
            key: key_of(s, cfg),
            project: project.clone(),
            name: format!("{}#pasted", s.session_id),
            path: s.path.clone(),
            title: clip(&format!("Pasted into: {title}"), 200),
            seed: clip(&first, SEED_MAX),
            chunks: pasted_chunks,
            status: format!("{}, {n_pasted} pasted blocks", s.harness),
            ..Default::default()
        });
    }
    out.insert(
        0,
        SideNote {
            id: session_id(s),
            corpus: "session".into(),
            kind: "session".into(),
            key: key_of(s, cfg),
            project,
            name: s.session_id.clone(),
            path: s.path.clone(),
            title,
            seed: clip(&first_prompt.replace('\n', " "), SEED_MAX),
            chunks,
            touched: touched_all,
            status: format!(
                "{}, {} turns, {first_ts} to {last_ts}",
                s.harness,
                turns.len()
            ),
            ..Default::default()
        },
    );
    Ok(out)
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
        let key = key_of(s, cfg);
        if known.get(&id) == Some(&key) {
            rep.kept += 1;
            out.push(SideNote {
                id,
                corpus: "session".into(),
                keep: true,
                ..Default::default()
            });
            // Its pasted blocks, when it has them, stay with it.
            let pasted = pasted_id(s);
            if known.get(&pasted) == Some(&key) {
                out.push(SideNote {
                    id: pasted,
                    corpus: "pasted".into(),
                    keep: true,
                    ..Default::default()
                });
            }
            continue;
        }
        match read_one(s, projects, cfg, &mut rep) {
            Ok(n) => {
                rep.read += 1;
                out.extend(n);
            }
            Err(_) => rep.failed += 1,
        }
    }
    (out, rep)
}

use std::collections::HashSet;
use std::os::unix::fs::FileExt;

use agent_base::gostd::filepath;
use agent_base::gostd::os::PathError;
use chrono::{DateTime, FixedOffset};

use crate::agy::{Agy, agy_conversation_dir, agy_file_options};
use crate::claude::{Claude, claude_subagent_turns};
use crate::events::{Event, OP_ADD, Options, Reader, Session};
use crate::kimi::{Kimi, kimi_file_options};
use crate::meta::read_claude_subagent_meta;

/// Reads an RFC 3339 timestamp, or returns `None`.
pub(crate) fn parse_time(s: &str) -> Option<DateTime<FixedOffset>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s).ok()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Guesses a transcript's format from its first lines: Kimi Code's records are
/// typed like `context.append_loop_event` and `turn.prompt`, after a metadata
/// line naming its `protocol_version`; agy's steps carry `step_index` and step
/// types like `PLANNER_RESPONSE`; anything else is read as Claude Code's.
pub fn detect(data: &[u8]) -> &'static dyn Reader {
    for (n, line) in data.split(|&b| b == b'\n').enumerate() {
        if n >= 30 {
            break;
        }
        if contains_bytes(line, br#""context.append_loop_event""#)
            || contains_bytes(line, br#""turn.prompt""#)
            || (contains_bytes(line, br#""protocol_version""#)
                && contains_bytes(line, br#""metadata""#))
        {
            return &Kimi;
        }
        if contains_bytes(line, br#""step_index""#)
            || contains_bytes(line, br#""PLANNER_RESPONSE""#)
            || contains_bytes(line, br#""USER_INPUT""#)
        {
            return &Agy;
        }
    }
    &Claude
}

/// Returns the [`Options`] for parsing the transcript at `path` with
/// `harness`'s reader: what the files beside it say that its records don't.
/// For a Claude subagent, the lookup of its parent's turn indexes
/// (`claude_subagent_turns`); for agy, which log it is and, for a subagent, its
/// parent conversation and the call and turn that started it; for Kimi Code,
/// the session's title and working directory, and a subagent's parent session.
/// [`read_file`] parses with these, and so should a resumed parse
/// ([`crate::resume_after`]).
pub fn file_options(harness: &str, path: &str) -> Options {
    match harness {
        "claude" => Options {
            turns: claude_subagent_turns(path),
            ..Options::default()
        },
        "agy" => agy_file_options(path),
        "kimi" => kimi_file_options(path),
        _ => Options::default(),
    }
}

/// Returned for a transcript none of whose lines is a JSON object: not a format
/// any [`Reader`] knows. Its text is the Go's whole message,
/// "unparseable transcript: none of its N lines is a JSON object" (the Go
/// wraps its `ErrUnparseable` with `%w` at the front), so `{:#}` prints it
/// once; check for it with `downcast_ref::<ErrUnparseable>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unparseable transcript: none of its {lines} lines is a JSON object")]
pub struct ErrUnparseable {
    /// The transcript's non-blank lines.
    pub lines: usize,
}

/// Read sizes for [`read_file`]: how much it reads at a time, and how much of
/// the head it reads to detect the format.
pub(crate) const READ_CHUNK: usize = 1 << 20;
const HEAD_CHUNK: usize = 256 << 10;

/// A transcript [`read_file`] read.
#[derive(Debug, Clone)]
pub struct File {
    pub session: Session,
    /// The end of the stream the transcript makes, updates included: its last
    /// events, as [`crate::latest`] counts them, and the updates to them.
    pub events: Vec<Event>,
    /// Set when the stream had more events than `events` keeps.
    pub partial: bool,
    /// Counts the non-blank lines read; `skipped`, those that weren't records
    /// of the format.
    pub lines: usize,
    pub skipped: usize,
}

/// Reads the transcript at `path` with `r` and keeps the last `min_events`
/// events it makes (counted as [`crate::latest`] counts them), with their
/// updates. It parses the whole file, since what a line means depends on the
/// lines before it, but holds only those events. A last line with no newline
/// yet is left out. A `None` `r` detects the format from the file's first
/// lines.
pub fn read_file(path: &str, r: Option<&dyn Reader>, min_events: usize) -> anyhow::Result<File> {
    // Go's os errors: "open <path>: no such file or directory".
    let f = std::fs::File::open(path).map_err(|e| PathError::new("open", path, e))?;
    let info = f.metadata().map_err(|e| PathError::new("stat", path, e))?;
    let size = info.len() as i64;
    let read_error =
        |e| anyhow::Error::new(PathError::new("read", path, e)).context("reading transcript");
    let reader: &dyn Reader = match r {
        Some(r) => r,
        None => {
            let head_len = (HEAD_CHUNK as i64).min(size).max(0) as usize;
            let mut head = vec![0u8; head_len];
            let n = read_at_some(&f, &mut head, 0).map_err(read_error)?;
            detect(&head[..n])
        }
    };

    let mut p = reader.new_parser(file_options(reader.harness(), path));
    let mut t = Tail::new(min_events);
    // pending holds the bytes read but not yet parsed: a line not yet whole.
    // It starts at p.pos().at.
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut off: i64 = 0;
    while off < size {
        let want = (buf.len() as i64).min(size - off) as usize;
        let n = read_at_some(&f, &mut buf[..want], off as u64).map_err(read_error)?;
        off += n as i64;
        pending.extend_from_slice(&buf[..n]);
        let start = p.pos().at;
        for e in p.feed(&pending) {
            t.push(e);
        }
        let consumed = (p.pos().at - start) as usize;
        pending.drain(..consumed);
        if n == 0 {
            break;
        }
    }
    if p.lines > 0 && p.skipped == p.lines {
        return Err(ErrUnparseable { lines: p.lines }.into());
    }

    let mut session = p.session();
    session.file(path, size);
    Ok(File {
        session,
        events: t.events(),
        partial: t.cut,
        lines: p.lines,
        skipped: p.skipped,
    })
}

/// Reads at `offset` until either `buf` is filled or EOF is reached (matching
/// Go's `os.File.ReadAt`, which loops until full or EOF).
pub(crate) fn read_at_some(
    f: &std::fs::File,
    buf: &mut [u8],
    mut offset: u64,
) -> std::io::Result<usize> {
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

/// Keeps the end of a stream: the events of the last `n` ids it adds, and the
/// updates to them.
struct Tail {
    n: usize,
    stream: Vec<Event>,
    /// Lists the kept ids, oldest first; `kept` holds them.
    added: Vec<String>,
    kept: HashSet<String>,
    /// Set once an id was dropped.
    cut: bool,
}

impl Tail {
    fn new(n: usize) -> Self {
        Self {
            n,
            stream: Vec::new(),
            added: Vec::new(),
            kept: HashSet::new(),
            cut: false,
        }
    }

    fn push(&mut self, e: Event) {
        if !self.kept.contains(&e.id) {
            if e.op != OP_ADD {
                return; // an update of a dropped event
            }
            self.kept.insert(e.id.clone());
            self.added.push(e.id.clone());
        }
        self.stream.push(e);
        if self.added.len() > self.n {
            let dropped = self.added.remove(0);
            self.kept.remove(&dropped);
            self.cut = true;
        }
        if self.stream.len() > 2 * self.n.saturating_add(512) {
            self.stream = self.events();
        }
    }

    /// Returns the kept events, in stream order.
    fn events(&self) -> Vec<Event> {
        self.stream
            .iter()
            .filter(|e| self.kept.contains(&e.id))
            .cloned()
            .collect()
    }
}

impl Session {
    /// Records in `self` the transcript file it was read from, and what the
    /// file's name and neighbours say: the session id, if no record named it,
    /// and a Claude subagent's `meta.json`.
    pub fn file(&mut self, path: &str, size: i64) {
        self.path = path.to_string();
        self.size = size;
        if self.session_id.is_empty() {
            self.session_id = session_id_from_path(path);
        }
        if self.parent_session.is_some()
            && let Some(meta) = read_claude_subagent_meta(path)
        {
            self.harvest_claude_subagent(&meta);
        }
    }
}

/// Names a session after its transcript's path:
///   - Claude Code: `<session id>.jsonl`, and a subagent's
///     `<session id>/subagents/agent-<id>.jsonl` (a workflow agent's
///     `.../subagents/workflows/<run>/agent-<id>.jsonl`) is `agent-<id>`;
///   - agy: `brain/<conversation id>/.system_generated/logs/transcript*.jsonl`,
///     or `brain/<conversation id>/transcript*.jsonl`;
///   - Kimi Code: `session_<uuid>/agents/main/wire.jsonl` is `<uuid>`, and a
///     subagent's `agents/<agent>/wire.jsonl` is `<uuid>:<agent>`.
///
/// Returns `""` for a path of none of these shapes.
pub fn session_id_from_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let base = filepath::base(path);
    let agent_dir = filepath::dir(path);
    if base == "wire.jsonl" && filepath::base(&filepath::dir(&agent_dir)) == "agents" {
        let session_dir = filepath::base(&filepath::dir(&filepath::dir(&agent_dir)));
        let session = session_dir.strip_prefix("session_").unwrap_or(&session_dir);
        let agent = filepath::base(&agent_dir);
        if agent != "main" {
            return format!("{session}:{agent}");
        }
        return session.to_string();
    }
    let ext = filepath::ext(&base);
    let stem = base.strip_suffix(&ext).unwrap_or(&base);
    if !stem.starts_with("transcript") {
        return stem.to_string();
    }
    let conv = agy_conversation_dir(path);
    if !conv.is_empty() {
        return filepath::base(&conv);
    }
    String::new()
}

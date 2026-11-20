use std::collections::HashSet;
use std::os::unix::fs::FileExt;

use agent_base::gostd::filepath;
use agent_transcript::{
    ParentSession, claude_meta_record, claude_subagent_session, harvest_claude,
    read_claude_subagent_meta, trim_space,
};

use crate::dirs::{HARNESS_CLAUDE, entry_name, os_path, read_dir_sorted};
use crate::resolve::canonical;
use crate::sessions::{FileStat, Session, SubState, from_meta, stub};

/// Lists the sessions under a projects dir, unread (stubs): `<dir>/*.jsonl`,
/// `<dir>/<project>/*.jsonl`, and their subagents.
pub(crate) fn list_claude(dir: &str) -> Vec<Session> {
    let mut out = Vec::new();
    let add = |folder: &str, project: &str, out: &mut Vec<Session>| {
        let Ok(entries) = read_dir_sorted(folder) else {
            return;
        };
        let canon = canonical(folder);
        for e in entries {
            let name = entry_name(folder, &e);
            let path = filepath::join(&[folder, &name]);
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_dir() {
                out.extend(list_claude_subagents(
                    &filepath::join(&[&path, "subagents"]),
                    project,
                ));
                continue;
            }
            if !name.ends_with(".jsonl") {
                continue;
            }
            if let Ok(info) = e.metadata()
                && info.len() > 0
            {
                let mut s = stub(HARNESS_CLAUDE, &path, FileStat::from_metadata(&info));
                s.folder = project.to_string();
                if !ft.is_symlink() {
                    s.key = filepath::join(&[&canon, &name]);
                }
                out.push(s);
            }
        }
    };
    add(dir, "", &mut out);
    if let Ok(entries) = read_dir_sorted(dir) {
        for e in entries {
            if e.file_type().is_ok_and(|ft| ft.is_dir()) {
                let name = entry_name(dir, &e);
                add(&filepath::join(&[dir, &name]), &name, &mut out);
            }
        }
    }
    out
}

fn list_claude_subagents(dir: &str, project: &str) -> Vec<Session> {
    let mut out = Vec::new();
    let mut listed = HashSet::new();
    let parent_id = filepath::base(&filepath::dir(dir));
    let mut add = |folder: &str| {
        let Ok(entries) = read_dir_sorted(folder) else {
            return;
        };
        let canon = canonical(folder);
        for e in entries {
            let name = entry_name(folder, &e);
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_dir()
                || !name.starts_with("agent-")
                || !name.ends_with(".jsonl")
                || listed.contains(&name)
            {
                continue;
            }
            if let Ok(info) = e.metadata()
                && info.len() > 0
            {
                listed.insert(name.clone());
                let mut s = stub(
                    HARNESS_CLAUDE,
                    &filepath::join(&[folder, &name]),
                    FileStat::from_metadata(&info),
                );
                s.parent_session = Some(ParentSession {
                    session_id: parent_id.clone(),
                    tool_id: String::new(),
                });
                s.folder = project.to_string();
                if !ft.is_symlink() {
                    s.key = filepath::join(&[&canon, &name]);
                }
                out.push(s);
            }
        }
    };
    add(dir);
    let workflows = filepath::join(&[dir, "workflows"]);
    if let Ok(runs) = read_dir_sorted(&workflows) {
        for run in runs {
            if run.file_type().is_ok_and(|ft| ft.is_dir()) {
                let name = entry_name(&workflows, &run);
                add(&filepath::join(&[&workflows, &name]));
            }
        }
    }
    out
}

pub(crate) fn is_claude_subagent(path: &str) -> bool {
    !claude_subagent_session(path).is_empty()
}

pub(crate) fn probe_claude_subagent(path: &str, stat: FileStat) -> Session {
    let mut m = read_claude_file(path, stat.size, true);
    m.parent_session = Some(ParentSession {
        session_id: filepath::base(&claude_subagent_session(path)),
        tool_id: String::new(),
    });
    let meta = read_claude_subagent_meta(path);
    if let Some(ref meta) = meta {
        m.harvest_claude_subagent(meta);
    }
    let mut s = from_meta(HARNESS_CLAUDE, path, stat, m);
    if let Some(meta) = meta
        && let Some(finished) = meta.finished
    {
        s.state = if finished {
            SubState::Finished
        } else {
            SubState::Running
        };
    }
    s
}

pub(crate) const CLAUDE_HEAD: i64 = 256 << 10;
pub(crate) const CLAUDE_TAIL_CHUNK: i64 = 64 << 10;
pub(crate) const CLAUDE_TAIL: i64 = 1 << 20;
pub(crate) const CLAUDE_TITLE_SLACK: i64 = 64 << 10;

pub(crate) fn probe_claude(path: &str, stat: FileStat) -> Session {
    from_meta(
        HARNESS_CLAUDE,
        path,
        stat,
        read_claude_file(path, stat.size, false),
    )
}

fn read_claude_file(path: &str, size: i64, subagent: bool) -> agent_transcript::Session {
    let Ok(f) = std::fs::File::open(os_path(path)) else {
        return agent_transcript::Session::default();
    };
    read_claude(&f, size, subagent)
}

pub(crate) trait ReaderAt {
    fn read_at_some(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize>;
}

impl ReaderAt for std::fs::File {
    fn read_at_some(&self, buf: &mut [u8], mut offset: u64) -> std::io::Result<usize> {
        let mut total = 0;
        while total < buf.len() {
            match self.read_at(&mut buf[total..], offset) {
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
}

impl ReaderAt for [u8] {
    fn read_at_some(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        let off = offset as usize;
        if off >= self.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.len() - off);
        buf[..n].copy_from_slice(&self[off..off + n]);
        Ok(n)
    }
}

pub(crate) fn read_claude<R: ReaderAt + ?Sized>(
    f: &R,
    size: i64,
    subagent: bool,
) -> agent_transcript::Session {
    if size <= 2 * CLAUDE_TAIL_CHUNK {
        let mut data = vec![0u8; size.max(0) as usize];
        let n = f.read_at_some(&mut data, 0).unwrap_or(0);
        return read_claude_inner(&data[..n], n as i64, subagent);
    }
    read_claude_inner(f, size, subagent)
}

fn read_claude_inner<R: ReaderAt + ?Sized>(
    f: &R,
    size: i64,
    subagent: bool,
) -> agent_transcript::Session {
    let mut m = agent_transcript::Session::default();
    let mut ai_title_at: i64 = -1;
    each_line_backward(f, size, CLAUDE_TAIL_CHUNK, CLAUDE_TAIL, |line, at| {
        if !m.model.is_empty()
            && !m.version.is_empty()
            && !m.cwd.is_empty()
            && !claude_meta_record(line)
        {
            return true;
        }
        harvest_claude(&mut m, line, at);
        if ai_title_at < 0 && !m.title.is_empty() {
            ai_title_at = at;
        }
        let meta_done = !m.model.is_empty() && !m.version.is_empty();
        let title_done = subagent
            || m.title_is_custom()
            || (ai_title_at >= 0 && ai_title_at - at > CLAUDE_TITLE_SLACK);
        !(meta_done && title_done)
    });

    let mut head: Vec<u8> = Vec::new();
    let mut first = agent_transcript::Session::default();
    let mut want: i64 = 16 << 10;
    let head_limit = CLAUDE_HEAD.min(size);
    while first.cwd.is_empty() && (head.len() as i64) < head_limit {
        let target = want.min(head_limit) as usize;
        let more_len = target.saturating_sub(head.len());
        if more_len == 0 {
            break;
        }
        let mut more = vec![0u8; more_len];
        let n = f.read_at_some(&mut more, head.len() as u64).unwrap_or(0);
        if n == 0 {
            break;
        }
        head.extend_from_slice(&more[..n]);
        let mut at = 0usize;
        while at < head.len() {
            let rest = &head[at..];
            let line = match rest.iter().position(|&b| b == b'\n') {
                Some(nl) => &rest[..nl],
                None => rest,
            };
            if line.windows(br#""cwd""#.len()).any(|w| w == br#""cwd""#) {
                harvest_claude(&mut first, line, at as i64);
                if !first.cwd.is_empty() {
                    harvest_claude(&mut m, line, at as i64);
                    break;
                }
            }
            at += line.len() + 1;
        }
        want *= 2;
    }
    m
}

/// Calls `fn` with each line of `f`'s first `size` bytes, last line first, with
/// the byte offset it starts at, until `fn` returns `false`.
pub(crate) fn each_line_backward<R: ReaderAt + ?Sized, F: FnMut(&[u8], i64) -> bool>(
    f: &R,
    size: i64,
    chunk: i64,
    limit: i64,
    mut cb: F,
) {
    let floor = (size - limit).max(0);
    let mut carry: Vec<u8> = Vec::new();
    let mut end = size;
    while end > floor {
        let start = (end - chunk).max(floor);
        let len = (end - start) as usize;
        let mut buf = vec![0u8; len];
        let Ok(n) = f.read_at_some(&mut buf, start as u64) else {
            return;
        };
        buf.truncate(n);
        buf.extend_from_slice(&carry);
        while let Some(nl) = buf.iter().rposition(|&b| b == b'\n') {
            let line = trim_space(&buf[nl + 1..]);
            if !line.is_empty() && !cb(line, start + nl as i64 + 1) {
                return;
            }
            buf.truncate(nl);
        }
        carry = buf;
        end = start;
    }
    let line = trim_space(&carry);
    if !line.is_empty() && floor == 0 {
        cb(line, 0);
    }
}

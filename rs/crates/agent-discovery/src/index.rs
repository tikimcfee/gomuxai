use std::collections::{HashMap, HashSet};

use agent_base::gostd::filepath;
use agent_transcript::project_of;
use chrono::{DateTime, Utc};

use crate::r#continue::{r#continue as continue_from_slice, within};
use crate::dirs::{Dir, HARNESS_CLAUDE, HARNESSES};
use crate::resolve::{Agent, Joined, Live, canonical, join, path_key};
use crate::sessions::{Session, list, sort_sessions, stub_at};

/// Every session the scanned dirs hold and the workgroup agents name, as
/// directory listings and stat say: no transcript is read until a caller asks
/// for the sessions it picked ([`Index::resolve`]), or a filter needs what only
/// a transcript says (a Claude session's cwd, for a project).
#[derive(Debug, Clone, Default)]
pub struct Index {
    pub(crate) j: Joined,
    by_key: HashMap<String, usize>,
    by_id: HashMap<String, usize>,
}

/// Lists the sessions in `dirs` and joins them to `agents`, as
/// [`crate::resolve`] does, reading no transcript.
pub fn new_index(dirs: &[Dir], agents: &[Agent]) -> Index {
    let mut x = Index {
        j: join(list(dirs), agents.to_vec()),
        by_key: HashMap::new(),
        by_id: HashMap::new(),
    };
    sort_sessions(&mut x.j.sessions);
    x.reindex();
    x
}

fn id_key(harness: &str, id: &str) -> String {
    format!("{harness}\0{id}")
}

impl Index {
    fn reindex(&mut self) {
        self.by_key.clear();
        self.by_id.clear();
        for (i, s) in self.j.sessions.iter().enumerate() {
            if s.path.is_empty() {
                continue;
            }
            self.by_key.insert(path_key(s), i);
            self.by_id
                .entry(id_key(&s.harness, &s.session_id))
                .or_insert(i);
        }
    }

    /// Returns every session, newest first, as listed.
    pub fn sessions(&self) -> Vec<Session> {
        self.j.sessions.clone()
    }

    /// Names every harness a session has: [`HARNESSES`], and the backend of a
    /// workgroup agent listed with no transcript (`aider`, say).
    pub fn harnesses(&self) -> Vec<String> {
        let mut out: Vec<String> = HARNESSES.iter().map(|s| (*s).to_string()).collect();
        for s in &self.j.sessions {
            if !out.contains(&s.harness) {
                out.push(s.harness.clone());
            }
        }
        out
    }

    /// Returns the session of the transcript at `path`: the listed one, or, for
    /// a transcript outside the scanned dirs, one added to the index. Returns
    /// `None` if no session lists `path` and there is no file there.
    pub fn lookup(&mut self, path: &str) -> Option<Session> {
        if let Some(&i) = self.by_key.get(&canonical(path)) {
            return Some(self.j.sessions[i].clone());
        }
        let s = stub_at(path)?;
        self.j.sessions.push(s.clone());
        sort_sessions(&mut self.j.sessions);
        self.reindex();
        Some(s)
    }

    /// Reads the `picked` sessions and the sessions above each subagent among
    /// them, and sets their workgroup and status by [`crate::resolve`]'s rules.
    /// Returns them newest first, ancestors included.
    pub fn resolve(&mut self, picked: &[Session], live: &Live) -> Vec<Session> {
        let mut idx = Vec::new();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for p in picked {
            if p.path.is_empty() {
                out.push(p.clone()); // a live agent with no transcript: resolved when joined
                continue;
            }
            let mut cur = self.by_key.get(&path_key(p)).copied();
            while let Some(i) = cur {
                if !seen.insert(i) {
                    break;
                }
                idx.push(i);
                let Some(ref parent) = self.j.sessions[i].parent_session else {
                    break;
                };
                cur = self
                    .by_id
                    .get(&id_key(&self.j.sessions[i].harness, &parent.session_id))
                    .copied();
            }
        }
        idx.sort_unstable();
        for i in idx {
            self.j.sessions[i].fill();
            out.push(self.j.sessions[i].clone());
        }
        self.j.status(&mut out, live);
        sort_sessions(&mut out);
        out
    }

    /// Returns the sessions `q` picks, newest group first, unread (pass them to
    /// [`Index::resolve`]), and how many more groups matched but were left out
    /// under `q.limit`.
    pub fn select(&mut self, q: &Query) -> (Vec<Session>, usize) {
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut tops: Vec<usize> = Vec::new();
        for i in 0..self.j.sessions.len() {
            let t = self.top(i);
            if !groups.contains_key(&t) {
                tops.push(t);
            }
            groups.entry(t).or_default().push(i);
        }
        tops.sort_by(|&a, &b| {
            let ua = self.j.sessions[a].path.is_empty();
            let ub = self.j.sessions[b].path.is_empty();
            match (ua, ub) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.cmp(&b),
            }
        });
        let project = ProjectMatch::new(&q.project);
        let mut shown = 0usize;
        let mut left = 0usize;
        let mut picked = Vec::new();
        for t in tops {
            if !self.keep(t, q, &project) {
                continue;
            }
            if q.limit > 0 && shown >= q.limit {
                left += 1;
                continue;
            }
            shown += 1;
            for &i in &groups[&t] {
                picked.push(self.j.sessions[i].clone());
            }
        }
        (picked, left)
    }

    fn top(&self, mut i: usize) -> usize {
        let mut seen = HashSet::new();
        seen.insert(i);
        loop {
            let s = &self.j.sessions[i];
            let Some(ref parent) = s.parent_session else {
                return i;
            };
            let Some(&p) = self.by_id.get(&id_key(&s.harness, &parent.session_id)) else {
                return i;
            };
            if !seen.insert(p) {
                return i;
            }
            i = p;
        }
    }

    fn keep(&mut self, i: usize, q: &Query, project: &ProjectMatch) -> bool {
        if !q.harness.is_empty() && self.j.sessions[i].harness != q.harness {
            return false;
        }
        if let Some(since) = q.since
            && !self.j.sessions[i].path.is_empty()
            && self.j.sessions[i].mtime.is_none_or(|m| m < since)
        {
            return false;
        }
        let owned = self.owned(i);
        if (q.workgroup && !owned) || (q.local && owned) {
            return false;
        }
        if project.is_set() {
            if !self.j.sessions[i].probed && !project.may_match(&self.j.sessions[i].folder) {
                return false;
            }
            self.j.sessions[i].fill();
            return project.cwd(&self.j.sessions[i].cwd);
        }
        true
    }

    fn owned(&self, i: usize) -> bool {
        let s = &self.j.sessions[i];
        if s.path.is_empty() {
            return s.workgroup.is_some();
        }
        self.j.owner.contains_key(&path_key(s))
    }

    /// Returns the session `<harness> --continue` would pick in `dir`, reading
    /// only the Claude transcripts whose project folder could be `dir`'s,
    /// newest first, until one ran in `dir`.
    pub fn r#continue(&mut self, harness: &str, dir: &str) -> Option<Session> {
        let project = ProjectMatch::new(dir);
        let exact = canonical(dir);
        let mut candidates = Vec::new();
        for s in &mut self.j.sessions {
            if s.harness != harness || s.parent_session.is_some() || s.path.is_empty() {
                continue;
            }
            if !s.probed {
                if !project.may_match(&s.folder) {
                    continue;
                }
                s.fill();
            }
            candidates.push(s.clone());
            if harness == HARNESS_CLAUDE && !s.cwd.is_empty() && canonical(&s.cwd) == exact {
                break;
            }
        }
        continue_from_slice(&candidates, harness, dir)
    }
}

/// What [`Index::select`] picks: sessions that match every filter set, by their
/// group (a session and the subagents under it).
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub harness: String,
    pub since: Option<DateTime<Utc>>,
    pub project: String,
    pub workgroup: bool,
    pub local: bool,
    pub limit: usize,
}

#[derive(Default)]
pub(crate) struct ProjectMatch {
    dirs: Vec<String>,
    name: String,
    runs: Vec<Vec<String>>,
}

impl ProjectMatch {
    pub(crate) fn new(q: &str) -> Self {
        let mut m = Self::default();
        if q.is_empty() {
            return m;
        }
        if q.starts_with('/') {
            let cleaned = filepath::clean(q);
            m.dirs.push(cleaned.clone());
            let c = canonical(q);
            if c != cleaned {
                m.dirs.push(c);
            }
            for d in &m.dirs {
                m.runs.push(alnum_runs(d));
            }
        } else {
            m.name = q.to_lowercase();
            m.runs.push(alnum_runs(&m.name));
        }
        m
    }

    fn is_set(&self) -> bool {
        !self.dirs.is_empty() || !self.name.is_empty()
    }

    fn cwd(&self, cwd: &str) -> bool {
        let cwd = cwd.strip_prefix("file://").unwrap_or(cwd);
        if cwd.is_empty() {
            return false;
        }
        if !self.name.is_empty() {
            return project_of(cwd).to_lowercase().contains(&self.name);
        }
        for c in [filepath::clean(cwd), canonical(cwd)] {
            for d in &self.dirs {
                if &c == d || within(&c, d) {
                    return true;
                }
            }
        }
        false
    }

    pub(crate) fn may_match(&self, folder: &str) -> bool {
        if folder.is_empty()
            || folder.len() >= 200
            || folder.chars().any(|r| r != '-' && !is_alnum(r))
        {
            return true;
        }
        let lower = folder.to_lowercase();
        for runs in &self.runs {
            if runs.iter().all(|r| lower.contains(&r.to_lowercase())) {
                return true;
            }
        }
        false
    }
}

fn alnum_runs(s: &str) -> Vec<String> {
    s.split(|r: char| !is_alnum(r))
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn is_alnum(r: char) -> bool {
    r.is_ascii_alphanumeric()
}

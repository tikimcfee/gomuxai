use crate::proof::{Host, System, Verdict, proof};
use crate::registry::{Entry, Problem, dir, parse_entry, read_dir};
use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::io;
use std::ops::Deref;
use std::path::Path;

/// An entry with its verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proven {
    pub entry: Entry,
    pub verdict: Verdict,
}

impl Deref for Proven {
    type Target = Entry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

/// The registry read at one moment, each entry checked against the running
/// processes.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub entries: Vec<Proven>,
    pub problems: Vec<Problem>,
}

/// Reads the registry in `dir` and proves each entry against `h`.
pub fn read(dir: impl AsRef<Path>, h: &(impl Host + ?Sized)) -> Result<Snapshot> {
    let (entries, problems) = read_dir(dir)?;
    let proven = entries
        .into_iter()
        .map(|entry| {
            let verdict = proof(&entry, h);
            Proven { entry, verdict }
        })
        .collect();
    Ok(Snapshot {
        entries: proven,
        problems,
    })
}

/// Reads this machine's registry.
pub fn read_system() -> Result<Snapshot> {
    let d = dir()?;
    read(d, &System)
}

/// Reports whether an entry's process is (or may be) running.
pub(crate) fn is_running(v: Verdict) -> bool {
    matches!(v, Verdict::Live | Verdict::Unproven)
}

/// Reports whether `a` should win over `b` for the same session: proven over
/// unproven, then the later update.
pub(crate) fn newer(a: &Proven, b: &Proven) -> bool {
    if (a.verdict == Verdict::Live) != (b.verdict == Verdict::Live) {
        return a.verdict == Verdict::Live;
    }
    a.updated_at > b.updated_at
}

/// The process tree, as [`Snapshot::under_pane`] needs it.
pub trait Tree {
    /// How many parent steps lead from `pid` up to `root` (`0` for `root`
    /// itself), or `None` when `root` isn't among `pid`'s ancestors.
    fn depth(&self, pid: i32, root: i32) -> Option<usize>;
}

impl Snapshot {
    /// Says which registry files couldn't be read as entries at all (not JSON,
    /// no pid or session id), or `None`. Such a file may be a running
    /// session's, so a reader that would conclude a session isn't running (it
    /// isn't listed, or a pane runs nothing) says so instead while there are
    /// any. (A half-written file reads this way for a moment.)
    pub fn err(&self) -> Option<anyhow::Error> {
        if self.problems.is_empty() {
            return None;
        }
        let joined = self
            .problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        Some(anyhow!(
            "Claude Code's session registry has entries gomuxai can't read, so it won't say what runs: {joined}"
        ))
    }

    /// Returns `Err` when [`Self::err`] is `Some`.
    pub fn check(&self) -> Result<()> {
        match self.err() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Returns, for each session a running process holds, the entry that holds
    /// it. When two processes claim one session (a CLI stopped with Ctrl-Z and
    /// the same session resumed in another), the one updated last wins: the
    /// stopped one stops updating.
    pub fn sessions(&self) -> HashMap<String, Proven> {
        let mut out: HashMap<String, Proven> = HashMap::new();
        for e in &self.entries {
            if !is_running(e.verdict) {
                continue;
            }
            if let Some(cur) = out.get(&e.session_id)
                && !newer(e, cur)
            {
                continue;
            }
            out.insert(e.session_id.clone(), e.clone());
        }
        out
    }

    /// Returns the proven-live entry for `pid`, if any.
    pub fn by_pid(&self, pid: i32) -> Option<Proven> {
        self.entries
            .iter()
            .find(|e| e.pid == pid && e.verdict == Verdict::Live)
            .cloned()
    }

    /// Returns the session a pane runs: of the proven-live entries whose
    /// process descends from `pane_pid`, the nearest to it. A Claude started
    /// from inside that session (a `claude -p` a tool ran) is deeper, so it is
    /// never mistaken for the pane's own. Returns `Ok(None)` when no Claude
    /// Code process runs in the pane. Two at the same depth (two CLIs side by
    /// side under one shell, one of them stopped with Ctrl-Z) are an error that
    /// names both, unless only one is the session most recently updated.
    pub fn under_pane(&self, t: &(impl Tree + ?Sized), pane_pid: i32) -> Result<Option<Proven>> {
        let mut hits: Vec<(usize, &Proven)> = self
            .entries
            .iter()
            .filter(|e| e.verdict == Verdict::Live)
            .filter_map(|e| t.depth(e.pid, pane_pid).map(|d| (d, e)))
            .collect();

        if hits.is_empty() {
            return Ok(None);
        }

        hits.sort_by(|(da, a), (db, b)| da.cmp(db).then_with(|| b.updated_at.cmp(&a.updated_at)));

        if hits.len() > 1 && hits[1].0 == hits[0].0 && hits[1].1.updated_at == hits[0].1.updated_at
        {
            bail!(
                "two Claude Code processes run at the same depth under pane process {} (pids {} and {}, sessions {} and {}), and neither updated last",
                pane_pid,
                hits[0].1.pid,
                hits[1].1.pid,
                hits[0].1.session_id,
                hits[1].1.session_id,
            );
        }

        Ok(Some(hits[0].1.clone()))
    }
}

/// Reads and proves the entry of one process, if it has one: returns `Ok(None)`
/// when `pid` has no registry file (it isn't a Claude Code CLI).
pub fn read_pid(
    dir: impl AsRef<Path>,
    pid: i32,
    h: &(impl Host + ?Sized),
) -> Result<Option<Proven>> {
    let path = dir.as_ref().join(format!("{pid}.json"));
    let data = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut entry = parse_entry(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    if entry.pid != pid {
        bail!("{pid}.json names pid {}", entry.pid);
    }
    entry.path = path;
    let verdict = proof(&entry, h);
    Ok(Some(Proven { entry, verdict }))
}

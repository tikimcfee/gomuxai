//! Handoff documents: finding an agent's handoff doc, checking whether it is
//! older than its git repository's latest commit, and building the revive
//! re-brief (`docs/FLEET.md` sections `"`agent fleet`"`, `"Revive"`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agent_base::gostd::filepath;
use agent_bus::FleetHandoff;
use agent_store::Time;
use chrono::{DateTime, Utc};

/// State key under which an agent's explicit handoff document path is stored.
pub const STATE_KEY_HANDOFF: &str = "handoff";

/// Timestamps proving a handoff document is older than the latest commit in its
/// git repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale {
    /// Modification time of the handoff document on disk (UTC).
    pub doc_at: Time,
    /// Committer timestamp of `HEAD` in the repository containing the doc (UTC).
    pub commit_at: Time,
}

impl Stale {
    /// Converts this staleness record into a wire [`FleetHandoff`] entry.
    pub fn into_fleet_handoff(
        self,
        agent: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> FleetHandoff {
        FleetHandoff {
            agent: agent.into(),
            path: path.as_ref().to_string_lossy().into_owned(),
            doc_at: self.doc_at.0.into(),
            commit_at: self.commit_at.0.into(),
        }
    }
}

/// Why a path can't be a handoff document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotADoc {
    /// It has a line break, which would let it add lines to the re-brief.
    LineBreak,
    /// It is relative, and there is no directory to take it from.
    NoBase,
    /// It can't be resolved: stat's error for the path (made absolute).
    Stat { path: String, err: String },
    /// It is a directory.
    Dir,
    /// It is something else that isn't a regular file (a FIFO, a device, a
    /// socket), which an agent reading it would hang on or get nothing from.
    NotRegular(&'static str),
}

impl std::fmt::Display for NotADoc {
    /// Words that follow the path: "--handoff x.md is a directory, not a
    /// file", "--handoff x.md: stat /w/x.md: no such file or directory".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotADoc::LineBreak => f.write_str(" has a line break in it"),
            NotADoc::NoBase => f.write_str(
                " is relative, and the agent has no directory on record to take it from",
            ),
            NotADoc::Stat { path, err } => write!(f, ": stat {path}: {err}"),
            NotADoc::Dir => f.write_str(" is a directory, not a file"),
            NotADoc::NotRegular(kind) => write!(f, " isn't a regular file (it is a {kind})"),
        }
    }
}

/// Resolves path (taken from base when relative) with the filesystem, as
/// opening it would: symlinks followed and `..` taken after them, never
/// collapsed lexically (`sym/../doc.md` is the doc beside sym's target), and
/// checks that what it names is a regular file. Returns its absolute,
/// resolved path.
pub fn regular_file(base: &Path, path: &str) -> Result<PathBuf, NotADoc> {
    if path.contains(['\n', '\r']) {
        return Err(NotADoc::LineBreak);
    }
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else if base.as_os_str().is_empty() {
        return Err(NotADoc::NoBase);
    } else {
        base.join(p)
    };
    let stat_err = |e: &std::io::Error| NotADoc::Stat {
        // Cleaned for reading only: the path that failed is the one joined.
        path: filepath::clean(&abs.to_string_lossy()),
        err: agent_base::gostd::os::errno_text(e),
    };
    let resolved = fs::canonicalize(&abs).map_err(|e| stat_err(&e))?;
    let meta = fs::metadata(&resolved).map_err(|e| stat_err(&e))?;
    let ft = meta.file_type();
    if ft.is_file() {
        return Ok(resolved);
    }
    if ft.is_dir() {
        return Err(NotADoc::Dir);
    }
    use std::os::unix::fs::FileTypeExt;
    Err(NotADoc::NotRegular(if ft.is_fifo() {
        "FIFO"
    } else if ft.is_char_device() {
        "character device"
    } else if ft.is_block_device() {
        "block device"
    } else if ft.is_socket() {
        "socket"
    } else {
        "special file"
    }))
}

/// Resolves an agent's handoff document:
/// 1. If `state_handoff` is set (non-empty), that path (relative ones taken
///    from `agent_cwd`), resolved with the filesystem and required to be a
///    regular file (regular_file): an error says why it isn't, rather than
///    falling through to the default, which the agent's state doesn't name.
/// 2. Otherwise `<agent_cwd>/docs/lanes/<agent_name>.md`, resolved, when that
///    regular file exists; None when it doesn't.
pub fn resolve(
    agent_cwd: impl AsRef<Path>,
    agent_name: &str,
    state_handoff: Option<&str>,
) -> Result<Option<PathBuf>, NotADoc> {
    let cwd = agent_cwd.as_ref();
    if let Some(raw) = state_handoff
        && !raw.is_empty()
    {
        return regular_file(cwd, raw).map(Some);
    }
    if cwd.as_os_str().is_empty() || agent_name.is_empty() {
        return Ok(None);
    }
    let default_doc = cwd
        .join("docs")
        .join("lanes")
        .join(format!("{agent_name}.md"));
    Ok(regular_file(cwd, &default_doc.to_string_lossy()).ok())
}

/// Finds an agent's handoff document as resolve does, None when there is
/// none or the one its state names isn't a regular file (agent fleet, which
/// only reports on the docs there are).
pub fn find(
    agent_cwd: impl AsRef<Path>,
    agent_name: &str,
    state_handoff: Option<&str>,
) -> Option<PathBuf> {
    resolve(agent_cwd, agent_name, state_handoff).ok().flatten()
}

/// Checks whether the handoff document at `path` is older than the latest
/// commit of the git repository containing it (`git -C <dir> log -1 --format=%ct`).
///
/// Returns `None` if `path` does not exist as a regular file, is not in a git
/// repository with at least one commit, `git` cannot be run, or the document's
/// modification time is at or after the repository's latest commit time.
pub fn stale(path: impl AsRef<Path>) -> Option<Stale> {
    let path = path.as_ref();
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified = meta.modified().ok()?;
    let doc_at = Time::from(DateTime::<Utc>::from(modified)).utc();

    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let commit_at = latest_commit_at(dir)?;
    if doc_at.before(&commit_at) {
        Some(Stale { doc_at, commit_at })
    } else {
        None
    }
}

fn latest_commit_at(dir: &Path) -> Option<Time> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "-1", "--format=%ct"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs: i64 = std::str::from_utf8(&out.stdout).ok()?.trim().parse().ok()?;
    let dt = DateTime::<Utc>::from_timestamp(secs, 0)?;
    Some(Time::from(dt).utc())
}

/// Builds the plain re-brief message sent to a revived agent.
///
/// With a handoff document, tells the agent it was revived on its own
/// conversation, names the handoff doc, and asks it to read it and carry on;
/// without a doc, says only that it was revived on its own conversation.
pub fn rebrief(name: &str, handoff: Option<&Path>) -> String {
    let who = if name.is_empty() {
        "You were revived on your own conversation.".to_string()
    } else {
        format!("You ({name}) were revived on your own conversation.")
    };
    match handoff {
        Some(path) => format!(
            "{who} Your handoff doc is {}: read it and carry on.",
            path.display()
        ),
        None => who,
    }
}

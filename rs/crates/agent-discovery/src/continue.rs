use agent_base::gostd::filepath;

use crate::dirs::HARNESS_AGY;
use crate::resolve::canonical;
use crate::sessions::Session;

/// Returns the session `<harness> --continue` would pick in `dir`, from
/// `sessions` ([`crate::scan`]'s, or [`crate::resolve`]'s), if there is one. It
/// goes by each CLI's own rule:
///
///   - `claude`: "the most recent conversation in the current directory": the
///     session whose recorded working directory is `dir` whose transcript was
///     written last.
///   - `agy`: the most recently active conversation whose workspace is `dir`;
///     if none, agy falls back to the most recent one in a parent or child
///     directory of `dir`, and so does this.
///
/// Both skip subagents' sessions, which aren't conversations of their own, and
/// transcripts with nothing in them.
pub fn r#continue(sessions: &[Session], harness: &str, dir: &str) -> Option<Session> {
    let dir = canonical(dir);
    let mut exact: Option<&Session> = None;
    let mut near: Option<&Session> = None;
    for s in sessions {
        if s.harness != harness
            || s.parent_session.is_some()
            || s.cwd.is_empty()
            || s.path.is_empty()
        {
            continue;
        }
        let cwd = canonical(&s.cwd);
        if cwd == dir {
            if exact.is_none_or(|prev| newer(s, prev)) {
                exact = Some(s);
            }
        } else if harness == HARNESS_AGY
            && (within(&cwd, &dir) || within(&dir, &cwd))
            && near.is_none_or(|prev| newer(s, prev))
        {
            near = Some(s);
        }
    }
    exact.or(near).cloned()
}

/// Reports whether `a` was written after `b` (by id, for a tie, as
/// `sort_sessions` orders them).
fn newer(a: &Session, b: &Session) -> bool {
    if a.mtime != b.mtime {
        return a.mtime > b.mtime;
    }
    a.session_id < b.session_id
}

/// Reports whether `path` is inside `dir` (not `dir` itself).
pub(crate) fn within(path: &str, dir: &str) -> bool {
    let Ok(rel) = filepath::rel(dir, path) else {
        return false;
    };
    rel != "."
        && rel != ".."
        && !rel.starts_with(&format!("..{}", filepath::SEPARATOR))
        && !filepath::is_abs(&rel)
}

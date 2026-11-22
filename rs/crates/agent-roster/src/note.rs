//! Adding a note (agent roster note, and dreams): the checks, and the
//! session it cites, resolved against this host's sessions.
use agent_base::config::Config;
use agent_discovery::{self as discovery, Dir, Overrides};
use agent_store::{RosterNote, Store, Time};
use anyhow::bail;

use crate::key::{Key, check_effort, check_task, check_verdict};

/// NoteInput is a note to add, as its writer gives it.
#[derive(Clone, Debug, Default)]
pub struct NoteInput {
    pub harness: String,
    /// "" for the harness's default model.
    pub model: String,
    /// "" for none.
    pub effort: String,
    /// The task kind it was tried on (one of TASK_KINDS).
    pub task: String,
    /// good, mixed or bad.
    pub verdict: String,
    /// What happened.
    pub text: String,
    pub why: String,
    /// The session the note comes from, as HARNESS:ID; the start of an id
    /// will do when it names one session.
    pub source: String,
    /// The agent or person writing it (dream-<id> for a dream).
    pub by: String,
    /// The note this one replaces; 0 for none.
    pub supersedes: i64,
}

/// session_dirs is where this host's sessions are, as agent sessions looks
/// (the app config's claude_projects_dir, agy_brain_dir, kimi_sessions_dir).
pub fn session_dirs(cfg: &Config) -> Vec<Dir> {
    discovery::dirs(&Overrides {
        claude: cfg.claude_projects_dir.clone(),
        agy: cfg.agy_brain_dir.clone(),
        kimi: cfg.kimi_sessions_dir.clone(),
    })
}

/// add_note checks a note and records it: the task kind, verdict and effort
/// must be ones the roster knows, the text and writer non-empty, a
/// superseded note must exist and be on the same entry, and the source must
/// be a session on this host (where cfg says sessions are). It returns the
/// note as stored, its source the session's full HARNESS:ID.
pub fn add_note(store: &Store, cfg: &Config, note: NoteInput) -> anyhow::Result<RosterNote> {
    add_note_in(store, &session_dirs(cfg), note)
}

/// add_note_in is add_note with the session directories given.
pub fn add_note_in(store: &Store, dirs: &[Dir], note: NoteInput) -> anyhow::Result<RosterNote> {
    let harness = note.harness.trim();
    if harness.is_empty() {
        bail!("the note names no harness: name the entry as HARNESS/MODEL[@EFFORT]");
    }
    let key = Key::new(harness, note.model.trim(), note.effort.trim());
    if !key.effort.is_empty() {
        check_effort(&key.effort)?;
    }
    let task = check_task(&note.task)?;
    let verdict = check_verdict(&note.verdict)?;
    let text = note.text.trim();
    if text.is_empty() {
        bail!("the note says nothing: say in a line or two what happened");
    }
    let by = note.by.trim();
    if by.is_empty() {
        bail!("the note has no writer: give the agent's name, or \"user\" for a person");
    }
    if note.supersedes < 0 {
        bail!("supersedes {}: give a note's id", note.supersedes);
    }
    if note.supersedes != 0 {
        let Some(old) = store.get_roster_note(note.supersedes)? else {
            bail!(
                "there is no note {} to supersede (agent roster show {key} lists its notes)",
                note.supersedes
            );
        };
        let old_key = Key::of_note(&old);
        if old_key != key {
            bail!(
                "note {} is on {old_key}, not {key}: a note supersedes one on the same entry",
                note.supersedes
            );
        }
    }
    let source = resolve_source(dirs, &note.source)?;
    let mut n = RosterNote {
        id: 0,
        harness: key.harness,
        model: key.model,
        effort: key.effort,
        task,
        verdict,
        text: text.to_string(),
        why: note.why.trim().to_string(),
        source,
        by: by.to_string(),
        supersedes: note.supersedes,
        created_at: Time::now(),
    };
    store.add_roster_note(&mut n)?;
    Ok(n)
}

/// resolve_source finds the session source names among this host's
/// sessions in dirs: HARNESS:ID, or HARNESS: and the start of an id that
/// names one session. It returns the session as HARNESS:ID.
pub fn resolve_source(dirs: &[Dir], source: &str) -> anyhow::Result<String> {
    let source = source.trim();
    let Some((harness, id)) = source.split_once(':') else {
        if source.is_empty() {
            bail!(
                "the note cites no session: give the session it came from as HARNESS:ID (agent sessions lists this host's)"
            );
        }
        bail!(
            "{source:?} isn't HARNESS:ID: cite the session as e.g. claude:{source} (agent sessions lists this host's)"
        );
    };
    let (harness, id) = (harness.trim(), id.trim());
    if harness.is_empty() || id.is_empty() {
        bail!(
            "{source:?} isn't HARNESS:ID: cite the session as e.g. claude:3f3a9c... (agent sessions lists this host's)"
        );
    }
    let sessions = discovery::new_index(dirs, &[]).sessions();
    let mut found: Vec<String> = sessions
        .iter()
        .filter(|s| s.harness == harness && s.session_id.starts_with(id))
        .map(|s| s.session_id.clone())
        .collect();
    found.sort_unstable();
    found.dedup();
    if found.iter().any(|s| s == id) {
        return Ok(format!("{harness}:{id}"));
    }
    match found.as_slice() {
        [one] => Ok(format!("{harness}:{one}")),
        [] => {
            if !sessions.iter().any(|s| s.harness == harness) {
                bail!(
                    "no {harness} session on this host is {id}: this host has no {harness} sessions at all (agent sessions lists the ones it has; a note cites a session that ran on this host)"
                );
            }
            bail!(
                "no {harness} session on this host is {id} (agent sessions --harness {harness} lists them; a note cites a session that ran on this host)"
            )
        }
        many => bail!(
            "{id} is the start of {} {harness} sessions: {}; give more of the id",
            many.len(),
            many.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        ),
    }
}

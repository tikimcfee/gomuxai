use std::collections::HashMap;
use std::os::fd::AsRawFd;

use agent_base::gostd::filepath;
use agent_transcript::{AgyFiles, AgySubagent, read_agy_history, read_agy_subagent};

use crate::dirs::{HARNESS_AGY, entry_name, is_dir, os_path, read_dir_sorted};
use crate::sessions::{FileStat, Liveness, Session, SubState, from_meta};

#[cfg(target_os = "linux")]
use crate::lock_linux::new_lock_probe;
#[cfg(not(target_os = "linux"))]
use crate::lock_other::new_lock_probe;

const AGY_TRANSCRIPTS: &[&str] = &["transcript_full.jsonl", "transcript.jsonl"];

pub(crate) struct AgyIndex {
    home: String,
    workspace: HashMap<String, String>,
    subagents: HashMap<String, AgySubagent>,
    locked: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

pub(crate) fn scan_agy(brain: &str) -> Vec<Session> {
    let Ok(entries) = read_dir_sorted(brain) else {
        return Vec::new();
    };
    let idx = new_agy_index(&filepath::dir(brain));
    let mut out = Vec::new();
    for e in entries {
        if !e.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let name = entry_name(brain, &e);
        let Some((path, stat)) = agy_transcript(&filepath::join(&[brain, &name])) else {
            continue;
        };
        out.push(probe_agy(&idx, &name, &path, stat));
    }
    // A subagent with no presence lock of its own is open while it says it is
    // alive and its parent is open.
    let mut live: HashMap<String, Liveness> = HashMap::new();
    for s in &out {
        live.insert(s.session_id.clone(), s.live);
    }
    for s in &mut out {
        if let Some(ref parent) = s.parent_session
            && s.live != Liveness::Yes
            && s.state == SubState::Running
            && live.get(&parent.session_id) == Some(&Liveness::Yes)
        {
            s.live = Liveness::Yes;
        }
    }
    out
}

fn agy_transcript(conv: &str) -> Option<(String, FileStat)> {
    for &name in AGY_TRANSCRIPTS {
        for path in [
            filepath::join(&[conv, ".system_generated", "logs", name]),
            filepath::join(&[conv, name]),
        ] {
            if let Ok(info) = std::fs::metadata(os_path(&path))
                && !info.is_dir()
                && info.len() > 0
            {
                return Some((path, FileStat::from_metadata(&info)));
            }
        }
    }
    None
}

pub(crate) fn probe_agy(idx: &AgyIndex, id: &str, path: &str, stat: FileStat) -> Session {
    let mut m = agent_transcript::Session {
        session_id: id.to_string(),
        ..Default::default()
    };
    let files = AgyFiles {
        home: idx.home.clone(),
        workspace: idx.workspace.get(id).cloned().unwrap_or_default(),
        subagent: idx.subagents.get(id).cloned(),
    };
    m.harvest_agy(&files);
    let mut s = from_meta(HARNESS_AGY, path, stat, m);
    if let Some(ref sub) = files.subagent {
        match sub.state.as_str() {
            "SUBAGENT_STATE_ALIVE" => s.state = SubState::Running,
            "" => {}
            _ => s.state = SubState::Finished,
        }
    }
    let presence = filepath::join(&[&idx.home, "presence"]);
    if is_dir(&presence) {
        s.live = if (idx.locked)(&filepath::join(&[&presence, &format!("{id}.lock")])) {
            Liveness::Yes
        } else {
            Liveness::No
        };
    }
    s
}

pub(crate) fn new_agy_index(home: &str) -> AgyIndex {
    let mut subagents = HashMap::new();
    let brain = filepath::join(&[home, "brain"]);
    if let Ok(parents) = read_dir_sorted(&brain) {
        for p in parents {
            if !p.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let pname = entry_name(&brain, &p);
            let dir = filepath::join(&[&brain, &pname, ".system_generated", "subagents"]);
            let Ok(subs) = read_dir_sorted(&dir) else {
                continue;
            };
            for f in subs {
                if f.file_type().is_ok_and(|ft| ft.is_dir()) {
                    continue;
                }
                let fname = entry_name(&dir, &f);
                if !fname.ends_with(".json") {
                    continue;
                }
                if let Some(sub) = read_agy_subagent(&filepath::join(&[&dir, &fname])) {
                    subagents.insert(sub.conversation_id.clone(), sub);
                }
            }
        }
    }
    AgyIndex {
        home: home.to_string(),
        workspace: read_agy_history(home),
        subagents,
        locked: new_lock_probe(),
    }
}

/// Reports whether a process holds an exclusive flock on `path`. Asks for a
/// shared lock without waiting, and gives it back at once.
pub(crate) fn flock_held(path: &str) -> bool {
    let Ok(f) = std::fs::File::open(os_path(path)) else {
        return false;
    };
    let fd = f.as_raw_fd();
    // SAFETY: `fd` is a valid open file descriptor owned by `f`.
    let rc = unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return err.raw_os_error() == Some(libc::EWOULDBLOCK)
            || err.kind() == std::io::ErrorKind::WouldBlock;
    }
    // SAFETY: `fd` is valid and holds the shared lock acquired above.
    unsafe {
        let _ = libc::flock(fd, libc::LOCK_UN);
    }
    false
}

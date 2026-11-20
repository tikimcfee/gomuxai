use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::io;
use std::path::{Path, PathBuf};

/// A CLI in a terminal (also `claude -p`).
pub const KIND_INTERACTIVE: &str = "interactive";
/// A `claude --bg` session.
pub const KIND_BACKGROUND: &str = "bg";
/// The background-session supervisor.
pub const KIND_DAEMON: &str = "daemon";
/// A session the daemon runs.
pub const KIND_DAEMON_WORKER: &str = "daemon-worker";

/// A turn is running.
pub const STATUS_BUSY: &str = "busy";
/// At its prompt, nothing running.
pub const STATUS_IDLE: &str = "idle";
/// Its turn over, while a shell command the session started still runs in the
/// background (its end starts the next turn).
pub const STATUS_SHELL: &str = "shell";
/// Stopped on something only a person can answer (`waiting_for` says what,
/// when Claude records it).
pub const STATUS_WAITING: &str = "waiting";

const KINDS: &[&str] = &[
    KIND_BACKGROUND,
    KIND_DAEMON,
    KIND_DAEMON_WORKER,
    KIND_INTERACTIVE,
];
const STATUSES: &[&str] = &[STATUS_BUSY, STATUS_IDLE, STATUS_SHELL, STATUS_WAITING];

/// One registry file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The file it was read from.
    pub path: PathBuf,
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    /// The Claude Code version that wrote it.
    pub version: String,
    pub kind: String,
    /// How the process was started: `"cli"` for a person's interactive CLI,
    /// `"sdk-cli"` for `claude -p`, and others.
    pub entrypoint: String,
    pub status: String,
    pub waiting_for: String,
    pub name: String,
    /// `"derived"`, `"user"`, `"peer"`, ...
    pub name_source: String,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    /// The process's start time as the OS gives it (Linux: the `starttime`
    /// field of `/proc/<pid>/stat`; macOS: `ps -o lstart=` in UTC), which
    /// [`crate::proof`] compares with the running process's.
    pub proc_start: String,
    /// Says `proc_start` came from `procStartFt`, whose form gomuxai hasn't
    /// seen written: a value unlike this OS's start time can't prove the entry
    /// stale, only unproven.
    pub proc_start_ft: bool,
    /// Names the pid space the pid belongs to (Linux: machine id and pid
    /// namespace), so an entry written in another container sharing the
    /// directory is not read as one of this host's processes.
    pub pid_domain: String,
    /// The entry's `tmux` field: Claude's environment's idea of its pane.
    /// A hint for people only; see the crate comment.
    pub tmux_hint: String,
    /// Says which of the entry's values gomuxai doesn't know (`""` if none):
    /// its status then reads as unknown.
    pub unknown: String,
}

/// A registry file that couldn't be read as an entry, and why.
#[derive(Debug)]
pub struct Problem {
    pub path: PathBuf,
    pub err: anyhow::Error,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.err)
    }
}

impl std::error::Error for Problem {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.err.as_ref())
    }
}

/// Returns the registry directory: `$CLAUDE_CONFIG_DIR/sessions`, else
/// `~/.claude/sessions`.
pub fn dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(d).join("sessions"));
    }
    let home = user_home_dir().context("finding the Claude Code session registry")?;
    Ok(home.join(".claude").join("sessions"))
}

fn user_home_dir() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    bail!("$HOME is not defined")
}

/// Reads every `<pid>.json` in `dir`. A missing `dir` is no entries (no Claude
/// Code has run). A file that can't be read as an entry is a [`Problem`]; the
/// others are still returned. Entries come sorted by pid.
pub fn read_dir(dir: impl AsRef<Path>) -> Result<(Vec<Entry>, Vec<Problem>)> {
    let dir = dir.as_ref();
    let files = match std::fs::read_dir(dir) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(e).context("reading the Claude Code session registry"),
    };

    let mut entries = Vec::new();
    let mut problems = Vec::new();

    for item in files {
        let f = match item {
            Ok(f) => f,
            Err(e) => return Err(e).context("reading the Claude Code session registry"),
        };
        let os_name = f.file_name();
        let Some(name) = os_name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".json") else {
            continue; // <pid>.<hash>.key files and anything else
        };
        if f.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let Ok(pid) = stem.parse::<i32>() else {
            continue; // not a <pid>.json
        };
        if pid.to_string() != stem {
            continue; // not a canonical <pid>.json
        }
        let path = dir.join(name);
        let data = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                continue; // its process exited between the listing and the read
            }
            Err(e) => {
                problems.push(Problem {
                    path,
                    err: e.into(),
                });
                continue;
            }
        };
        let mut e = match parse_entry(&data) {
            Ok(e) => e,
            Err(err) => {
                problems.push(Problem { path, err });
                continue;
            }
        };
        if e.pid != pid {
            problems.push(Problem {
                path,
                err: anyhow!("names pid {}, not the {} its file name says", e.pid, pid),
            });
            continue;
        }
        e.path = path;
        entries.push(e);
    }

    entries.sort_by_key(|e| e.pid);
    Ok((entries, problems))
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    pid: Option<i32>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "waitingFor")]
    waiting_for: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "nameSource")]
    name_source: Option<String>,
    #[serde(default, rename = "startedAt")]
    started_at: Option<i64>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<i64>,
    #[serde(default, rename = "statusUpdatedAt")]
    status_updated_at: Option<i64>,
    #[serde(default, rename = "procStart")]
    proc_start: Option<String>,
    #[serde(default, rename = "procStartFt")]
    proc_start_ft: Option<String>,
    #[serde(default, rename = "pidDomain")]
    pid_domain: Option<String>,
    #[serde(default)]
    tmux: Option<String>,
}

/// Reads one registry file's JSON. It refuses a file without a pid or session
/// id. A kind or status outside the known vocabulary is kept raw with
/// [`Entry::unknown`] saying so: a Claude Code that writes a new value needs
/// this crate taught what it means, and until then that one session's status is
/// unknown, never guessed (its process can still be proven and placed).
pub fn parse_entry(data: &[u8]) -> Result<Entry> {
    let obj: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(data).map_err(|e| anyhow!("not a registry entry: {e}"))?;
    let raw: RawEntry = serde_json::from_value(serde_json::Value::Object(obj))
        .map_err(|e| anyhow!("not a registry entry: {e}"))?;
    let pid = raw.pid.unwrap_or(0);
    if pid <= 1 {
        bail!("no pid");
    }
    let session_id = raw.session_id.unwrap_or_default();
    if session_id.is_empty() {
        bail!("no sessionId");
    }

    // Claude Code reads a missing kind as interactive and a missing status
    // as not yet known; so does this.
    let mut kind = raw.kind.unwrap_or_default();
    if kind.is_empty() {
        kind = KIND_INTERACTIVE.to_owned();
    }
    let status = raw.status.unwrap_or_default();
    let version = raw.version.unwrap_or_default();

    let mut unknown = Vec::new();
    if !KINDS.contains(&kind.as_str()) {
        unknown.push(format!(
            "kind {kind:?} is not one gomuxai knows ({})",
            KINDS.join(", ")
        ));
    }
    if !status.is_empty() && !STATUSES.contains(&status.as_str()) {
        unknown.push(format!(
            "status {status:?} is not one gomuxai knows ({})",
            STATUSES.join(", ")
        ));
    }
    let unknown_text = if unknown.is_empty() {
        String::new()
    } else {
        format!(
            "{}, written by Claude Code {}",
            unknown.join("; "),
            or_unknown(&version)
        )
    };

    // Claude Code writes the start time as procStart, or (behind a switch
    // it ships off) as procStartFt instead: the same value under its new name.
    let raw_proc_start = raw.proc_start.unwrap_or_default();
    let raw_proc_start_ft = raw.proc_start_ft.unwrap_or_default();
    let (proc_start, proc_start_ft) = if raw_proc_start.is_empty() {
        let has_ft = !raw_proc_start_ft.is_empty();
        (raw_proc_start_ft, has_ft)
    } else {
        (raw_proc_start, false)
    };

    let status_updated_at = raw.status_updated_at.unwrap_or(0);
    let updated = if status_updated_at == 0 {
        raw.updated_at.unwrap_or(0)
    } else {
        status_updated_at
    };

    Ok(Entry {
        path: PathBuf::new(),
        pid,
        session_id,
        cwd: raw.cwd.unwrap_or_default(),
        version,
        kind,
        entrypoint: raw.entrypoint.unwrap_or_default(),
        status,
        waiting_for: raw.waiting_for.unwrap_or_default(),
        name: raw.name.unwrap_or_default(),
        name_source: raw.name_source.unwrap_or_default(),
        started_at: millis(raw.started_at.unwrap_or(0)),
        updated_at: millis(updated),
        proc_start,
        proc_start_ft,
        pid_domain: raw.pid_domain.unwrap_or_default(),
        tmux_hint: raw.tmux.unwrap_or_default(),
        unknown: unknown_text,
    })
}

fn millis(ms: i64) -> Option<DateTime<Utc>> {
    if ms <= 0 {
        None
    } else {
        Utc.timestamp_millis_opt(ms).single()
    }
}

fn or_unknown(v: &str) -> &str {
    if v.is_empty() {
        "(version not recorded)"
    } else {
        v
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::proof::{Host, Verdict, proof, stat_start_time};
    use crate::running::{Proven, Snapshot, Tree, read};
    use chrono::Duration;
    use std::collections::HashMap;

    // A registry file in the shape Claude Code 2.1.316 writes on Linux (values
    // made up).
    const ENTRY_JSON: &str = r#"{"pid":886208,"sessionId":"11111111-2222","cwd":"/home/user/dev/p","startedAt":1793455699056,
"procStart":"61266397","version":"2.1.316","peerProtocol":1,"peerFeatures":["notify_idle"],"kind":"interactive",
"entrypoint":"cli","pidDomain":"linux:0000:pid:[4026531836]","tmux":"agent-x:@9241.%9241",
"messagingSocketPath":"/run/user/1000/cc-socks/886208.sock","name":"p-97","nameSource":"derived",
"status":"busy","updatedAt":1793464592054,"statusUpdatedAt":1793464592054}"#;

    pub(crate) fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("claudereg-")
            .tempdir_in("/var/tmp")
            .or_else(|_| tempfile::tempdir())
            .expect("tempdir")
    }

    #[test]
    fn test_parse_entry() {
        let e = parse_entry(ENTRY_JSON.as_bytes()).unwrap();
        assert_eq!(e.pid, 886208);
        assert_eq!(e.session_id, "11111111-2222");
        assert_eq!(e.kind, KIND_INTERACTIVE);
        assert_eq!(e.status, STATUS_BUSY);
        assert_eq!(e.proc_start, "61266397");
        assert_eq!(e.entrypoint, "cli");
        assert_eq!(e.tmux_hint, "agent-x:@9241.%9241");
        assert_eq!(
            e.updated_at,
            Utc.timestamp_millis_opt(1793464592054).single()
        );
    }

    #[test]
    fn test_parse_entry_refuses_broken_files() {
        for (json, want) in [
            (r#"{"sessionId":"s"}"#, "no pid"),
            (r#"{"pid":5}"#, "no sessionId"),
            (r#"[1]"#, "not a registry entry"),
        ] {
            let err = parse_entry(json.as_bytes()).unwrap_err();
            assert!(
                err.to_string().contains(want),
                "{json}: err {err}, want {want:?}"
            );
        }
        // A missing kind is interactive and a missing status is not yet known,
        // as Claude Code reads them. Null optional fields are also accepted.
        let e = parse_entry(br#"{"pid":5,"sessionId":"s","waitingFor":null,"tmux":null}"#).unwrap();
        assert_eq!(e.kind, KIND_INTERACTIVE);
        assert_eq!(e.status, "");
    }

    #[test]
    fn test_read_dir() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let write = |name: &str, body: &str| {
            std::fs::write(dir.join(name), body.as_bytes()).unwrap();
        };
        write("886208.json", ENTRY_JSON);
        write("886208.abcdef.key", "secret"); // never read
        write("12.json", r#"{"pid":13,"sessionId":"s"}"#);
        write("7.json", r#"{"pid":7,"sessionId":"s","status":"dreaming""#); // cut off mid-write
        write("notes.json", "{}");

        let (entries, problems) = read_dir(dir).unwrap();
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0].pid, 886208);
        assert_eq!(entries[0].path, dir.join("886208.json"));
        assert_eq!(problems.len(), 2, "problems: {problems:?}");

        let (missing_entries, missing_problems) = read_dir(dir.join("missing")).unwrap();
        assert!(missing_entries.is_empty() && missing_problems.is_empty());
    }

    #[test]
    fn test_stat_start_time() {
        // A command name with spaces and a ")" in it.
        let stat = "886208 (claude (x) y) S 886207 886208 886151 34816 886208 4194560 1 2 3 4 5 6 7 8 20 0 15 0 61266397 7 8";
        assert_eq!(stat_start_time(stat).as_deref(), Some("61266397"));
        assert_eq!(stat_start_time("1 (x) S 1 2"), None);
    }

    #[test]
    fn test_stat_start_time_self() {
        let Ok(data) = std::fs::read_to_string("/proc/self/stat") else {
            return;
        };
        let s = stat_start_time(&data).expect("start time in /proc/self/stat");
        assert!(
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()),
            "start time {s:?}"
        );
    }

    #[test]
    fn test_stat_start_time_non_utf8_command_name() {
        // The kernel cut the command name at 15 bytes, inside an "é".
        let stat: &[u8] =
            b"4242 (abcdefghijklmn\xc3) S 1 4242 4242 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 61266397 7 8";
        assert_eq!(stat_start_time(stat).as_deref(), Some("61266397"));
        let state = crate::proof::stat_fields(stat).and_then(|mut f| f.next());
        assert_eq!(state, Some(b"S".as_slice()));
    }

    /// A pid reused by a process whose name isn't valid UTF-8 is still told
    /// apart by its start time (Stale), as Go does, not left Unproven (which
    /// counts as running).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_proof_of_a_process_whose_name_is_not_utf8() {
        use crate::proof::System;
        use std::process::{Command, Stdio};

        // The kernel names a process after the path it was exec'd by, cut at
        // 15 bytes: 14 ASCII bytes and a 2-byte "é" leave half the "é".
        let tmp = temp_dir();
        let link = tmp.path().join("abcdefghijklmn\u{e9}");
        std::os::unix::fs::symlink("/bin/sh", &link).unwrap();
        let mut child = Command::new(&link)
            .args(["-c", "read x"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;

        let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
        let start = System.proc_start(pid);
        let running = System.running(pid);
        let live = Entry {
            pid,
            proc_start: start.clone().unwrap_or_default(),
            ..Default::default()
        };
        let reused = Entry {
            pid,
            proc_start: "1".into(),
            ..Default::default()
        };
        let (live_verdict, reused_verdict) = (proof(&live, &System), proof(&reused, &System));
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            std::str::from_utf8(&stat).is_err(),
            "the stat line is valid UTF-8 after all"
        );
        assert!(
            start.is_some(),
            "no start time for a process with a non-UTF-8 name"
        );
        assert!(running, "a running process read as not running");
        assert_eq!(live_verdict, Verdict::Live);
        assert_eq!(reused_verdict, Verdict::Stale);
    }

    struct FakeHost {
        running: HashMap<i32, String>, // pid -> start ("" = can't read)
        domain: String,
    }

    impl Host for FakeHost {
        fn running(&self, pid: i32) -> bool {
            self.running.contains_key(&pid)
        }

        fn proc_start(&self, pid: i32) -> Option<String> {
            self.running.get(&pid).filter(|s| !s.is_empty()).cloned()
        }

        fn pid_domain(&self) -> String {
            self.domain.clone()
        }
    }

    #[test]
    fn test_proof() {
        let h = FakeHost {
            running: HashMap::from([
                (10, "100".to_owned()),
                (11, String::new()),
                (12, "999".to_owned()),
            ]),
            domain: "linux:m:ns".to_owned(),
        };
        let cases = [
            (
                Entry {
                    pid: 10,
                    proc_start: "100".into(),
                    ..Default::default()
                },
                Verdict::Live,
            ),
            (
                Entry {
                    pid: 10,
                    proc_start: "100".into(),
                    pid_domain: "linux:m:ns".into(),
                    ..Default::default()
                },
                Verdict::Live,
            ),
            (
                Entry {
                    pid: 10,
                    proc_start: "100".into(),
                    pid_domain: "linux:other:ns".into(),
                    ..Default::default()
                },
                Verdict::Foreign,
            ),
            // the pid was reused
            (
                Entry {
                    pid: 12,
                    proc_start: "100".into(),
                    ..Default::default()
                },
                Verdict::Stale,
            ),
            // not running
            (
                Entry {
                    pid: 13,
                    proc_start: "100".into(),
                    ..Default::default()
                },
                Verdict::Stale,
            ),
            // no procStart recorded
            (
                Entry {
                    pid: 10,
                    ..Default::default()
                },
                Verdict::Unproven,
            ),
            (
                Entry {
                    pid: 11,
                    proc_start: "100".into(),
                    ..Default::default()
                },
                Verdict::Unproven,
            ),
        ];
        for (e, want) in cases {
            assert_eq!(proof(&e, &h), want, "{e:?}");
        }
    }

    #[test]
    fn test_sessions_prefers_the_later_update() {
        let at = Utc.timestamp_opt(1000, 0).single().unwrap();
        let s = Snapshot {
            entries: vec![
                Proven {
                    entry: Entry {
                        pid: 1,
                        session_id: "a".into(),
                        updated_at: Some(at),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
                // resumed after a Ctrl-Z
                Proven {
                    entry: Entry {
                        pid: 2,
                        session_id: "a".into(),
                        updated_at: Some(at + Duration::minutes(1)),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
                Proven {
                    entry: Entry {
                        pid: 3,
                        session_id: "b".into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Stale,
                },
                Proven {
                    entry: Entry {
                        pid: 4,
                        session_id: "c".into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Unproven,
                },
            ],
            problems: Vec::new(),
        };
        let got = s.sessions();
        assert_eq!(got.len(), 2);
        assert_eq!(got["a"].pid, 2);
        assert_eq!(got["c"].pid, 4);
    }

    struct FakeTree(HashMap<i32, i32>);

    impl Tree for FakeTree {
        fn depth(&self, mut pid: i32, root: i32) -> Option<usize> {
            for d in 0..64 {
                if pid == root {
                    return Some(d);
                }
                pid = *self.0.get(&pid)?;
            }
            None
        }
    }

    // A `claude -p` run from inside an agent's session registers too, with the
    // agent's pane in its tmux field. The pane's session is the shallower one.
    #[test]
    fn test_under_pane_skips_nested_claude() {
        // pane shell 100 -> sh 101 -> claude 102 -> bash 103 -> claude -p 104
        let pt = FakeTree(HashMap::from([
            (101, 100),
            (102, 101),
            (103, 102),
            (104, 103),
            (200, 1),
        ]));
        let s = Snapshot {
            entries: vec![
                Proven {
                    entry: Entry {
                        pid: 104,
                        session_id: "nested".into(),
                        tmux_hint: "agent-x:@1.%1".into(),
                        updated_at: Utc.timestamp_opt(2000, 0).single(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
                Proven {
                    entry: Entry {
                        pid: 102,
                        session_id: "agent".into(),
                        tmux_hint: "agent-x:@1.%1".into(),
                        updated_at: Utc.timestamp_opt(1000, 0).single(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
                Proven {
                    entry: Entry {
                        pid: 200,
                        session_id: "elsewhere".into(),
                        tmux_hint: "agent-x:@1.%1".into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
            ],
            problems: Vec::new(),
        };
        let e = s
            .under_pane(&pt, 100)
            .unwrap()
            .expect("agent under pane 100");
        assert_eq!(e.session_id, "agent");
        assert!(
            s.under_pane(&pt, 999).unwrap().is_none(),
            "nothing runs under pane 999"
        );
    }

    // A stale entry under the pane (a CLI that crashed) is not its session.
    #[test]
    fn test_under_pane_needs_proof() {
        let pt = FakeTree(HashMap::from([(101, 100)]));
        let s = Snapshot {
            entries: vec![Proven {
                entry: Entry {
                    pid: 101,
                    session_id: "old".into(),
                    ..Default::default()
                },
                verdict: Verdict::Stale,
            }],
            problems: Vec::new(),
        };
        assert!(s.under_pane(&pt, 100).unwrap().is_none());
    }

    #[test]
    fn test_under_pane_ambiguous() {
        let pt = FakeTree(HashMap::from([(101, 100), (102, 100)]));
        let s = Snapshot {
            entries: vec![
                Proven {
                    entry: Entry {
                        pid: 101,
                        session_id: "a".into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
                Proven {
                    entry: Entry {
                        pid: 102,
                        session_id: "b".into(),
                        ..Default::default()
                    },
                    verdict: Verdict::Live,
                },
            ],
            problems: Vec::new(),
        };
        let err = s.under_pane(&pt, 100).unwrap_err();
        assert!(err.to_string().contains("same depth"), "err: {err}");
    }

    // A file gomuxai can't read at all may be a running session's: the snapshot
    // says so rather than let a reader call that session gone.
    #[test]
    fn test_snapshot_err() {
        let tmp = temp_dir();
        let dir = tmp.path();
        std::fs::write(dir.join("7.json"), br#"{"pid":7,"sessionId":"s","stat"#).unwrap();
        std::fs::write(
            dir.join("8.json"),
            br#"{"pid":8,"sessionId":"t","status":"idle"}"#,
        )
        .unwrap();
        let s = read(
            dir,
            &FakeHost {
                running: HashMap::from([(8, String::new())]),
                domain: String::new(),
            },
        )
        .unwrap();
        let err = s.err().expect("snapshot error");
        let msg = err.to_string();
        assert!(
            msg.contains("7.json") && msg.contains("won't say what runs"),
            "err = {msg}"
        );
        assert_eq!(s.entries.len(), 1);
    }

    // Claude Code ships a switch that writes the start time as procStartFt.
    // The same value proves the entry; another form only fails to prove it.
    #[test]
    fn test_proc_start_ft() {
        let e = parse_entry(br#"{"pid":10,"sessionId":"s","procStartFt":"100"}"#).unwrap();
        assert_eq!(e.proc_start, "100");
        assert!(e.proc_start_ft);

        let mut h = FakeHost {
            running: HashMap::from([(10, "100".to_owned())]),
            domain: String::new(),
        };
        assert_eq!(proof(&e, &h), Verdict::Live);

        h.running.insert(10, "133777000000000000".to_owned());
        assert_eq!(proof(&e, &h), Verdict::Unproven);
    }

    // One entry with a value gomuxai doesn't know costs that session only.
    #[test]
    fn test_unknown_value_is_one_session() {
        let e =
            parse_entry(br#"{"pid":5,"sessionId":"s","status":"compacting","version":"2.2.0"}"#)
                .unwrap();
        assert_eq!(e.status, "compacting");
        assert!(
            e.unknown.contains(r#"status "compacting""#),
            "unknown: {}",
            e.unknown
        );
        assert!(e.unknown.contains("2.2.0"), "unknown: {}", e.unknown);
    }

    #[test]
    fn test_read_pid_and_system_proof() {
        use crate::proof::System;
        use crate::running::read_pid;

        let tmp = temp_dir();
        let dir = tmp.path();
        let self_pid = std::process::id() as i32;
        assert!(System.running(self_pid));
        assert!(!System.running(0));

        assert!(read_pid(dir, self_pid, &System).unwrap().is_none());

        if let Some(start) = System.proc_start(self_pid) {
            let domain = System.pid_domain();
            let body = serde_json::json!({
                "pid": self_pid,
                "sessionId": "self-sess",
                "procStart": start,
                "pidDomain": domain,
                "kind": "bg",
                "status": "idle",
            });
            std::fs::write(dir.join(format!("{self_pid}.json")), body.to_string()).unwrap();

            let proven = read_pid(dir, self_pid, &System)
                .unwrap()
                .expect("found self");
            assert_eq!(proven.verdict, Verdict::Live);
            assert_eq!(proven.session_id, "self-sess");

            let snap = read(dir, &System).unwrap();
            assert_eq!(
                snap.by_pid(self_pid).map(|p| p.entry.session_id),
                Some("self-sess".to_owned())
            );
        }

        // Mismatched pid inside <pid>.json fails.
        std::fs::write(dir.join("42.json"), br#"{"pid":43,"sessionId":"bad"}"#).unwrap();
        let err = read_pid(dir, 42, &System).unwrap_err();
        assert!(
            err.to_string().contains("42.json names pid 43"),
            "err = {err}"
        );
    }
}

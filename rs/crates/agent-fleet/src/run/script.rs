//! Scripts: finding the one to run (a path, or a promoted script's name),
//! how to start it, and promoting one to the shared scripts dir.

use std::ffi::CString;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use agent_base::gostd::{filepath, os};
use agent_base::util::write_file_atomic;
use agent_store::{PromotedScript, Store, Time};

/// scripts_dir is where promoted scripts live: `<data dir>/scripts`.
pub fn scripts_dir(data_dir: &str) -> String {
    filepath::join(&[data_dir, "scripts"])
}

/// Resolved is the script a run names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// As given.
    pub given: String,
    /// Absolute.
    pub path: String,
    /// The promoted script's name, when it is one.
    pub promoted: Option<String>,
}

/// resolve finds the script a run names: a path (relative to cwd), or, for
/// a name with no '/' that isn't a file in cwd, the promoted script of that
/// name.
pub fn resolve(script: &str, cwd: &str, data_dir: &str) -> Result<Resolved> {
    if script.is_empty() {
        bail!("no script given");
    }
    let local = if filepath::is_abs(script) {
        filepath::clean(script)
    } else {
        filepath::join(&[cwd, script])
    };
    let found = |path: String, promoted: Option<String>| Resolved {
        given: script.to_string(),
        path,
        promoted,
    };
    if script.contains('/') || is_file(&local) {
        if !is_file(&local) {
            // The OS's own words: no such file, a directory, ...
            let e = std::fs::File::open(&local)
                .and_then(|f| f.metadata())
                .map(|m| {
                    if m.is_dir() {
                        "is a directory".to_string()
                    } else {
                        "not a regular file".to_string()
                    }
                })
                .unwrap_or_else(|e| os::errno_text(&e));
            bail!("script {local}: {e}");
        }
        return Ok(found(local, None));
    }
    let promoted = filepath::join(&[scripts_dir(data_dir).as_str(), script]);
    if is_file(&promoted) {
        return Ok(found(promoted, Some(script.to_string())));
    }
    bail!(
        "no script {script:?}: not a file in {cwd}, nor a promoted script (agent run --promote <file> --as {script} promotes one)"
    )
}

fn is_file(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// sha256_hex is data's sha256 in lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// is_executable says whether this user may execute path.
pub fn is_executable(path: &str) -> bool {
    let Ok(c) = CString::new(path) else {
        return false;
    };
    // SAFETY: c is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// command_line is the argv that runs the script at path (content is its
/// bytes): the script itself when it is executable and has a `#!` line or
/// is a binary; else the interpreter its `#!` line names; else sh.
pub fn command_line(path: &str, content: &[u8], executable: bool, args: &[String]) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(line) = content.strip_prefix(b"#!") {
        if executable {
            argv.push(path.to_string());
        } else {
            let line = line.split(|&c| c == b'\n').next().unwrap_or_default();
            let line = String::from_utf8_lossy(line);
            let line = line.trim();
            // As the kernel reads it: the interpreter, then at most one
            // argument, the rest of the line.
            match line.split_once([' ', '\t']) {
                Some((interp, arg)) => {
                    argv.push(interp.to_string());
                    if !arg.trim().is_empty() {
                        argv.push(arg.trim().to_string());
                    }
                }
                None if !line.is_empty() => argv.push(line.to_string()),
                None => argv.push("sh".to_string()),
            }
            argv.push(path.to_string());
        }
    } else if executable && is_binary(content) {
        argv.push(path.to_string());
    } else {
        argv.push("sh".to_string());
        argv.push(path.to_string());
    }
    argv.extend(args.iter().cloned());
    argv
}

/// is_binary says whether content starts like an executable file (ELF or
/// Mach-O).
fn is_binary(content: &[u8]) -> bool {
    const MAGICS: &[&[u8]] = &[
        b"\x7fELF",
        b"\xfe\xed\xfa\xce",
        b"\xfe\xed\xfa\xcf",
        b"\xce\xfa\xed\xfe",
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
    ];
    MAGICS.iter().any(|m| content.starts_with(m))
}

/// validate_name checks a promoted script's name: one path element of
/// letters, digits, '.', '_' and '-', not starting with '.'.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!(
            "invalid script name {name:?}: use letters, digits, '.', '_' and '-', not starting with '.'"
        );
    }
    Ok(())
}

/// What promote did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// No script had that name.
    New,
    /// The same script was already promoted under that name.
    Unchanged,
    /// The same script was promoted under that name, but its file had been
    /// changed or removed since: promote wrote it back.
    Restored,
    /// A different script had that name, and force replaced it.
    Replaced,
}

/// Promoted is a promoted script: its record, where it lives, and what
/// promote did.
#[derive(Clone, Debug, PartialEq)]
pub struct Promoted {
    pub script: PromotedScript,
    pub path: String,
    pub outcome: PromoteOutcome,
}

/// promote copies file (relative to cwd) to `<data dir>/scripts/<name>`,
/// mode 0755, and records its sha256. name defaults to the file's name.
/// What is promoted under a name is what the store records (the scripts
/// table), not what the file there holds: a different script than the one
/// recorded replaces it only with force, whatever the file holds now (it
/// may have been edited or removed since); the recorded script promoted
/// again writes its file back. A file there with no record is replaced only
/// with force too, unless it is this script.
pub fn promote(
    store: &Store,
    data_dir: &str,
    cwd: &str,
    file: &str,
    name: Option<&str>,
    by: &str,
    force: bool,
) -> Result<Promoted> {
    let src = if filepath::is_abs(file) {
        filepath::clean(file)
    } else {
        filepath::join(&[cwd, file])
    };
    let content = os::read_file(&src)?;
    let name = name
        .map(str::to_string)
        .unwrap_or_else(|| filepath::base(&src));
    validate_name(&name)?;
    let sha = sha256_hex(&content);
    let dir = scripts_dir(data_dir);
    let dest = filepath::join(&[dir.as_str(), &name]);

    let on_disk = match std::fs::read(&dest) {
        Ok(old) => Some(sha256_hex(&old)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {dest}")),
    };
    let recorded = store.get_script(&name)?;
    let outcome = match (&recorded, &on_disk) {
        (Some(r), Some(d)) if r.sha256 == sha && *d == sha => PromoteOutcome::Unchanged,
        (Some(r), _) if r.sha256 == sha => PromoteOutcome::Restored,
        (Some(r), _) if !force => bail!(
            "a different script is already promoted as {name} (sha256 {} recorded, {} in {src}); --force replaces it",
            short(&r.sha256),
            short(&sha)
        ),
        (Some(_), _) => PromoteOutcome::Replaced,
        (None, Some(d)) if *d == sha => PromoteOutcome::New,
        (None, Some(d)) if !force => bail!(
            "{dest} holds a different script (sha256 {}) that was never promoted; --force replaces it",
            short(d)
        ),
        (None, Some(_)) => PromoteOutcome::Replaced,
        (None, None) => PromoteOutcome::New,
    };
    os::mkdir_all(&dir, 0o755).with_context(|| format!("creating directory {dir}"))?;
    if on_disk.as_deref() != Some(sha.as_str()) {
        write_file_atomic(&dest, &content, 0o755).with_context(|| format!("promoting {src}"))?;
    } else {
        // The same bytes; make sure it runs.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {dest}"))?;
    }
    if let Some(old) = recorded.filter(|r| r.sha256 == sha) {
        return Ok(Promoted {
            script: old,
            path: dest,
            outcome,
        });
    }
    let record = PromotedScript {
        name,
        sha256: sha,
        source: src,
        promoted_by: by.to_string(),
        promoted_at: Time::now(),
    };
    store.set_script(&record)?;
    Ok(Promoted {
        script: record,
        path: dest,
        outcome,
    })
}

/// check_promoted refuses to run the promoted script name whose content
/// (as read for the run) has sha256 sha, unless the store records that very
/// script as promoted under name.
pub fn check_promoted(store: &Store, name: &str, sha: &str) -> Result<()> {
    match store.get_script(name)? {
        None => bail!(
            "{name} is in the scripts dir but was never promoted (no record of it); agent run --promote <file> --as {name} promotes one"
        ),
        Some(rec) if rec.sha256 != sha => bail!(
            "promoted script {name} changed since it was promoted (sha256 {} then, {} now); promote it again to run it",
            short(&rec.sha256),
            short(sha)
        ),
        Some(_) => Ok(()),
    }
}

/// job_dir is where a job's own files live: `<data dir>/jobs/<id>`.
pub fn job_dir(data_dir: &str, id: i64) -> String {
    filepath::join(&[data_dir, "jobs", &id.to_string()])
}

/// write_job_copy writes content, the script as read for job id, to
/// `<data dir>/jobs/<id>/script` (mode 0700, in dirs of mode 0700), the
/// file the job runs, so what runs is what was recorded whatever happens to
/// the original meanwhile. A copy left there by an earlier store (ids start
/// again at 1) is replaced.
pub fn write_job_copy(data_dir: &str, id: i64, content: &[u8]) -> Result<String> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let private = |p: &str| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    let jobs = filepath::join(&[data_dir, "jobs"]);
    let dir = job_dir(data_dir, id);
    let path = filepath::join(&[dir.as_str(), "script"]);
    let made = || -> std::io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&jobs)?;
        private(&jobs)?;
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        private(&dir)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        private(&path)
    };
    made().with_context(|| format!("writing the job's copy of the script to {path}"))?;
    Ok(path)
}

/// job_command_line is the argv that runs a job's copy (copy; content its
/// bytes; executable whether the original was), as command_line runs the
/// original: a `#!` script by the interpreter its `#!` line names (as the
/// kernel would, which also keeps the copy from being exec'd itself), an
/// executable binary as itself, anything else with sh. $0 is the copy's
/// path, not the original's.
pub fn job_command_line(
    copy: &str,
    content: &[u8],
    executable: bool,
    args: &[String],
) -> Vec<String> {
    let executable = executable && !content.starts_with(b"#!");
    command_line(copy, content, executable, args)
}

/// short is a sha256's first 12 hex digits, for messages.
pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_lines() {
        let args = s(&["a", "b c"]);
        assert_eq!(
            command_line("/w/x.sh", b"#!/bin/bash\necho\n", true, &args),
            s(&["/w/x.sh", "a", "b c"])
        );
        assert_eq!(
            command_line("/w/x.sh", b"#!/bin/bash\necho\n", false, &args),
            s(&["/bin/bash", "/w/x.sh", "a", "b c"])
        );
        assert_eq!(
            command_line(
                "/w/x.py",
                b"#! /usr/bin/env python3 -u \nprint()\n",
                false,
                &[]
            ),
            s(&["/usr/bin/env", "python3 -u", "/w/x.py"])
        );
        assert_eq!(
            command_line("/w/x", b"#!\n", false, &[]),
            s(&["sh", "/w/x"])
        );
        assert_eq!(
            command_line("/w/x.sh", b"echo hi\n", true, &args),
            s(&["sh", "/w/x.sh", "a", "b c"])
        );
        assert_eq!(
            command_line("/w/x.sh", b"echo hi\n", false, &[]),
            s(&["sh", "/w/x.sh"])
        );
        assert_eq!(
            command_line("/w/bin", b"\x7fELF\x02\x01", true, &[]),
            s(&["/w/bin"])
        );
    }

    #[test]
    fn sha256_of_known_input() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn names() {
        for ok in ["nightly", "build.sh", "a_b-c.1"] {
            validate_name(ok).unwrap();
        }
        for bad in ["", ".hidden", "a/b", "a b", "..", "x;y"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }
}

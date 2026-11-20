use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::gostd::filepath;
use crate::gostd::os::{LinkError, PathError};

/// Writes `data` to `path` through a temporary file in the same directory,
/// renamed over `path` once it is on disk, so a reader sees the old file or
/// the new one, never part of one. A path that is a symlink has its target
/// written, and stays a symlink. `perm` is the file's mode (Go's
/// `os.FileMode`, set with chmod, so the umask doesn't apply).
pub fn write_file_atomic(path: &str, data: &[u8], perm: u32) -> anyhow::Result<()> {
    let path = match filepath::eval_symlinks(path) {
        Ok(resolved) => resolved,
        Err(_) => path.to_string(),
    };
    let (mut tmp, name) = create_temp(
        &filepath::dir(&path),
        &format!(".{}.*", filepath::base(&path)),
    )?;
    // Removed on every return; a no-op once renamed.
    struct Remove<'a>(&'a str);
    impl Drop for Remove<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _remove = Remove(&name);
    tmp.write_all(data)
        .map_err(|e| PathError::new("write", &name, e))?;
    tmp.set_permissions(std::fs::Permissions::from_mode(perm & 0o7777))
        .map_err(|e| PathError::new("chmod", &name, e))?;
    tmp.sync_all()
        .map_err(|e| PathError::new("sync", &name, e))?;
    drop(tmp);
    std::fs::rename(&name, &path).map_err(|e| LinkError {
        op: "rename".to_string(),
        old: name.clone(),
        new: path.clone(),
        err: e,
    })?;
    Ok(())
}

/// Go's `os.CreateTemp`: a new file, mode 0600, in `dir`, named by `pattern`
/// with its last "*" replaced by a random number.
fn create_temp(dir: &str, pattern: &str) -> anyhow::Result<(std::fs::File, String)> {
    let (prefix, suffix) = match pattern.rfind('*') {
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
        None => (pattern, ""),
    };
    // Go's joinPath: plain concatenation, no cleaning.
    let prefix = if dir.is_empty() || dir.ends_with('/') {
        format!("{dir}{prefix}")
    } else {
        format!("{dir}/{prefix}")
    };
    let mut tries = 0;
    loop {
        let name = format!("{prefix}{}{suffix}", next_random());
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&name)
        {
            Ok(f) => return Ok((f, name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tries += 1;
                if tries < 10000 {
                    continue;
                }
                return Err(PathError::new("createtemp", &format!("{prefix}*{suffix}"), e).into());
            }
            Err(e) => return Err(PathError::new("open", &name, e).into()),
        }
    }
}

// next_random is a decimal number for a temp file name, as Go's CreateTemp
// uses: different across processes and calls, not secret.
fn next_random() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut x = nanos
        ^ std::process::id().rotate_left(16)
        ^ COUNTER.fetch_add(0x9e37_79b9, Ordering::Relaxed);
    // xorshift to spread the bits.
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces() {
        let dir = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let path = dir.path().join("f.json");
        let p = path.to_str().unwrap();
        write_file_atomic(p, b"one", 0o600).unwrap();
        write_file_atomic(p, b"two", 0o644).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        // No temp files left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn writes_through_a_symlink() {
        let dir = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_file_atomic(link.to_str().unwrap(), b"new", 0o600).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn a_missing_dir_is_an_open_error() {
        let err = write_file_atomic("/var/tmp/agent-base-no-such-dir/x/f", b"", 0o600).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.starts_with("open /var/tmp/agent-base-no-such-dir/x/.f."),
            "{msg}"
        );
        assert!(msg.ends_with(": no such file or directory"), "{msg}");
    }
}

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;

use agent_base::gostd::filepath;

use crate::agy::flock_held;

/// Reads `/proc/locks` once, so finding whether agy holds a presence lock takes
/// no lock at all (a probe's brief shared lock could make agy's own
/// non-blocking exclusive one fail). `/proc/locks` names a file by device and
/// inode, but its device is the superblock's, which differs from stat's on
/// btrfs subvolumes; so a lock on the same inode counts when the holder has the
/// file open (its `/proc/<pid>/fd`), or when that can't be read. Without
/// `/proc/locks` it falls back to [`flock_held`].
pub(crate) fn new_lock_probe() -> Box<dyn Fn(&str) -> bool + Send + Sync> {
    let Ok(data) = std::fs::read_to_string("/proc/locks") else {
        return Box::new(flock_held);
    };
    let mut holders: HashMap<u64, Vec<String>> = HashMap::new();
    for line in data.split('\n') {
        // "78: FLOCK  ADVISORY  WRITE 730124 00:23:15646352 0 EOF"; a process
        // waiting for the lock has "->" after the number.
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 || f[1] == "->" {
            continue;
        }
        let parts: Vec<&str> = f[5].split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let Ok(ino) = parts[2].parse::<u64>() else {
            continue;
        };
        holders.entry(ino).or_default().push(f[4].to_string());
    }
    Box::new(move |path: &str| {
        let Ok(info) = std::fs::metadata(path) else {
            return false;
        };
        let Some(pids) = holders.get(&info.ino()) else {
            return false;
        };
        for pid in pids {
            if opens(pid, path) {
                return true;
            }
        }
        false
    })
}

/// Reports whether process `pid` has `path` open, or `true` if its open files
/// can't be read.
fn opens(pid: &str, path: &str) -> bool {
    let fd_dir = filepath::join(&["/proc", pid, "fd"]);
    let Ok(fds) = std::fs::read_dir(&fd_dir) else {
        return true;
    };
    let want = filepath::eval_symlinks(path).unwrap_or_else(|_| path.to_string());
    for fd in fds.flatten() {
        if let Ok(target) = std::fs::read_link(fd.path())
            && target.to_str() == Some(want.as_str())
        {
            return true;
        }
    }
    false
}

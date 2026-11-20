use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::process::Command;

use crate::gostd::os::exit_status_text;

/// The host's processes as child → parent pids, read at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcTree(pub HashMap<i32, i32>);

impl Deref for ProcTree {
    type Target = HashMap<i32, i32>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ProcTree {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Lists every process's pid and parent pid with ps, which reads the same on
/// Linux and macOS.
pub fn read_proc_tree() -> anyhow::Result<ProcTree> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("ps: exec: \"ps\": {}", crate::gostd::os::errno_text(&e)))?;
    if !out.status.success() {
        anyhow::bail!("ps: {}", exit_status_text(&out.status));
    }
    Ok(parse_proc_tree(&String::from_utf8_lossy(&out.stdout)))
}

/// Reads ps's "pid ppid" lines; a line that isn't two numbers is skipped.
pub fn parse_proc_tree(out: &str) -> ProcTree {
    let mut t = ProcTree::default();
    for line in out.split('\n') {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 2 {
            continue;
        }
        let (Ok(pid), Ok(ppid)) = (f[0].parse::<i32>(), f[1].parse::<i32>()) else {
            continue;
        };
        t.0.insert(pid, ppid);
    }
    t
}

impl ProcTree {
    /// How many parent steps lead from `pid` up to `root`: 0 for root itself;
    /// None when root isn't among pid's ancestors (or pid isn't running).
    pub fn depth(&self, mut pid: i32, root: i32) -> Option<usize> {
        for d in 0..=self.0.len() {
            if pid == root {
                return Some(d);
            }
            match self.0.get(&pid) {
                Some(&parent) if parent != pid && parent > 0 => pid = parent,
                _ => return None,
            }
        }
        None
    }

    /// `pid`'s parent, its parent, and so on up to (not including) pid 1 or a
    /// process the tree doesn't know.
    pub fn ancestors(&self, mut pid: i32) -> Vec<i32> {
        let mut out = Vec::new();
        for _ in 0..self.0.len() {
            match self.0.get(&pid) {
                Some(&parent) if parent > 1 && parent != pid => {
                    out.push(parent);
                    pid = parent;
                }
                _ => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_depth_ancestors() {
        let t = parse_proc_tree("  1     0\n 10 1\n 20 10\n 30 20\nbad line\n 40 x\n\n");
        assert_eq!(t.len(), 4);
        assert_eq!(t.depth(30, 10), Some(2));
        assert_eq!(t.depth(10, 10), Some(0));
        assert_eq!(t.depth(10, 30), None);
        assert_eq!(t.depth(99, 1), None);
        assert_eq!(t.ancestors(30), vec![20, 10]);
        assert_eq!(t.ancestors(99), Vec::<i32>::new());
    }
}

/// `pid`'s parent, its parent, and so on up to (not including) pid 1, read
/// from /proc one process at a time: a hook needs only its own line of
/// descent, not the whole table. It never fails on Linux; the Result matches
/// the other platforms'.
pub fn proc_ancestors(mut pid: i32) -> anyhow::Result<Vec<i32>> {
    let mut out = Vec::new();
    for _ in 0..4096 {
        let Ok(data) = std::fs::read(format!("/proc/{pid}/stat")) else {
            break; // it exited, or isn't visible
        };
        let stat = String::from_utf8_lossy(&data);
        let Some(i) = stat.rfind(')') else {
            break;
        };
        let f: Vec<&str> = stat[i + 1..].split_whitespace().collect();
        if f.len() < 2 {
            break;
        }
        // field 4, ppid
        match f[1].parse::<i32>() {
            Ok(parent) if parent > 1 && parent != pid => {
                out.push(parent);
                pid = parent;
            }
            _ => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_ancestors_include_our_parent() {
        let ours = proc_ancestors(std::process::id() as i32).unwrap();
        let parent = std::os::unix::process::parent_id() as i32;
        if parent > 1 {
            assert_eq!(ours.first(), Some(&parent));
        }
        assert!(!ours.contains(&1));
    }
}

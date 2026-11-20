use super::read_proc_tree;

/// `pid`'s parent, its parent, and so on up to (not including) pid 1, from
/// one ps listing.
pub fn proc_ancestors(pid: i32) -> anyhow::Result<Vec<i32>> {
    let t = read_proc_tree()?;
    Ok(t.ancestors(pid))
}

/// A process's command line as its words, or None if it can't be read.
pub fn proc_args(pid: i32) -> Option<Vec<String>> {
    let data = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if data.is_empty() {
        return None;
    }
    let s = String::from_utf8_lossy(&data);
    Some(
        s.trim_end_matches('\0')
            .split('\0')
            .map(str::to_string)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_our_own_args() {
        let args = proc_args(std::process::id() as i32).unwrap();
        let want: Vec<String> = std::env::args().collect();
        assert_eq!(args, want);
        assert_eq!(proc_args(-1), None);
    }
}

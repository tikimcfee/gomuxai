/// No start time gomuxai can match to Claude Code's here, so entries are
/// [`crate::Verdict::Unproven`].
pub(crate) fn proc_start(_pid: i32) -> Option<String> {
    None
}

pub(crate) fn pid_domain() -> String {
    String::new()
}

pub(crate) fn zombie(_pid: i32) -> bool {
    false
}

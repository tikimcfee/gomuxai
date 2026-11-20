//! Handing this process over to another program (Go: internal/cli/exec.go).

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::history;
use super::root::Ctx;

/// Replaces this process with the program named (looked up on PATH) and argv,
/// which starts with its name. Attach hands the terminal over this way, rather
/// than running the program as a child: a signal to agent attach then reaches the
/// program directly, and nothing is left behind if it is killed. The
/// command-log row is written first, since nothing runs after. It returns only
/// if the exec failed.
pub fn exec_into(ctx: &Ctx, name: &str, argv: &[String]) -> anyhow::Result<()> {
    let path = match &ctx.exec_calls {
        // Go: stubExec's lookPath: a name with a slash is itself, any other
        // is /usr/bin/<name>.
        Some(_) if name.contains('/') => PathBuf::from(name),
        Some(_) => PathBuf::from(format!("/usr/bin/{name}")),
        None => look_path(name)?,
    };
    if !ctx.run_recorded.swap(true, Ordering::Relaxed) {
        // The line the run was given (Go: os.Args[1:]); a word that isn't
        // UTF-8 is recorded as execute read it, never a panic here, after
        // spawn or resume has already started the agent.
        let args: Vec<String> = ctx.argv.clone().unwrap_or_else(|| {
            std::env::args_os()
                .skip(1)
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        });
        history::record_command(ctx, &args, ctx.start, None);
    }
    ctx.close_bus_publisher();
    if let Some(calls) = &ctx.exec_calls {
        calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ExecCall {
                path: path.to_string_lossy().into_owned(),
                argv: argv.to_vec(),
            });
        return Ok(());
    }
    let mut cmd = std::process::Command::new(&path);
    if let Some((first, rest)) = argv.split_first() {
        cmd.arg0(first).args(rest);
    }
    let err = cmd.exec();
    anyhow::bail!("running {}: {}", path.display(), go_errno_text(&err))
}

/// What exec_into would have replaced this process with, in a test that
/// stubs it (Ctx::exec_calls; Go: stubExec's execCall).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCall {
    pub path: String,
    pub argv: Vec<String>,
}

/// Go's exec.LookPath: a name with a slash is used as is, if it is an executable
/// file; else the first executable file of that name in a PATH directory.
pub fn look_path(name: &str) -> anyhow::Result<PathBuf> {
    if name.contains('/') {
        if is_executable(Path::new(name)) {
            return Ok(PathBuf::from(name));
        }
        anyhow::bail!(
            "exec: {}: {}",
            super::flags::go_quote(name),
            missing_or_denied(Path::new(name))
        );
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        // An empty PATH element means the current directory, which Go refuses
        // (exec.ErrDot) unless the name has a slash.
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "exec: {}: executable file not found in $PATH",
        super::flags::go_quote(name)
    )
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
}

/// Why a path isn't a program, as Go's findExecutable says.
fn missing_or_denied(p: &Path) -> String {
    match std::fs::metadata(p) {
        Err(e) => format!("stat {}: {}", p.display(), go_errno_text(&e)),
        Ok(m) if m.is_dir() => "is a directory".to_string(),
        Ok(_) => "permission denied".to_string(),
    }
}

/// An exec error as Go's syscall.Errno prints it: the C library's text in lower
/// case, without Rust's " (os error N)".
fn go_errno_text(err: &std::io::Error) -> String {
    let text = err.to_string();
    let text = match text.rfind(" (os error ") {
        Some(i) => &text[..i],
        None => &text,
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn look_path_finds_programs_and_says_what_is_missing() {
        assert!(look_path("sh").is_ok_and(|p| p.ends_with("sh")));
        let err = look_path("no-such-program-gomuxai").unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"exec: "no-such-program-gomuxai": executable file not found in $PATH"#
        );
        let err = look_path("/nonexistent/x").unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"exec: "/nonexistent/x": stat /nonexistent/x: no such file or directory"#
        );
    }

    // Review 5, finding 5: the command-log row exec_into writes is the line
    // execute ran (its words that weren't UTF-8 already made U+FFFD), not
    // std::env::args(), which panicked on such a word after spawn or resume
    // had started the agent.
    #[test]
    fn exec_into_records_the_line_execute_ran() {
        let mut env = crate::cli::testenv::TestEnv::new();
        let calls = env.stub_exec();
        let line: Vec<String> = ["--backend", "claude\u{fffd}", "attach", "ex1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        env.ctx.argv = Some(line.clone());
        env.ctx.path = "attach".into();
        exec_into(&env.ctx, "g3d", &["g3d".to_string(), "attach".to_string()]).unwrap();
        assert_eq!(calls.lock().unwrap().len(), 1, "exec'd");
        let rows = env
            .store()
            .list_commands(&agent_store::CommandFilter::default())
            .unwrap();
        assert!(
            rows.len() == 1 && rows[0].args == line && rows[0].exit_code == 0,
            "recorded {rows:?}"
        );
    }

    #[test]
    fn errno_text_reads_as_go() {
        let enoent = std::io::Error::from_raw_os_error(2);
        assert_eq!(go_errno_text(&enoent), "no such file or directory");
    }
}

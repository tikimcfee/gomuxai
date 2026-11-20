//! Go's `os` errors and the few `os` calls whose errors the port prints: Go
//! says "open /x: no such file or directory" where Rust's `io::Error` says
//! "No such file or directory (os error 2)" and leaves out the path.
//!
//! The errors here carry the `io::Error` without making it their `source`, so
//! `{:#}` on an anyhow chain prints Go's one line, not the errno twice. Check
//! for a kind of failure with `downcast_ref::<PathError>()` and `.err.kind()`,
//! or with `is_not_exist`, as Go code uses `errors.Is(err, fs.ErrNotExist)`.

use std::io;

/// The text Go gives an errno: the C library's message, lowercased at the
/// start ("no such file or directory"). An `io::Error` that isn't an errno
/// keeps its own text.
pub fn errno_text(err: &io::Error) -> String {
    let Some(code) = err.raw_os_error() else {
        return err.to_string();
    };
    let full = io::Error::from_raw_os_error(code).to_string();
    let text = match full.rfind(" (os error ") {
        Some(i) => &full[..i],
        None => full.as_str(),
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Go's `*fs.PathError`: "<op> <path>: <errno text>".
#[derive(Debug, thiserror::Error)]
#[error("{op} {path}: {}", errno_text(.err))]
pub struct PathError {
    pub op: String,
    pub path: String,
    pub err: io::Error,
}

impl PathError {
    pub fn new(op: &str, path: &str, err: io::Error) -> Self {
        PathError {
            op: op.to_string(),
            path: path.to_string(),
            err,
        }
    }
}

/// Go's `*os.LinkError`, from rename, link and symlink: "<op> <old> <new>:
/// <errno text>".
#[derive(Debug, thiserror::Error)]
#[error("{op} {old} {new}: {}", errno_text(.err))]
pub struct LinkError {
    pub op: String,
    pub old: String,
    pub new: String,
    pub err: io::Error,
}

/// Go's `syscall.Errno` on its own (no op or path): its text only.
#[derive(Debug, thiserror::Error)]
#[error("{}", errno_text(.0))]
pub struct Errno(pub io::Error);

impl Errno {
    pub fn from_raw(code: i32) -> Self {
        Errno(io::Error::from_raw_os_error(code))
    }
}

/// The `io::Error` underneath `err`, if any link of its chain is one of this
/// module's errors or an `io::Error`.
pub fn io_error(err: &anyhow::Error) -> Option<&io::Error> {
    err.chain().find_map(|e| {
        if let Some(p) = e.downcast_ref::<PathError>() {
            Some(&p.err)
        } else if let Some(l) = e.downcast_ref::<LinkError>() {
            Some(&l.err)
        } else if let Some(n) = e.downcast_ref::<Errno>() {
            Some(&n.0)
        } else {
            e.downcast_ref::<io::Error>()
        }
    })
}

/// Go's `errors.Is(err, fs.ErrNotExist)`.
pub fn is_not_exist(err: &anyhow::Error) -> bool {
    io_error(err).is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
}

/// Go's `errors.Is(err, fs.ErrExist)`.
pub fn is_exist(err: &anyhow::Error) -> bool {
    io_error(err).is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
}

/// Go's `errors.Is(err, fs.ErrPermission)`.
pub fn is_permission(err: &anyhow::Error) -> bool {
    io_error(err).is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
}

/// Go's `os.ReadFile`: the error is "open <path>: ..." or "read <path>: ...".
pub fn read_file(path: &str) -> Result<Vec<u8>, PathError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| PathError::new("open", path, e))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| PathError::new("read", path, e))?;
    Ok(buf)
}

/// Go's `os.WriteFile`: create or truncate `path` with `perm` (before the
/// umask) and write `data`.
pub fn write_file(path: &str, data: &[u8], perm: u32) -> Result<(), PathError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(perm)
        .open(path)
        .map_err(|e| PathError::new("open", path, e))?;
    f.write_all(data)
        .map_err(|e| PathError::new("write", path, e))
}

/// Go's `os.Mkdir`.
pub fn mkdir(path: &str, perm: u32) -> Result<(), PathError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(perm)
        .create(path)
        .map_err(|e| PathError::new("mkdir", path, e))
}

/// Go's `os.MkdirAll`: `path` and every missing parent, each with `perm`
/// (before the umask). A failure names the directory that couldn't be made.
pub fn mkdir_all(path: &str, perm: u32) -> Result<(), PathError> {
    // Fast path: if we can tell whether path is a directory or file, stop
    // with success or error.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.is_dir() {
            return Ok(());
        }
        return Err(PathError::new(
            "mkdir",
            path,
            io::Error::from_raw_os_error(libc::ENOTDIR),
        ));
    }
    // Slow path: make sure the parent exists, then make path.
    let b = path.as_bytes();
    let mut i = b.len() as isize - 1;
    while i >= 0 && b[i as usize] == b'/' {
        i -= 1;
    }
    while i >= 0 && b[i as usize] != b'/' {
        i -= 1;
    }
    let i = i.max(0) as usize;
    let parent = &path[..i];
    if !parent.is_empty() {
        mkdir_all(parent, perm)?;
    }
    if let Err(err) = mkdir(path, perm) {
        // Handle arguments like "foo/." by double-checking that the
        // directory doesn't exist.
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

/// Go's `os.Getwd`.
pub fn getwd() -> anyhow::Result<String> {
    if let Ok(pwd) = std::env::var("PWD")
        && pwd.starts_with('/')
        && let (Ok(a), Ok(b)) = (std::fs::metadata(&pwd), std::fs::metadata("."))
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() == b.dev() && a.ino() == b.ino() {
            return Ok(pwd);
        }
    }
    let wd = std::env::current_dir().map_err(|e| PathError::new("getwd", "", e))?;
    Ok(wd.to_string_lossy().into_owned())
}

/// Go's `os.Hostname`: the kernel's name for this machine.
pub fn hostname() -> anyhow::Result<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates
    // within it on success (or truncates, which we cut at the first NUL).
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return Err(anyhow::Error::new(Errno(io::Error::last_os_error())).context("hostname"));
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Go's `os.Executable`: the path of the running executable.
pub fn executable() -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    Ok(exe.to_string_lossy().into_owned())
}

/// Go's `os.UserHomeDir`: $HOME, or an error when it isn't set.
pub fn user_home_dir() -> anyhow::Result<String> {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => Ok(h),
        _ => anyhow::bail!("$HOME is not defined"),
    }
}

/// Go's `*exec.ExitError` text for a finished command: "exit status 1", or
/// "signal: killed".
pub fn exit_status_text(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    match status.signal() {
        Some(sig) => format!("signal: {}", signal_name(sig)),
        None => status.to_string(),
    }
}

/// Go's name for a signal (`syscall.Signal.String`), for the common ones.
pub fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGHUP => "hangup".to_string(),
        libc::SIGINT => "interrupt".to_string(),
        libc::SIGQUIT => "quit".to_string(),
        libc::SIGILL => "illegal instruction".to_string(),
        libc::SIGTRAP => "trace/breakpoint trap".to_string(),
        libc::SIGABRT => "aborted".to_string(),
        libc::SIGBUS => "bus error".to_string(),
        libc::SIGFPE => "floating point exception".to_string(),
        libc::SIGKILL => "killed".to_string(),
        libc::SIGUSR1 => "user defined signal 1".to_string(),
        libc::SIGSEGV => "segmentation fault".to_string(),
        libc::SIGUSR2 => "user defined signal 2".to_string(),
        libc::SIGPIPE => "broken pipe".to_string(),
        libc::SIGALRM => "alarm clock".to_string(),
        libc::SIGTERM => "terminated".to_string(),
        n => format!("signal {n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_error_reads_like_go() {
        let err = read_file("/nonexistent/path/config.json").unwrap_err();
        assert_eq!(
            err.to_string(),
            "open /nonexistent/path/config.json: no such file or directory"
        );
        let err = anyhow::Error::new(err).context("reading config file");
        assert!(is_not_exist(&err));
        assert_eq!(
            format!("{err:#}"),
            "reading config file: open /nonexistent/path/config.json: no such file or directory"
        );
    }

    #[test]
    fn mkdir_all_names_the_failing_dir() {
        let dir = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let root = dir.path().to_str().unwrap();
        let file = format!("{root}/file");
        std::fs::write(&file, "x").unwrap();
        mkdir_all(&format!("{root}/a/b/c"), 0o700).unwrap();
        mkdir_all(&format!("{root}/a/b/c/."), 0o700).unwrap();
        let err = mkdir_all(&format!("{file}/sub"), 0o700).unwrap_err();
        // Go's MkdirAll fails on the parent, which is a file.
        assert_eq!(err.to_string(), format!("mkdir {file}: not a directory"));
        let err = mkdir_all(&file, 0o700).unwrap_err();
        assert_eq!(err.to_string(), format!("mkdir {file}: not a directory"));
    }
}

//! Running the `agent` binary, never on the UI thread, and reading the view.
//!
//! The binary is `AGENT_MANAGER_AGENT` if set, else the `agent` beside this executable,
//! else `agent` on `PATH`. `AGENT_MANAGER_FIXTURE=/path/view.json` reads the view from that
//! file instead of running `agent manage view`; in that mode an action runs only when
//! `AGENT_MANAGER_AGENT` names a binary to run it (a fake one, in tests), and otherwise says
//! what it would have run.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::Instant;

use agent_manage::View;

use crate::commands::Command;

/// The arguments that read the view: this host and every linked peer, untracked sessions
/// included, at most 2s spent counting tokens.
pub const VIEW_ARGS: &[&str] = &[
    "manage",
    "view",
    "--json",
    "--untracked",
    "--usage-budget",
    "2s",
];

/// Where the view comes from and what runs actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub fixture: Option<PathBuf>,
    /// `None` in fixture mode without `AGENT_MANAGER_AGENT`: actions are only described.
    pub agent: Option<PathBuf>,
}

impl Source {
    pub fn from_env() -> Source {
        let fixture = std::env::var_os("AGENT_MANAGER_FIXTURE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let explicit = std::env::var_os("AGENT_MANAGER_AGENT")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let agent = match (&fixture, explicit) {
            (_, Some(p)) => Some(p),
            (Some(_), None) => None,
            (None, None) => Some(beside_exe().unwrap_or_else(|| PathBuf::from("agent"))),
        };
        Source { fixture, agent }
    }

    pub fn describe(&self) -> String {
        match (&self.fixture, &self.agent) {
            (Some(f), Some(a)) => format!("fixture {} · actions run {}", f.display(), a.display()),
            (Some(f), None) => format!("fixture {} · actions not run", f.display()),
            (None, Some(a)) => a.display().to_string(),
            (None, None) => String::new(),
        }
    }
}

/// The note of a live build: `AGENT_MANAGER_LIVE` names a file whose first line says what
/// is on screen but not approved (`tools/live` writes it). `None` for an installed build.
pub fn live_note() -> Option<String> {
    let path = std::env::var_os("AGENT_MANAGER_LIVE").filter(|v| !v.is_empty())?;
    Some(live_note_of(std::fs::read_to_string(path).ok().as_deref()))
}

fn live_note_of(file: Option<&str>) -> String {
    match file.and_then(|t| t.lines().next()).map(str::trim) {
        Some(l) if !l.is_empty() => format!("live build · {l}"),
        _ => "live build".to_string(),
    }
}

fn beside_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("agent");
    p.is_file().then_some(p)
}

/// What a command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub command: Command,
    /// `None` if it didn't run or was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u128,
    /// Why it couldn't start, or that it wasn't run (fixture mode).
    pub error: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// One line: the command and how it ended.
    pub fn summary(&self) -> String {
        let end = match (self.code, self.error.is_empty()) {
            (_, false) => self.error.clone(),
            (Some(c), true) => format!("exit {c}"),
            (None, true) => "killed".to_string(),
        };
        format!("{} → {end} ({}ms)", self.command.line(), self.elapsed_ms)
    }

    /// A short title: how it ended, and the start of the command.
    pub fn headline(&self) -> String {
        let end = match (self.code, self.error.is_empty()) {
            (_, false) if self.error.starts_with("not run") => "not run".to_string(),
            (_, false) => "couldn't run".to_string(),
            (Some(0), true) => "done".to_string(),
            (Some(c), true) => format!("failed (exit {c})"),
            (None, true) => "killed".to_string(),
        };
        format!("{end}: {}", crate::clip(&self.command.line(), 56))
    }

    /// Everything it printed, for a person.
    pub fn text(&self) -> String {
        let mut s = String::new();
        if !self.error.is_empty() {
            s.push_str(&self.error);
            s.push('\n');
        }
        if !self.stdout.trim().is_empty() {
            s.push_str(self.stdout.trim_end());
            s.push('\n');
        }
        if !self.stderr.trim().is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(self.stderr.trim_end());
            s.push('\n');
        }
        if s.is_empty() {
            s.push_str("(no output)\n");
        }
        s
    }
}

/// Runs `agent` with the command's arguments, waiting for it.
pub fn run(agent: Option<&Path>, command: &Command) -> Output {
    let start = Instant::now();
    let mut out = Output {
        command: command.clone(),
        code: None,
        stdout: String::new(),
        stderr: String::new(),
        elapsed_ms: 0,
        error: String::new(),
    };
    let Some(agent) = agent else {
        out.error = format!(
            "not run: the view is a fixture (set AGENT_MANAGER_AGENT to run actions): {}",
            command.line()
        );
        return out;
    };
    match Process::new(agent)
        .args(&command.args)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) => {
            out.code = o.status.code();
            out.stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            out.stderr = String::from_utf8_lossy(&o.stderr).into_owned();
        }
        Err(e) => out.error = format!("couldn't run {}: {e}", agent.display()),
    }
    out.elapsed_ms = start.elapsed().as_millis();
    out
}

/// Reads the view from the fixture, or by running `agent manage view`.
pub fn read_view(source: &Source) -> Result<View, String> {
    if let Some(f) = &source.fixture {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
        return serde_json::from_str(&text).map_err(|e| format!("{}: {e}", f.display()));
    }
    let out = run(source.agent.as_deref(), &Command::new(VIEW_ARGS));
    if !out.ok() {
        let mut why = out.text();
        why.truncate(why.trim_end().len());
        return Err(format!("{}: {why}", out.command.line()));
    }
    serde_json::from_str(&out.stdout).map_err(|e| format!("{}: bad JSON: {e}", out.command.line()))
}

/// Runs `job` on its own thread and hands its result to `done` there; the UI wakes itself
/// from `done` (Makepad: `Cx::post_action`).
pub fn spawn<R, J, D>(job: J, done: D)
where
    R: Send + 'static,
    J: FnOnce() -> R + Send + 'static,
    D: FnOnce(R) + Send + 'static,
{
    std::thread::spawn(move || done(job()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("agent-manager-core-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn fake_agent(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("agent");
        std::fs::write(
            &p,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$(dirname \"$0\")/calls\"\necho \"ran $1\"\necho oops >&2\n[ \"$1\" = fail ] && exit 3\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    // The only test that writes and runs a script: two doing it at once race (a fork in
    // one inherits the other's open write and exec fails with "Text file busy").
    #[cfg(unix)]
    #[test]
    fn runs_the_binary_and_reports() {
        let d = scratch("run");
        let agent = fake_agent(&d);
        let out = run(Some(&agent), &Command::new(&["cleanup", "x y"]));
        assert!(out.ok(), "{out:?}");
        assert_eq!(out.stdout, "ran cleanup\n");
        assert!(out.text().contains("oops"));
        let calls = std::fs::read_to_string(d.join("calls")).unwrap();
        assert_eq!(calls, "cleanup\nx y\n", "arguments go through unsplit");
        let out = run(Some(&agent), &Command::new(&["fail"]));
        assert_eq!(out.code, Some(3));
        assert!(out.summary().contains("exit 3"));
        assert_eq!(out.headline(), "failed (exit 3): agent fail");
        let out = run(Some(&d.join("nope")), &Command::new(&["list"]));
        assert!(out.summary().contains("couldn't run"));

        // A view that isn't JSON says which command printed it.
        let src = Source {
            fixture: None,
            agent: Some(agent),
        };
        let e = read_view(&src).unwrap_err();
        assert!(
            e.contains("agent manage view --json") && e.contains("bad JSON"),
            "{e}"
        );
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn fixture_mode_without_an_agent_runs_nothing() {
        let out = run(None, &Command::new(&["cleanup", "builder"]));
        assert!(!out.ok());
        assert!(out.error.contains("not run"));
    }

    #[test]
    fn reads_a_fixture() {
        let d = scratch("fixture");
        let f = d.join("view.json");
        let v = crate::fixture::view(crate::fixture::default_now());
        std::fs::write(&f, serde_json::to_string(&v).unwrap()).unwrap();
        let src = Source {
            fixture: Some(f.clone()),
            agent: None,
        };
        assert_eq!(read_view(&src).unwrap(), v);
        std::fs::write(&f, "{").unwrap();
        assert!(read_view(&src).unwrap_err().contains("view.json"));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn spawn_hands_back_the_result() {
        let (tx, rx) = mpsc::channel();
        spawn(|| 6 * 7, move |r| tx.send(r).unwrap());
        assert_eq!(rx.recv().unwrap(), 42);
    }
    #[test]
    fn live_note_is_the_files_first_line() {
        assert_eq!(live_note_of(None), "live build");
        assert_eq!(live_note_of(Some("\n")), "live build");
        assert_eq!(
            live_note_of(Some("2 unapproved: wider held column\nmore")),
            "live build · 2 unapproved: wider held column"
        );
    }
}

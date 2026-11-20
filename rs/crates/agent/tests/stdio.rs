//! The built binary writing to a pipe whose reader is gone, as Go's runtime
//! takes it (review 5, finding 3): a write to a closed stdout ends the
//! process quietly by SIGPIPE (the shell sees 141), with nothing on stderr and
//! no failed run in the command log; agent transcript, which ignores SIGPIPE
//! as the Go does, takes it as the end of its stream and exits 0. Each test
//! runs a copy of the binary in a temp dir under /var/tmp, whose .gomuxai is
//! the test's.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Bin {
    dir: tempfile::TempDir,
    bin: PathBuf,
}

impl Bin {
    fn new() -> Bin {
        let dir = tempfile::Builder::new()
            .prefix("agent-cli-stdio-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        // A termd socket dir of its own, with no daemon in it: nothing here
        // can reach the live g3d-termd.
        std::fs::create_dir_all(dir.path().join("termd")).unwrap();
        let bin = bin_dir.join("agent");
        let built = env!("CARGO_BIN_EXE_agent");
        if std::fs::hard_link(built, &bin).is_err() {
            std::fs::copy(built, &bin).unwrap();
        }
        Bin { dir, bin }
    }

    fn command(&self, words: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(words)
            .current_dir(self.dir.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir.path().join("home"))
            .env("CLAUDE_CONFIG_DIR", self.dir.path().join("home/.claude"))
            .env("AGENT_HOST", "hub")
            .env("G3D_TERMD_DIR", self.dir.path().join("termd"))
            .stdin(Stdio::null());
        c
    }

    /// Runs agent with stdout a pipe whose reader has already gone; returns
    /// how it ended and its stderr.
    fn run_into_closed_pipe(&self, words: &[&str]) -> (std::process::ExitStatus, String) {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let out = self
            .command(words)
            .stdout(writer)
            .stderr(Stdio::piped())
            .output()
            .expect("running agent");
        (
            out.status,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn history(&self) -> String {
        let out = self
            .command(&["history"])
            .output()
            .expect("running agent history");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

#[test]
fn a_closed_stdout_ends_the_run_quietly_by_sigpipe() {
    let b = Bin::new();
    // Commands that reach no g3d-termd (this binary would find the live one).
    for words in [&["statuses"][..], &["help", "status"], &["version"]] {
        let (status, stderr) = b.run_into_closed_pipe(words);
        assert_eq!(
            (status.signal(), stderr.as_str()),
            (Some(libc::SIGPIPE), ""),
            "agent {words:?}: {status:?}"
        );
    }
    let history = b.history();
    assert!(
        !history.contains("statuses"),
        "a run that died writing was recorded:\n{history}"
    );
}

#[test]
fn transcript_takes_a_closed_stdout_as_the_end() {
    let b = Bin::new();
    let path = write_transcript(b.dir.path());
    let (status, stderr) = b.run_into_closed_pipe(&["transcript", &path]);
    assert_eq!(
        (status.code(), stderr.as_str()),
        (Some(0), ""),
        "{status:?}"
    );
}

/// A synthetic Claude transcript, long enough to need more than one write.
fn write_transcript(dir: &Path) -> String {
    let sid = "3f3a9c00-0000-4000-8000-000000000001";
    let project = dir.join("home/.claude/projects/-var-tmp-x");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join(format!("{sid}.jsonl"));
    let body: String = (0..200)
        .map(|i| {
            format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"hi {i}\"}},\"sessionId\":\"{sid}\",\"timestamp\":\"2026-11-20T00:00:00Z\",\"uuid\":\"u{i}\",\"cwd\":\"/var/tmp/x\"}}\n"
            )
        })
        .collect();
    std::fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

//! Helpers the test files share. Temp dirs go under /var/tmp, never /tmp.

#![allow(dead_code)]

use std::process::Command;

/// A temp dir under /var/tmp, removed when dropped.
pub fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("agent-backend-test-")
        .tempdir_in("/var/tmp")
        .expect("temp dir")
}

/// The path of `name` in `dir`, as a string.
pub fn path_in(dir: &tempfile::TempDir, name: &str) -> String {
    dir.path()
        .join(name)
        .to_str()
        .expect("utf-8 path")
        .to_string()
}

/// Lets a real shell split `args` (shell words) and returns them, as
/// `sh -c "printf '%s\000' <args>"` prints them.
pub fn shell_args(args: &str) -> Vec<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("printf '%s\\000' {args}"))
        .output()
        .expect("sh");
    assert!(
        out.status.success(),
        "sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).expect("utf-8");
    s.strip_suffix('\0')
        .unwrap_or(&s)
        .split('\0')
        .map(str::to_string)
        .collect()
}

// Screens trimmed from Claude Code v2.1.310 (visible pane, ANSI stripped).

pub const CLAUDE_BASH_DIALOG: &str = "● Bash(touch fx-a.txt)

────────────────────────────────────────────────────────────────────────────────
 Bash command
 touch fx-a.txt
 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and always allow access to fx1 from this project
   3. Yes, and switch to auto mode · auto mode handles these prompts for you
   4. No

 Esc to cancel · Tab to amend

";

pub const CLAUDE_WRITE_DIALOG: &str = "● Write(fx-b.txt)

────────────────────────────────────────────────────────────────────────────────
 Create file
 fx-b.txt
 Do you want to create fx-b.txt?
 ❯ 1. Yes
   2. Yes, and switch to accept edits (auto-approve file edits and common file
      commands) for this session (shift+tab)
   3. No

 Esc to cancel · Tab to amend
";

pub const CLAUDE_RUNNING: &str = "● Bash(touch fx-a.txt)
  ⎿  Running…

✻ Cogitating… (3s · esc to interrupt)

────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  ⏸ manual mode on · ? for shortcuts
";

pub const CLAUDE_IDLE_AFTER_LIST: &str = "● Options:
  1. Yes, refactor it
  2. No, leave it
  Esc to cancel is what the dialog says.

────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  ⏸ manual mode on · ? for shortcuts
";

/// A screen from testdata (captured with escapes from Claude Code v2.1.319).
pub fn read_screen(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name);
    std::fs::read_to_string(path).expect("testdata")
}

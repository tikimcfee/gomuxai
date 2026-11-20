// Records what Go's toolchain embeds in a binary and buildinfo reads back: the
// VCS revision, whether the tree had uncommitted changes, and the commit time,
// plus the compiler's version. Nothing here may fail the build: without git (a
// source tarball, a sandbox) the values are empty and `agent version` says
// "(no VCS info)", as a Go build outside a checkout does.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    println!("cargo:rerun-if-changed=build.rs");

    let revision = git(&manifest_dir, &["rev-parse", "HEAD"]).unwrap_or_default();
    let mut time = String::new();
    let mut modified = String::new();
    if !revision.is_empty() {
        // Go records the commit time in UTC (vcs.time).
        if let Some(secs) = git(
            &manifest_dir,
            &[
                "-c",
                "log.showsignature=false",
                "show",
                "-s",
                "--format=%ct",
            ],
        )
        .and_then(|s| s.parse::<i64>().ok())
        {
            time = rfc3339_utc(secs);
        }
        // Go's vcs.modified is true when `git status --porcelain` prints anything,
        // untracked files included. --no-optional-locks keeps status from
        // rewriting the index, which would make this script rerun every build.
        if let Some(status) = git_raw(
            &manifest_dir,
            &["--no-optional-locks", "status", "--porcelain"],
        ) {
            modified = (!status.trim().is_empty()).to_string();
        }
        watch_git(&manifest_dir);
    }

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let toolchain = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            // "rustc 1.98.1 (48a229cea 2026-09-01)" -> "rustc 1.98.1"
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rustc unknown".to_string());

    println!("cargo:rustc-env=AGENT_BUILD_VCS_REVISION={revision}");
    println!("cargo:rustc-env=AGENT_BUILD_VCS_TIME={time}");
    println!("cargo:rustc-env=AGENT_BUILD_VCS_MODIFIED={modified}");
    println!("cargo:rustc-env=AGENT_BUILD_TOOLCHAIN={toolchain}");
}

fn git_raw(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    git_raw(dir, args)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Reruns this script when HEAD moves (a commit, a checkout) or the index
/// changes (an add), so the revision and the dirty flag follow the tree. An
/// edit not yet added doesn't rerun it: rerunning on every build would rebuild
/// every crate that depends on this one.
fn watch_git(dir: &Path) {
    let abs = |p: String| {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    let Some(git_dir) = git(dir, &["rev-parse", "--git-dir"]).map(abs) else {
        return;
    };
    let common = git(dir, &["rev-parse", "--git-common-dir"])
        .map(abs)
        .unwrap_or_else(|| git_dir.clone());
    let mut watch = vec![
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common.join("packed-refs"),
    ];
    if let Some(head_ref) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        watch.push(common.join(head_ref));
    }
    for p in watch {
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
}

/// Formats seconds since the epoch as RFC 3339 in UTC, e.g.
/// "2026-10-24T21:18:44Z".
fn rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// Howard Hinnant's days -> (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

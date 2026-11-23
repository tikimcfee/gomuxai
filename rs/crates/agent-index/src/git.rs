//! The `git` CLI, as the crawl uses it: file lists with blob ids, tree ids,
//! renames since the last crawl, and history for co-change.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Runs git in `dir`, returning stdout; fails with git's stderr.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ok(dir: &Path, args: &[&str]) -> Option<String> {
    run(dir, args).ok().map(|s| s.trim().to_string())
}

/// Whether `dir` is the top of a work tree of its own (not a subdirectory of
/// another repo).
pub fn is_toplevel(dir: &Path) -> bool {
    match ok(dir, &["rev-parse", "--show-toplevel"]) {
        Some(top) => std::fs::canonicalize(&top).ok().as_deref() == Some(dir),
        None => false,
    }
}

/// When `dir` is a linked worktree, the main repo's directory.
pub fn worktree_of(dir: &Path) -> Option<std::path::PathBuf> {
    if !is_toplevel(dir) {
        return None;
    }
    let common = ok(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = std::fs::canonicalize(common).ok()?;
    if common == dir.join(".git") {
        return None;
    }
    // .../main/.git → .../main
    let main = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent()?.to_path_buf()
    } else {
        common
    };
    if main == dir { None } else { Some(main) }
}

pub fn head(dir: &Path) -> String {
    ok(dir, &["rev-parse", "--verify", "-q", "HEAD"]).unwrap_or_default()
}

/// The first-parent root commit: the project's identity when it has no remote.
pub fn root_commit(dir: &Path) -> String {
    ok(
        dir,
        &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
    )
    .and_then(|s| s.lines().last().map(str::to_string))
    .unwrap_or_default()
}

/// The origin remote, normalized: `git@host:a/b.git` and `https://host/a/b`
/// both read `host/a/b`.
pub fn remote(dir: &Path) -> String {
    let Some(url) = ok(dir, &["config", "--get", "remote.origin.url"]) else {
        return String::new();
    };
    normalize_remote(&url)
}

pub fn normalize_remote(url: &str) -> String {
    let mut u = url.trim().to_string();
    for p in ["https://", "http://", "ssh://", "git://", "git+ssh://"] {
        if let Some(r) = u.strip_prefix(p) {
            u = r.to_string();
        }
    }
    if let Some((_, r)) = u.split_once('@') {
        u = r.to_string();
    }
    // scp form host:path
    if let Some((h, p)) = u.split_once(':')
        && !h.contains('/')
        && !p.starts_with("//")
    {
        let p = p
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches('/');
        u = format!("{h}/{p}");
    }
    u.trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase()
}

/// One file git lists.
#[derive(Debug, Clone)]
pub struct GitFile {
    pub rel: String,
    pub mode: String,
    /// Its blob id, or the hash of its working-tree content when it differs.
    pub key: String,
    pub untracked: bool,
    /// Changed in the working tree (or untracked): its dir's tree id doesn't hold.
    pub dirty: bool,
}

fn z_split(s: &str) -> impl Iterator<Item = &str> {
    s.split('\0').filter(|x| !x.is_empty())
}

/// Every tracked file (with its index blob id) and every untracked one that
/// isn't ignored; working-tree changes hashed with `git hash-object`.
pub fn list_files(dir: &Path) -> Result<Vec<GitFile>> {
    let mut files: Vec<GitFile> = Vec::new();
    let staged = run(dir, &["ls-files", "-s", "--cached", "-z"])?;
    let mut seen = std::collections::HashSet::new();
    for rec in z_split(&staged) {
        // "<mode> <blob> <stage>\t<path>"
        let Some((meta, rel)) = rec.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split(' ');
        let mode = parts.next().unwrap_or("").to_string();
        let blob = parts.next().unwrap_or("").to_string();
        if !seen.insert(rel.to_string()) {
            continue; // merge stages
        }
        files.push(GitFile {
            rel: rel.to_string(),
            mode,
            key: blob,
            untracked: false,
            dirty: false,
        });
    }
    let deleted: std::collections::HashSet<String> = z_split(&run(dir, &["ls-files", "-z", "-d"])?)
        .map(str::to_string)
        .collect();
    files.retain(|f| !deleted.contains(&f.rel));
    let modified: std::collections::HashSet<String> =
        z_split(&run(dir, &["ls-files", "-z", "-m"])?)
            .map(str::to_string)
            .collect();
    let others = run(dir, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    for rel in z_split(&others) {
        // Never hash what an untracked symlink points at.
        let link =
            std::fs::symlink_metadata(dir.join(rel)).is_ok_and(|m| m.file_type().is_symlink());
        files.push(GitFile {
            rel: rel.to_string(),
            mode: if link { "120000" } else { "100644" }.into(),
            key: String::new(),
            untracked: true,
            dirty: true,
        });
    }
    // Index vs HEAD: staged changes also make a dir's HEAD tree id wrong.
    let staged_changes: std::collections::HashSet<String> =
        z_split(&run(dir, &["diff", "--cached", "--name-only", "-z"]).unwrap_or_default())
            .map(str::to_string)
            .collect();
    let mut to_hash: Vec<usize> = Vec::new();
    for (i, f) in files.iter_mut().enumerate() {
        if staged_changes.contains(&f.rel) {
            f.dirty = true;
        }
        if (f.untracked || modified.contains(&f.rel)) && f.mode != "120000" && f.mode != "160000" {
            f.dirty = true;
            to_hash.push(i);
        }
    }
    if !to_hash.is_empty() {
        let paths: Vec<String> = to_hash.iter().map(|&i| files[i].rel.clone()).collect();
        let keys = hash_objects(dir, &paths)?;
        for (&i, k) in to_hash.iter().zip(keys) {
            files[i].key = k;
        }
    }
    Ok(files)
}

/// `git hash-object` over paths (relative to dir), in one process.
fn hash_objects(dir: &Path, paths: &[String]) -> Result<Vec<String>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["hash-object", "--no-filters", "--stdin-paths"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git hash-object")?;
    let mut stdin = child.stdin.take().context("git hash-object stdin")?;
    let input: String = paths.iter().map(|p| format!("{p}\n")).collect();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    let s = String::from_utf8_lossy(&out.stdout);
    let mut keys: Vec<String> = s.lines().map(str::to_string).collect();
    // A path that vanished or is a dir makes git stop; key the rest by nothing.
    keys.resize(paths.len(), String::new());
    Ok(keys)
}

/// HEAD's tree ids by directory ("" is the top).
pub fn tree_ids(dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(top) = ok(dir, &["rev-parse", "HEAD^{tree}"]) {
        out.insert(String::new(), top);
    }
    let Ok(s) = run(dir, &["ls-tree", "-r", "-t", "-z", "HEAD"]) else {
        return out;
    };
    for rec in z_split(&s) {
        let Some((meta, rel)) = rec.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split(' ');
        let _mode = parts.next();
        if parts.next() == Some("tree")
            && let Some(id) = parts.next()
        {
            out.insert(rel.to_string(), id.to_string());
        }
    }
    out
}

/// Renames between two commits: (old, new) paths.
pub fn renames(dir: &Path, from: &str, to: &str) -> Vec<(String, String)> {
    let Ok(s) = run(dir, &["diff", "-M", "--name-status", "-z", from, to]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut it = z_split(&s);
    while let Some(status) = it.next() {
        if status.starts_with('R') {
            let (Some(a), Some(b)) = (it.next(), it.next()) else {
                break;
            };
            out.push((a.to_string(), b.to_string()));
        } else if status.starts_with('C') {
            it.next();
            it.next();
        } else {
            it.next();
        }
    }
    out
}

/// How many commits `to` is ahead of `from` (0 when unknown).
pub fn commits_between(dir: &Path, from: &str, to: &str) -> usize {
    ok(dir, &["rev-list", "--count", &format!("{from}..{to}")])
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// The files each of the last `n` commits touched.
pub fn log_files(dir: &Path, n: usize) -> Vec<Vec<String>> {
    let Ok(s) = run(
        dir,
        &[
            "log",
            "--no-merges",
            "--name-only",
            "--format=%x01",
            "-n",
            &n.to_string(),
        ],
    ) else {
        return Vec::new();
    };
    s.split('\x01')
        .map(|c| {
            c.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|c: &Vec<String>| !c.is_empty())
        .collect()
}

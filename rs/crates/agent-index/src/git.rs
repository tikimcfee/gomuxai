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
    // host:port/path or the scp form host:path. Only an all-digit first
    // segment is a port (`git@host:1password/cli` keeps its org).
    if let Some((h, p)) = u.split_once(':')
        && !h.contains('/')
        && !p.starts_with("//")
    {
        let p = match p.split_once('/') {
            Some((port, rest)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                rest
            }
            _ => p,
        };
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
    /// Its index blob id ("" for an untracked file). When `modified`, the
    /// working tree differs and the caller keys it (git.rs never reads a
    /// working-tree file: the caller checks it is safe to read first).
    pub key: String,
    pub untracked: bool,
    /// The working-tree file differs from the index (or is untracked).
    pub modified: bool,
    /// Changed against HEAD in the index or working tree: its dirs' HEAD tree
    /// ids don't hold.
    pub dirty: bool,
}

fn z_split(s: &str) -> impl Iterator<Item = &str> {
    s.split('\0').filter(|x| !x.is_empty())
}

/// The paths that differ from HEAD in the index or the working tree,
/// deletions included (`git diff HEAD`): their directories' HEAD tree ids
/// no longer describe them. Empty without a HEAD.
pub fn changed_vs_head(dir: &Path) -> Vec<String> {
    run(dir, &["diff", "--name-only", "--no-renames", "-z", "HEAD"])
        .map(|s| z_split(&s).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Renames between HEAD and the index or working tree (a `git mv` not yet
/// committed).
pub fn worktree_renames(dir: &Path) -> Vec<(String, String)> {
    renames_in(run(dir, &["diff", "-M", "--name-status", "-z", "HEAD"]).unwrap_or_default())
}

/// Every tracked file (with its index blob id) and every untracked one that
/// isn't ignored, marking those whose working-tree content differs. Nested
/// repos git lists as `dir/` are left out.
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
            modified: false,
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
        // A nested repo is listed as its directory: not a file.
        if rel.ends_with('/') {
            continue;
        }
        files.push(GitFile {
            rel: rel.to_string(),
            mode: "100644".into(),
            key: String::new(),
            untracked: true,
            modified: true,
            dirty: true,
        });
    }
    let changed: std::collections::HashSet<String> = changed_vs_head(dir).into_iter().collect();
    for f in files.iter_mut() {
        if modified.contains(&f.rel) {
            f.modified = true;
        }
        if f.modified || changed.contains(&f.rel) {
            f.dirty = true;
        }
    }
    Ok(files)
}

/// `git hash-object` over regular files (relative to dir) the caller has
/// checked are safe to read. A path git can't hash (it vanished, or can't be
/// read) gets "" and the rest still get theirs.
pub fn hash_paths(dir: &Path, paths: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::with_capacity(paths.len());
    while keys.len() < paths.len() {
        let rest = &paths[keys.len()..];
        let got = hash_objects(dir, rest).unwrap_or_default();
        let n = got.len();
        keys.extend(got);
        if n < rest.len() {
            // git stopped at rest[n]: it has no key; go on after it.
            keys.push(String::new());
        }
    }
    keys
}

/// `git hash-object` over paths, in one process: the keys of the paths up to
/// the first it couldn't hash.
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
    Ok(s.lines().map(str::to_string).collect())
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
    renames_in(run(dir, &["diff", "-M", "--name-status", "-z", from, to]).unwrap_or_default())
}

/// The renames in `git diff -M --name-status -z` output.
fn renames_in(s: String) -> Vec<(String, String)> {
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

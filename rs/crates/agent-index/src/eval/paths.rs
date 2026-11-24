//! Any path mapped to the same file in its repo's main checkout, as the
//! Python harness's `Paths` does it: a git worktree to its main checkout by
//! its `.git` file, a worktree that is gone by how its path is named.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// A path with `.` and `..` resolved by its text, as `os.path.normpath`.
pub fn normpath(p: &str) -> String {
    let abs = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else if !abs {
                    out.push("..");
                }
            }
            x => out.push(x),
        }
    }
    let body = out.join("/");
    match (abs, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => ".".to_string(),
        (false, false) => body,
    }
}

/// `os.path.dirname`.
pub fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => {
            let d = &p[..i];
            let t = d.trim_end_matches('/');
            if t.is_empty() { "/".into() } else { t.into() }
        }
        None => String::new(),
    }
}

/// `os.path.relpath(path, start)` for absolute paths.
pub fn relpath(path: &str, start: &str) -> String {
    let a: Vec<Component> = Path::new(path).components().collect();
    let b: Vec<Component> = Path::new(start).components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..b.len() {
        parts.push("..".into());
    }
    for c in &a[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// `os.path.join(a, b)` for a relative b.
pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

pub fn inside(path: &str, d: &str) -> bool {
    path == d || path.starts_with(&format!("{}/", d.trim_end_matches('/')))
}

fn is_dir(p: &str) -> bool {
    Path::new(p).is_dir()
}

static CLAUDE_WT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+)/\.claude/worktrees/([^/]+)(/.*)?$").unwrap());
static WT_DIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+)-wt/([^/]+)(/.*)?$").unwrap());
static WT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*)/([^/]+?)-wt-([^/]+)(/.*)?$").unwrap());
static GITDIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^gitdir: (.*)/\.git/worktrees/[^/]+/?$").unwrap());

/// (checkout top, main checkout top).
type Repo = Option<(String, String)>;

pub struct Paths {
    pub dev_root: String,
    repo: RefCell<HashMap<String, Repo>>,
}

impl Paths {
    pub fn new(dev_root: &Path) -> Paths {
        let real = std::fs::canonicalize(dev_root).unwrap_or_else(|_| dev_root.to_path_buf());
        Paths {
            dev_root: real.to_string_lossy().into_owned(),
            repo: RefCell::new(HashMap::new()),
        }
    }

    /// The repo holding directory `d`: (its checkout's top, its main
    /// checkout's top).
    pub fn repo_of(&self, d: &str) -> Repo {
        let mut chain = Vec::new();
        let mut cur = d.to_string();
        let mut found: Repo = None;
        loop {
            if let Some(r) = self.repo.borrow().get(&cur) {
                found = r.clone();
                break;
            }
            chain.push(cur.clone());
            let dotgit = join(&cur, ".git");
            let p = Path::new(&dotgit);
            if p.is_dir() {
                found = Some((cur.clone(), cur.clone()));
                break;
            }
            if p.is_file() {
                let main = main_of_worktree(&dotgit).unwrap_or_else(|| cur.clone());
                found = Some((cur.clone(), main));
                break;
            }
            let parent = dirname(&cur);
            if parent == cur || parent.is_empty() {
                break;
            }
            cur = parent;
        }
        let mut cache = self.repo.borrow_mut();
        for c in chain {
            cache.insert(c, found.clone());
        }
        found
    }

    /// A worktree that no longer exists, mapped by how its path is named:
    /// REPO/.claude/worktrees/NAME, REPO-wt/NAME, ANY/REPO-wt-NAME, and a
    /// sibling REPO-NAME, for a REPO in the dev root.
    fn by_convention(&self, path: &str) -> String {
        if let Some(m) = CLAUDE_WT.captures(path)
            && !is_dir(&format!("{}/.claude/worktrees/{}", &m[1], &m[2]))
        {
            return format!("{}{}", &m[1], m.get(3).map_or("", |x| x.as_str()));
        }
        if let Some(m) = WT_DIR.captures(path)
            && is_dir(&m[1])
            && !is_dir(&format!("{}-wt/{}", &m[1], &m[2]))
        {
            return format!("{}{}", &m[1], m.get(3).map_or("", |x| x.as_str()));
        }
        if let Some(m) = WT_NAME.captures(path)
            && !is_dir(&format!("{}/{}-wt-{}", &m[1], &m[2], &m[3]))
        {
            let main = join(&self.dev_root, &m[2]);
            if is_dir(&main) {
                return format!("{main}{}", m.get(4).map_or("", |x| x.as_str()));
            }
        }
        if self.in_dev(path) && path != self.dev_root {
            let rel = relpath(path, &self.dev_root);
            let (first, rest) = rel.split_once('/').unwrap_or((&rel, ""));
            if !is_dir(&join(&self.dev_root, first)) {
                let mut cut = first.rfind('-');
                while let Some(c) = cut.filter(|c| *c > 0) {
                    let main = join(&self.dev_root, &first[..c]);
                    if Path::new(&join(&main, ".git")).exists() {
                        return if rest.is_empty() {
                            main
                        } else {
                            join(&main, rest)
                        };
                    }
                    cut = first[..c].rfind('-');
                }
            }
        }
        path.to_string()
    }

    /// The path in the main checkout (which may not exist there).
    pub fn canon(&self, path: &str) -> String {
        let mut path = self.by_convention(&normpath(path));
        let mut d = if is_dir(&path) {
            path.clone()
        } else {
            dirname(&path)
        };
        while !d.is_empty() && !is_dir(&d) {
            d = dirname(&d);
        }
        let repo = if d.is_empty() { None } else { self.repo_of(&d) };
        if let Some((top, main)) = repo
            && top != main
        {
            path = join(&main, &relpath(&path, &top));
        }
        normpath(&path)
    }

    pub fn in_dev(&self, path: &str) -> bool {
        path == self.dev_root || path.starts_with(&format!("{}/", self.dev_root))
    }

    /// (the directory the baseline searches, whether it is a project).
    pub fn project_of(&self, cwd: &str) -> (String, bool) {
        let mut c = self.canon(cwd);
        while !c.is_empty() && !is_dir(&c) {
            c = dirname(&c);
        }
        if c.is_empty() || !self.in_dev(&c) {
            return (self.dev_root.clone(), false);
        }
        if let Some((_, main)) = self.repo_of(&c)
            && self.in_dev(&main)
        {
            return (main, true);
        }
        let known = c != self.dev_root;
        (c, known)
    }
}

fn main_of_worktree(dotgit: &str) -> Option<String> {
    let text = std::fs::read_to_string(dotgit).ok()?;
    let line = text.lines().next()?.trim();
    GITDIR.captures(line).map(|m| m[1].to_string())
}

/// A path as a `PathBuf` (for the callers that want one).
pub fn pb(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_paths() {
        assert_eq!(normpath("/a/b/../c/./d/"), "/a/c/d");
        assert_eq!(normpath("/.."), "/");
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(relpath("/a/b/c", "/a"), "b/c");
        assert_eq!(relpath("/a", "/a"), ".");
        assert!(inside("/a/b", "/a") && !inside("/ab", "/a"));
    }

    /// A worktree maps to its main checkout, a gone one by its name.
    #[test]
    fn worktrees() {
        let t = tempfile::tempdir().unwrap();
        let dev = t.path().join("dev");
        let main = dev.join("proj");
        std::fs::create_dir_all(main.join(".git/worktrees/wt1")).unwrap();
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/a.rs"), "x").unwrap();
        let wt = dev.join("proj-wt-feature");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let main_s = std::fs::canonicalize(&main)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {main_s}/.git/worktrees/wt1\n"),
        )
        .unwrap();
        let p = Paths::new(&dev);
        let wt_s = format!("{}/proj-wt-feature", p.dev_root);
        assert_eq!(
            p.canon(&format!("{wt_s}/src/a.rs")),
            format!("{main_s}/src/a.rs")
        );
        // Gone: proj-old (no such dir) maps to proj by its name.
        assert_eq!(
            p.canon(&format!("{}/proj-old/src/a.rs", p.dev_root)),
            format!("{main_s}/src/a.rs")
        );
        assert_eq!(
            p.project_of(&format!("{main_s}/src")),
            (main_s.clone(), true)
        );
        assert_eq!(p.project_of("/nowhere/at/all"), (p.dev_root.clone(), false));
    }
}

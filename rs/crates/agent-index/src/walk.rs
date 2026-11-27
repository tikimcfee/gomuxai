//! A plain walk for a project that isn't a git repo, honoring its `.gitignore`
//! files reasonably (comments, `!`, `/`-anchored and dir-only patterns, `*`,
//! `?` and `**`), skipping build output, and never following a symlink.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use regex::Regex;

/// One file the walk found.
#[derive(Debug, Clone)]
pub struct WalkFile {
    pub rel: String,
    pub symlink: bool,
    pub size: u64,
    /// Nanoseconds since the epoch, so two edits in one second differ.
    pub mtime: i64,
}

/// Walking stops past this many files, so a huge plain tree can't stall a crawl.
pub const MAX_FILES: usize = 100_000;

struct Rule {
    re: Regex,
    negate: bool,
    dir_only: bool,
}

/// The rules of one `.gitignore`, for paths relative to its directory.
struct Ignore {
    base: String,
    rules: Vec<Rule>,
}

fn glob_to_regex(glob: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                // `**/` any dirs; trailing `**` anything.
                if chars.get(i + 2) == Some(&'/') {
                    out.push_str("(?:.*/)?");
                    i += 3;
                    continue;
                }
                out.push_str(".*");
                i += 2;
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '[' => {
                if let Some(end) = chars[i..].iter().position(|&x| x == ']') {
                    let class: String = chars[i + 1..i + end].iter().collect();
                    out.push('[');
                    out.push_str(&class.replace('!', "^").replace('\\', "\\\\"));
                    out.push(']');
                    i += end + 1;
                    continue;
                }
                out.push_str("\\[");
            }
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

impl Ignore {
    fn parse(base: &str, text: &str) -> Ignore {
        let mut rules = Vec::new();
        for line in text.lines() {
            let mut l = line.trim_end();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let negate = l.starts_with('!');
            if negate {
                l = &l[1..];
            }
            let dir_only = l.ends_with('/');
            let l = l.trim_end_matches('/');
            let anchored = l.contains('/');
            let l = l.trim_start_matches('/');
            let body = glob_to_regex(l);
            let pat = if anchored {
                format!("^{body}(?:/.*)?$")
            } else {
                format!("(?:^|/){body}(?:/.*)?$")
            };
            if let Ok(re) = Regex::new(&pat) {
                rules.push(Rule {
                    re,
                    negate,
                    dir_only,
                });
            }
        }
        Ignore {
            base: base.to_string(),
            rules,
        }
    }

    /// Some(true) ignored, Some(false) re-included, None no opinion.
    fn verdict(&self, rel: &str, is_dir: bool) -> Option<bool> {
        let sub = if self.base.is_empty() {
            rel
        } else {
            rel.strip_prefix(&self.base)?.strip_prefix('/')?
        };
        let mut v = None;
        for r in &self.rules {
            // A dir-only rule never matches a file: files under such a dir are
            // never reached, since the walk doesn't enter an ignored dir.
            if r.dir_only && !is_dir {
                continue;
            }
            if r.re.is_match(sub) {
                v = Some(!r.negate);
            }
        }
        v
    }
}

/// Every file under `root` the walk keeps, relative, with `/` separators.
/// `skip` holds relative dirs not to enter (nested projects).
/// The ignore files in force in a directory, its own last.
type Ignores = Rc<Vec<Rc<Ignore>>>;

/// What a walk found, and whether it stopped at [`MAX_FILES`].
#[derive(Debug, Default)]
pub struct Walk {
    pub files: Vec<WalkFile>,
    pub truncated: bool,
}

/// Every file under `root` the walk keeps, relative, with `/` separators,
/// level by level (breadth first): when a huge tree hits [`MAX_FILES`], what
/// is cut is the deepest level, never a later sibling's top. `skip` holds
/// relative dirs not to enter (nested projects); `max_depth` limits how many
/// directory levels are entered (1: the top's files only; 0: no limit).
pub fn walk(root: &Path, skip: &[String], max_depth: usize) -> Walk {
    walk_capped(root, skip, max_depth, MAX_FILES)
}

/// [`walk`] with a file cap of its own.
pub fn walk_capped(root: &Path, skip: &[String], max_depth: usize, cap: usize) -> Walk {
    let mut out = Walk::default();
    let mut queue: VecDeque<(String, usize, Ignores)> = VecDeque::new();
    queue.push_back((String::new(), 1, Rc::new(Vec::new())));
    while let Some((prefix, depth, inherited)) = queue.pop_front() {
        let dir = if prefix.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&prefix)
        };
        let ignores = match fs::read_to_string(dir.join(".gitignore")) {
            Ok(t) => {
                let mut v = (*inherited).clone();
                v.push(Rc::new(Ignore::parse(&prefix, &t)));
                Rc::new(v)
            }
            Err(_) => inherited,
        };
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if crate::files::skipped(name.as_str())
                    || skip.iter().any(|s| s == &rel)
                    || ignored(&ignores, &rel, true)
                    || (max_depth > 0 && depth >= max_depth)
                {
                    continue;
                }
                queue.push_back((rel, depth + 1, ignores.clone()));
            } else if ft.is_file() || ft.is_symlink() {
                if ignored(&ignores, &rel, false) {
                    continue;
                }
                if out.files.len() >= cap {
                    out.truncated = true;
                    return out;
                }
                let md = e.metadata().ok();
                out.files.push(WalkFile {
                    rel,
                    symlink: ft.is_symlink(),
                    size: md.as_ref().map(|m| m.len()).unwrap_or(0),
                    mtime: md
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0),
                });
            }
        }
    }
    out
}

fn ignored(ignores: &[Rc<Ignore>], rel: &str, is_dir: bool) -> bool {
    let mut v = false;
    for ig in ignores {
        if let Some(x) = ig.verdict(rel, is_dir) {
            v = x;
        }
    }
    v
}

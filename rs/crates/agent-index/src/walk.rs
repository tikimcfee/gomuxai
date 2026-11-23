//! A plain walk for a project that isn't a git repo, honoring its `.gitignore`
//! files reasonably (comments, `!`, `/`-anchored and dir-only patterns, `*`,
//! `?` and `**`), skipping build output, and never following a symlink.

use std::fs;
use std::path::Path;

use regex::Regex;

use crate::files::SKIP_DIRS;

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
pub fn walk(root: &Path, skip: &[String]) -> Vec<WalkFile> {
    let mut out = Vec::new();
    let mut ignores: Vec<Ignore> = Vec::new();
    walk_dir(root, "", skip, &mut ignores, &mut out);
    out
}

fn ignored(ignores: &[Ignore], rel: &str, is_dir: bool) -> bool {
    let mut v = false;
    for ig in ignores {
        if let Some(x) = ig.verdict(rel, is_dir) {
            v = x;
        }
    }
    v
}

fn walk_dir(
    root: &Path,
    prefix: &str,
    skip: &[String],
    ignores: &mut Vec<Ignore>,
    out: &mut Vec<WalkFile>,
) {
    if out.len() >= MAX_FILES {
        return;
    }
    let dir = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    let pushed = match fs::read_to_string(dir.join(".gitignore")) {
        Ok(t) => {
            ignores.push(Ignore::parse(prefix, &t));
            true
        }
        Err(_) => false,
    };
    let Ok(rd) = fs::read_dir(&dir) else {
        if pushed {
            ignores.pop();
        }
        return;
    };
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
            if SKIP_DIRS.contains(&name.as_str())
                || skip.iter().any(|s| s == &rel)
                || ignored(ignores, &rel, true)
            {
                continue;
            }
            walk_dir(root, &rel, skip, ignores, out);
        } else if ft.is_file() || ft.is_symlink() {
            if ignored(ignores, &rel, false) {
                continue;
            }
            let md = e.metadata().ok();
            out.push(WalkFile {
                rel,
                symlink: ft.is_symlink(),
                size: md.as_ref().map(|m| m.len()).unwrap_or(0),
                mtime: md
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0),
            });
            if out.len() >= MAX_FILES {
                break;
            }
        }
    }
    if pushed {
        ignores.pop();
    }
}

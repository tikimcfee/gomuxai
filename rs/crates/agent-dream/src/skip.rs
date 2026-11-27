//! The skip list: directory names no walk of a project goes into (build
//! output, caches, VCS internals), shared by the index's crawl and
//! watcher and by dreams (docs/DREAMING.md, "Scope").
//!
//! It lives in a file a person can edit, `~/.config/gomuxai/skip-dirs.txt`
//! (`$XDG_CONFIG_HOME/gomuxai/` when set, whatever the index's `--config`):
//! one name per line, `#` starting a comment. With no file, the list is
//! [`DEFAULT_DIRS`]. A process reads it once, at its first use, and again on
//! [`reload`] (the index watcher does when told the list changed). A dream
//! never changes it: it proposes a change, and a person accepts or rejects
//! it (`agent dream proposals`).

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result, bail};

/// The list with no file: VCS internals, build output and caches.
pub const DEFAULT_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pixi",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "dist",
    "build",
    ".gomuxai",
    ".cache",
];

/// The file's name, in `~/.config/gomuxai/` (or `$XDG_CONFIG_HOME/gomuxai/`).
pub const FILE_NAME: &str = "skip-dirs.txt";

/// The list in effect in this process, once read.
static DIRS: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// The default file: `$XDG_CONFIG_HOME/gomuxai/skip-dirs.txt`, else
/// `~/.config/gomuxai/skip-dirs.txt`.
pub fn default_file() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("gomuxai").join(FILE_NAME))
}

/// The names a file's text lists: one per line, comments and blanks left out.
pub fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The list at `path`: its names, or the defaults when there is no file.
pub fn read(path: Option<&Path>) -> Result<Vec<String>> {
    let Some(path) = path else {
        return Ok(defaults());
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(defaults()),
        Err(e) => Err(e).with_context(|| format!("reading the skip list {}", path.display())),
    }
}

fn defaults() -> Vec<String> {
    DEFAULT_DIRS.iter().map(|s| s.to_string()).collect()
}

/// The list in effect: read from the default file at first use (the
/// defaults if it can't be read).
pub fn dirs() -> Vec<String> {
    if let Some(d) = DIRS.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return d.clone();
    }
    reload();
    DIRS.read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(defaults)
}

/// Whether a walk skips a directory of this name.
pub fn contains(name: &str) -> bool {
    {
        let g = DIRS.read().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = g.as_ref() {
            return d.iter().any(|s| s == name);
        }
    }
    dirs().iter().any(|s| s == name)
}

/// Reads the default file again; what it couldn't read leaves the defaults.
/// Returns the list now in effect.
pub fn reload() -> Vec<String> {
    let list = read(default_file().as_deref()).unwrap_or_else(|_| defaults());
    *DIRS.write().unwrap_or_else(|e| e.into_inner()) = Some(list.clone());
    list
}

/// Sets the list in effect for this process (tests, and a caller that read
/// another file).
pub fn set(list: Vec<String>) {
    *DIRS.write().unwrap_or_else(|e| e.into_inner()) = Some(list);
}

/// A short hash of a list (sha256 of its names, one per line, first 12 hex
/// digits), so an eval run or a crawl report can record which list it ran
/// under.
pub fn fingerprint(list: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for n in list {
        h.update(n.as_bytes());
        h.update(b"\n");
    }
    let d = h.finalize();
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Checks a name for the list: one directory name, no path.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('#')
        || name.trim() != name
        || name == "."
        || name == ".."
    {
        bail!(
            "{name:?} isn't a directory name: the skip list holds names (target, node_modules), not paths"
        );
    }
    Ok(())
}

/// What `add` or `remove` did to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added,
    Removed,
    /// The list already was so.
    Unchanged,
}

/// Adds `name` to the file at `path` (writing the defaults first when
/// there is no file), keeping its comments and order.
pub fn add(path: &Path, name: &str) -> Result<Change> {
    check_name(name)?;
    let mut text = file_text(path)?;
    if parse(&text).iter().any(|n| n == name) {
        return Ok(Change::Unchanged);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(name);
    text.push('\n');
    write(path, &text)?;
    Ok(Change::Added)
}

/// Removes `name` from the file at `path` (writing the defaults first when
/// there is no file), keeping the other lines as they are.
pub fn remove(path: &Path, name: &str) -> Result<Change> {
    check_name(name)?;
    let text = file_text(path)?;
    if !parse(&text).iter().any(|n| n == name) {
        return Ok(Change::Unchanged);
    }
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| l.split('#').next().unwrap_or("").trim() != name)
        .collect();
    let mut out = kept.join("\n");
    out.push('\n');
    write(path, &out)?;
    Ok(Change::Removed)
}

/// The file's text, or the defaults as a file would hold them.
fn file_text(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut t = String::from(
                "# Directory names no walk of a project goes into: the index's crawl and\n\
                 # watcher, and dreams. One per line. agent index skip add|remove edits it.\n",
            );
            for d in DEFAULT_DIRS {
                t.push_str(d);
                t.push('\n');
            }
            Ok(t)
        }
        Err(e) => Err(e).with_context(|| format!("reading the skip list {}", path.display())),
    }
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_edited_in_place_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gomuxai").join(FILE_NAME);
        assert_eq!(
            read(Some(&path)).unwrap(),
            defaults(),
            "no file: the defaults"
        );
        assert_eq!(add(&path, "out").unwrap(), Change::Added);
        assert_eq!(add(&path, "out").unwrap(), Change::Unchanged);
        let list = read(Some(&path)).unwrap();
        assert!(list.contains(&"out".to_string()) && list.contains(&"target".to_string()));
        assert_eq!(remove(&path, "dist").unwrap(), Change::Removed);
        assert_eq!(remove(&path, "dist").unwrap(), Change::Unchanged);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# Directory names"),
            "the comment stays:\n{text}"
        );
        assert!(!parse(&text).contains(&"dist".to_string()));
        assert!(add(&path, "a/b").is_err() && add(&path, "").is_err() && add(&path, "..").is_err());
        assert_eq!(parse("x # a comment\n\n  y\n# z\n"), ["x", "y"]);
    }
}

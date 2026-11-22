//! What a memory dir says about its repos that the repos no longer bear out
//! (docs/DREAMING.md, `agent dream memory check --repo`): backticked paths that
//! exist in none of them, `make` targets no Makefile defines, and `agent`
//! subcommands and flags this binary doesn't know. Mechanical and report-only;
//! anything it can't be sure of is left alone, so a finding is worth a look.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::memory::{Finding, INDEX, check_file_name};

/// The `agent` commands a binary knows, so the check doesn't need the CLI. Each
/// command is keyed by its path below `agent` ("" is `agent` itself, "dream
/// memory check" a leaf), aliases included.
#[derive(Debug, Clone, Default)]
pub struct AgentCommands {
    pub commands: BTreeMap<String, AgentCommand>,
}

/// One `agent` command.
#[derive(Debug, Clone, Default)]
pub struct AgentCommand {
    /// Its subcommands' names and aliases.
    pub subs: BTreeSet<String>,
    /// Whether a word after it must be a subcommand (a group): otherwise it may be
    /// a positional argument.
    pub needs_sub: bool,
    /// The long flags it takes, global ones included, without the dashes.
    pub flags: BTreeSet<String>,
}

/// What the check compares a memory dir with.
#[derive(Debug, Clone, Default)]
pub struct Repos {
    pub roots: Vec<PathBuf>,
    /// None leaves `agent` commands unchecked.
    pub agent: Option<AgentCommands>,
}

/// Extensions that make a token without a `/` a file name.
const EXTENSIONS: &[&str] = &[
    "rs", "go", "md", "toml", "json", "jsonl", "yaml", "yml", "sh", "py", "js", "ts", "tsx", "c",
    "h", "cpp", "swift", "plist", "lock", "db", "log", "txt", "html", "css", "service", "timer",
    "mod", "sum", "conf", "ini", "sql", "nix",
];

/// Extensions of source files: the only bare names (no `/`) checked.
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "go", "py", "js", "ts", "tsx", "c", "h", "cpp", "swift", "sh",
];

/// Directories the walk of a repo skips: build output and VCS internals.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__"];

/// A walk of a repo stops here, so a huge tree can't stall a check.
const MAX_ENTRIES: usize = 200_000;

/// One repo as the check sees it.
struct Repo {
    root: PathBuf,
    /// Every file and dir under it, relative, with `/` separators.
    entries: BTreeSet<String>,
    /// Make targets, or None when it has no Makefile; `unsure` when an include
    /// couldn't be read.
    make: Option<MakeTargets>,
}

struct MakeTargets {
    targets: BTreeSet<String>,
    unsure: bool,
}

impl Repo {
    fn load(root: &Path) -> Repo {
        let mut entries = BTreeSet::new();
        walk(root, "", &mut entries);
        Repo {
            root: root.to_path_buf(),
            entries,
            make: make_targets(root),
        }
    }
}

fn walk(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut names: Vec<(String, bool)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let is_dir = e.file_type().ok()?.is_dir();
            Some((name, is_dir))
        })
        .collect();
    names.sort();
    for (name, is_dir) in names {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let rel = format!("{prefix}{name}");
        let path = dir.join(&name);
        // Build output, VCS internals, and checkouts of their own inside the
        // repo (worktrees, vendored clones): what is there isn't the repo's.
        if is_dir && (SKIP_DIRS.contains(&name.as_str()) || path.join(".git").exists()) {
            out.insert(rel);
            continue;
        }
        out.insert(rel.clone());
        if is_dir {
            walk(&path, &format!("{rel}/"), out);
        }
    }
}

/// The targets a repo's Makefile defines (`target:` lines, `include`d files too),
/// or None without one.
fn make_targets(root: &Path) -> Option<MakeTargets> {
    let file = ["GNUmakefile", "makefile", "Makefile"]
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())?;
    let mut t = MakeTargets {
        targets: BTreeSet::new(),
        unsure: false,
    };
    read_makefile(root, &file, &mut t, 0);
    Some(t)
}

fn read_makefile(root: &Path, file: &Path, t: &mut MakeTargets, depth: usize) {
    let Ok(text) = fs::read_to_string(file) else {
        t.unsure = true;
        return;
    };
    for line in text.lines() {
        if let Some(rest) = line
            .strip_prefix("include ")
            .or_else(|| line.strip_prefix("-include "))
            .or_else(|| line.strip_prefix("sinclude "))
        {
            for inc in rest.split_whitespace() {
                if inc.contains('$') || inc.contains('*') || depth > 4 {
                    t.unsure = true;
                } else {
                    read_makefile(root, &root.join(inc), t, depth + 1);
                }
            }
            continue;
        }
        if line.starts_with(['\t', ' ', '#']) {
            continue;
        }
        let Some((head, tail)) = line.split_once(':') else {
            continue;
        };
        // `x := 1` and `x ::= 1` are assignments, not rules.
        if tail.starts_with('=') || tail.starts_with(":=") || head.contains('=') {
            continue;
        }
        if head.contains('$') {
            // A target named by a variable: anything might be one.
            t.unsure = true;
            continue;
        }
        for target in head.split_whitespace() {
            if !target.contains('%') {
                t.targets.insert(target.to_string());
            }
        }
    }
}

/// The backticked spans of a line (single backticks, outside fences).
fn spans(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find('`') {
        let after = &rest[i + 1..];
        let Some(j) = after.find('`') else { break };
        let s = after[..j].trim();
        if !s.is_empty() {
            out.push(s);
        }
        rest = &after[j + 1..];
    }
    out
}

/// Strips `:12` or `:12:3` (a line reference) and a trailing `/`'s friends from a
/// path token.
fn trim_path(tok: &str) -> &str {
    let mut t = tok.trim_end_matches([',', '.', ';', ')']);
    // `x.rs:12`, `x.rs:12:3`, `x.py:Class`: a path, then where in it.
    if let Some((head, _)) = t.split_once(':')
        && has_extension(head)
    {
        t = head;
    }
    t.strip_prefix("./").unwrap_or(t)
}

fn has_extension(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && EXTENSIONS.contains(&ext),
        None => false,
    }
}

/// Whether a token is a path the check can judge: has a `/` or a known
/// extension, no spaces, no URL, link, glob, variable or placeholder.
fn looks_like_path(tok: &str) -> bool {
    if tok.is_empty() || tok.chars().any(char::is_whitespace) {
        return false;
    }
    if tok.contains("://") || tok.starts_with("www.") || tok.contains('@') {
        return false;
    }
    const PLACEHOLDER: &[char] = &['<', '>', '*', '?', '[', ']', '{', '}', '$', '|', '=', '%'];
    if tok.contains(PLACEHOLDER) || tok.contains("...") || tok.contains('…') {
        return false;
    }
    if tok.contains("::") || tok.starts_with('-') {
        return false;
    }
    tok.contains('/') || has_extension(tok)
}

/// Why a path token exists in none of the repos, or None when it does or the
/// check can't tell. `memory_files` are the memory dir's own file names.
fn stale_path(tok: &str, repos: &[Repo], memory_files: &BTreeSet<String>) -> Option<String> {
    let p = trim_path(tok);
    if !looks_like_path(p) || p.starts_with('~') {
        return None;
    }
    if p.starts_with('/') {
        let path = Path::new(p);
        let under: Vec<&Repo> = repos.iter().filter(|r| path.starts_with(&r.root)).collect();
        if under.is_empty() {
            return None;
        }
        if under
            .iter()
            .any(|r| path.exists() || r.has(path) || r.ignores(path))
        {
            return None;
        }
        return Some("no such file in the repo it is under".into());
    }
    let rel = p.trim_end_matches('/');
    if rel.is_empty() || rel.starts_with("../") || rel.split('/').any(|c| c == "..") {
        return None;
    }
    if !rel.contains('/') {
        // A bare file name: anywhere in a repo will do. The memory dir's own
        // files are named this way too. Only source files: a bare data or doc
        // name (`peer.pub.json`, `notes.md`) is as often a file outside every
        // repo, and a bare dir (`memory/`) may be anywhere.
        let base_ext = rel.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if !has_extension(rel)
            || !CODE_EXTENSIONS.contains(&base_ext)
            || memory_files.contains(rel)
            || rel == INDEX
        {
            return None;
        }
        if repos.iter().any(|r| r.has_name(rel) || r.truncated()) {
            return None;
        }
        return Some("no file of that name in any repo".into());
    }
    if repos
        .iter()
        .any(|r| r.has_rel(rel) || r.has_suffix(rel) || r.ignores(&r.root.join(rel)))
    {
        // At a repo's root, or a tail of a path in one (crates/x/src/y.rs under
        // rs/, tools/x under app/), or a path the repo ignores (local settings,
        // build output), which may well exist elsewhere.
        return None;
    }
    if repos.iter().any(Repo::truncated) {
        // A walk cut short: a tail could be in what it didn't reach.
        return None;
    }
    let first = rel.split('/').next().unwrap_or("");
    if repos.iter().any(|r| r.entries.contains(first)) {
        return Some("not in any repo".into());
    }
    if has_extension(rel) {
        // Not from a repo's root. Only when the dir it names is in a repo is it
        // the repo's file that went; otherwise it may be another project's.
        let parent = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if repos
            .iter()
            .any(|r| r.has_rel(parent) || r.has_suffix(parent))
        {
            return Some("not in any repo, though its dir is".into());
        }
    }
    // `origin/main`, `claude/model`, `and/or`: not a path any repo starts.
    None
}

impl Repo {
    fn has(&self, abs: &Path) -> bool {
        abs.strip_prefix(&self.root)
            .ok()
            .and_then(|r| r.to_str())
            .is_some_and(|r| r.is_empty() || self.has_rel(r))
    }

    fn has_rel(&self, rel: &str) -> bool {
        self.entries.contains(rel) || self.under_skipped(rel) || self.root.join(rel).exists()
    }

    /// Whether a path is inside a dir the walk skips (target/, .git/): those
    /// are checked on disk only.
    fn under_skipped(&self, rel: &str) -> bool {
        rel.split('/')
            .next()
            .is_some_and(|f| SKIP_DIRS.contains(&f) && self.root.join(rel).exists())
    }

    /// Whether the walk stopped at MAX_ENTRIES.
    fn truncated(&self) -> bool {
        self.entries.len() >= MAX_ENTRIES
    }

    /// Whether git ignores the path in this repo (`git check-ignore`; never when
    /// the repo isn't one or git can't be run).
    fn ignores(&self, path: &Path) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["check-ignore", "-q", "--no-index", "--"])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn has_name(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e == name || e.ends_with(&format!("/{name}")))
    }

    fn has_suffix(&self, tail: &str) -> bool {
        let t = format!("/{tail}");
        self.entries.iter().any(|e| e.ends_with(&t))
    }
}

/// Why `make ...` names a target no Makefile defines, as (target, why).
fn stale_make(tok: &str, repos: &[Repo]) -> Vec<(String, String)> {
    let Some(rest) = tok.strip_prefix("make ") else {
        return Vec::new();
    };
    let makes: Vec<&MakeTargets> = repos.iter().filter_map(|r| r.make.as_ref()).collect();
    if makes.is_empty() || makes.iter().any(|m| m.unsure) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for w in rest.split_whitespace() {
        if matches!(w, "&&" | "||" | ";" | "|") || w.starts_with('>') {
            break;
        }
        if w.starts_with('-') {
            // -C dir, -f file: another Makefile than a repo's own.
            if matches!(w, "-C" | "-f" | "--file" | "--directory")
                || w.starts_with("-C")
                || w.starts_with("-f")
            {
                return Vec::new();
            }
            continue;
        }
        if w.contains('=') {
            continue;
        }
        if !is_word(w) {
            return out;
        }
        if !makes.iter().any(|m| m.targets.contains(w)) {
            out.push((w.to_string(), "no Makefile defines that target".into()));
        }
    }
    out
}

/// A word that names something (a subcommand, a target), not a placeholder.
fn is_word(w: &str) -> bool {
    let mut cs = w.chars();
    cs.next().is_some_and(|c| c.is_ascii_lowercase())
        && w.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// The words of an `agent ...` token after `agent`, or None if it isn't one.
fn agent_words(tok: &str) -> Option<Vec<&str>> {
    let mut words = tok.split_whitespace();
    let first = words.next()?;
    let bin = first.rsplit('/').next().unwrap_or(first);
    if bin != "agent" || (first != "agent" && !first.ends_with("/agent")) {
        return None;
    }
    let mut out = Vec::new();
    for w in words {
        if matches!(w, "&&" | "||" | ";" | "|") || w.starts_with('>') || w.starts_with('#') {
            break;
        }
        out.push(w);
    }
    Some(out)
}

/// The command an `agent ...` token names, as its path, and a finding's why when
/// a word that must be a subcommand is none. `Err(None)` is a token the check
/// can't judge.
fn resolve_agent(words: &[&str], cmds: &AgentCommands) -> Result<String, Option<String>> {
    let mut path = String::new();
    let mut seen_word = false;
    for w in words {
        if w.starts_with('-') {
            // A flag before the subcommand: only -q is a root flag that takes
            // no value, so anything else could swallow the next word.
            if !seen_word && !matches!(*w, "-q" | "--quiet") {
                return Err(None);
            }
            break;
        }
        seen_word = true;
        let Some(cmd) = cmds.commands.get(&path) else {
            return Err(None);
        };
        if cmd.subs.contains(*w) {
            let next = if path.is_empty() {
                (*w).to_string()
            } else {
                format!("{path} {w}")
            };
            // An alias resolves to the command it names, keyed by its own path.
            if !cmds.commands.contains_key(&next) {
                return Err(None);
            }
            path = next;
            continue;
        }
        if cmd.needs_sub && is_word(w) {
            let name = if path.is_empty() {
                "agent".to_string()
            } else {
                format!("agent {path}")
            };
            return Err(Some(format!("{name} has no subcommand {w:?}")));
        }
        break;
    }
    Ok(path)
}

/// The long flags in some words, without dashes or values.
fn long_flags<'a>(words: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    words
        .into_iter()
        .filter_map(|w| w.strip_prefix("--"))
        .map(|f| f.split_once('=').map_or(f, |(n, _)| n))
        .filter(|f| {
            !f.is_empty()
                && f.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
        .collect()
}

/// Findings for `agent` tokens on one line: unknown subcommands, and flags the
/// command doesn't take. A flag in its own backticks on the line counts when the
/// line names exactly one `agent` command and nothing else that looks like a
/// command.
fn stale_agent(line_spans: &[&str], cmds: &AgentCommands) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut resolved: Vec<String> = Vec::new();
    let mut other_commands = 0;
    for s in line_spans {
        let Some(words) = agent_words(s) else {
            if s.contains(' ') && !s.starts_with('-') {
                other_commands += 1;
            }
            continue;
        };
        if words.is_empty() {
            continue;
        }
        match resolve_agent(&words, cmds) {
            Err(Some(why)) => out.push(((*s).to_string(), why)),
            Err(None) => {}
            Ok(path) => {
                let cmd = &cmds.commands[&path];
                for f in long_flags(words.iter().copied()) {
                    if !cmd.flags.contains(f) {
                        out.push(((*s).to_string(), no_flag(&path, f)));
                    }
                }
                resolved.push(path);
            }
        }
    }
    if let [path] = resolved.as_slice()
        && other_commands == 0
        && !path.is_empty()
    {
        let cmd = &cmds.commands[path];
        for s in line_spans {
            if s.starts_with("--") && !s.contains(' ') {
                for f in long_flags([*s]) {
                    if !cmd.flags.contains(f) {
                        out.push(((*s).to_string(), no_flag(path, f)));
                    }
                }
            }
        }
    }
    out
}

fn no_flag(path: &str, flag: &str) -> String {
    format!("agent {path} has no --{flag} flag")
}

/// The stale findings for a memory dir against some repos, sorted.
pub fn check(dir: &Path, repos: &Repos) -> Result<Vec<Finding>> {
    let loaded: Vec<Repo> = repos.roots.iter().map(|r| Repo::load(r)).collect();
    let mut names: Vec<String> = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type()?.is_file() && check_file_name(&name).is_ok() {
            names.push(name);
        }
    }
    names.sort();
    let memory_files: BTreeSet<String> = names.iter().cloned().collect();
    let mut out = Vec::new();
    for name in &names {
        let Ok(text) = fs::read_to_string(dir.join(name)) else {
            continue;
        };
        let mut fence = false;
        for (i, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("```") {
                fence = !fence;
                continue;
            }
            if fence {
                continue;
            }
            let ss = spans(line);
            let lineno = i + 1;
            let push = |out: &mut Vec<Finding>,
                        f: fn(String, usize, String, String) -> Finding,
                        token: String,
                        why: String| {
                out.push(f(name.clone(), lineno, token, why));
            };
            for s in &ss {
                if !s.contains(' ')
                    && let Some(why) = stale_path(s, &loaded, &memory_files)
                {
                    push(&mut out, Finding::stale_path, (*s).to_string(), why);
                }
                for (target, why) in stale_make(s, &loaded) {
                    push(&mut out, Finding::stale_make, format!("make {target}"), why);
                }
            }
            if let Some(cmds) = &repos.agent {
                for (tok, why) in stale_agent(&ss, cmds) {
                    push(&mut out, Finding::stale_agent, tok, why);
                }
            }
        }
    }
    out.sort_by(|a, b| a.order().cmp(&b.order()));
    out.dedup();
    Ok(out)
}

#[cfg(test)]
#[path = "stale_test.rs"]
mod tests;

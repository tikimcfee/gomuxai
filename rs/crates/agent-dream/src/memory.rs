//! Memory directories (Claude Code's per-project `memory/`) and the guard a dream
//! writes them through (docs/DREAMING.md).
//!
//! A memory dir holds one fact per `.md` file: frontmatter (`name`, `description`,
//! `metadata.type`), a body with `[[name]]` links to other memories, and, for what a
//! dream writes, source lines (`[source: claude:<id>, friction:<uid>, commit:<hash>;
//! added: YYYY-MM-DD]`). `MEMORY.md` is the index: one `- [Title](file.md) — hook`
//! line per memory.
//!
//! Writes go through [`put`] and [`remove`], which take the version the caller read
//! (optimistic concurrency), refuse what would lose evidence, and save the prior file
//! under the dream's snapshot dir first. [`take_snapshot`] copies the dir when a
//! dream opens; [`guard`] compares the dir to it at commit, and [`restore`] puts it
//! back. [`check`] is the deterministic, report-only lint.
//!
//! A snapshot dir holds `files/` (the dir as the dream found it), `MANIFEST` (the
//! file names it held, one per line), and `history/` (each prior version saved by a
//! put, remove or restore, as `<file>.<n>`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The index file's name.
pub const INDEX: &str = "MEMORY.md";

/// The version of a file that doesn't exist: `put --expect new` creates one.
pub const NEW: &str = "new";

/// The memory types frontmatter may name in `metadata.type`.
pub const TYPES: &[&str] = &["user", "feedback", "project", "reference"];

/// A rewrite (of the index) or the whole dir may lose at most this share of its
/// entries: more than a quarter gone is refused.
const MAX_DROP_NUM: usize = 1;
const MAX_DROP_DEN: usize = 4;

/// Dice similarity at or above which two entries are near duplicates.
pub const NEAR_DUPLICATE: f64 = 0.8;
/// Entries shorter than this many words aren't compared for near duplicates.
pub const MIN_WORDS: usize = 5;

static SOURCE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[source:\s*([^\]]*?);\s*added:\s*(\d{4}-\d{2}-\d{2})\s*\]").unwrap()
});
static REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]*:[^\s,;\]]+$").unwrap());
static LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\[\]]+)\]\]").unwrap());
static INDEX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*[-*]\s+\[([^\]]*)\]\(([^)\s]+)\)\s*(?:[—–-]+\s*(.*))?$").unwrap()
});

/// A file's frontmatter: the keys the memory format uses, and every key as read
/// (`metadata.type` for nested ones).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub mem_type: String,
    pub fields: BTreeMap<String, String>,
}

/// A `[source: ...; added: ...]` line: what it cites and when it was added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Source {
    /// `harness:session-id`, `friction:<uid>`, `commit:<hash>`, ...
    pub refs: Vec<String>,
    pub added: String,
}

/// One fact file.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryFile {
    pub file: String,
    pub version: String,
    /// None when the file has no valid frontmatter (`error` says why).
    pub frontmatter: Option<Frontmatter>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip)]
    pub body: String,
    pub links: Vec<String>,
    pub sources: Vec<Source>,
}

impl MemoryFile {
    /// Every reference its source lines cite.
    pub fn citations(&self) -> BTreeSet<String> {
        citations(&self.sources)
    }
}

/// A line of the index that names a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexLine {
    /// 1-based line number in MEMORY.md.
    pub line: usize,
    pub title: String,
    pub file: String,
    pub hook: String,
}

/// A memory dir as read.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryDir {
    pub path: PathBuf,
    /// The fact files (every top-level `.md` but the index), by name.
    pub files: Vec<MemoryFile>,
    /// The index's version, or NEW when there is none.
    pub index_version: String,
    pub index: Vec<IndexLine>,
}

/// The version of some content: a short content hash.
pub fn version(content: &[u8]) -> String {
    let sum = Sha256::digest(content);
    sum.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The version of `file` in `dir` now: its content hash, or NEW when it doesn't
/// exist.
pub fn version_of(dir: &Path, file: &str) -> Result<String> {
    check_file_name(file)?;
    match fs::read(dir.join(file)) {
        Ok(b) => Ok(version(&b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NEW.to_string()),
        Err(e) => Err(e).with_context(|| format!("reading {file}")),
    }
}

/// `file`'s content and version, or None when it doesn't exist.
pub fn get(dir: &Path, file: &str) -> Result<Option<(String, String)>> {
    check_file_name(file)?;
    match fs::read_to_string(dir.join(file)) {
        Ok(s) => {
            let v = version(s.as_bytes());
            Ok(Some((s, v)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {file}")),
    }
}

/// Refuses a name that isn't a plain `.md` file in the dir.
pub fn check_file_name(file: &str) -> Result<()> {
    if file.is_empty()
        || file.contains(['/', '\\', '\0'])
        || file.starts_with('.')
        || !file.ends_with(".md")
        || file.len() <= 3
    {
        bail!("{file:?} isn't a memory file: give a plain name ending in .md, in the memory dir");
    }
    Ok(())
}

/// Splits content into its frontmatter and body, and checks the frontmatter is
/// what a memory needs: `name`, `description`, and a `metadata.type` (or `type`)
/// of user, feedback, project or reference.
pub fn parse_frontmatter(content: &str) -> Result<(Frontmatter, &str)> {
    let mut lines = content.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        bail!("no frontmatter: the file must start with a --- line");
    }
    let start = content.find('\n').map_or(content.len(), |i| i + 1);
    let mut at = start;
    let mut split = None;
    for l in lines {
        if l.trim_end() == "---" {
            split = Some((&content[start..at], &content[at + l.len()..]));
            break;
        }
        at += l.len();
    }
    let (head, body) = split.context("the frontmatter has no closing --- line")?;
    let mut fm = Frontmatter::default();
    let mut parent = String::new();
    for raw in head.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        let Some((k, v)) = line.trim().split_once(':') else {
            bail!("frontmatter line {:?} isn't key: value", line.trim());
        };
        let (k, v) = (k.trim(), unquote(v.trim()));
        if k.is_empty() {
            bail!("frontmatter line {:?} has no key", line.trim());
        }
        let key = if indented {
            if parent.is_empty() {
                bail!(
                    "frontmatter line {:?} is indented under nothing",
                    line.trim()
                );
            }
            format!("{parent}.{k}")
        } else {
            parent = if v.is_empty() {
                k.to_string()
            } else {
                String::new()
            };
            k.to_string()
        };
        fm.fields.insert(key, v);
    }
    fm.name = fm.fields.get("name").cloned().unwrap_or_default();
    fm.description = fm.fields.get("description").cloned().unwrap_or_default();
    fm.mem_type = fm
        .fields
        .get("metadata.type")
        .or_else(|| fm.fields.get("type"))
        .cloned()
        .unwrap_or_default();
    if fm.name.is_empty() {
        bail!("the frontmatter has no name");
    }
    if fm.name.contains(['[', ']', '\n']) {
        bail!(
            "the frontmatter's name {:?} can't be linked as [[name]]",
            fm.name
        );
    }
    if fm.description.is_empty() {
        bail!("the frontmatter has no description");
    }
    if !TYPES.contains(&fm.mem_type.as_str()) {
        bail!(
            "the frontmatter's metadata.type is {:?}: it must be one of {}",
            fm.mem_type,
            TYPES.join(", ")
        );
    }
    Ok((fm, body))
}

fn unquote(v: &str) -> String {
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].replace("\\\"", "\"");
        }
    }
    v.to_string()
}

/// The source lines in some text. A line that looks like one but cites nothing
/// valid is an error.
pub fn parse_sources(text: &str) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for c in SOURCE_RE.captures_iter(text) {
        let added = c[2].to_string();
        if chrono::NaiveDate::parse_from_str(&added, "%Y-%m-%d").is_err() {
            bail!("source line's added date {added:?} isn't a date");
        }
        let mut refs = Vec::new();
        for r in c[1].split(',').map(str::trim).filter(|r| !r.is_empty()) {
            if !REF_RE.is_match(r) {
                bail!(
                    "source reference {r:?} isn't kind:id (claude:<session-id>, friction:<uid>, commit:<hash>)"
                );
            }
            refs.push(r.to_string());
        }
        if refs.is_empty() {
            bail!("a source line cites nothing");
        }
        out.push(Source { refs, added });
    }
    Ok(out)
}

fn citations(sources: &[Source]) -> BTreeSet<String> {
    sources
        .iter()
        .flat_map(|s| s.refs.iter().cloned())
        .collect()
}

/// The `[[name]]` links in some text, in order, without repeats.
pub fn parse_links(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    LINK_RE
        .captures_iter(text)
        .map(|c| c[1].trim().to_string())
        .filter(|l| seen.insert(l.clone()))
        .collect()
}

/// The index's lines that name a file.
pub fn parse_index(content: &str) -> Vec<IndexLine> {
    content
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            INDEX_RE.captures(l).map(|c| IndexLine {
                line: i + 1,
                title: c[1].trim().to_string(),
                file: c[2].trim().to_string(),
                hook: c
                    .get(3)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Parses one fact file's content.
pub fn parse_file(file: &str, content: &str) -> MemoryFile {
    let mut m = MemoryFile {
        file: file.to_string(),
        version: version(content.as_bytes()),
        frontmatter: None,
        error: String::new(),
        body: content.to_string(),
        links: Vec::new(),
        sources: Vec::new(),
    };
    match parse_frontmatter(content) {
        Ok((fm, body)) => {
            m.frontmatter = Some(fm);
            m.body = body.to_string();
        }
        Err(e) => m.error = e.to_string(),
    }
    m.links = parse_links(&m.body);
    match parse_sources(&m.body) {
        Ok(s) => m.sources = s,
        Err(e) if m.error.is_empty() => m.error = e.to_string(),
        Err(_) => {}
    }
    m
}

/// The top-level `.md` file names in a dir, sorted (the index among them).
fn md_names(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type()?.is_file() && check_file_name(&name).is_ok() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Reads a memory dir.
pub fn parse_dir(dir: &Path) -> Result<MemoryDir> {
    let mut md = MemoryDir {
        path: dir.to_path_buf(),
        files: Vec::new(),
        index_version: NEW.to_string(),
        index: Vec::new(),
    };
    for name in md_names(dir)? {
        let content =
            fs::read_to_string(dir.join(&name)).with_context(|| format!("reading {name}"))?;
        if name == INDEX {
            md.index_version = version(content.as_bytes());
            md.index = parse_index(&content);
        } else {
            md.files.push(parse_file(&name, &content));
        }
    }
    Ok(md)
}

/// What a put did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    /// Written; the new version.
    Written(String),
    /// The content is what the file already holds; nothing written.
    Unchanged(String),
}

/// Writes `content` to `file` in `dir`, if the file is still at `expect_version`
/// (NEW for a file that doesn't exist yet). Refuses content without valid
/// frontmatter, a new or changed fact file with no source line, a rewrite that
/// loses a citation the old file had, and an index rewrite that drops more than a
/// quarter of its lines. The prior file is saved under `snapshot_dir` first, and
/// the write is atomic.
pub fn put(
    dir: &Path,
    file: &str,
    content: &str,
    expect_version: &str,
    snapshot_dir: &Path,
) -> Result<PutOutcome> {
    check_file_name(file)?;
    let old = get(dir, file)?;
    let current = old.as_ref().map(|(_, v)| v.as_str()).unwrap_or(NEW);
    if current != expect_version {
        bail!(
            "{file} is at version {current}, not {expect_version}: it changed since you read it; get it again and redo your edit"
        );
    }
    if let Some((old_content, v)) = &old
        && old_content == content
    {
        return Ok(PutOutcome::Unchanged(v.clone()));
    }
    if file == INDEX {
        check_index_rewrite(old.as_ref().map(|(c, _)| c.as_str()), content)?;
    } else {
        check_fact_rewrite(file, old.as_ref().map(|(c, _)| c.as_str()), content)?;
    }
    if let Some((old_content, _)) = &old {
        save_history(snapshot_dir, file, old_content.as_bytes())?;
    }
    write_atomic(&dir.join(file), content.as_bytes())?;
    Ok(PutOutcome::Written(version(content.as_bytes())))
}

fn check_fact_rewrite(file: &str, old: Option<&str>, content: &str) -> Result<()> {
    let (_, body) =
        parse_frontmatter(content).with_context(|| format!("refusing {file}: bad frontmatter"))?;
    let sources = parse_sources(body).with_context(|| format!("refusing {file}"))?;
    if sources.is_empty() {
        bail!(
            "refusing {file}: it has no source line; add [source: claude:<session-id>, friction:<uid>, commit:<hash>; added: YYYY-MM-DD]"
        );
    }
    if let Some(old) = old {
        let had = citations(&parse_file(file, old).sources);
        let has = citations(&sources);
        let lost: Vec<_> = had.difference(&has).cloned().collect();
        if !lost.is_empty() {
            bail!(
                "refusing {file}: it would lose the source citation{} {}; keep every citation the file had",
                if lost.len() == 1 { "" } else { "s" },
                lost.join(", ")
            );
        }
    }
    Ok(())
}

fn check_index_rewrite(old: Option<&str>, content: &str) -> Result<()> {
    let Some(old) = old else { return Ok(()) };
    let had: BTreeSet<String> = parse_index(old).into_iter().map(|l| l.file).collect();
    let has: BTreeSet<String> = parse_index(content).into_iter().map(|l| l.file).collect();
    let dropped = had.difference(&has).count();
    if too_many_dropped(dropped, had.len()) {
        bail!(
            "refusing {INDEX}: it would drop {dropped} of its {} lines (more than a quarter); remove memories one at a time instead",
            had.len()
        );
    }
    Ok(())
}

fn too_many_dropped(dropped: usize, of: usize) -> bool {
    dropped * MAX_DROP_DEN > of * MAX_DROP_NUM
}

/// Moves `file` out of `dir` into the snapshot's history, if it is still at
/// `expect_version`. Refuses the index, and a file whose citations no other file
/// in the dir carries (merge them into the file that stays first).
pub fn remove(dir: &Path, file: &str, expect_version: &str, snapshot_dir: &Path) -> Result<()> {
    check_file_name(file)?;
    if file == INDEX {
        bail!("refusing to remove {INDEX}: rewrite it with put");
    }
    let Some((content, current)) = get(dir, file)? else {
        bail!("{file} doesn't exist");
    };
    if current != expect_version {
        bail!(
            "{file} is at version {current}, not {expect_version}: it changed since you read it; get it again"
        );
    }
    let had = parse_file(file, &content).citations();
    let mut elsewhere = BTreeSet::new();
    for f in parse_dir(dir)?.files.iter().filter(|f| f.file != file) {
        elsewhere.extend(f.citations());
    }
    let lost: Vec<_> = had.difference(&elsewhere).cloned().collect();
    if !lost.is_empty() {
        bail!(
            "refusing to remove {file}: no other memory cites {}; merge its sources into the memory that keeps its fact first",
            lost.join(", ")
        );
    }
    save_history(snapshot_dir, file, content.as_bytes())?;
    fs::remove_file(dir.join(file)).with_context(|| format!("removing {file}"))?;
    Ok(())
}

/// Writes `data` to `path` through a temporary file in the same dir and a rename.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent dir")?;
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let res = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.with_context(|| format!("writing {}", path.display()))
}

/// Saves a prior version of `file` as `history/<file>.<n>`, n one past the last.
fn save_history(snapshot_dir: &Path, file: &str, data: &[u8]) -> Result<PathBuf> {
    let hist = snapshot_dir.join("history");
    fs::create_dir_all(&hist).with_context(|| format!("creating {}", hist.display()))?;
    let mut n = 1;
    loop {
        let p = hist.join(format!("{file}.{n}"));
        match fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(mut f) => {
                f.write_all(data)?;
                f.sync_all()?;
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e).with_context(|| format!("saving {}", p.display())),
        }
    }
}

/// Copies the dir's `.md` files into `snapshot_dir/files/` and lists them in
/// `snapshot_dir/MANIFEST`: the dir as a dream found it. Refuses a snapshot dir
/// that already holds one.
pub fn take_snapshot(dir: &Path, snapshot_dir: &Path) -> Result<()> {
    let manifest = snapshot_dir.join("MANIFEST");
    if manifest.exists() {
        bail!("{} already holds a snapshot", snapshot_dir.display());
    }
    let files = snapshot_dir.join("files");
    fs::create_dir_all(&files).with_context(|| format!("creating {}", files.display()))?;
    let names = md_names(dir)?;
    for n in &names {
        fs::copy(dir.join(n), files.join(n)).with_context(|| format!("copying {n}"))?;
    }
    let mut list = names.join("\n");
    list.push('\n');
    write_atomic(&manifest, list.as_bytes())
}

fn read_manifest(snapshot_dir: &Path) -> Result<Vec<String>> {
    let p = snapshot_dir.join("MANIFEST");
    let s = fs::read_to_string(&p)
        .with_context(|| format!("{} holds no snapshot (no MANIFEST)", snapshot_dir.display()))?;
    let names: Vec<String> = s
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    for n in &names {
        check_file_name(n).context("bad MANIFEST")?;
    }
    Ok(names)
}

/// Why the dir fails the whole-dir guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Violation {
    /// More than a quarter of the snapshot's files are gone.
    FilesGone { gone: Vec<String>, of: usize },
    /// A citation the snapshot carried no file in the dir carries now.
    CitationLost { citation: String, was_in: String },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::FilesGone { gone, of } => write!(
                f,
                "{} of the {of} memory files are gone (more than a quarter): {}",
                gone.len(),
                gone.join(", ")
            ),
            Violation::CitationLost { citation, was_in } => {
                write!(f, "source citation {citation} (was in {was_in}) is gone")
            }
        }
    }
}

/// Compares the dir to the snapshot taken when the dream opened: more than a
/// quarter of its files gone, or any source citation lost, is a violation.
/// Empty means the dir passes.
pub fn guard(dir: &Path, snapshot_dir: &Path) -> Result<Vec<Violation>> {
    let before = read_manifest(snapshot_dir)?;
    let now: BTreeSet<String> = md_names(dir)?.into_iter().collect();
    let facts: Vec<&String> = before.iter().filter(|n| *n != INDEX).collect();
    let gone: Vec<String> = facts
        .iter()
        .filter(|n| !now.contains(**n))
        .map(|n| (*n).clone())
        .collect();
    let mut out = Vec::new();
    if too_many_dropped(gone.len(), facts.len()) {
        out.push(Violation::FilesGone {
            gone,
            of: facts.len(),
        });
    }
    let mut has = BTreeSet::new();
    for f in parse_dir(dir)?.files {
        has.extend(f.citations());
    }
    let files = snapshot_dir.join("files");
    let mut lost = BTreeMap::new();
    for n in facts {
        let content =
            fs::read_to_string(files.join(n)).with_context(|| format!("reading snapshot {n}"))?;
        for c in parse_file(n, &content).citations() {
            if !has.contains(&c) {
                lost.entry(c).or_insert_with(|| n.clone());
            }
        }
    }
    out.extend(
        lost.into_iter()
            .map(|(citation, was_in)| Violation::CitationLost { citation, was_in }),
    );
    Ok(out)
}

/// Puts the dir back as the snapshot holds it: every snapshot file restored, and
/// every `.md` file the snapshot didn't have moved out. What it replaces or moves
/// is saved in the history first. Returns the files it changed.
pub fn restore(dir: &Path, snapshot_dir: &Path) -> Result<Vec<String>> {
    let before = read_manifest(snapshot_dir)?;
    let files = snapshot_dir.join("files");
    let mut changed = Vec::new();
    for n in &before {
        let want = fs::read(files.join(n)).with_context(|| format!("reading snapshot {n}"))?;
        let have = fs::read(dir.join(n)).ok();
        if have.as_deref() == Some(want.as_slice()) {
            continue;
        }
        if let Some(h) = &have {
            save_history(snapshot_dir, n, h)?;
        }
        write_atomic(&dir.join(n), &want)?;
        changed.push(n.clone());
    }
    let before: BTreeSet<&String> = before.iter().collect();
    for n in md_names(dir)? {
        if !before.contains(&n) {
            save_history(snapshot_dir, &n, &fs::read(dir.join(&n))?)?;
            fs::remove_file(dir.join(&n)).with_context(|| format!("removing {n}"))?;
            changed.push(n);
        }
    }
    changed.sort();
    Ok(changed)
}

/// One thing `check` found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Finding {
    /// A file whose frontmatter or source lines don't parse.
    BadFile { file: String, error: String },
    /// Two files whose text is the same (title and source lines aside).
    ExactDuplicate { file: String, other: String },
    /// Two files whose words are nearly the same (Dice similarity).
    NearDuplicate {
        file: String,
        other: String,
        similarity: f64,
    },
    /// A `[[link]]` that names no memory.
    BrokenLink { file: String, link: String },
    /// An index line naming a file the dir doesn't have.
    IndexMissingFile { line: usize, file: String },
    /// A file the index doesn't name.
    Unindexed { file: String },
}

impl Finding {
    fn order(&self) -> (u8, &str, &str, usize) {
        match self {
            Finding::BadFile { file, error } => (0, file, error, 0),
            Finding::ExactDuplicate { file, other } => (1, file, other, 0),
            Finding::NearDuplicate { file, other, .. } => (2, file, other, 0),
            Finding::BrokenLink { file, link } => (3, file, link, 0),
            Finding::IndexMissingFile { line, file } => (4, file, "", *line),
            Finding::Unindexed { file } => (5, file, "", 0),
        }
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::BadFile { file, error } => write!(f, "{file}: {error}"),
            Finding::ExactDuplicate { file, other } => {
                write!(f, "{file}: same text as {other}")
            }
            Finding::NearDuplicate {
                file,
                other,
                similarity,
            } => write!(f, "{file}: nearly the same as {other} ({similarity:.2})"),
            Finding::BrokenLink { file, link } => {
                write!(f, "{file}: [[{link}]] names no memory")
            }
            Finding::IndexMissingFile { line, file } => {
                write!(f, "{INDEX}:{line}: names {file}, which doesn't exist")
            }
            Finding::Unindexed { file } => write!(f, "{file}: not in {INDEX}"),
        }
    }
}

/// The words a duplicate check compares: the body lowercased, without headings and
/// source lines.
fn words(body: &str) -> Vec<String> {
    let text: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| SOURCE_RE.replace_all(l, " ").into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Dice similarity of two word multisets: twice the shared words over all words.
pub fn dice(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for w in a {
        *counts.entry(w).or_default() += 1;
    }
    let mut shared = 0;
    for w in b {
        if let Some(c) = counts.get_mut(w.as_str())
            && *c > 0
        {
            *c -= 1;
            shared += 1;
        }
    }
    2.0 * shared as f64 / (a.len() + b.len()) as f64
}

/// Lints a memory dir: bad files, exact and near duplicates, `[[links]]` naming no
/// memory, index lines naming no file, and files the index doesn't name. Changes
/// nothing; the findings are sorted.
pub fn check(dir: &Path) -> Result<Vec<Finding>> {
    let md = parse_dir(dir)?;
    let mut out = Vec::new();
    for f in &md.files {
        if !f.error.is_empty() {
            out.push(Finding::BadFile {
                file: f.file.clone(),
                error: f.error.clone(),
            });
        }
    }
    let ws: Vec<Vec<String>> = md.files.iter().map(|f| words(&f.body)).collect();
    for i in 0..md.files.len() {
        for j in i + 1..md.files.len() {
            let (a, b) = (&ws[i], &ws[j]);
            let (file, other) = (md.files[i].file.clone(), md.files[j].file.clone());
            if a == b && !a.is_empty() {
                out.push(Finding::ExactDuplicate { file, other });
            } else if a.len() >= MIN_WORDS && b.len() >= MIN_WORDS {
                let s = dice(a, b);
                if s >= NEAR_DUPLICATE {
                    out.push(Finding::NearDuplicate {
                        file,
                        other,
                        similarity: (s * 100.0).round() / 100.0,
                    });
                }
            }
        }
    }
    let names: BTreeSet<&str> = md
        .files
        .iter()
        .filter_map(|f| f.frontmatter.as_ref().map(|fm| fm.name.as_str()))
        .collect();
    for f in &md.files {
        for l in &f.links {
            if !names.contains(l.as_str()) {
                out.push(Finding::BrokenLink {
                    file: f.file.clone(),
                    link: l.clone(),
                });
            }
        }
    }
    let present: BTreeSet<&str> = md.files.iter().map(|f| f.file.as_str()).collect();
    for l in &md.index {
        if !present.contains(l.file.as_str()) {
            out.push(Finding::IndexMissingFile {
                line: l.line,
                file: l.file.clone(),
            });
        }
    }
    let indexed: BTreeSet<&str> = md.index.iter().map(|l| l.file.as_str()).collect();
    for f in &md.files {
        if !indexed.contains(f.file.as_str()) {
            out.push(Finding::Unindexed {
                file: f.file.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.order().cmp(&b.order()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "[source: claude:aaaa-1111, friction:box-01; added: 2026-11-01]";

    fn fact(name: &str, body: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: \"a fact about {name}\"\nmetadata:\n  type: feedback\n---\n\n{body}\n"
        )
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let mem = t.path().join("memory");
        let snap = t.path().join("snap");
        fs::create_dir_all(&mem).unwrap();
        (t, mem, snap)
    }

    fn write(dir: &Path, file: &str, s: &str) {
        fs::write(dir.join(file), s).unwrap();
    }

    fn err(r: Result<impl std::fmt::Debug>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    #[test]
    fn frontmatter_parses_nested_and_quoted() {
        let c = fact("alpha", "Body.");
        let (fm, body) = parse_frontmatter(&c).unwrap();
        assert_eq!(fm.name, "alpha");
        assert_eq!(fm.description, "a fact about alpha");
        assert_eq!(fm.mem_type, "feedback");
        assert_eq!(body, "\nBody.\n");
        let top = "---\nname: b\ndescription: d\ntype: user\n---\nx";
        assert_eq!(parse_frontmatter(top).unwrap().0.mem_type, "user");
    }

    #[test]
    fn frontmatter_refusals() {
        for (bad, why) in [
            ("no front", "no frontmatter"),
            ("---\nname: a\n", "no closing"),
            (
                "---\ndescription: d\nmetadata:\n  type: user\n---\n",
                "no name",
            ),
            (
                "---\nname: a\nmetadata:\n  type: user\n---\n",
                "no description",
            ),
            (
                "---\nname: a\ndescription: d\nmetadata:\n  type: opinion\n---\n",
                "metadata.type",
            ),
            (
                "---\nname: a\ndescription: d\n  type: user\n---\n",
                "indented under nothing",
            ),
            ("---\nname a\n---\n", "key: value"),
        ] {
            let e = parse_frontmatter(bad).unwrap_err().to_string();
            assert!(e.contains(why), "{bad:?}: {e}");
        }
    }

    #[test]
    fn sources_links_and_index_parse() {
        let s = parse_sources(&format!(
            "x\n{SRC}\n[source: commit:abc123; added: 2026-11-02]"
        ))
        .unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].refs, vec!["claude:aaaa-1111", "friction:box-01"]);
        assert_eq!(s[1].added, "2026-11-02");
        assert!(parse_sources("[source: nope; added: 2026-11-01]").is_err());
        assert!(parse_sources("[source: claude:x; added: 2026-13-45]").is_err());
        assert_eq!(parse_links("see [[a]] and [[b]], [[a]]"), vec!["a", "b"]);
        let idx = parse_index("# Index\n\n- [A](a.md) — hook a\n* [B](b.md)\nprose\n");
        assert_eq!(idx.len(), 2);
        assert_eq!(
            (idx[0].line, idx[0].file.as_str(), idx[0].hook.as_str()),
            (3, "a.md", "hook a")
        );
        assert_eq!(idx[1].hook, "");
    }

    #[test]
    fn file_names_are_checked() {
        for bad in ["", "../x.md", "a/b.md", ".hidden.md", "x.txt", ".md"] {
            assert!(check_file_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_file_name("ok.md").is_ok());
        let (_t, mem, snap) = dirs();
        assert!(put(&mem, "../escape.md", "x", NEW, &snap).is_err());
    }

    #[test]
    fn put_creates_and_versions() {
        let (_t, mem, snap) = dirs();
        let c = fact("alpha", &format!("Do X.\n{SRC}"));
        let PutOutcome::Written(v) = put(&mem, "a.md", &c, NEW, &snap).unwrap() else {
            panic!()
        };
        assert_eq!(v, version_of(&mem, "a.md").unwrap());
        assert_eq!(get(&mem, "a.md").unwrap().unwrap(), (c.clone(), v.clone()));
        assert_eq!(
            put(&mem, "a.md", &c, &v, &snap).unwrap(),
            PutOutcome::Unchanged(v)
        );
        // No history for a new file, and no temp files left behind.
        assert!(!snap.join("history").exists());
        assert_eq!(md_names(&mem).unwrap(), vec!["a.md"]);
        assert_eq!(fs::read_dir(&mem).unwrap().count(), 1);
    }

    #[test]
    fn put_refuses_a_version_mismatch() {
        let (_t, mem, snap) = dirs();
        let c = fact("alpha", &format!("Do X.\n{SRC}"));
        write(&mem, "a.md", &c);
        assert!(err(put(&mem, "a.md", &c, NEW, &snap)).contains("changed since you read it"));
        assert!(
            err(put(&mem, "b.md", &c, "0123456789abcdef", &snap)).contains("is at version new")
        );
        assert_eq!(fs::read_to_string(mem.join("a.md")).unwrap(), c);
    }

    #[test]
    fn put_refuses_bad_frontmatter() {
        let (_t, mem, snap) = dirs();
        let e = err(put(
            &mem,
            "a.md",
            &format!("Just text.\n{SRC}\n"),
            NEW,
            &snap,
        ));
        assert!(e.contains("bad frontmatter"), "{e}");
        assert!(!mem.join("a.md").exists());
    }

    #[test]
    fn put_refuses_a_fact_with_no_source() {
        let (_t, mem, snap) = dirs();
        let e = err(put(&mem, "a.md", &fact("alpha", "Do X."), NEW, &snap));
        assert!(e.contains("no source line"), "{e}");
        // A changed file the dream didn't write still needs one.
        write(&mem, "b.md", &fact("beta", "Old."));
        let v = version_of(&mem, "b.md").unwrap();
        assert!(
            err(put(&mem, "b.md", &fact("beta", "New."), &v, &snap)).contains("no source line")
        );
    }

    #[test]
    fn put_refuses_losing_a_citation_and_saves_history() {
        let (_t, mem, snap) = dirs();
        let old = fact("alpha", &format!("Do X.\n{SRC}"));
        write(&mem, "a.md", &old);
        let v = version_of(&mem, "a.md").unwrap();
        let fewer = fact(
            "alpha",
            "Do X.\n[source: claude:aaaa-1111; added: 2026-11-01]",
        );
        let e = err(put(&mem, "a.md", &fewer, &v, &snap));
        assert!(e.contains("friction:box-01"), "{e}");
        assert!(!snap.join("history").exists());
        let more = fact(
            "alpha",
            &format!("Do X, then Y.\n{SRC}\n[source: commit:abc; added: 2026-11-03]"),
        );
        put(&mem, "a.md", &more, &v, &snap).unwrap();
        assert_eq!(
            fs::read_to_string(snap.join("history/a.md.1")).unwrap(),
            old
        );
        let v2 = version_of(&mem, "a.md").unwrap();
        put(&mem, "a.md", &format!("{more}more\n"), &v2, &snap).unwrap();
        assert_eq!(
            fs::read_to_string(snap.join("history/a.md.2")).unwrap(),
            more
        );
    }

    #[test]
    fn put_refuses_an_index_dropping_over_a_quarter() {
        let (_t, mem, snap) = dirs();
        let line = |c: char| format!("- [{c}]({c}.md) — about {c}\n");
        let all: String = "abcdefgh".chars().map(line).collect();
        write(&mem, INDEX, &all);
        let v = version_of(&mem, INDEX).unwrap();
        // 2 of 8 dropped is a quarter: allowed.
        let six: String = "abcdef".chars().map(line).collect();
        let five: String = "abcde".chars().map(line).collect();
        let e = err(put(&mem, INDEX, &five, &v, &snap));
        assert!(e.contains("drop 3 of its 8"), "{e}");
        put(&mem, INDEX, &six, &v, &snap).unwrap();
        // The index needs no frontmatter or source line.
        assert_eq!(
            fs::read_to_string(snap.join("history/MEMORY.md.1")).unwrap(),
            all
        );
    }

    #[test]
    fn remove_moves_the_file_and_guards_citations() {
        let (_t, mem, snap) = dirs();
        let a = fact("alpha", &format!("Do X.\n{SRC}"));
        write(&mem, "a.md", &a);
        let v = version_of(&mem, "a.md").unwrap();
        assert!(err(remove(&mem, "a.md", NEW, &snap)).contains("changed since"));
        assert!(err(remove(&mem, "a.md", &v, &snap)).contains("no other memory cites"));
        assert!(err(remove(&mem, INDEX, NEW, &snap)).contains("rewrite it with put"));
        assert!(err(remove(&mem, "zz.md", NEW, &snap)).contains("doesn't exist"));
        write(
            &mem,
            "b.md",
            &fact("beta", &format!("Do X and more.\n{SRC}")),
        );
        remove(&mem, "a.md", &v, &snap).unwrap();
        assert!(!mem.join("a.md").exists());
        assert_eq!(fs::read_to_string(snap.join("history/a.md.1")).unwrap(), a);
    }

    #[test]
    fn guard_and_restore() {
        let (_t, mem, snap) = dirs();
        for c in ['a', 'b', 'c', 'd'] {
            let src = format!("[source: commit:{c}{c}{c}; added: 2026-11-01]");
            write(&mem, &format!("{c}.md"), &fact(&c.to_string(), &src));
        }
        write(&mem, INDEX, "- [A](a.md)\n");
        assert!(err(guard(&mem, &snap)).contains("no MANIFEST"));
        take_snapshot(&mem, &snap).unwrap();
        assert!(err(take_snapshot(&mem, &snap)).contains("already holds"));
        assert_eq!(guard(&mem, &snap).unwrap(), vec![]);

        // One of four gone, its citation kept elsewhere: fine.
        let b = fs::read_to_string(mem.join("b.md")).unwrap();
        write(
            &mem,
            "a.md",
            &fact("a", "[source: commit:aaa, commit:ddd; added: 2026-11-01]"),
        );
        fs::remove_file(mem.join("d.md")).unwrap();
        assert_eq!(guard(&mem, &snap).unwrap(), vec![]);

        // A citation lost.
        write(
            &mem,
            "b.md",
            &fact("b", "[source: commit:zzz; added: 2026-11-01]"),
        );
        assert_eq!(
            guard(&mem, &snap).unwrap(),
            vec![Violation::CitationLost {
                citation: "commit:bbb".into(),
                was_in: "b.md".into()
            }]
        );

        // Two of four gone (over a quarter), with c's citation.
        write(&mem, "b.md", &b);
        fs::remove_file(mem.join("c.md")).unwrap();
        write(&mem, "new.md", &fact("new", SRC));
        let v = guard(&mem, &snap).unwrap();
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(matches!(&v[0], Violation::FilesGone { gone, of: 4 } if gone == &["c.md", "d.md"]));
        assert!(v[1].to_string().contains("commit:ccc"));

        let changed = restore(&mem, &snap).unwrap();
        assert_eq!(changed, vec!["a.md", "c.md", "d.md", "new.md"]);
        assert_eq!(
            md_names(&mem).unwrap(),
            vec!["MEMORY.md", "a.md", "b.md", "c.md", "d.md"]
        );
        assert_eq!(guard(&mem, &snap).unwrap(), vec![]);
        assert!(snap.join("history/new.md.1").exists());
        assert!(snap.join("history/a.md.1").exists());
        assert_eq!(restore(&mem, &snap).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn dice_counts_multisets() {
        let w = |s: &str| words(s);
        assert_eq!(dice(&w("a a b"), &w("a b b")), 2.0 * 2.0 / 6.0);
        assert_eq!(dice(&w("x y"), &w("X, y!")), 1.0);
        assert_eq!(dice(&[], &[]), 1.0);
        // Headings and source lines don't count.
        assert_eq!(w(&format!("# Title\nOne two\n{SRC}")), vec!["one", "two"]);
    }

    #[test]
    fn check_finds_everything_in_order() {
        let (_t, mem, _snap) = dirs();
        let long = "always run the full test suite before you commit anything";
        write(
            &mem,
            "a.md",
            &fact(
                "alpha",
                &format!("# Alpha\n{long}\nsee [[beta]] and [[ghost]]\n{SRC}"),
            ),
        );
        // b and a are near duplicates; c is an exact duplicate of a once titles go.
        write(
            &mem,
            "b.md",
            &fact(
                "beta",
                &format!("{long} please\nsee [[beta]] and [[ghost]]"),
            ),
        );
        write(
            &mem,
            "c.md",
            &fact(
                "gamma",
                &format!("# Other title\n{long}\nsee [[beta]] and [[ghost]]"),
            ),
        );
        write(&mem, "d.md", "no frontmatter at all\n");
        // Short entries aren't near duplicates of each other.
        write(&mem, "e.md", &fact("eps", "one two three"));
        write(&mem, "f.md", &fact("phi", "one two four"));
        write(
            &mem,
            INDEX,
            "# Memory\n- [A](a.md) — a\n- [Gone](gone.md) — gone\n- [B](b.md)\n- [E](e.md)\n- [F](f.md)\n",
        );
        let got: Vec<String> = check(&mem).unwrap().iter().map(|f| f.to_string()).collect();
        assert_eq!(
            got,
            vec![
                "d.md: no frontmatter: the file must start with a --- line",
                "a.md: same text as c.md",
                "a.md: nearly the same as b.md (0.97)",
                "b.md: nearly the same as c.md (0.97)",
                "a.md: [[ghost]] names no memory",
                "b.md: [[ghost]] names no memory",
                "c.md: [[ghost]] names no memory",
                "MEMORY.md:3: names gone.md, which doesn't exist",
                "c.md: not in MEMORY.md",
                "d.md: not in MEMORY.md",
            ]
        );
        // Deterministic.
        assert_eq!(check(&mem).unwrap(), check(&mem).unwrap());
        let j = serde_json::to_value(&check(&mem).unwrap()[1]).unwrap();
        assert_eq!(j["kind"], "exact-duplicate");
    }
}

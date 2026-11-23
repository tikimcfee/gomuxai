//! The side corpora's notes (docs/INDEX-V2.md, "Corpora, composed"), read
//! for the sync: memory notes from their directories, and friction entries
//! from fields the caller took from the agent store (this crate doesn't
//! open it). Both are personal: they live in the local database only.

use std::path::{Path, PathBuf};

use super::sync::SideNote;
use crate::extract::{SEED_MAX, clip, refs_in};

/// The directories a `[memory].dirs` list names: `~` the home directory,
/// `{data}` this agent's data directory, a `*` component any one directory.
/// Those that exist, sorted.
pub fn expand_dirs(patterns: &[String], data: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut out = Vec::new();
    for p in patterns {
        let p = p.replace("{data}", &data.to_string_lossy());
        let p = match p.strip_prefix("~/") {
            Some(rest) => home.join(rest).to_string_lossy().into_owned(),
            None => p,
        };
        let mut cur: Vec<PathBuf> = vec![PathBuf::from(if p.starts_with('/') { "/" } else { "." })];
        for comp in p.split('/').filter(|c| !c.is_empty()) {
            let mut next = Vec::new();
            for base in &cur {
                if comp == "*" {
                    if let Ok(rd) = std::fs::read_dir(base) {
                        for e in rd.flatten() {
                            if e.file_type().is_ok_and(|t| t.is_dir()) {
                                next.push(e.path());
                            }
                        }
                    }
                } else {
                    next.push(base.join(comp));
                }
            }
            cur = next;
        }
        out.extend(cur.into_iter().filter(|d| d.is_dir()));
    }
    out.sort();
    out.dedup();
    out
}

/// A name for a directory of notes: the indexed project a Claude Code
/// project directory stands for (`-srv-dev-alpha` for `/srv/dev/alpha`),
/// else `machine` for this agent's own memory, else the directory's name.
fn note_project(dir: &Path, data: &Path, projects: &[(String, String)]) -> (String, String) {
    let parent = dir.parent().unwrap_or(dir);
    if dir == data.join("memory") {
        return ("machine".into(), String::new());
    }
    let name = parent
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let encode = |p: &str| -> String {
        p.chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect()
    };
    for (pname, ppath) in projects {
        if encode(ppath) == name {
            return (pname.clone(), pname.clone());
        }
    }
    (name.trim_start_matches('-').to_string(), String::new())
}

/// The front matter's `key: value` lines and the text after it.
fn front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (Vec::new(), text);
    };
    let Some(end) = rest.find("\n---") else {
        return (Vec::new(), text);
    };
    let fm = rest[..end]
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect();
    let body = rest[end + 4..].trim_start_matches(|c| c != '\n');
    (fm, body.trim_start_matches('\n'))
}

/// The first paragraph that isn't a heading.
fn first_paragraph(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        if t.is_empty() {
            if out.is_empty() {
                continue;
            }
            break;
        }
        if t.starts_with('#') && out.is_empty() {
            continue;
        }
        out.push(t);
    }
    out.join(" ")
}

/// Every Markdown note in the directories (`*.md`; `MEMORY.md`, an index of
/// the others, is left out), as `memory:<project>/<file>`.
pub fn memory_notes(dirs: &[PathBuf], data: &Path, projects: &[(String, String)]) -> Vec<SideNote> {
    let mut out = Vec::new();
    for d in dirs {
        let (who, project) = note_project(d, data, projects);
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md") && p.is_file())
            .collect();
        files.sort();
        for f in files {
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name == "MEMORY.md" {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            if text.len() > 1 << 20 {
                continue;
            }
            let (fm, body) = front_matter(&text);
            let get = |k: &str| {
                fm.iter()
                    .find(|(x, _)| x == k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            let heading = body
                .lines()
                .find_map(|l| l.strip_prefix("# "))
                .map(|h| h.trim().to_string())
                .unwrap_or_default();
            let title = [
                get("name"),
                heading,
                name.trim_end_matches(".md").to_string(),
            ]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or_default();
            let mut seed = get("description");
            if seed.is_empty() {
                seed = first_paragraph(body);
            }
            let refs = body
                .lines()
                .flat_map(|l| refs_in(l).into_iter().map(|(r, _)| r))
                .collect();
            out.push(SideNote {
                id: format!("memory:{who}/{name}"),
                corpus: "memory".into(),
                kind: "note".into(),
                project: project.clone(),
                name: name.clone(),
                path: f.to_string_lossy().into_owned(),
                title,
                seed: clip(&seed, SEED_MAX),
                body: body.to_string(),
                refs,
                status: get("type"),
            });
        }
    }
    out
}

/// A friction entry's fields, as the agent store holds them.
#[derive(Debug, Clone, Default)]
pub struct FrictionEntry {
    pub uid: String,
    pub kind: String,
    pub status: String,
    pub summary: String,
    pub detail: String,
    pub resolution: String,
    pub accept_note: String,
    /// Where it was filed from (its project, when inside one).
    pub cwd: String,
    pub filed_by: String,
    pub created_at: String,
    /// The failed command's error, when one was attached.
    pub error: String,
}

/// A friction entry as a note: `friction:<uid>`, its summary for a title,
/// the rest for its text, the paths it names for its edges.
pub fn friction_note(f: &FrictionEntry, projects: &[(String, String)]) -> SideNote {
    let body = [
        f.summary.as_str(),
        f.detail.as_str(),
        f.error.as_str(),
        f.accept_note.as_str(),
        f.resolution.as_str(),
    ]
    .iter()
    .filter(|s| !s.is_empty())
    .copied()
    .collect::<Vec<_>>()
    .join("\n\n");
    let refs = body
        .lines()
        .flat_map(|l| refs_in(l).into_iter().map(|(r, _)| r))
        .collect();
    let project = projects
        .iter()
        .filter(|(_, p)| f.cwd == *p || f.cwd.starts_with(&format!("{p}/")))
        .max_by_key(|(_, p)| p.len())
        .map(|(n, _)| n.clone())
        .unwrap_or_default();
    let mut seed = f.summary.clone();
    let more = first_paragraph(&f.detail);
    if !more.is_empty() {
        seed = format!("{seed}. {more}");
    }
    SideNote {
        id: format!("friction:{}", f.uid),
        corpus: "friction".into(),
        kind: "friction".into(),
        project,
        name: f.uid.clone(),
        path: String::new(),
        title: f.summary.clone(),
        seed: clip(&seed, SEED_MAX),
        body,
        refs,
        status: format!(
            "{}, {}, filed by {} {}",
            f.status, f.kind, f.filed_by, f.created_at
        ),
    }
}

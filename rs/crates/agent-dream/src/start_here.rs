//! A project's start-here note: the one memory a dream keeps short and
//! current for an agent waking into the project (what it is now, how to
//! build and test, what is in flight, what trips agents up). Spawn puts it
//! in a new agent's briefing, whatever its harness.

use std::path::Path;

/// The start-here note's file name in a memory directory.
pub const FILE: &str = "start_here.md";

/// The most of a note's lines a briefing carries; a note longer than this
/// is cut, saying so (a dream keeps it far shorter).
pub const MAX_LINES: usize = 80;

/// Claude Code's folder name for a project directory: every character that
/// isn't an ASCII letter or digit becomes '-'.
pub fn project_folder(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Where Claude Code keeps its per-project folders: `configured` (the app
/// config's claude_projects_dir; Some("") turns it off), else
/// `$CLAUDE_CONFIG_DIR/projects`, else `<home>/.claude/projects`.
pub fn projects_dir(configured: Option<&str>, config_dir: &str, home: &str) -> Option<String> {
    match configured {
        Some("") => None,
        Some(p) => Some(match p.strip_prefix("~/") {
            Some(rest) if !home.is_empty() => format!("{home}/{rest}"),
            _ => p.to_string(),
        }),
        None if !config_dir.is_empty() => Some(format!("{config_dir}/projects")),
        None if !home.is_empty() => Some(format!("{home}/.claude/projects")),
        None => None,
    }
}

/// The start-here note for an agent starting in `cwd`: the nearest one of
/// cwd and its parents whose project has one. Returns its path and text.
pub fn find(projects_dir: &str, cwd: &str) -> Option<(String, String)> {
    let mut dir = Some(Path::new(cwd));
    while let Some(d) = dir {
        let s = d.to_string_lossy();
        if s.is_empty() || s == "/" {
            break;
        }
        let path = Path::new(projects_dir)
            .join(project_folder(&s))
            .join("memory")
            .join(FILE);
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some((path.to_string_lossy().into_owned(), text));
        }
        dir = d.parent();
    }
    None
}

/// The briefing section for a note read from `path`: its body without the
/// frontmatter, cut at MAX_LINES.
pub fn briefing_section(path: &str, text: &str) -> String {
    let body = strip_frontmatter(text).trim();
    if body.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = body.lines().collect();
    let mut out = format!(
        "## Start here\n\nThis project's start-here note, kept current by dreaming \
         ({path}). Treat it as what was true when it was written; the repo wins \
         where they disagree.\n\n"
    );
    for l in lines.iter().take(MAX_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    if lines.len() > MAX_LINES {
        out.push_str(&format!(
            "\n(cut at {MAX_LINES} of {} lines; the rest is in the file)\n",
            lines.len()
        ));
    }
    out
}

fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    match rest.find("\n---\n") {
        Some(end) => &rest[end + 5..],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_and_projects_dir() {
        assert_eq!(project_folder("/srv/my_app.v2"), "-srv-my-app-v2");
        assert_eq!(projects_dir(Some(""), "", "/h"), None);
        assert_eq!(projects_dir(Some("~/p"), "", "/h").as_deref(), Some("/h/p"));
        assert_eq!(
            projects_dir(None, "/c", "/h").as_deref(),
            Some("/c/projects")
        );
        assert_eq!(
            projects_dir(None, "", "/h").as_deref(),
            Some("/h/.claude/projects")
        );
        assert_eq!(projects_dir(None, "", ""), None);
    }

    #[test]
    fn finds_the_nearest_project_and_strips_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().to_string_lossy().into_owned();
        let mem = tmp.path().join(project_folder("/srv/app")).join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(
            mem.join(FILE),
            "---\nname: start-here\n---\n\nBuild with make.\n",
        )
        .unwrap();
        let (path, text) = find(&projects, "/srv/app/sub/dir").unwrap();
        assert!(path.ends_with(FILE));
        let s = briefing_section(&path, &text);
        assert!(s.starts_with("## Start here"), "{s}");
        assert!(
            s.contains("Build with make.") && !s.contains("name: start-here"),
            "{s}"
        );
        assert!(find(&projects, "/srv/other").is_none());
    }

    #[test]
    fn a_long_note_is_cut() {
        let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let s = briefing_section("/p", &body);
        assert!(s.contains("line 79\n") && !s.contains("line 80\n"));
        assert!(s.contains("cut at 80 of 100 lines"));
    }
}

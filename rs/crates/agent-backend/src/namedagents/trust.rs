//! Go `namedagents/trust.go`.

/// A CLI's workspace trust dialog, which Claude Code and agy draw alike (as
/// seen with Claude Code v2.1.310 and agy, 2026-11):
///
/// ```text
/// Accessing workspace:
///
/// /path/to/the/folder, wrapped at the screen's width
///
/// <question>
/// ...
/// <marker> No, exit
///   Yes, I trust this folder
/// <footer>
/// ```
///
/// The options sit at the bottom of the screen with the highlighted one
/// marked, and Enter picks it; only the footer (and agy's model line) is
/// below them. Claude lists "No, exit" first and highlights it; agy lists
/// "Yes" first and highlights that.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TrustDialog {
    /// Starts the line under the folder, e.g. "Quick safety check".
    pub question: &'static str,
    /// Marks the highlighted option, e.g. "❯".
    pub marker: &'static str,
    /// "Yes" is listed above "No".
    pub yes_first: bool,
    /// Is in the line under the options, e.g. "Enter to confirm".
    pub footer: &'static str,
    /// At most this many non-blank lines are under the options.
    pub below: usize,
}

const TRUST_YES: &str = "Yes, I trust this folder";
const TRUST_NO: &str = "No, exit";
const TRUST_HEADER: &str = "Accessing workspace:";

/// Bounds where the options may be: among the last non-blank lines of the
/// screen. Text that merely quotes the options higher up, in a transcript or
/// a file shown on screen, isn't the dialog.
const TRUST_TAIL_LINES: usize = 6;

impl TrustDialog {
    /// Finds the dialog on screen: `(folder, highlighted)`. `None` unless
    /// the question and both options are there, the options at the bottom
    /// with only the footer under them: a CLI that exited leaves its last
    /// frame on screen, the shell's prompt under it. `highlighted` is
    /// TRUST_YES or TRUST_NO, or "" if neither is marked.
    pub(crate) fn read(&self, screen: &str) -> Option<(String, &'static str)> {
        let lines: Vec<&str> = screen.split('\n').collect();
        let (mut yes, mut no): (Option<usize>, Option<usize>) = (None, None);
        let mut highlighted = "";
        let mut seen = 0;
        for i in (0..lines.len()).rev() {
            if seen >= TRUST_TAIL_LINES {
                break;
            }
            let line = lines[i].trim();
            if line.is_empty() {
                continue;
            }
            seen += 1;
            let marked = line.starts_with(self.marker);
            match line.strip_prefix(self.marker).unwrap_or(line).trim() {
                TRUST_YES => {
                    yes = Some(i);
                    if marked {
                        highlighted = TRUST_YES;
                    }
                }
                TRUST_NO => {
                    no = Some(i);
                    if marked {
                        highlighted = TRUST_NO;
                    }
                }
                _ => {}
            }
        }
        let (yes, no) = (yes?, no?);
        if (yes < no) != self.yes_first || !self.footer_under(&lines[yes.max(no) + 1..]) {
            return None;
        }
        let question = (0..yes.min(no))
            .rev()
            .find(|&i| lines[i].trim().starts_with(self.question))?;
        Some((trust_folder(&lines[..question]), highlighted))
    }

    /// Reports whether `lines`, those under the options, are the dialog's
    /// footer: no more than `self.below` non-blank ones, the first with
    /// `self.footer` in it.
    fn footer_under(&self, lines: &[&str]) -> bool {
        let under: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
        !under.is_empty() && under.len() <= self.below && under[0].contains(self.footer)
    }

    /// Returns the keys that pick "Yes" (trust) or "No" from the
    /// highlighted option, or `None` if the dialog isn't on screen or
    /// neither option is marked.
    pub(crate) fn keys(&self, screen: &str, trust: bool) -> Option<Vec<String>> {
        let (_, highlighted) = self.read(screen)?;
        if highlighted.is_empty() {
            return None;
        }
        let want = if trust { TRUST_YES } else { TRUST_NO };
        if highlighted == want {
            return Some(vec!["Enter".to_string()]);
        }
        // The other option is highlighted: move to this one.
        let first = if (want == TRUST_YES) == self.yes_first {
            "Up"
        } else {
            "Down"
        };
        Some(vec![first.to_string(), "Enter".to_string()])
    }
}

/// Reads the folder under the dialog's header: the lines after it up to a
/// blank one, joined (the path wraps at the screen's width). It returns ""
/// if the header isn't on screen.
fn trust_folder(lines: &[&str]) -> String {
    let Some(header) = (0..lines.len())
        .rev()
        .find(|&i| lines[i].trim() == TRUST_HEADER)
    else {
        return String::new();
    };
    let mut b = String::new();
    for line in &lines[header + 1..] {
        let line = line.trim();
        if line.is_empty() && !b.is_empty() {
            break;
        }
        b.push_str(line);
    }
    b
}

//! Go `namedagents/claude_dialog.go`.
//!
//! Claude Code draws every dialog the same way: a rule across the screen,
//! its title on the next line, its body and options (the highlighted one
//! marked "❯"), and a footer of key hints such as "Enter to confirm · Esc to
//! cancel". Some take the input box's place (a permission prompt, a setup
//! wizard); others are drawn beside it, keys going to the dialog: under the
//! box in the classic renderer, above it in the fullscreen one. Offers like
//! "Teach auto mode about your environment?" come up when a turn ends, and
//! fire no hook, and Claude's session registry still says idle.
//!
//! So a dialog is read off the screen by its footer, wherever one can sit:
//! the bottom line of the screen, or the last line above the input box. A
//! running turn's footer ("esc to interrupt") and the status line under the
//! box aren't key hints alone, so neither counts.

use std::sync::LazyLock;

use agent_base::util::strip_ansi;
use regex::Regex;

use super::claude::ClaudeBackend;
use super::hooks::{input_box, is_rule};
use crate::provider::{Dialog, DialogReader, PermissionResponse};

/// Bounds how far above its footer a dialog's top rule is looked for.
const CLAUDE_DIALOG_MAX_LINES: usize = 40;

/// Matches one key hint of a dialog's footer: a key, "to", and what it does
/// ("Esc to cancel", "←/→ to change", "Tab to amend"). (Go's `\S` is any
/// byte but ASCII whitespace.)
static CLAUDE_KEY_HINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\t\n\f\r ]{1,16}) to [^\t\n\f\r ].*$").expect("regex"));

/// The keys a dialog's footer offers for answering it: a footer with
/// neither is something else.
const CLAUDE_ANSWER_KEYS: &[&str] = &["enter", "esc"];

/// Reports whether `line` is a dialog's footer: key hints separated by
/// " · ", one of them for Enter or Esc. "esc to interrupt" is a running
/// turn's, so it disqualifies the line.
fn is_claude_dialog_footer(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    let mut answers = false;
    for seg in line.split(" · ") {
        let seg = seg.trim();
        let Some(m) = CLAUDE_KEY_HINT.captures(seg) else {
            return false;
        };
        if seg.ends_with("to interrupt") {
            return false;
        }
        answers = answers || CLAUDE_ANSWER_KEYS.contains(&m[1].to_lowercase().as_str());
    }
    answers
}

impl DialogReader for ClaudeBackend {
    /// Reads a Claude dialog off the screen: a footer of key hints at the
    /// bottom of the screen or right above the input box, under a rule whose
    /// next line is the dialog's title.
    fn dialog(&self, screen: &str) -> Option<Dialog> {
        let stripped = strip_ansi(screen);
        let lines: Vec<&str> = stripped.trim_end_matches('\n').split('\n').collect();
        for footer in claude_footer_candidates(&lines) {
            if !is_claude_dialog_footer(lines[footer]) {
                continue;
            }
            if let Some(d) = claude_dialog_above(&lines, footer) {
                return Some(d);
            }
        }
        None
    }

    /// Answers a Claude dialog: Enter picks the highlighted option, Escape
    /// closes it (its footer's "Esc to cancel"; an offer takes it for "not
    /// now").
    fn dialog_key(&self, resp: PermissionResponse) -> anyhow::Result<String> {
        match resp {
            PermissionResponse::Approve => Ok("Enter".to_string()),
            PermissionResponse::Deny => Ok("Escape".to_string()),
            PermissionResponse::ApproveAlways => anyhow::bail!(
                "a dialog has no \"always\" answer: approve picks its highlighted option, deny closes it"
            ),
        }
    }
}

/// Returns where a dialog's footer can be: the last non-blank line, and the
/// last non-blank line above the input box.
fn claude_footer_candidates(lines: &[&str]) -> Vec<usize> {
    let mut out = Vec::new();
    if let Some(i) = last_non_blank(lines, lines.len()) {
        out.push(i);
    }
    if let Some((start, _)) = input_box(lines, "❯", 0) {
        // start-1 is the box's top rule.
        if let Some(i) = last_non_blank(lines, start - 1)
            && (out.is_empty() || i != out[0])
        {
            out.push(i);
        }
    }
    out
}

/// Returns the index of the last non-blank line before `end`, or `None`.
fn last_non_blank(lines: &[&str], end: usize) -> Option<usize> {
    (0..end).rev().find(|&i| !lines[i].trim().is_empty())
}

/// Reports whether `line` is a dialog's top border: a rule of "─" (as the
/// input box's), or a line of "▔" (a picker's, as /model's).
fn is_dialog_border(line: &str) -> bool {
    if is_rule(line) {
        return true;
    }
    let line = line.trim();
    line.chars().count() >= 10 && line.trim_matches('▔').is_empty()
}

/// Reads the dialog whose footer is `lines[footer]`: its title is the first
/// line under the nearest border above, and its highlighted option the line
/// between them marked "❯".
fn claude_dialog_above(lines: &[&str], footer: usize) -> Option<Dialog> {
    let lowest = footer.saturating_sub(CLAUDE_DIALOG_MAX_LINES);
    for top in (lowest..footer).rev() {
        if !is_dialog_border(lines[top]) {
            continue;
        }
        let mut d = Dialog::default();
        for line in &lines[top + 1..footer] {
            // No-break spaces too.
            let text = line.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
            } else if d.title.is_empty() {
                d.title = text;
            } else if d.highlighted.is_empty() && text.starts_with('❯') {
                d.highlighted = text.strip_prefix('❯').unwrap_or(&text).trim().to_string();
            }
        }
        return if d.title.is_empty() { None } else { Some(d) };
    }
    None
}

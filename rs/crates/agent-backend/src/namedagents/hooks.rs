//! Go `namedagents/hooks.go`: shared by the backends whose CLI reports
//! status through hooks (claude, agy).

use agent_base::types::PermissionResponse;
use agent_base::util::strip_ansi;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::gojson;

/// Bounds each hook; `agent report` is a local DB write.
pub(crate) const HOOK_TIMEOUT_SEC: i64 = 10;

/// A hook entry both CLIs' settings take.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct HookCommand {
    #[serde(rename = "type")]
    pub r#type: String,
    pub command: String,
    pub timeout: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HookMatcher {
    pub matcher: String,
    pub hooks: Vec<HookCommand>,
}

pub(crate) fn new_hook_command(command: String) -> HookCommand {
    HookCommand {
        r#type: "command".to_string(),
        command,
        timeout: HOOK_TIMEOUT_SEC,
    }
}

/// Keeps error details to a single readable line.
const MAX_DETAIL_LEN: usize = 120;

/// Bounds the tool input a report stores. Larger inputs (e.g. a Write of a
/// big file) aren't stored; the detail still names the call.
const MAX_TOOL_INPUT_LEN: usize = 64 << 10;

/// Renders a tool call as "tool: <what it's about>": the file, else the
/// most telling input, from the tool table. It is kept whole, line breaks
/// included, and paths stay absolute: for a permission prompt, it is what
/// an answer allows. `input` is the call's raw JSON (empty if none).
pub(crate) fn tool_summary(tool: &str, input: &[u8]) -> String {
    let s = agent_transcript::describe_tool(tool, input, "");
    let v = if !s.target.is_empty() {
        s.target
    } else {
        s.detail
    };
    if !v.is_empty() {
        return format!("{tool}: {v}");
    }
    tool.to_string()
}

/// Identifies a tool call by its tool and input, for `HookReport::call_key`.
/// It hashes the input, which may be too large to store.
pub(crate) fn call_key(tool: &str, input: &[u8]) -> String {
    let compacted = gojson::compact(input).unwrap_or_else(|| input.to_vec());
    let mut h = Sha256::new();
    h.update(tool.as_bytes());
    h.update([0u8]);
    h.update(&compacted);
    let sum = h.finalize();
    sum[..12].iter().map(|b| format!("{b:02x}")).collect()
}

/// Returns `input`, compacted, for `HookReport::tool_input`, or "" if it
/// isn't a JSON object or is too large to store.
pub(crate) fn tool_input(input: &[u8]) -> String {
    match gojson::compact(input) {
        Some(b) if b.len() <= MAX_TOOL_INPUT_LEN && b.starts_with(b"{") => {
            agent_base::gostd::json::lossy(&b).into_owned()
        }
        _ => String::new(),
    }
}

/// Returns the key that answers a numbered permission dialog: "1" approves
/// once, "2" approves always, Escape denies. `question_tools` raise a dialog
/// that asks the user something instead, where "1" would pick an answer.
pub(crate) fn permission_key(
    resp: PermissionResponse,
    tool: &str,
    question_tools: &[&str],
) -> anyhow::Result<String> {
    if question_tools.contains(&tool) {
        anyhow::bail!(
            "{tool} is a question for the user, not a permission prompt; answer it with agent keys (see it with agent capture), or --force"
        );
    }
    Ok(match resp {
        PermissionResponse::Approve => "1",
        PermissionResponse::ApproveAlways => "2",
        PermissionResponse::Deny => "Escape",
    }
    .to_string())
}

/// Bounds dialog detection to the bottom of the screen, where the dialog
/// sits, so numbered options in an older reply can't match.
const DIALOG_TAIL_LINES: usize = 12;

/// Returns the last DIALOG_TAIL_LINES non-blank lines of `screen`, bottom
/// line first.
pub(crate) fn screen_tail(screen: &str) -> Vec<&str> {
    let mut tail = Vec::new();
    for line in screen.split('\n').rev() {
        if tail.len() >= DIALOG_TAIL_LINES {
            break;
        }
        if !line.trim().is_empty() {
            tail.push(line);
        }
    }
    tail
}

/// Finds the input box at the bottom of the screen (`lines`, as split from
/// it): lines between two consecutive rules of "─", the first starting with
/// `prompt`. The box is the bottom such region, or, with `panels` > 0, one
/// of up to that many regions above it: agy draws a panel of running
/// background tasks between rules of its own below its box. It returns the
/// box's indexes (start, end), rules excluded, or `None` if there is no such
/// box.
pub(crate) fn input_box<S: AsRef<str>>(
    lines: &[S],
    prompt: &str,
    panels: usize,
) -> Option<(usize, usize)> {
    let mut rules = Vec::new();
    for i in (0..lines.len()).rev() {
        if rules.len() >= panels + 2 {
            break;
        }
        if is_rule(&strip_ansi(lines[i].as_ref())) {
            rules.push(i);
        }
    }
    let mut k = 0;
    while k + 1 < rules.len() {
        let (top, bottom) = (rules[k + 1], rules[k]);
        k += 1;
        if bottom - top < 2 {
            continue; // an empty region is no box (and no panel)
        }
        if strip_ansi(lines[top + 1].as_ref())
            .trim_start_matches(' ')
            .starts_with(prompt)
        {
            return Some((top + 1, bottom));
        }
    }
    None
}

/// Returns the text typed in the input box of `screen` (with its styling
/// escapes), whose first line starts with `prompt`, found as [`input_box`]
/// finds it. Text drawn faint (placeholders, hints, suggestions: unfaint)
/// is left out; the prompt itself may be drawn grey (Claude's, mid-turn).
pub(crate) fn input_box_text(screen: &str, prompt: &str, panels: usize) -> Option<String> {
    let lines: Vec<&str> = screen.trim_end_matches('\n').split('\n').collect();
    let (start, end) = input_box(&lines, prompt, panels)?;
    let mut text = Vec::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let line = if i == 0 {
            let at = line.find(prompt)?;
            if !strip_ansi(&line[..at]).trim().is_empty() {
                return None;
            }
            let (dim, grey) = faint_after(&line[..at + prompt.len()]);
            unfaint_from(&line[at + prompt.len()..], dim, grey)
        } else {
            unfaint(line)
        };
        let t = line.replace('\u{a0}', " ");
        let t = t.trim();
        if !t.is_empty() {
            text.push(t.to_string());
        }
    }
    Some(text.join("\n"))
}

/// Reports whether `line` is a horizontal rule of "─", plain or with a
/// label set into it: Claude draws a named session's name (--name, /rename)
/// into its input box's top rule, as in "──────── worker ─".
pub(crate) fn is_rule(line: &str) -> bool {
    let line = line.trim();
    let rest = line.trim_start_matches('─');
    let label = rest.trim_end_matches('─');
    let dashes = line.chars().count() - label.chars().count();
    if dashes < 10 {
        return false;
    }
    if label.is_empty() {
        return true;
    }
    // A label: both ends of the rule, and a space either side of the text.
    let lb = label.as_bytes();
    rest != line
        && label != rest
        && lb.len() > 2
        && lb[0] == b' '
        && lb[lb.len() - 1] == b' '
        && !label.contains('─')
}

/// Returns `line`'s text without escapes, and without the text drawn faint:
/// dim (SGR 2, until 22 or a reset) or in a grey (bright black, 90; a grey
/// of the 256-color palette, 38;5;8 or 232-250; or an RGB grey, until
/// another foreground, 39, or a reset). That is how a CLI draws what sits in
/// its input box without having been typed: Claude's "Try ..." placeholder
/// and suggestions (dim), agy's mode hint, "Auto mode: edits & safe commands
/// auto-approved (shift+tab to cycle)", which 1.5.8 shows after every turn
/// (90). Typed text is drawn in the default foreground. Reading the style,
/// not the words, keeps working when a release changes the hint.
pub(crate) fn unfaint(line: &str) -> String {
    unfaint_from(line, false, false)
}

/// The end of the CSI sequence whose parameters start at `from`: the index
/// of its final byte (0x40-0x7e), or `s.len()`.
fn csi_end(s: &[u8], from: usize) -> usize {
    let mut j = from;
    while j < s.len() && (s[j] < 0x40 || s[j] > 0x7e) {
        j += 1;
    }
    j
}

/// Returns whether text after `s` is drawn dim, and in a grey.
pub(crate) fn faint_after(s: &str) -> (bool, bool) {
    let b = s.as_bytes();
    let (mut dim, mut grey) = (false, false);
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b || i + 1 >= b.len() || b[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let j = csi_end(b, i + 2);
        if j < b.len() && b[j] == b'm' {
            (dim, grey) = sgr_faint(&params(&b[i + 2..j]), dim, grey);
        }
        i = j + 1;
    }
    (dim, grey)
}

fn params(b: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(b)
        .split(';')
        .map(str::to_string)
        .collect()
}

/// [`unfaint`] for text that starts drawn dim or grey.
pub(crate) fn unfaint_from(line: &str, mut dim: bool, mut grey: bool) -> String {
    let b = line.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            if !dim && !grey {
                out.push(b[i]);
            }
            i += 1;
            continue;
        }
        if i + 1 >= b.len() || b[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let j = csi_end(b, i + 2);
        if j < b.len() && b[j] == b'm' {
            (dim, grey) = sgr_faint(&params(&b[i + 2..j]), dim, grey);
        }
        i = j + 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Applies one SGR sequence's parameters to whether text is drawn dim and
/// whether its foreground is a grey.
pub(crate) fn sgr_faint(params: &[String], mut dim: bool, mut grey: bool) -> (bool, bool) {
    // Go's strconv.Atoi, -1 for anything else.
    let num = |k: usize| -> i64 {
        params
            .get(k)
            .and_then(|p| p.parse::<i64>().ok())
            .unwrap_or(-1)
    };
    let mut k = 0;
    while k < params.len() {
        let n = num(k);
        if params[k].is_empty() || n == 0 {
            (dim, grey) = (false, false);
        } else if n == 2 {
            dim = true;
        } else if n == 22 {
            dim = false;
        } else if n == 39 || (30..=37).contains(&n) || (91..=97).contains(&n) {
            grey = false;
        } else if n == 90 {
            grey = true;
        } else if n == 38 && num(k + 1) == 5 {
            let c = num(k + 2);
            grey = c == 8 || (232..=250).contains(&c);
            k += 2;
        } else if n == 38 && num(k + 1) == 2 {
            let (r, g, b) = (num(k + 2), num(k + 3), num(k + 4));
            grey = r == g && g == b && (48..=200).contains(&r);
            k += 4;
        } else if n == 48 && num(k + 1) == 5 {
            k += 2;
        } else if n == 48 && num(k + 1) == 2 {
            k += 4;
        }
        k += 1;
    }
    (dim, grey)
}

pub(crate) fn truncate(s: &str) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let runes: Vec<char> = joined.chars().collect();
    if runes.len() > MAX_DETAIL_LEN {
        let mut out: String = runes[..MAX_DETAIL_LEN - 1].iter().collect();
        out.push('…');
        return out;
    }
    joined
}

pub(crate) fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values.iter().copied().find(|v| !v.is_empty()).unwrap_or("")
}

/// Go's `json.Marshal` of a hook's output.
pub(crate) fn marshal<T: Serialize + ?Sized>(v: &T) -> Vec<u8> {
    gojson::marshal(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert!(is_rule(&"─".repeat(10)));
        assert!(!is_rule(&"─".repeat(9)));
        assert!(is_rule(&format!("{} worker ─", "─".repeat(10))));
        assert!(!is_rule(&format!("{} worker", "─".repeat(10))));
        assert!(!is_rule("─ worker ─"));
    }

    #[test]
    fn truncates_to_one_line() {
        assert_eq!(truncate("a\n  b"), "a b");
        let long = "é".repeat(200);
        let got = truncate(&long);
        assert_eq!(got.chars().count(), 120);
        assert!(got.ends_with('…'));
    }

    #[test]
    fn call_keys() {
        assert_eq!(
            call_key("Bash", br#"{"command": "x"}"#),
            call_key("Bash", br#"{"command":"x"}"#)
        );
        assert_ne!(
            call_key("Bash", br#"{"command":"x"}"#),
            call_key("Read", br#"{"command":"x"}"#)
        );
        assert_eq!(call_key("Bash", b"").len(), 24);
    }
}

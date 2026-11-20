//! Go tests/backend/claude_dialog_test.go.

mod common;

use agent_backend::namedagents;
use agent_backend::*;
use common::*;

/// A dialog no hook reports must read as one wherever Claude draws it:
/// under the input box (classic renderer), above it (fullscreen), or in its
/// place. Busy and idle screens, and replies that quote a dialog, must not.
#[test]
fn test_claude_dialog() {
    let rule = "─".repeat(80);
    // The auto-mode setup wizard the offer opens, drawn in the input box's
    // place (built from the strings in Claude Code v2.1.319: it needs a
    // signed-in account to open).
    let wizard = [
        "❯ /auto-mode-setup",
        "",
        rule.as_str(),
        "  Teach auto mode about your environment?",
        "",
        "  Claude Code reads this project, your recent Claude sessions, and optionally",
        "  your shell history and other repositories.",
        "",
        "  ❯ How you use Claude here   Mixed",
        "    Also scan shell history   [✔]",
        "    Also scan your other repos [ ]",
        "",
        "    Continue",
        "",
        "  ←/→ to change · Enter to continue · Esc to cancel",
    ]
    .join("\n");
    let idle_box = [
        rule.as_str(),
        "❯ ",
        rule.as_str(),
        "  ⏵⏵ auto mode on (shift+tab to cycle)",
    ];
    let with_box = |head: &[&str]| {
        let mut lines = head.to_vec();
        lines.extend_from_slice(&idle_box);
        lines.join("\n")
    };
    /// The title and highlighted option a screen should read as.
    type Want<'a> = Option<(&'a str, &'a str)>;
    let tests: Vec<(&str, String, Want)> = vec![
        (
            "offer under the input box (classic)",
            read_screen("claude-offer-under-box.txt"),
            Some(("Teach auto mode about your environment?", "1. Yes")),
        ),
        (
            "offer above the input box (fullscreen)",
            read_screen("claude-offer-above-box.txt"),
            Some(("Teach auto mode about your environment?", "1. Yes")),
        ),
        (
            "setup wizard in the input box's place",
            wizard,
            Some((
                "Teach auto mode about your environment?",
                "How you use Claude here Mixed",
            )),
        ),
        // A picker a person opens: its border is "▔", and the registry says
        // waiting with nothing on what.
        (
            "/model picker",
            read_screen("claude-model-picker.txt"),
            Some((
                "Select model",
                "1. Default (recommended) ✔ Use the default model (currently Opus 5.5) ·",
            )),
        ),
        (
            "permission prompt",
            CLAUDE_BASH_DIALOG.to_string(),
            Some(("Bash command", "1. Yes")),
        ),
        ("busy", read_screen("claude-busy.txt"), None),
        ("idle", read_screen("claude-idle.txt"), None),
        ("running turn", CLAUDE_RUNNING.to_string(), None),
        (
            "idle, reply mentions options",
            CLAUDE_IDLE_AFTER_LIST.to_string(),
            None,
        ),
        (
            "starting, no input box yet",
            " ▐▛███▛█   Claude Code v2.1.319\n▝▜██████▀  Opus 5.5\n".to_string(),
            None,
        ),
        (
            "reply quoting a footer above the box, no rule",
            with_box(&[
                "● The dialog said:",
                "  Enter to confirm · Esc to cancel",
                "",
            ]),
            None,
        ),
        (
            "key hints with no Enter or Esc",
            with_box(&[
                rule.as_str(),
                "  Some panel",
                "  Tab to switch · Space to toggle",
            ]),
            None,
        ),
        (
            "a running turn's footer alone",
            [idle_box[0], idle_box[1], idle_box[2], "  esc to interrupt"].join("\n"),
            None,
        ),
        ("empty", String::new(), None),
    ];
    let b = namedagents::new_claude_backend();
    for (name, screen, want) in tests {
        let got = b.dialog(&screen);
        let got = got
            .as_ref()
            .map(|d| (d.title.as_str(), d.highlighted.as_str()));
        assert_eq!(got, want, "{name}");
    }
}

/// approve picks the highlighted option; deny closes the dialog (Esc, which
/// an offer takes for "not now").
#[test]
fn test_claude_dialog_key() {
    let b = namedagents::new_claude_backend();
    assert_eq!(b.dialog_key(PermissionResponse::Approve).unwrap(), "Enter");
    assert_eq!(b.dialog_key(PermissionResponse::Deny).unwrap(), "Escape");
    let err = b
        .dialog_key(PermissionResponse::ApproveAlways)
        .expect_err("a dialog has no always");
    assert_eq!(
        err.to_string(),
        "a dialog has no \"always\" answer: approve picks its highlighted option, deny closes it"
    );
}

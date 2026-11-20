//! Go tests/backend/trust_test.go (the folders made up).

use agent_backend::namedagents;
use agent_backend::*;

/// Claude Code v2.1.310's folder trust dialog, as agent capture showed it
/// for an agent spawned in a folder Claude hadn't trusted (2026-11, an
/// 80-column pane): the folder wraps at the edge.
const CLAUDE_TRUST_SCREEN: &str = "sh '/opt/gomuxai/.gomuxai/agents/t1/launch.sh'
────────────────────────────────────────────────────────────────────────────────
 Accessing workspace:

 /private/tmp/claude-501/-Users-user-localdev-proj--claude-worktrees-trust-prom
 pt/scratchpad/trust-cap

 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel


";

/// agy's workspace trust prompt, captured the same way.
const AGY_TRUST_SCREEN: &str = "Accessing workspace:

/private/tmp/claude-501/-Users-user-localdev-proj--claude-worktrees-trust-promp
t/scratchpad/agy-cap

Do you trust the contents of this project?

Antigravity CLI requires permission to read, edit, and execute files here.

> Yes, I trust this folder
  No, exit

  ↑/↓ Navigate · enter Confirm
                                                           Gemini 4 Argon · high




";

const CLAUDE_FOLDER: &str = "/private/tmp/claude-501/-Users-user-localdev-proj--claude-worktrees-trust-prompt/scratchpad/trust-cap";
const AGY_FOLDER: &str = "/private/tmp/claude-501/-Users-user-localdev-proj--claude-worktrees-trust-prompt/scratchpad/agy-cap";

#[test]
fn test_trust_prompt() {
    let claude = namedagents::new_claude_backend();
    let agy = namedagents::new_agy_backend();
    // The other option highlighted: Claude after an Up, agy after a Down.
    let claude_yes = CLAUDE_TRUST_SCREEN
        .replacen(" ❯ No, exit", "   No, exit", 1)
        .replacen("   Yes, I", " ❯ Yes, I", 1);
    let agy_no = AGY_TRUST_SCREEN
        .replacen("> Yes, I", "  Yes, I", 1)
        .replacen("  No, exit", "> No, exit", 1);
    let rule40 = "─".repeat(40);

    let tests: Vec<(&str, &dyn TrustPrompter, String, &str, &str, &str)> = vec![
        (
            "claude, No highlighted",
            &claude,
            CLAUDE_TRUST_SCREEN.to_string(),
            CLAUDE_FOLDER,
            "Down,Enter",
            "Enter",
        ),
        (
            "claude, Yes highlighted",
            &claude,
            claude_yes,
            CLAUDE_FOLDER,
            "Enter",
            "Up,Enter",
        ),
        (
            "agy, Yes highlighted",
            &agy,
            AGY_TRUST_SCREEN.to_string(),
            AGY_FOLDER,
            "Enter",
            "Down,Enter",
        ),
        (
            "agy, No highlighted",
            &agy,
            agy_no,
            AGY_FOLDER,
            "Up,Enter",
            "Enter",
        ),
        // Each CLI's dialog is its own: the other's question isn't there.
        (
            "claude reading agy's",
            &claude,
            AGY_TRUST_SCREEN.to_string(),
            "",
            "",
            "",
        ),
        (
            "agy reading claude's",
            &agy,
            CLAUDE_TRUST_SCREEN.to_string(),
            "",
            "",
            "",
        ),
        (
            "claude at its prompt",
            &claude,
            "● ok\n\n❯ Try \"refactor <filepath>\"\n".to_string(),
            "",
            "",
            "",
        ),
        // The dialog's words quoted higher up, with the idle prompt below (a
        // transcript, a file on screen), aren't the dialog.
        (
            "claude, quoted above its prompt",
            &claude,
            format!(
                "{CLAUDE_TRUST_SCREEN}\n● That was the trust dialog.\n\n{rule40}\n❯ \n{rule40}\n  ? for shortcuts\n"
            ),
            "",
            "",
            "",
        ),
        // A CLI that exited at the dialog leaves it on screen, the shell's
        // prompt under it.
        (
            "claude, exited",
            &claude,
            format!("{CLAUDE_TRUST_SCREEN}➜  trust-cap \n"),
            "",
            "",
            "",
        ),
        (
            "agy, exited",
            &agy,
            format!("{AGY_TRUST_SCREEN}$ \n"),
            "",
            "",
            "",
        ),
        (
            "agy, quoted above its prompt",
            &agy,
            format!("{AGY_TRUST_SCREEN}\nok\n\n{rule40}\n> \n{rule40}\n? for shortcuts\n"),
            "",
            "",
            "",
        ),
    ];
    for (name, tp, screen, folder, trust, refuse) in tests {
        let want = (!folder.is_empty()).then(|| folder.to_string());
        assert_eq!(tp.trust_prompt(&screen), want, "{name}: trust_prompt");
        let keys = |k: Option<Vec<String>>| k.map(|k| k.join(",")).unwrap_or_default();
        assert_eq!(keys(tp.trust_keys(&screen)), trust, "{name}: trust_keys");
        assert_eq!(
            keys(tp.decline_trust_keys(&screen)),
            refuse,
            "{name}: decline_trust_keys"
        );
    }
}

/// With neither option marked, no keys are guessed: Enter could pick
/// either.
#[test]
fn test_trust_keys_nothing_highlighted() {
    let claude = namedagents::new_claude_backend();
    let screen = CLAUDE_TRUST_SCREEN.replacen(" ❯ No, exit", "   No, exit", 1);
    assert!(
        claude.trust_prompt(&screen).is_some(),
        "the dialog should still be recognized"
    );
    assert_eq!(claude.trust_keys(&screen), None);
    assert_eq!(claude.decline_trust_keys(&screen), None);
}

/// A dialog whose header scrolled off still counts; its folder is unknown.
#[test]
fn test_trust_prompt_no_header() {
    let screen = &CLAUDE_TRUST_SCREEN[CLAUDE_TRUST_SCREEN.find(" Quick safety check").unwrap()..];
    assert_eq!(
        namedagents::new_claude_backend().trust_prompt(screen),
        Some(String::new())
    );
}

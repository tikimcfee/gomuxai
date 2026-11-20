//! Go `internal/service/slash.go`.

use std::sync::LazyLock;

use regex::Regex;

/// slashCommand matches a CLI slash command: "/compact", "/model
/// claude-fable-5-1", "/plugin:skill args". A path ("/tmp/out.txt is ready")
/// has a second slash before any space, and isn't one.
static SLASH_COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/[A-Za-z][A-Za-z0-9_:.-]*(?:[\t\n\f\r ]|$)").unwrap());

/// Reports whether message is a slash command for the CLI it is typed into.
/// Such a message is typed as is: a "[from ...]" prefix would turn it into
/// chat, and a hook that hands over text can't run it.
pub fn is_slash_command(message: &str) -> bool {
    SLASH_COMMAND.is_match(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go TestIsSlashCommand (slash_test.go).
    #[test]
    fn test_is_slash_command() {
        for (msg, want) in [
            ("/compact", true),
            ("/model claude-fable-5-1", true),
            ("/model\nopus", true),
            ("/plugin:skill args", true),
            ("/clear", true),
            ("/tmp/out.txt is ready", false),
            ("/", false),
            ("/ compact", false),
            ("/1abc", false),
            ("hi /compact", false),
            ("[from lead@h1] /compact", false),
            ("/etc/hosts", false),
            ("/usr/bin/env is missing", false),
        ] {
            assert_eq!(is_slash_command(msg), want, "{msg:?}");
        }
    }
}

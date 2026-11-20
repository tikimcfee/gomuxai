//! Go `internal/backend/generic.go`.

use serde::Deserialize;

use crate::backend::Backend;

/// Implements [`Backend`] for custom CLI tools.
#[derive(Debug, Clone, Default)]
pub struct GenericBackend {
    name: String,
    launch_command: String,
    init_message_format: String,
    idle_pattern: String,
    processing_pattern: String,
}

/// Holds configuration for a generic backend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GenericConfig {
    pub name: String,
    pub launch_command: String,
    /// Use %s for the task file.
    pub init_message_format: String,
    pub idle_pattern: String,
    pub processing_pattern: String,
}

/// Creates a new generic backend from config.
pub fn new_generic_backend(cfg: GenericConfig) -> GenericBackend {
    GenericBackend {
        name: cfg.name,
        launch_command: cfg.launch_command,
        init_message_format: cfg.init_message_format,
        idle_pattern: cfg.idle_pattern,
        processing_pattern: cfg.processing_pattern,
    }
}

impl Backend for GenericBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn launch_command(&self) -> String {
        self.launch_command.clone()
    }

    fn init_message(&self, task_file: &str) -> String {
        if !task_file.is_empty() && !self.init_message_format.is_empty() {
            return sprintf_string(&self.init_message_format, task_file);
        }
        String::new()
    }

    fn idle_pattern(&self) -> &str {
        &self.idle_pattern
    }

    fn processing_pattern(&self) -> &str {
        &self.processing_pattern
    }
}

/// Go's `fmt.Sprintf(format, arg)` for one string argument, as a person's
/// `init_message_format` uses it: `%s` and `%v` print it, `%q` quotes it,
/// `%%` is a percent sign. What Go prints for the rest is kept too: a verb
/// past the one argument is `%!s(MISSING)`, a verb that doesn't take a
/// string is `%!d(string=...)`, and an argument no verb used is appended as
/// `%!(EXTRA string=...)`. Flags, width and precision are passed over.
fn sprintf_string(format: &str, arg: &str) -> String {
    let mut out = String::new();
    let mut used = false;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Flags, width and precision.
        while let Some(&f) = chars.peek() {
            if "+-# 0123456789.".contains(f) {
                chars.next();
            } else {
                break;
            }
        }
        let Some(verb) = chars.next() else {
            out.push_str("%!(NOVERB)");
            break;
        };
        if verb == '%' {
            out.push('%');
            continue;
        }
        if used {
            out.push_str(&format!("%!{verb}(MISSING)"));
            continue;
        }
        used = true;
        match verb {
            's' | 'v' => out.push_str(arg),
            'q' => out.push_str(&agent_base::gostd::strconv::quote(arg)),
            'x' => out.push_str(&hex(arg, false)),
            'X' => out.push_str(&hex(arg, true)),
            _ => out.push_str(&format!("%!{verb}(string={arg})")),
        }
    }
    if !used {
        out.push_str(&format!("%!(EXTRA string={arg})"));
    }
    out
}

fn hex(s: &str, upper: bool) -> String {
    s.bytes()
        .map(|b| {
            if upper {
                format!("{b:02X}")
            } else {
                format!("{b:02x}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprintf_like_go() {
        assert_eq!(sprintf_string("/read %s", "t.md"), "/read t.md");
        assert_eq!(sprintf_string("read %q", "t.md"), "read \"t.md\"");
        assert_eq!(sprintf_string("100%% %s", "t"), "100% t");
        assert_eq!(sprintf_string("%s and %s", "t"), "t and %!s(MISSING)");
        assert_eq!(sprintf_string("%d", "t"), "%!d(string=t)");
        assert_eq!(sprintf_string("no verb", "t"), "no verb%!(EXTRA string=t)");
        assert_eq!(
            sprintf_string("trailing %", "t"),
            "trailing %!(NOVERB)%!(EXTRA string=t)"
        );
    }

    #[test]
    fn generic_init_message() {
        let b = new_generic_backend(GenericConfig {
            name: "my-cli".into(),
            launch_command: "my-cli".into(),
            init_message_format: "Please read %s".into(),
            ..Default::default()
        });
        assert_eq!(b.init_message("t.md"), "Please read t.md");
        assert_eq!(b.init_message(""), "");
        assert_eq!(b.name(), "my-cli");
    }
}

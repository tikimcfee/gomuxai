//! pflag's flag syntax and words, over clap. No Go file: this is the part of
//! spf13/pflag the Go relied on.
//!
//! What pflag does that clap doesn't by default, and every command here gets:
//! a bool flag takes `--json=false` (and never the next word), a flag's value
//! may start with a dash (`--agent -`), a flag given twice keeps the last value,
//! ints parse as Go's strconv.ParseInt(s, 0, 64), durations as Go's
//! time.ParseDuration, and errors say what pflag says.

use std::time::Duration;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgAction};

/// Gives every flag of `cmd` pflag's syntax: bools become `--flag[=bool]`, and
/// flags that take a value take one that starts with a dash.
pub fn go_syntax(cmd: clap::Command) -> clap::Command {
    cmd.mut_args(|arg| {
        if arg.is_positional() {
            return arg;
        }
        match arg.get_action() {
            ArgAction::SetTrue => arg
                .action(ArgAction::Set)
                .num_args(0..=1)
                .require_equals(true)
                .default_missing_value("true")
                .default_value("false")
                .value_parser(parse_bool),
            ArgAction::Set | ArgAction::Append => arg.allow_hyphen_values(true),
            _ => arg,
        }
    })
}

/// Whether `arg` is a bool flag, which never takes the next word as its value
/// (pflag: it has a NoOptDefVal).
pub fn is_bool(arg: &Arg) -> bool {
    arg.is_require_equals_set()
}

/// How pflag names a flag in errors: "-n, --lines" or "--lines".
pub fn flag_name(arg: &Arg) -> String {
    let long = arg.get_long().unwrap_or_default();
    match arg.get_short() {
        Some(s) => format!("-{s}, --{long}"),
        None => format!("--{long}"),
    }
}

/// Go's strconv.ParseBool, with its error.
pub fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(format!(
            "strconv.ParseBool: parsing {}: invalid syntax",
            go_quote(s)
        )),
    }
}

/// Go's strconv.ParseInt(s, 0, 64), pflag's parse of an int or int64 flag: an
/// optional sign, then decimal, or 0x, 0o, 0b or 0 (octal) prefixed digits, with
/// underscores between digits only after a prefix.
pub fn parse_int(s: &str) -> Result<i64, String> {
    let syntax = || format!("strconv.ParseInt: parsing {}: invalid syntax", go_quote(s));
    let range = || {
        format!(
            "strconv.ParseInt: parsing {}: value out of range",
            go_quote(s)
        )
    };
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if body.is_empty() {
        return Err(syntax());
    }
    let lower = body.to_ascii_lowercase();
    let (radix, digits, prefixed) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d.to_string(), true)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d.to_string(), true)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d.to_string(), true)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, lower[1..].to_string(), true)
    } else {
        (10, lower.clone(), false)
    };
    if digits.contains('_') && !(prefixed && underscores_ok(&lower)) {
        return Err(syntax());
    }
    let digits: String = digits.chars().filter(|&c| c != '_').collect();
    if digits.is_empty() {
        return Err(syntax());
    }
    let mut n: u128 = 0;
    for c in digits.chars() {
        let d = c.to_digit(radix).ok_or_else(syntax)? as u128;
        n = n * radix as u128 + d;
        if n > 1 << 64 {
            return Err(range());
        }
    }
    if neg {
        if n > 1 << 63 {
            return Err(range());
        }
        Ok((n as i128).wrapping_neg() as i64)
    } else {
        i64::try_from(n).map_err(|_| range())
    }
}

/// Go's strconv underscoreOK: underscores only between digits (or after the base
/// prefix), never doubled or trailing.
fn underscores_ok(s: &str) -> bool {
    // 'saw' is the last kind of character: '^' start, '0' digit or prefix, '_'.
    let mut saw = '^';
    let b = s.as_bytes();
    let mut i = 0;
    let hex = s.len() >= 2 && (b[1] == b'x' || b[1] == b'o' || b[1] == b'b') && b[0] == b'0';
    if s.len() >= 2 && b[0] == b'0' && (b[1] as char).is_ascii_alphabetic() {
        i = 2;
        saw = '0';
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() || (hex && c.is_ascii_hexdigit()) {
            saw = '0';
        } else if c == b'_' {
            if saw != '0' {
                return false;
            }
            saw = '_';
        } else {
            if saw == '_' {
                return false;
            }
            saw = '!';
        }
        i += 1;
    }
    saw != '_'
}

/// Go's time.ParseDuration ("1h30m", "2s", "1.5m", "300ms"), with its errors;
/// a negative duration comes back as zero (see agent_base::gostd::time).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    agent_base::gostd::time::parse_duration(s).map_err(|e| e.to_string())
}

/// Go's strconv.Quote (fmt's %q of a string).
pub use agent_base::gostd::strconv::quote as go_quote;

/// Go's %q of a rune: 'x'.
use agent_base::gostd::strconv::quote_rune as go_quote_rune;

/// What pflag would have said for the error clap gave parsing `argv` with `cmd`.
pub fn pflag_message(err: &clap::Error, cmd: &clap::Command, argv: &[String]) -> String {
    let context = |kind| match err.get(kind) {
        Some(ContextValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    let invalid_arg = context(ContextKind::InvalidArg).unwrap_or_default();
    // clap names the flag as typed ("--jsn=1"), or as it shows it ("--lines <int>",
    // "--json[=<JSON>]").
    let long_of = |s: &str| -> Option<String> {
        let name = s.strip_prefix("--")?;
        Some(
            name.chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '+'))
                .collect(),
        )
    };
    let find_arg = |s: &str| -> Option<&Arg> {
        if let Some(long) = long_of(s) {
            return cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(long.as_str()));
        }
        let short = s.strip_prefix('-')?.chars().next()?;
        cmd.get_arguments().find(|a| a.get_short() == Some(short))
    };
    match err.kind() {
        ErrorKind::UnknownArgument => {
            if let Some(long) = long_of(&invalid_arg) {
                return format!("unknown flag: --{long}");
            }
            let short = invalid_arg
                .trim_start_matches('-')
                .chars()
                .next()
                .unwrap_or('-');
            let token = short_token(argv, short).unwrap_or_else(|| invalid_arg.clone());
            format!(
                "unknown shorthand flag: {} in {token}",
                go_quote_rune(short)
            )
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation | ErrorKind::NoEquals => {
            let Some(arg) = find_arg(&invalid_arg) else {
                return first_line(err);
            };
            let value = context(ContextKind::InvalidValue).unwrap_or_default();
            if err.kind() == ErrorKind::InvalidValue && value.is_empty() {
                // A flag at the end of the line, with no value after it.
                return needs_argument(arg, argv);
            }
            let why = std::error::Error::source(err)
                .map(|e| e.to_string())
                .unwrap_or_else(|| first_line(err));
            format!(
                "invalid argument {} for {} flag: {why}",
                go_quote(&value),
                go_quote(&flag_name(arg))
            )
        }
        _ => first_line(err),
    }
}

/// pflag's error for a flag given no value: as it was typed, long or short.
fn needs_argument(arg: &Arg, argv: &[String]) -> String {
    let long = format!("--{}", arg.get_long().unwrap_or_default());
    if !argv.contains(&long)
        && let Some(short) = arg.get_short()
        && let Some(token) = short_token(argv, short)
    {
        return format!(
            "flag needs an argument: {} in {token}",
            go_quote_rune(short)
        );
    }
    format!("flag needs an argument: {long}")
}

/// How pflag shows the cluster of short flags `short` was in: a dash and the
/// cluster from that letter on ("-x" for the x of "-qx"), from the last word of
/// argv that has it.
fn short_token(argv: &[String], short: char) -> Option<String> {
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let word = argv[..end]
        .iter()
        .rev()
        .find(|a| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(short))?;
    let at = word[1..].find(short)? + 1;
    Some(format!("-{}", &word[at..]))
}

fn first_line(err: &clap::Error) -> String {
    let text = err
        .kind()
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string());
    text.lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches("error: ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_ints() {
        for (s, want) in [
            ("50", 50),
            ("-1", -1),
            ("+7", 7),
            ("0x1f", 31),
            ("0o17", 15),
            ("017", 15),
            ("0b101", 5),
            ("0x_1f", 31),
            ("0", 0),
        ] {
            assert_eq!(parse_int(s), Ok(want), "{s}");
        }
        assert_eq!(
            parse_int("abc").unwrap_err(),
            r#"strconv.ParseInt: parsing "abc": invalid syntax"#
        );
        assert_eq!(
            parse_int("1_000").unwrap_err(),
            r#"strconv.ParseInt: parsing "1_000": invalid syntax"#
        );
        assert_eq!(
            parse_int("99999999999999999999").unwrap_err(),
            r#"strconv.ParseInt: parsing "99999999999999999999": value out of range"#
        );
        assert_eq!(parse_int("-9223372036854775808"), Ok(i64::MIN));
        assert!(parse_int("").is_err());
    }

    #[test]
    fn go_durations() {
        let ms = Duration::from_millis;
        for (s, want) in [
            ("2s", ms(2000)),
            ("0", ms(0)),
            ("0s", ms(0)),
            ("1h30m", ms(5_400_000)),
            ("1.5m", ms(90_000)),
            ("300ms", ms(300)),
            (".5s", ms(500)),
            ("1us", Duration::from_micros(1)),
        ] {
            assert_eq!(parse_duration(s), Ok(want), "{s}");
        }
        assert_eq!(
            parse_duration("abc").unwrap_err(),
            r#"time: invalid duration "abc""#
        );
        assert_eq!(
            parse_duration("5").unwrap_err(),
            r#"time: missing unit in duration "5""#
        );
        assert_eq!(
            parse_duration("5x").unwrap_err(),
            r#"time: unknown unit "x" in duration "5x""#
        );
        assert_eq!(
            parse_duration("").unwrap_err(),
            r#"time: invalid duration """#
        );
    }

    #[test]
    fn go_quoting() {
        assert_eq!(go_quote("a\"b\\c\n"), r#""a\"b\\c\n""#);
        assert_eq!(go_quote("Straße"), "\"Straße\"");
        assert_eq!(go_quote_rune('x'), "'x'");
    }
}

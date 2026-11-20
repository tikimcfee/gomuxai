//! A command's help, in cobra's layout: its default help and usage templates and
//! pflag's flag table, rendered from the command's Doc and clap flags. No Go file:
//! the Go used cobra's templates as they are.

use std::collections::HashSet;

use clap::{Arg, Args as _};

use super::command::{Cmd, Def};
use super::flags::{self, go_quote};
use super::root::Globals;

/// cobra's minNamePadding: the least width of the command-name column.
const MIN_NAME_PADDING: usize = 11;

/// What `agent help <command>` and `agent <command> --help` print (cobra's help
/// template).
pub fn render(cmd: &Cmd) -> String {
    let def = cmd.def();
    let mut out = String::new();
    let text = if def.doc.long.is_empty() {
        def.doc.short.into()
    } else {
        def.doc.long_text()
    };
    if !text.is_empty() {
        out.push_str(text.trim_end());
        out.push_str("\n\n");
    }
    if def.runnable() || !def.subs.is_empty() {
        out.push_str(&usage(cmd));
    }
    out
}

/// cobra's usage template.
pub fn usage(cmd: &Cmd) -> String {
    let def = cmd.def();
    let mut out = String::from("Usage:");
    if def.runnable() {
        out.push_str(&format!("\n  {}", cmd.use_line()));
    }
    let has_subs = def.has_available_subs();
    if has_subs {
        out.push_str(&format!("\n  {} [command]", cmd.command_path()));
    }
    if !def.doc.aliases.is_empty() {
        let mut names = vec![def.name()];
        names.extend(def.doc.aliases);
        out.push_str(&format!("\n\nAliases:\n  {}", names.join(", ")));
    }
    if !def.doc.example.is_empty() {
        out.push_str(&format!("\n\nExamples:\n{}", def.doc.example));
    }
    if has_subs {
        let pad = name_padding(def);
        // The root's help command is listed though cobra doesn't count it as
        // available: it is its parent's help command.
        let listed = |s: &Def| s.is_available() || s.name() == "help";
        let line = |s: &Def| format!("\n  {:<pad$} {}", s.name(), s.doc.short);
        if def.groups.is_empty() {
            out.push_str("\n\nAvailable Commands:");
            for s in def.subs.iter().filter(|s| listed(s)) {
                out.push_str(&line(s));
            }
        } else {
            for (id, title) in &def.groups {
                out.push_str(&format!("\n\n{title}"));
                for s in def.subs.iter().filter(|s| s.group_id == *id && listed(s)) {
                    out.push_str(&line(s));
                }
            }
            if def.subs.iter().any(|s| s.group_id.is_empty() && listed(s)) {
                out.push_str("\n\nAdditional Commands:");
                for s in def
                    .subs
                    .iter()
                    .filter(|s| s.group_id.is_empty() && listed(s))
                {
                    out.push_str(&line(s));
                }
            }
        }
    }
    let parser = def.clap_command();
    let globals = global_ids();
    let flags: Vec<&Arg> = parser
        .get_arguments()
        .filter(|a| !a.is_positional() && !a.is_hide_set())
        .collect();
    // The root's own flags include the global ones it defines.
    let (local, inherited): (Vec<&Arg>, Vec<&Arg>) = flags
        .into_iter()
        .partition(|a| !cmd.has_parent() || !globals.contains(a.get_id().as_str()));
    if !local.is_empty() {
        out.push_str(&format!("\n\nFlags:\n{}", flag_usages(local).trim_end()));
    }
    if !inherited.is_empty() {
        out.push_str(&format!(
            "\n\nGlobal Flags:\n{}",
            flag_usages(inherited).trim_end()
        ));
    }
    // cobra's "Additional help topics" lists non-runnable commands without
    // subcommands; agent has none.
    if has_subs {
        out.push_str(&format!(
            "\n\nUse \"{} [command] --help\" for more information about a command.",
            cmd.command_path()
        ));
    }
    out.push('\n');
    out
}

/// The ids of the global flags (Go: root's persistent flags).
pub fn global_ids() -> HashSet<String> {
    Globals::augment_args(clap::Command::new("globals"))
        .get_arguments()
        .map(|a| a.get_id().to_string())
        .collect()
}

/// The width of a command's subcommand-name column: its longest subcommand name,
/// hidden ones too, and at least cobra's minimum.
fn name_padding(def: &Def) -> usize {
    def.subs
        .iter()
        .map(|s| s.name().len())
        .max()
        .unwrap_or(0)
        .max(MIN_NAME_PADDING)
}

/// pflag's FlagUsages: one line per flag, sorted by name, the descriptions lined
/// up two spaces past the longest "-x, --name type".
pub fn flag_usages(mut args: Vec<&Arg>) -> String {
    args.sort_by_key(|a| a.get_long().unwrap_or_default());
    let mut lines = Vec::new();
    for arg in args {
        let long = arg.get_long().unwrap_or_default();
        let mut head = match arg.get_short() {
            Some(s) => format!("  -{s}, --{long}"),
            None => format!("      --{long}"),
        };
        let var = var_name(arg);
        if !var.is_empty() {
            head.push(' ');
            head.push_str(&var);
        }
        let mut usage = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
        if let Some(default) = shown_default(arg) {
            usage.push_str(&format!(" (default {default})"));
        }
        lines.push((head, usage));
    }
    // pflag measures each head with a separator byte after it, then pads to that
    // and adds a space on either side of the padding.
    let max = lines.iter().map(|(h, _)| h.len() + 1).max().unwrap_or(0);
    let mut out = String::new();
    for (head, usage) in lines {
        let spacing = " ".repeat(max - head.len());
        let usage = usage.replace('\n', &format!("\n{}", " ".repeat(max + 2)));
        out.push_str(&format!("{head} {spacing} {usage}\n"));
    }
    out
}

/// The type pflag shows after a flag that takes a value: its value name in the
/// Args struct ("string", "int", "duration", "strings", "stringArray"); none for a
/// bool.
fn var_name(arg: &Arg) -> String {
    if flags::is_bool(arg) {
        return String::new();
    }
    arg.get_value_names()
        .and_then(|v| v.first())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// The default pflag shows after a flag's description, unless it is the type's
/// zero value; strings quoted, lists as "[a,b]".
fn shown_default(arg: &Arg) -> Option<String> {
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let var = var_name(arg);
    let value = match var.as_str() {
        "strings" | "stringArray" => format!("[{}]", defaults.join(",")),
        _ => defaults.join(","),
    };
    // A flag with no default at all (an Option field: run's --show) shows none.
    let zero = defaults.is_empty()
        || match var.as_str() {
            "" => value == "false",
            "string" => value.is_empty(),
            "duration" => value == "0" || value == "0s",
            "int" => value == "0",
            "strings" | "stringArray" => value == "[]",
            _ => value.is_empty() || value == "0" || value == "false",
        };
    if zero {
        return None;
    }
    Some(if var == "string" {
        go_quote(&value)
    } else {
        value
    })
}

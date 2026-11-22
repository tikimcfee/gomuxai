//! What every command shares about output and usage errors (Go:
//! internal/cli/output.go).
//!
//! Every command prints its result on stdout and at most one hint, a suggested
//! next step, on stderr. A hint is only printed after a check showed the step is
//! needed, and --quiet drops it.

use std::fmt::Display;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_base::util::{self, similar};
use clap::ArgMatches;
use serde::Serialize;

use super::command::Cmd;
use super::flags::go_quote;
use super::help;
use super::root::Ctx;

impl Ctx {
    /// Prints a suggested next step on stderr: the first one per command, unless
    /// --quiet.
    pub fn hint(&self, msg: impl Display) {
        if self.quiet() || self.hinted.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = writeln!(self.err(), "hint: {msg}");
    }

    /// Tells, on stderr, what a command whose stdout is a stream of data is doing,
    /// unless --quiet.
    pub fn notice(&self, msg: impl Display) {
        if !self.quiet() {
            let _ = writeln!(self.err(), "{msg}");
        }
    }

    /// Prints v as indented JSON on stdout, for --json. &, < and > print as
    /// themselves, so commands read as typed.
    pub fn print_json<T: Serialize + ?Sized>(&self, v: &T) -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, GoJson::new());
        v.serialize(&mut ser)?;
        buf.push(b'\n');
        self.out().write_all(&buf)?;
        Ok(())
    }
}

/// encoding/json's Encoder with SetIndent("", "  ") and SetEscapeHTML(false):
/// serde_json's pretty printer, but escaping U+2028 and U+2029 as Go always does,
/// and indenting a raw value (Go: json.RawMessage) as Go's encoder does, at the
/// depth it sits at, where serde_json writes it as it came.
struct GoJson<'a> {
    pretty: serde_json::ser::PrettyFormatter<'a>,
    /// How many arrays and objects are open where the next value goes.
    depth: usize,
}

impl GoJson<'_> {
    fn new() -> Self {
        GoJson {
            pretty: serde_json::ser::PrettyFormatter::new(),
            depth: 0,
        }
    }
}

/// The indent print_json writes per level.
const JSON_INDENT: &[u8] = b"  ";

/// Writes s, escaping U+2028 and U+2029 as Go's encoder always does.
fn write_go_escaped<W: ?Sized + Write>(w: &mut W, s: &str) -> std::io::Result<()> {
    let mut rest = s;
    while let Some(i) = rest.find(['\u{2028}', '\u{2029}']) {
        w.write_all(&rest.as_bytes()[..i])?;
        let c = rest[i..].chars().next().unwrap_or('\u{2028}');
        write!(w, "\\u{:04x}", c as u32)?;
        rest = &rest[i + c.len_utf8()..];
    }
    w.write_all(rest.as_bytes())
}

/// Writes raw, a valid JSON value, as Go's encoder with SetIndent("", "  ")
/// writes a json.RawMessage `depth` levels deep: the whitespace between its
/// tokens dropped, each member and element on a line of its own, an empty
/// object or array as `{}` or `[]`, and the text of its strings and numbers as
/// it came (escapes and all), but for U+2028 and U+2029, which are escaped.
fn write_indented_raw<W: ?Sized + Write>(
    w: &mut W,
    raw: &str,
    depth: usize,
) -> std::io::Result<()> {
    let b = raw.as_bytes();
    let newline = |w: &mut W, depth: usize| -> std::io::Result<()> {
        w.write_all(b"\n")?;
        for _ in 0..depth {
            w.write_all(JSON_INDENT)?;
        }
        Ok(())
    };
    let mut depth = depth;
    // An opening bracket's line break waits for what follows it: none if it
    // is closed at once.
    let mut opened = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
            continue;
        }
        if opened && c != b'}' && c != b']' {
            newline(w, depth)?;
        }
        let was_open = std::mem::replace(&mut opened, false);
        match c {
            b'{' | b'[' => {
                w.write_all(&[c])?;
                depth += 1;
                opened = true;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if !was_open {
                    newline(w, depth)?;
                }
                w.write_all(&[c])?;
                i += 1;
            }
            b',' => {
                w.write_all(b",")?;
                newline(w, depth)?;
                i += 1;
            }
            b':' => {
                w.write_all(b": ")?;
                i += 1;
            }
            b'"' => {
                // To the closing quote, past escaped ones.
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(b.len());
                write_go_escaped(w, &raw[i..end])?;
                i = end;
            }
            _ => {
                // A number or a literal, to the next delimiter.
                let mut j = i;
                while j < b.len()
                    && !matches!(
                        b[j],
                        b',' | b':' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'
                    )
                {
                    j += 1;
                }
                w.write_all(&b[i..j])?;
                i = j;
            }
        }
    }
    Ok(())
}

impl serde_json::ser::Formatter for GoJson<'_> {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        write_go_escaped(w, fragment)
    }
    fn write_raw_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        write_indented_raw(w, fragment, self.depth)
    }
    fn begin_array<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.depth += 1;
        self.pretty.begin_array(w)
    }
    fn end_array<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.depth = self.depth.saturating_sub(1);
        self.pretty.end_array(w)
    }
    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.pretty.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.end_array_value(w)
    }
    fn begin_object<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.depth += 1;
        self.pretty.begin_object(w)
    }
    fn end_object<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.depth = self.depth.saturating_sub(1);
        self.pretty.end_object(w)
    }
    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.pretty.begin_object_key(w, first)
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.end_object_value(w)
    }
}

// Writes to a closed pipe, as Go's runtime takes them.

/// Set by [`ignore_sigpipe`] (Go: signal.Ignore(syscall.SIGPIPE)).
static SIGPIPE_IGNORED: AtomicBool = AtomicBool::new(false);

/// Go's signal.Ignore(syscall.SIGPIPE): a write to a closed pipe on stdout or
/// stderr then fails with EPIPE, which the command handles, instead of ending
/// the process (agent transcript, whose reader closing the pipe is how a
/// display says it's done).
pub fn ignore_sigpipe() {
    SIGPIPE_IGNORED.store(true, Ordering::SeqCst);
}

/// Go's runtime on a write to stdout or stderr that failed with EPIPE (its
/// reader gone: `agent list | head -1`): unless the program ignores SIGPIPE,
/// the process dies of SIGPIPE, at once and quietly, as a C program would
/// (the shell sees 141). Rust's runtime ignores SIGPIPE instead, so without
/// this every such write would be an error the command prints, and a command
/// that runs until Ctrl-C (wait --follow, events, watch) would go on with no
/// one reading it.
fn sigpipe() {
    if SIGPIPE_IGNORED.load(Ordering::SeqCst) {
        return;
    }
    // Go's dieFromSignal: the default disposition, unblocked, then raised;
    // exit 2 if the process somehow survives it.
    // SAFETY: plain libc calls on this thread's signal state; nothing after
    // them runs unless the signal didn't end the process.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(libc::SIGPIPE);
    }
    std::process::exit(2);
}

fn epipe_check<T>(res: std::io::Result<T>) -> std::io::Result<T> {
    if let Err(e) = &res
        && e.raw_os_error() == Some(libc::EPIPE)
    {
        sigpipe();
    }
    res
}

/// This process's stdout or stderr as Go's os.Stdout and os.Stderr are:
/// unbuffered, every write made at once, and a write to a closed pipe ends
/// the process as Go's does (see sigpipe).
pub enum GoStdio {
    Stdout,
    Stderr,
}

impl Write for GoStdio {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        epipe_check(match self {
            GoStdio::Stdout => {
                let mut out = std::io::stdout().lock();
                out.write(buf).and_then(|n| out.flush().map(|()| n))
            }
            GoStdio::Stderr => std::io::stderr().write(buf),
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        epipe_check(match self {
            GoStdio::Stdout => std::io::stdout().flush(),
            GoStdio::Stderr => std::io::stderr().flush(),
        })
    }
}

/// "1 agent" or "3 agents".
pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Adds cmd's usage line and first example to err. Every command's argument
/// errors go through it, so a missing argument says what goes there (Go:
/// explainArgErrors wraps each command's Args with it).
pub fn usage_error(cmd: &Cmd, err: anyhow::Error) -> anyhow::Error {
    let mut msg = format!("{err:#}\nUsage: {}", cmd.use_line());
    if let Some(ex) = first_example(cmd) {
        msg.push_str("\nExample: ");
        msg.push_str(&ex);
    }
    anyhow::anyhow!(msg)
}

/// The first command line in cmd's Example.
pub fn first_example(cmd: &Cmd) -> Option<String> {
    cmd.def()
        .doc
        .example
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("agent "))
        .map(str::to_string)
}

/// Adds close flag names, and where to see them all, to an unknown flag error
/// (pflag's words in msg).
pub fn flag_error(cmd: &Cmd, msg: &str) -> anyhow::Error {
    let Some(name) = msg.strip_prefix("unknown flag: --") else {
        if let Some(short) = msg.strip_prefix("unknown shorthand flag: ") {
            return anyhow::anyhow!("{short} (see {} --help for its flags)", cmd.command_path());
        }
        return anyhow::anyhow!("{msg}\nUsage: {}", cmd.use_line());
    };
    let parser = cmd.def().clap_command();
    let names: Vec<String> = parser
        .get_arguments()
        .filter(|a| !a.is_positional())
        .filter_map(|a| a.get_long().map(str::to_string))
        .collect();
    let similar = similar(name, &names);
    if !similar.is_empty() {
        let flags: Vec<String> = similar.iter().map(|s| format!("--{s}")).collect();
        return anyhow::anyhow!("{msg}; did you mean {}?", or_list(&flags));
    }
    anyhow::anyhow!("{msg} (see {} --help for its flags)", cmd.command_path())
}

/// The run of a command that only groups subcommands: alone it prints its help,
/// and an unknown subcommand is an error that suggests close ones, as for the
/// root command (Go: runGroup).
pub fn run_group(ctx: &Ctx, cmd: &Cmd, args: &[String]) -> anyhow::Result<()> {
    match args.first() {
        None => ctx.write_out(&help::render(cmd)),
        Some(word) => Err(unknown_command(cmd, word)),
    }
}

/// The root command's argument rule: any word that isn't a command is an unknown
/// command, suggesting close ones by the same rule as run_group. (Cobra's own
/// suggestions take any command within two edits, so "bogus" suggested logs and
/// bus.)
pub fn root_args(cmd: &Cmd, _m: &ArgMatches, args: &[String]) -> anyhow::Result<()> {
    match args.first() {
        Some(word) => Err(unknown_command(cmd, word)),
        None => Ok(()),
    }
}

/// The error for a word that isn't one of cmd's subcommands: close ones if any,
/// and where to see them all.
pub fn unknown_command(cmd: &Cmd, name: &str) -> anyhow::Error {
    let mut msg = format!(
        "unknown command {} for {}",
        go_quote(name),
        go_quote(&cmd.command_path())
    );
    let names: Vec<String> = cmd
        .def()
        .subs
        .iter()
        .filter(|s| s.is_available() || s.name() == "help")
        .map(|s| s.name().to_string())
        .collect();
    let s = similar(name, &names);
    if !s.is_empty() {
        msg.push_str(&format!("; did you mean {}?", or_list(&s)));
    }
    let mut help = "agent help".to_string();
    if cmd.has_parent() {
        help.push(' ');
        help.push_str(&cmd.path_below_root());
    }
    anyhow::anyhow!("{msg} (see {help})")
}

/// "a", "a or b", or "a, b, or c" (Go: util.OrList(items, nil)).
fn or_list(items: &[String]) -> String {
    util::or_list(items, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::root::{SharedBuf, execute_with, find, tree};

    fn cmd(path: &str) -> Cmd<'static> {
        Cmd::root(tree())
            .lookup(path)
            .unwrap_or_else(|| panic!("no command {path}"))
    }

    /// Runs a command line, returning (stdout, stderr, the error's text).
    fn run_agent(args: &[&str]) -> (String, String, Option<String>) {
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let res = execute_with(&argv, out.writer(), err.writer());
        (
            out.contents(),
            err.contents(),
            res.err().map(|e| format!("{e:#}")),
        )
    }

    #[test]
    fn hint_once_per_command_and_quiet() {
        let (ctx, _, hints) = Ctx::for_test();
        ctx.hint(format_args!("first {}", 1));
        ctx.hint("second");
        assert_eq!(hints.contents(), "hint: first 1\n", "want only the first");

        let (mut ctx, _, hints) = Ctx::for_test();
        ctx.globals.quiet = true;
        ctx.hint("dropped");
        ctx.notice("dropped too");
        assert_eq!(hints.contents(), "", "--quiet printed");
    }

    #[test]
    fn arg_errors_show_usage_and_example() {
        let (_, _, err) = run_agent(&["send", "helper"]);
        let want = "requires at least 2 arg(s), only received 1\nUsage: agent send <name>[@host] <message> [flags]\nExample: agent send researcher";
        let err = err.unwrap_or_default();
        assert!(
            err.starts_with(want),
            "err = {err:?}, want it to start with {want:?}"
        );
        // Subcommands' arguments too.
        let (_, _, err) = run_agent(&["state", "get"]);
        assert!(
            err.unwrap_or_default()
                .contains("Usage: agent state get <name> <key>")
        );
    }

    #[test]
    fn unknown_flag_suggestions() {
        let err = flag_error(&cmd("list"), "unknown flag: --jsn");
        assert_eq!(err.to_string(), "unknown flag: --jsn; did you mean --json?");
        let err = flag_error(&cmd("list"), "unknown flag: --zzzzzz");
        assert!(
            err.to_string().contains("see agent list --help"),
            "err = {err}"
        );
        // And from the parse itself.
        let (_, _, err) = run_agent(&["list", "--jsn"]);
        assert_eq!(
            err.as_deref(),
            Some("unknown flag: --jsn; did you mean --json?")
        );
    }

    #[test]
    fn unknown_subcommand_suggestions() {
        let (ctx, _, _) = Ctx::for_test();
        let err = run_group(&ctx, &cmd("state"), &["sett".to_string()]).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"unknown command "sett" for "agent state"; did you mean set? (see agent help state)"#
        );
    }

    // The root command suggests by the same rule as groups, not cobra's, which
    // took any command within two edits ("bogus": logs, bus).
    #[test]
    fn unknown_command_suggestions() {
        for (word, want) in [
            (
                "bogus",
                r#"unknown command "bogus" for "agent" (see agent help)"#,
            ),
            (
                "lsit",
                r#"unknown command "lsit" for "agent"; did you mean list? (see agent help)"#,
            ),
        ] {
            let (_, stderr, err) = run_agent(&[word]);
            assert_eq!(err.as_deref(), Some(want), "agent {word}");
            assert_eq!(
                stderr,
                format!("Error: {want}\n"),
                "agent {word} prints its error as cobra does"
            );
            let (_, _, err) = run_agent(&["help", word]);
            assert_eq!(err.as_deref(), Some(want), "agent help {word}");
        }
    }

    #[test]
    fn find_walks_past_flags() {
        let line = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        let (cmd, rest) = find(tree(), &line("-q --backend claude state set a b c"));
        assert_eq!(cmd.command_path(), "agent state set");
        assert_eq!(rest, line("-q --backend claude a b c"));
        // An unknown flag takes the next word as its value, as cobra's does.
        let (cmd, _) = find(tree(), &line("--bogus send x y"));
        assert_eq!(cmd.command_path(), "agent");
        let (cmd, _) = find(tree(), &line("friction vote 3"));
        assert_eq!(cmd.command_path(), "agent friction +1");
    }

    #[test]
    fn helpers() {
        assert_eq!(plural(1, "agent"), "1 agent");
        assert_eq!(plural(3, "agent"), "3 agents");
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            similar("stauts", &names(&["status", "statuses", "stop"])),
            names(&["status"])
        );
        assert_eq!(
            similar("stat", &names(&["status", "statuses", "stop"])),
            names(&["status", "statuses"])
        );
        assert!(similar("bogus", &names(&["logs", "bus"])).is_empty());
        assert_eq!(or_list(&names(&["a", "b", "c"])), "a, b, or c");
    }

    #[test]
    fn print_json_reads_as_typed() {
        let (ctx, out, _) = Ctx::for_test();
        ctx.print_json(
            &serde_json::json!({"cmd": "a && b < c", "n": [1, 2], "e": {}, "x": "\u{2028}"}),
        )
        .unwrap();
        assert_eq!(
            out.contents(),
            "{\n  \"cmd\": \"a && b < c\",\n  \"n\": [\n    1,\n    2\n  ],\n  \"e\": {},\n  \"x\": \"\\u2028\"\n}\n"
        );
    }

    // A json.RawMessage is indented where it sits, its text otherwise as it
    // came: what Go 1.27's Encoder with SetIndent("", "  ") and
    // SetEscapeHTML(false) printed for the same values.
    #[test]
    fn print_json_indents_raw_values_as_go() {
        use serde_json::value::RawValue;
        #[derive(Serialize)]
        struct In {
            r: Box<RawValue>,
            l: Vec<Box<RawValue>>,
        }
        #[derive(Serialize)]
        struct Out {
            #[serde(rename = "in")]
            inner: In,
        }
        let raw = |s: &str| RawValue::from_string(s.to_string()).unwrap();
        let v = Out {
            inner: In {
                r: raw(
                    "{ \"a\\\"b\" : [ ] , \"c\":{} ,\"d\":[ \"x\\\\\", -1.5e+3, true,null,{\"e\":\"\u{2028}<&>\"}] }",
                ),
                l: vec![raw(" 5 "), raw("\"s\""), raw("[[]]")],
            },
        };
        let (ctx, out, _) = Ctx::for_test();
        ctx.print_json(&v).unwrap();
        // U+2028 comes out escaped: ESCAPED and the character stand for its
        // six characters.
        let want = r#"{
  "in": {
    "r": {
      "a\"b": [],
      "c": {},
      "d": [
        "x\\",
        -1.5e+3,
        true,
        null,
        {
          "e": "ESCAPED <&>"
        }
      ]
    },
    "l": [
      5,
      "s",
      [
        []
      ]
    ]
  }
}
"#
        .replace("ESCAPED\u{2028}", "\\u2028");
        assert_eq!(out.contents(), want);
    }

    // The rest of output_test.go tests command bodies.

    fn words(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn status_args(target: &str, verbose: bool, json: bool) -> crate::cli::status::Args {
        crate::cli::status::Args {
            json,
            verbose,
            args: words(&[target]),
            ..Default::default()
        }
    }

    fn queue_args(a: &[&str]) -> crate::cli::queue::Args {
        crate::cli::queue::Args {
            from: String::new(),
            args: words(a),
        }
    }

    fn send_args(a: &[&str]) -> crate::cli::send::Args {
        crate::cli::send::Args {
            force: false,
            now: false,
            args: words(a),
            ..Default::default()
        }
    }

    fn err_text(res: anyhow::Result<()>) -> String {
        res.err().map(|e| format!("{e:#}")).unwrap_or_default()
    }

    #[test]
    fn agent_not_found_suggests() {
        use crate::cli::status;
        use crate::cli::testenv::TestEnv;
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        env.create_live_agent(&agent_store::Agent {
            name: "helper".to_string(),
            session_name: "rpc-cli-helper".to_string(),
            backend: "claude".to_string(),
            created_at: agent_store::Time::now(),
            ..Default::default()
        });
        for (name, want) in [
            (
                "helpr",
                r#"agent "helpr" not found on host local; did you mean helper?"#,
            ),
            (
                "rpc-cli-helper",
                r#"agent "rpc-cli-helper" not found on host local: that is the session of agent helper, so use the agent name helper"#,
            ),
            (
                "zzz",
                r#"agent "zzz" not found on host local (2 agents there: agent list)"#,
            ),
        ] {
            let err = err_text(status::run(&env.ctx, &status_args(name, false, false)));
            assert_eq!(err, want, "status {name}");
        }
    }

    // queue's hint about a missing watcher comes from the watch lock.
    #[test]
    fn queue_hints_only_without_a_watcher() {
        use crate::cli::testenv::TestEnv;
        use crate::cli::{queue, watch};
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        env.forbid_bus();

        queue::run(&env.ctx, &queue_args(&["worker", "hi"])).unwrap();
        let hints = env.take_err();
        assert!(
            hints.contains("no watcher is running"),
            "hints = {hints:?}, want the missing-watcher hint"
        );

        let _lock = watch::hold_watch_lock(&env.ctx).unwrap();
        env.ctx
            .hinted
            .store(false, std::sync::atomic::Ordering::Relaxed);
        queue::run(&env.ctx, &queue_args(&["worker", "hi"])).unwrap();
        assert_eq!(env.take_err(), "", "with a watcher running, hints printed");
    }

    // queue tags a message from "user" as send does: a person, or an AI session
    // a person started.
    #[test]
    fn queue_tags_unrecorded_senders() {
        use crate::cli::queue;
        use crate::cli::testenv::TestEnv;
        crate::require_termd!();
        let mut env = TestEnv::setup_cli_private();
        env.forbid_bus();
        env.set_env("AGENT_NAME", "");
        env.set_env("AI_AGENT", "");
        for (claudecode, want) in [
            ("", "[from a person on local] hi"),
            ("1", "[from a Claude session on local] hi"),
        ] {
            env.set_env("CLAUDECODE", claudecode);
            queue::run(&env.ctx, &queue_args(&["worker", "hi"])).unwrap();
            let pending = env.store().get_pending_messages("worker", 100).unwrap();
            let got = &pending.last().expect("a queued message").content;
            assert_eq!(got, want, "CLAUDECODE={claudecode:?}");
        }
    }

    #[test]
    fn send_refuses_exited_agent() {
        use crate::cli::send;
        use crate::cli::testenv::TestEnv;
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        env.service()
            .report(
                "worker",
                agent_service::StatusReport {
                    status: agent_service::AGENT_STATUS_ENDED,
                    ..Default::default()
                },
            )
            .unwrap();
        let err = err_text(send::run(&env.ctx, &send_args(&["worker", "hi"])));
        assert!(
            err.contains("agent worker's session has ended, so the message would be typed into the shell left behind"),
            "err = {err}"
        );
    }

    #[test]
    fn send_refuses_waiting_agent_saying_what_to_do() {
        use crate::cli::send;
        use crate::cli::testenv::TestEnv;
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        let err = err_text(send::run(&env.ctx, &send_args(&["worker", "hi"])));
        let want = "agent worker is waiting on a prompt (permission: Bash: ls), where the message's Enter would answer it: see it with agent status worker -v, then agent approve worker or agent deny worker";
        assert!(err.starts_with(want), "err = {err}");
    }
    #[test]
    fn session_commands_name_the_agents_session() {
        use crate::cli::testenv::TestEnv;
        use crate::cli::{key, keys, session};
        let env = TestEnv::with_termd();
        env.store()
            .create_agent(&agent_store::Agent {
                name: "worker".to_string(),
                session_name: "rpc-cli-worker".to_string(),
                backend: "claude".to_string(),
                created_at: agent_store::Time::now(),
                ..Default::default()
            })
            .unwrap();
        let want = r#"no session "worker", but there is an agent worker: this command takes session names, and its session is rpc-cli-worker"#;
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ctx = &env.ctx;
        let runs: [(&str, anyhow::Result<()>); 3] = [
            (
                "keys",
                keys::run(
                    ctx,
                    &keys::Args {
                        no_enter: false,
                        args: args(&["worker", "hi"]),
                    },
                ),
            ),
            (
                "key",
                key::run(
                    ctx,
                    &key::Args {
                        args: args(&["worker", "Enter"]),
                    },
                ),
            ),
            (
                "kill",
                session::run_session_kill(
                    ctx,
                    &session::SessionKillArgs {
                        args: args(&["worker"]),
                    },
                ),
            ),
        ];
        for (name, res) in runs {
            let err = res.err().map(|e| format!("{e:#}"));
            assert_eq!(err.as_deref(), Some(want), "{name}");
        }
    }
    #[test]
    fn state_errors_and_clear() {
        use crate::cli::state;
        use crate::cli::testenv::TestEnv;
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        let ctx = &env.ctx;

        let err = err_text(state::run_state_set(
            ctx,
            &state::StateSetArgs {
                args: words(&["coordinator", "k", "v"]),
            },
        ));
        assert!(
            err.contains("state belongs to an agent record")
                && err.contains("can't have state: pass results with agent send"),
            "state set for a non-agent: err = {err}"
        );

        state::run_state_set(
            ctx,
            &state::StateSetArgs {
                args: words(&["worker", "progress", "3/5"]),
            },
        )
        .unwrap();
        assert_eq!(env.take_out(), "Set worker.progress = \"3/5\".\n");
        let err = err_text(state::run_state_get(
            ctx,
            &state::StateGetArgs {
                json: false,
                args: words(&["worker", "progres"]),
            },
        ));
        assert_eq!(
            err,
            r#"agent worker has no state "progres"; did you mean progress?"#
        );
        assert_eq!(env.take_out(), "");

        state::run_state_clear(
            ctx,
            &state::StateClearArgs {
                args: words(&["worker"]),
            },
        )
        .unwrap();
        assert_eq!(
            env.take_out(),
            "Cleared 1 key for agent worker (kept 6 \"_\" keys that status reports write).\n"
        );
        let st = env.service().status_info("worker").unwrap();
        assert_eq!(
            st.status,
            agent_service::AGENT_STATUS_WAITING,
            "state clear removed the reported status"
        );
    }
    #[test]
    fn task_get_missing() {
        use crate::cli::task;
        use crate::cli::testenv::TestEnv;
        let env = TestEnv::new();
        env.store()
            .set_task(&agent_store::Task {
                name: "review-auth".to_string(),
                content: "x".to_string(),
                created_at: agent_store::Time::now(),
                ..Default::default()
            })
            .unwrap();
        let get = task::TaskGetArgs {
            json: false,
            args: vec!["review-ath".to_string()],
        };
        let err = task::run_task_get(&env.ctx, &get).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"no task "review-ath"; did you mean review-auth?"#
        );
        assert_eq!(env.out.contents(), "");
        let clear = task::TaskClearArgs {
            args: vec!["nope".to_string()],
        };
        assert!(
            task::run_task_clear(&env.ctx, &clear).is_err(),
            "task clear of a missing task: no error"
        );
    }
    #[test]
    fn report_says_what_it_recorded() {
        use crate::cli::report;
        use crate::cli::testenv::TestEnv;
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        let args = |as_: &str, a: &[&str]| report::Args {
            as_: as_.to_string(),
            end_reason: String::new(),
            event: String::new(),
            hook: false,
            spawn_id: String::new(),
            args: words(a),
        };
        report::run(&env.ctx, &args("worker", &["processing", "running tests"])).unwrap();
        assert_eq!(
            env.take_out(),
            "Reported worker processing (running tests).\n"
        );
        let err = err_text(report::run(&env.ctx, &args("ghost", &["idle"])));
        assert!(
            err.contains(r#"agent "ghost" not found"#),
            "report for a non-agent: err = {err}"
        );
    }

    #[test]
    fn json_output() {
        use crate::cli::testenv::TestEnv;
        use crate::cli::{list, status};
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        env.forbid_bus();

        list::run(
            &env.ctx,
            &list::Args {
                host: String::new(),
                json: true,
                args: vec![],
            },
        )
        .unwrap();
        let out = env.take_out();
        for field in [
            r#""name": "worker""#,
            r#""host": "local""#,
            r#""session": "rpc-cli-worker""#,
        ] {
            assert!(out.contains(field), "list --json lacks {field}: {out}");
        }

        status::run(&env.ctx, &status_args("worker", false, true)).unwrap();
        let out = env.take_out();
        for field in [
            r#""agent": "worker""#,
            r#""status": "waiting""#,
            r#""detail": "permission: Bash: ls""#,
            r#""tool": "Bash""#,
        ] {
            assert!(out.contains(field), "status --json lacks {field}: {out}");
        }
        assert_eq!(env.take_err(), "", "--json printed hints");
    }

    // status -v and --json show the whole tool call a prompt asks about, here
    // and over the bus.
    #[test]
    fn status_shows_whole_permission_input() {
        use crate::cli::status;
        use crate::cli::testenv::TestEnv;
        crate::require_termd!();
        let mut env = TestEnv::setup_cli();
        crate::require_bus!(env.serve_as_h1());
        let command = format!(
            "cat TASK.md; python3 -c \"\nimport wordfreq\nprint('हिन्दी Straße')\"{}",
            " x".repeat(80)
        );
        let input = serde_json::json!({"command": command, "description": "check it"}).to_string();
        env.service()
            .report(
                "worker",
                agent_service::StatusReport {
                    status: agent_service::AGENT_STATUS_WAITING,
                    event: "PermissionRequest".to_string(),
                    tool: "Bash".to_string(),
                    detail: format!("permission: Bash: {command}"),
                    tool_input: input,
                    ..Default::default()
                },
            )
            .unwrap();

        for target in ["worker", "worker@h1"] {
            status::run(&env.ctx, &status_args(target, true, false)).unwrap();
            let out = env.take_out();
            for want in [
                "  input:  command: cat TASK.md; python3 -c \"\n",
                "\n            import wordfreq\n            print('हिन्दी Straße')\" x x",
                "\n          description: check it\n",
            ] {
                assert!(
                    out.contains(want),
                    "status -v {target} lacks {want:?}:\n{out}"
                );
            }
            let at = out.find("input:").unwrap();
            let xs = out[at..].split_whitespace().filter(|f| *f == "x").count();
            assert_eq!(xs, 80, "status -v {target} cut the command:\n{out}");
        }

        status::run(&env.ctx, &status_args("worker@h1", false, true)).unwrap();
        let out = env.take_out();
        let got: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            got["tool_input"]["command"].as_str(),
            Some(command.as_str()),
            "status --json tool_input:\n{out}"
        );
    }

    // wait for idle says, once, that the agent is blocked on a prompt.
    #[test]
    fn wait_hints_when_agent_waits() {
        use crate::cli::testenv::TestEnv;
        use crate::cli::wait;
        crate::require_termd!();
        let env = TestEnv::setup_cli();
        env.no_bus();
        let want = "hint: worker is waiting on a person (a permission prompt, a question, a workspace trust prompt, or a dialog of its CLI's own), so it won't be idle until someone answers: agent status worker -v shows what it's asking (--for idle,waiting returns on prompts)\n";
        // The agent answers once wait has seen it waiting and said so.
        let (hints, svc) = (env.err.clone(), std::sync::Arc::clone(env.service()));
        let answer = std::thread::spawn(move || {
            agent_base::await_::until("wait's hint", || {
                let h = hints.contents();
                let ok = h.contains(want);
                (h, ok)
            });
            svc.report(
                "worker",
                agent_service::StatusReport {
                    status: agent_service::AGENT_STATUS_IDLE,
                    ..Default::default()
                },
            )
            .unwrap();
        });
        let res = wait::run(
            &env.ctx,
            &wait::Args {
                any: false,
                follow: false,
                for_: "idle".to_string(),
                interval: std::time::Duration::from_millis(20),
                next: false,
                // Only so a failure ends; the Go waits forever.
                timeout: std::time::Duration::from_secs(60),
                args: words(&["worker"]),
            },
        );
        answer.join().unwrap();
        res.unwrap();
        assert_eq!(env.take_out(), "worker: idle\n");
        assert_eq!(env.take_err(), want);
    }

    #[test]
    fn wrap_indented() {
        let got = crate::cli::status::wrap_indented(
            "permission: Bash: echo one two three\nsecond line",
            20,
            "  ",
        );
        assert_eq!(
            got,
            "permission: Bash:\n  echo one two three\n  second line"
        );
    }
}

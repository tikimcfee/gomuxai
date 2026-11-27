//! What keeps tripping agents in a dream's batch (docs/DREAMING.md,
//! "Evidence"): failed `agent` commands, failed tool calls and corrections,
//! counted and grouped so a dream can write the start-here note's Tripwires
//! from evidence and apply its three-session rule. The rules (normalizing,
//! grouping, ranking, the digest section) are here and deterministic; the CLI
//! reads the store's command log and the batch's transcripts into the inputs.
//!
//! - A failed command is grouped by its shape (its subcommand words and the
//!   flags it was given, without their values) and the first line of its
//!   error with ids, paths, names, numbers and quoted values normalized away.
//! - A failed tool call is grouped by its tool and the normalized first line
//!   of its error; a shell call by the program it ran (and its subcommand,
//!   for git, cargo, agent and the like) and its exit code.
//! - Failures that are a program's normal answer (grep finding nothing, a
//!   wait timing out, a script `agent run` ran exiting non-zero) are no trip
//!   and are left out.
//! - Groups seen from two or more sessions (or agents) lead, most often seen
//!   first; groups seen from one come after them, marked as such. At most
//!   [`MAX_GROUPS`] per source.
//! - A correction is a person's (or a peer's) message that starts by
//!   correcting the agent ("no", "don't", "stop", "that's not", ...): too
//!   varied to group, so the latest few per session are listed, scrubbed and
//!   clipped.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;

use crate::scrub;
use crate::stale::AgentCommands;

/// The most groups listed per source.
pub const MAX_GROUPS: usize = 15;
/// The most corrections listed per session, and overall.
pub const MAX_CORRECTIONS_PER_SESSION: usize = 3;
pub const MAX_CORRECTIONS: usize = 20;
/// A correction's excerpt, in characters.
pub const CORRECTION_CHARS: usize = 160;
/// A normalized error line, in characters.
pub const ERROR_CHARS: usize = 100;
/// The most sources named on a group's line.
pub const MAX_SOURCES_SHOWN: usize = 4;
/// The digest section's budget, in bytes: lines past it are left out, saying
/// how many.
pub const MAX_BYTES: usize = 6000;

/// A failed `agent` command from the store's command log.
#[derive(Clone, Debug, Default)]
pub struct FailedCommand {
    /// The agent that ran it; "" for a person.
    pub agent: String,
    pub args: Vec<String>,
    pub exit_code: i64,
    pub error: String,
    pub at: DateTime<Utc>,
}

/// A failed tool call in a transcript.
#[derive(Clone, Debug, Default)]
pub struct FailedTool {
    /// The session's `harness:id`.
    pub session: String,
    pub tool: String,
    /// The shell command a shell tool ran; "" for other tools.
    pub command: String,
    /// The tool's result.
    pub error: String,
    pub at: DateTime<Utc>,
}

/// A message into a session from a person or a peer.
#[derive(Clone, Debug, Default)]
pub struct Prompt {
    pub session: String,
    pub text: String,
    pub at: DateTime<Utc>,
}

/// One recurring failure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Group {
    /// What failed: `agent logs --lines`, `Bash cargo test`, `Edit`.
    pub what: String,
    /// The normalized first line of the error.
    pub error: String,
    pub count: usize,
    /// The distinct sessions (`harness:id`) or agents it was seen from.
    pub sources: Vec<String>,
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
    /// The scoped projects it was seen in, when the evidence pools several
    /// (the global dream's); empty otherwise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
}

impl Group {
    /// Seen from one session (or agent) only.
    pub fn single(&self) -> bool {
        self.sources.len() < 2
    }
}

/// A message that corrected the agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Correction {
    pub session: String,
    pub at: DateTime<Utc>,
    pub excerpt: String,
}

/// What kept tripping agents in a batch.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Evidence {
    /// Counted from here on.
    pub since: Option<DateTime<Utc>>,
    /// Failed `agent` commands counted (noise left out), and their groups.
    pub commands_failed: usize,
    pub commands: Vec<Group>,
    /// Transcripts read, failed tool calls counted, and their groups.
    pub sessions_read: usize,
    pub tools_failed: usize,
    pub tools: Vec<Group>,
    pub corrections: Vec<Correction>,
}

impl Evidence {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.tools.is_empty() && self.corrections.is_empty()
    }
}

// Normalizing.

static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</?[A-Za-z_][A-Za-z0-9_-]*>").unwrap());
static DQUOTE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""[^"]*""#).unwrap());
static BQUOTE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`]*`").unwrap());
static SQUOTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(^|[\s(=:,\[])'[^'\n]*'"#).unwrap());
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").unwrap()
});
static HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]*[0-9][0-9a-f]*\b").unwrap());
static PATH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\s,;()]*/[^\s,;()]*").unwrap());
/// A dashed name (an agent's, a host's): not a flag's (`--dry-run`).
static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(^|[^\w-])[a-z0-9_]+(?:-[a-z0-9_]+)+\b").unwrap());
static NUM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
static NAME_RUN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<name>(?:(?:,\s*(?:or\s+|and\s+)?|\s+(?:or|and)\s+)<name>)+").unwrap()
});

/// The first line of `error` with what varies between occurrences replaced:
/// harness tags (`<tool_use_error>`) and a leading `Error:` dropped, quoted
/// values as `"…"`, paths and URLs as `<path>`, uuids and hex ids as `<id>`,
/// dashed names (agents, hosts; not flags) as `<name>` and a list of them as
/// `<names>`, numbers as `N`; whitespace collapsed, clipped to
/// [`ERROR_CHARS`]. A line that
/// looks like it holds a credential is withheld.
pub fn normalize_error(error: &str) -> String {
    let untagged = TAG_RE.replace_all(error, "");
    let line = untagged
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line
        .strip_prefix("Error:")
        .or_else(|| line.strip_prefix("error:"))
        .unwrap_or(line)
        .trim();
    // Secrets redacted in place; the rest is the error's shape.
    let line = scrub::redact(line);
    let s = DQUOTE_RE.replace_all(&line, "\"…\"");
    let s = BQUOTE_RE.replace_all(&s, "`…`");
    let s = SQUOTE_RE.replace_all(&s, "$1'…'");
    let s = PATH_RE.replace_all(&s, "<path>");
    let s = UUID_RE.replace_all(&s, "<id>");
    let s = HEX_RE.replace_all(&s, |c: &regex::Captures| {
        let m = &c[0];
        if m.len() >= 7 && m.chars().any(|ch| ch.is_ascii_alphabetic()) {
            "<id>".to_string()
        } else {
            m.to_string()
        }
    });
    let s = NAME_RE.replace_all(&s, "$1<name>");
    let s = NAME_RUN_RE.replace_all(&s, "<names>");
    let s = NUM_RE.replace_all(&s, "N");
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    scrub::clip(&flat, ERROR_CHARS)
}

/// A command's shape: its subcommand words (by `tree`; without one, the
/// leading lowercase words, at most two) and the flags it was given, sorted,
/// without their values. `-q`/`--quiet` is left out.
pub fn command_shape(args: &[String], tree: Option<&AgentCommands>) -> String {
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    // Root flags before the subcommand.
    while i < args.len() && args[i].starts_with('-') {
        i += 1;
    }
    match tree {
        Some(t) => {
            let mut path = String::new();
            while i < args.len() {
                let Some(cmd) = t.commands.get(&path) else {
                    break;
                };
                let a = args[i].as_str();
                if a.starts_with('-') || !cmd.subs.contains(a) {
                    break;
                }
                words.push(a);
                path = if path.is_empty() {
                    a.to_string()
                } else {
                    format!("{path} {a}")
                };
                i += 1;
            }
        }
        None => {
            while i < args.len() && words.len() < 2 && is_word(&args[i]) {
                words.push(&args[i]);
                i += 1;
            }
        }
    }
    let mut flags: BTreeSet<&str> = BTreeSet::new();
    for a in args {
        if a == "--" {
            break;
        }
        // A flag is "-x" or "--xy", never "-" or "-5".
        let second = a.chars().nth(1);
        if !a.starts_with('-') || second.is_none_or(|c| c.is_ascii_digit() || c.is_whitespace()) {
            continue;
        }
        let f = a.split_once('=').map_or(a.as_str(), |(f, _)| f);
        if f != "-q" && f != "--quiet" {
            flags.insert(f);
        }
    }
    let mut out = String::from("agent");
    for w in words.iter().copied().chain(flags) {
        out.push(' ');
        out.push_str(w);
    }
    out
}

fn is_word(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Programs whose second word is a subcommand worth keeping.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "agent",
    "git",
    "cargo",
    "make",
    "npm",
    "pnpm",
    "yarn",
    "go",
    "uv",
    "gh",
    "systemctl",
    "docker",
    "kubectl",
    "pip",
    "rustup",
    "brew",
    "g3d",
];

/// Words that only set up a shell segment: the segment's program follows.
const SETUP_WORDS: &[&str] = &[
    "cd", "export", "set", "source", ".", "pushd", "popd", "true",
];

/// Wrappers whose program is the next word.
const WRAPPERS: &[&str] = &["env", "sudo", "time", "nice", "nohup", "command", "exec"];

/// Shell words a program follows (`if grep ...`, `until agent wait ...`).
const SHELL_LEADS: &[&str] = &["if", "elif", "while", "until", "then", "do", "else", "!"];

/// Shell words whose segment runs no program of its own (`for x in a b`), or
/// only tests something (`[ -f x ] && ...`).
const SHELL_SKIPS: &[&str] = &[
    "for", "case", "done", "fi", "esac", "function", "{", "}", "[", "[[", "test",
];

/// The program a shell command ran, and its subcommand for git, cargo, agent
/// and the like: `cargo test`, `agent send`, `ls`. The first segment (lines
/// split at `&&`, `||`, `;`, `|`, comments dropped) that doesn't only set
/// things up (`cd`, `export`, `for x in ...`) counts; `VAR=x` prefixes,
/// wrappers (`env`, `timeout 5`) and the shell's own words (`if`, `until`)
/// are skipped, and a path is cut to its last part. "" when nothing is left.
pub fn shell_shape(command: &str) -> String {
    let joined = command.replace("\\\n", " ");
    let segments = joined
        .lines()
        .map(|l| l.split(" #").next().unwrap_or(l))
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split(['|', ';', '&']));
    for seg in segments {
        let mut words = seg.split_whitespace().peekable();
        // VAR=x prefixes, wrappers and the shell's own words.
        let mut prog = "";
        while let Some(w) = words.next() {
            if (w.contains('=') && !w.starts_with('-')) || WRAPPERS.contains(&w) {
                continue;
            }
            if SHELL_LEADS.contains(&w) {
                continue;
            }
            if SHELL_SKIPS.contains(&w) {
                break;
            }
            if w == "timeout" {
                words.next_if(|n| n.chars().next().is_some_and(|c| c.is_ascii_digit()));
                continue;
            }
            prog = w;
            break;
        }
        let prog = prog.trim_matches(['(', ')', '{', '}', '"', '\'']);
        let prog = prog.rsplit('/').next().unwrap_or(prog);
        if prog.is_empty() || SETUP_WORDS.contains(&prog) {
            continue;
        }
        let mut out = prog.to_string();
        if SUBCOMMAND_PROGRAMS.contains(&prog) {
            let mut skip_value = false;
            for w in words {
                if skip_value {
                    skip_value = false;
                    continue;
                }
                if w == "-C" || w == "--manifest-path" || w == "--git-dir" {
                    skip_value = true;
                    continue;
                }
                if w.starts_with('-') || w.starts_with('+') {
                    continue;
                }
                if is_word(w) {
                    out.push(' ');
                    out.push_str(w);
                }
                break;
            }
        }
        return out;
    }
    String::new()
}

/// A shell tool's exit: Claude's `Exit code 1`, agy's `Command exited with
/// code 1.`
static EXIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:Exit code|Command exited with code) (-?\d+)").unwrap());

/// Results a harness marks failed that aren't: agy's note that a long output
/// was cut.
const NOT_FAILURES: &[&str] = &["The tool's output was truncated"];

/// Exit codes that are a program's normal answer, not a trip: (program
/// shape, code).
const NORMAL_EXITS: &[(&str, i64)] = &[
    ("grep", 1),
    ("rg", 1),
    ("egrep", 1),
    ("diff", 1),
    ("cmp", 1),
    ("test", 1),
    ("[", 1),
    ("[[", 1),
    ("pgrep", 1),
    ("agent wait", 2),
];

/// Builds and tests: their failing is the work going on, not a trip (what
/// failed is in their output, which the dream reads in the session). A shape
/// counts when it is one of these or starts with one and a space.
const DEV_LOOP: &[&str] = &[
    "cargo",
    "make",
    "go build",
    "go test",
    "go vet",
    "npm test",
    "npm run",
    "pnpm",
    "yarn",
    "pytest",
    "tsc",
    "rustc",
    "gcc",
    "clang",
    "swift",
    "xcodebuild",
    "ninja",
    "cmake",
];

fn is_dev_loop(shape: &str) -> bool {
    DEV_LOOP
        .iter()
        .any(|d| shape == *d || shape.strip_prefix(d).is_some_and(|r| r.starts_with(' ')))
}

/// What a failed tool call is grouped by: (what, error). None for a failure
/// that is a program's normal answer (grep finding nothing), or a build or
/// test failing.
pub fn tool_key(tool: &str, command: &str, error: &str) -> Option<(String, String)> {
    let shape = shell_shape(command);
    let first = error.trim_start();
    if NOT_FAILURES.iter().any(|p| first.starts_with(p)) {
        return None;
    }
    if !command.is_empty() {
        let what = if shape.is_empty() {
            tool.to_string()
        } else {
            format!("{tool} {shape}")
        };
        if let Some(c) = EXIT_RE.captures(first) {
            let code: i64 = c[1].parse().unwrap_or_default();
            if NORMAL_EXITS.contains(&(shape.as_str(), code)) || is_dev_loop(&shape) {
                return None;
            }
            return Some((what, format!("exit {code}")));
        }
        return Some((what, normalize_error(error)));
    }
    Some((tool.to_string(), normalize_error(error)))
}

/// Whether a failed `agent` command is a program's normal answer rather than
/// a trip: a wait that timed out (exit 2), or `agent run` passing on its
/// script's exit code.
pub fn command_is_noise(shape: &str, c: &FailedCommand) -> bool {
    let sub = shape.split_whitespace().nth(1).unwrap_or("");
    (sub == "wait" && c.exit_code == 2) || (sub == "run" && c.error.starts_with("job "))
}

/// How many failed commands one agent may run within [`BURST_WINDOW_MS`]
/// before they count as a script's (a test suite, a loop) rather than its
/// own calls.
pub const BURST_COUNT: usize = 3;
pub const BURST_WINDOW_MS: i64 = 1000;

/// For each command, whether it is part of a burst: [`BURST_COUNT`] or more
/// failed commands by the same agent within [`BURST_WINDOW_MS`] of each other.
pub fn bursts(cmds: &[FailedCommand]) -> Vec<bool> {
    let mut idx: Vec<usize> = (0..cmds.len()).collect();
    idx.sort_by(|&a, &b| {
        cmds[a]
            .agent
            .cmp(&cmds[b].agent)
            .then(cmds[a].at.cmp(&cmds[b].at))
    });
    let mut out = vec![false; cmds.len()];
    let mut start = 0;
    for end in 0..idx.len() {
        let e = &cmds[idx[end]];
        while cmds[idx[start]].agent != e.agent
            || (e.at - cmds[idx[start]].at).num_milliseconds() > BURST_WINDOW_MS
        {
            start += 1;
        }
        if end + 1 - start >= BURST_COUNT {
            for &i in &idx[start..=end] {
                out[i] = true;
            }
        }
    }
    out
}

// Grouping.

#[derive(Default)]
struct Acc {
    count: usize,
    sources: BTreeSet<String>,
    first: Option<DateTime<Utc>>,
    last: Option<DateTime<Utc>>,
}

impl Acc {
    fn add(&mut self, source: &str, at: DateTime<Utc>) {
        self.count += 1;
        self.sources.insert(source.to_string());
        self.first = Some(self.first.map_or(at, |f| f.min(at)));
        self.last = Some(self.last.map_or(at, |l| l.max(at)));
    }
}

/// The groups, ranked: seen from two or more sources first, then by count,
/// sources and recency; at most [`MAX_GROUPS`].
fn ranked(acc: BTreeMap<(String, String), Acc>) -> Vec<Group> {
    let mut groups: Vec<Group> = acc
        .into_iter()
        .map(|((what, error), a)| Group {
            what,
            error,
            count: a.count,
            sources: a.sources.into_iter().collect(),
            first: a.first.unwrap_or_default(),
            last: a.last.unwrap_or_default(),
            projects: Vec::new(),
        })
        .collect();
    groups.sort_by(|a, b| {
        a.single()
            .cmp(&b.single())
            .then(b.count.cmp(&a.count))
            .then(b.sources.len().cmp(&a.sources.len()))
            .then(b.last.cmp(&a.last))
            .then(a.what.cmp(&b.what))
            .then(a.error.cmp(&b.error))
    });
    groups.truncate(MAX_GROUPS);
    groups
}

/// Sets each group's projects: those its sources (sessions or agents) were
/// seen in, by `projects_of`. For evidence pooled across projects, where a
/// trip seen in two or more is the machine's.
pub fn set_projects(
    groups: &mut [Group],
    projects_of: &BTreeMap<String, std::collections::BTreeSet<String>>,
) {
    for g in groups {
        let mut p = std::collections::BTreeSet::new();
        for s in &g.sources {
            if let Some(of) = projects_of.get(s) {
                p.extend(of.iter().filter(|d| !d.is_empty()).cloned());
            }
        }
        g.projects = p.into_iter().collect();
    }
}

/// Failed commands grouped by shape and error, noise left out. Returns the
/// groups and how many failures were counted. A person's commands count
/// under "(person)".
pub fn group_commands(cmds: &[FailedCommand], tree: Option<&AgentCommands>) -> (Vec<Group>, usize) {
    let mut acc: BTreeMap<(String, String), Acc> = BTreeMap::new();
    let mut n = 0;
    let burst = bursts(cmds);
    for (c, burst) in cmds.iter().zip(burst) {
        let shape = command_shape(&c.args, tree);
        if burst || command_is_noise(&shape, c) {
            continue;
        }
        n += 1;
        let source = if c.agent.is_empty() {
            "(person)"
        } else {
            &c.agent
        };
        acc.entry((shape, normalize_error(&c.error)))
            .or_default()
            .add(source, c.at);
    }
    (ranked(acc), n)
}

/// Failed tool calls grouped by tool and error, noise left out. Returns the
/// groups and how many failures were counted.
pub fn group_tools(tools: &[FailedTool]) -> (Vec<Group>, usize) {
    let mut acc: BTreeMap<(String, String), Acc> = BTreeMap::new();
    let mut n = 0;
    for t in tools {
        let Some(key) = tool_key(&t.tool, &t.command, &t.error) else {
            continue;
        };
        n += 1;
        acc.entry(key).or_default().add(&t.session, t.at);
    }
    (ranked(acc), n)
}

// Corrections.

static ENVELOPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\[from ([^\]]+)\]\s*").unwrap());
static ARRIVED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:A message|\d+ messages) arrived while you were working[^:]*:\s*").unwrap()
});
static GOMUXAI_MARK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*…?\s*\[gomuxai message #\d+[^\]]*\]\s*$").unwrap());
/// Words a correction starts with. "No" only as a word of its own ("no,"
/// "no!" "no -"), not "no pending edits"; not "nope", which as often agrees
/// ("nope, all good").
static START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:no(?:\s*[,.!:;—-]|\s*$)|(?:please\s+)?(?:don'?t|do not|stop)\b|(?:wrong|actually|instead|again|not that|that'?s not|that is not|i said|i told you|you didn'?t)\b)",
    )
    .unwrap()
});
/// Words that make a message a correction within its first [`NEAR_CHARS`]:
/// only those that say the agent got something wrong ("don't" there is as
/// often an instruction or a report).
static NEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:stop|wrong|that'?s not|that is not|i said|i told you|not what i|you didn'?t)\b",
    )
    .unwrap()
});

/// How far into a message a correction's words may come.
const NEAR_CHARS: usize = 60;

/// The messages a prompt carries, envelopes dropped: gomuxai's `[from ...]`
/// tag (and the sender's name repeated after it), the held-message preamble
/// (`A message arrived while you were working:`), several held messages
/// split apart, and a long message's `[gomuxai message #N, ...]` marker.
pub fn messages(text: &str) -> Vec<String> {
    let text = ARRIVED_RE.replace(text, "");
    let mut out = Vec::new();
    // Held messages come one after another, each with its own tag.
    let mut parts: Vec<&str> = Vec::new();
    let mut rest: &str = &text;
    loop {
        let next = rest.match_indices("\n[from ").map(|(i, _)| i).next();
        match next {
            Some(i) => {
                parts.push(&rest[..i]);
                rest = &rest[i + 1..];
            }
            None => {
                parts.push(rest);
                break;
            }
        }
    }
    for p in parts {
        let mut m = p.trim().to_string();
        if let Some(c) = ENVELOPE_RE.captures(&m) {
            let sender = c[1].split('@').next().unwrap_or("").to_string();
            m = m[c.get(0).unwrap().end()..].to_string();
            if let Some(r) = m.strip_prefix(&format!("{sender}:")) {
                m = r.trim_start().to_string();
            }
        }
        let m = GOMUXAI_MARK_RE.replace(&m, "").trim().to_string();
        if !m.is_empty() {
            out.push(m);
        }
    }
    out
}

/// Whether a message corrects the agent: it starts with a correcting word
/// ("no", "don't", "stop", "wrong", "actually", "instead", "again", "that's
/// not", "I said", ...), or has one of those within its first
/// [`NEAR_CHARS`] characters. Slash commands never do.
pub fn is_correction(message: &str) -> bool {
    let m = message.trim_start();
    if m.starts_with('/') {
        return false;
    }
    if START_RE.is_match(m) {
        return true;
    }
    let head: String = m.chars().take(NEAR_CHARS).collect();
    NEAR_RE.is_match(&head)
}

/// The corrections among prompts: the latest [`MAX_CORRECTIONS_PER_SESSION`]
/// per session, newest first, at most [`MAX_CORRECTIONS`] in all; each
/// scrubbed and clipped to [`CORRECTION_CHARS`], on one line.
pub fn corrections(prompts: &[Prompt]) -> Vec<Correction> {
    let mut by_session: BTreeMap<&str, Vec<Correction>> = BTreeMap::new();
    for p in prompts {
        for m in messages(&p.text) {
            if !is_correction(&m) {
                continue;
            }
            // Kept whole but for its secrets, then cut to one line ("…"
            // marks a cut; the session id says where to read it all).
            let ex = scrub::for_reading(&m, CORRECTION_CHARS * 4);
            let ex = scrub::clip(
                &ex.split_whitespace().collect::<Vec<_>>().join(" "),
                CORRECTION_CHARS,
            );
            if ex.is_empty() {
                continue;
            }
            by_session.entry(&p.session).or_default().push(Correction {
                session: p.session.clone(),
                at: p.at,
                excerpt: ex,
            });
        }
    }
    let mut out: Vec<Correction> = Vec::new();
    for (_, mut cs) in by_session {
        cs.sort_by_key(|c| std::cmp::Reverse(c.at));
        cs.truncate(MAX_CORRECTIONS_PER_SESSION);
        out.extend(cs);
    }
    out.sort_by(|a, b| b.at.cmp(&a.at).then(a.session.cmp(&b.session)));
    out.truncate(MAX_CORRECTIONS);
    out
}

// The digest's section.

fn day(t: DateTime<Utc>) -> String {
    t.format("%m-%d").to_string()
}

fn group_line(g: &Group, noun: &str) -> String {
    let n = g.sources.len();
    let mut names: Vec<String> = g
        .sources
        .iter()
        .take(MAX_SOURCES_SHOWN)
        .map(|s| format!("`{s}`"))
        .collect();
    if n > MAX_SOURCES_SHOWN {
        names.push(format!("+{} more", n - MAX_SOURCES_SHOWN));
    }
    let when = if day(g.first) == day(g.last) {
        day(g.first)
    } else {
        format!("{} to {}", day(g.first), day(g.last))
    };
    let mut line = format!(
        "- {}× `{}`: {}\n  {n} {noun}{} ({when}): {}\n",
        g.count,
        g.what,
        g.error,
        if n == 1 { "" } else { "s" },
        names.join(", ")
    );
    if !g.projects.is_empty() {
        let p = g.projects.len();
        let _ = writeln!(
            line,
            "  in {p} project{}: {}",
            if p == 1 { "" } else { "s" },
            g.projects
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    line
}

/// Lines added to `b` while the section stays within `budget` bytes; the
/// rest are counted in a closing line.
fn push_bounded(b: &mut String, lines: &[String], budget: usize) {
    for (i, l) in lines.iter().enumerate() {
        if b.len() + l.len() > budget {
            let _ = writeln!(b, "- ({} more left out for space)", lines.len() - i);
            return;
        }
        b.push_str(l);
    }
}

fn groups_lines(groups: &[Group], noun: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut said_single = false;
    for g in groups {
        if g.single() && !said_single {
            said_single = true;
            lines.push(format!("- Seen in one {noun} only:\n"));
        }
        lines.push(group_line(g, noun));
    }
    lines
}

/// The digest's `## evidence` section, at most about [`MAX_BYTES`]; "" when
/// there is nothing in it.
pub fn format(e: &Evidence) -> String {
    if e.is_empty() {
        return String::new();
    }
    let mut b = String::from("\n## evidence\n\n");
    let since = e
        .since
        .map(|t| format!(" since {}", t.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_default();
    let _ = writeln!(
        b,
        "What kept tripping agents in this batch{since}, counted, for the start-here \
         note's Tripwires. A trip seen in three or more sessions is a candidate; read \
         those sessions before you write it, and cite them. Counts are of failures, \
         not of distinct problems: check a group is one problem before you trust its \
         count."
    );
    // Each source gets a third of the budget, and what it leaves over goes on.
    let third = MAX_BYTES / 3;
    let _ = writeln!(
        b,
        "\n### failed agent commands\n\n{} failed (waits timing out and scripts `agent run` \
         ran failing left out), by command shape and error:\n",
        e.commands_failed
    );
    if e.commands.is_empty() {
        b.push_str("- none\n");
    }
    let budget = b.len() + third;
    push_bounded(&mut b, &groups_lines(&e.commands, "agent"), budget);

    let _ = writeln!(
        b,
        "\n### failed tool calls\n\n{} failed in {} session{} (builds and tests failing, \
         grep finding nothing and the like left out), by tool and error:\n",
        e.tools_failed,
        e.sessions_read,
        if e.sessions_read == 1 { "" } else { "s" }
    );
    if e.tools.is_empty() {
        b.push_str("- none\n");
    }
    let budget = (b.len() + third).max(2 * MAX_BYTES / 3);
    push_bounded(&mut b, &groups_lines(&e.tools, "session"), budget);

    let _ = writeln!(
        b,
        "\n### corrections\n\nMessages that corrected an agent, newest first (at most {} \
         per session), scrubbed and clipped:\n",
        MAX_CORRECTIONS_PER_SESSION
    );
    if e.corrections.is_empty() {
        b.push_str("- none\n");
    }
    let lines: Vec<String> = e
        .corrections
        .iter()
        .map(|c| {
            format!(
                "- `{}` {}: {}\n",
                c.session,
                c.at.format("%m-%d %H:%M"),
                c.excerpt
            )
        })
        .collect();
    push_bounded(&mut b, &lines, MAX_BYTES);
    b
}

#[cfg(test)]
#[path = "evidence_test.rs"]
mod evidence_test;

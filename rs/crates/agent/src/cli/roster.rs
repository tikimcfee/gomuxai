//! `agent roster` and its subcommands (no Go file: new in the Rust build;
//! docs/ROSTER.md). The rules are agent_roster's; this file parses flags and
//! prints. This host only for now: reads over the bus come later.
use std::fmt::Write as _;

use agent_roster::{
    EntryChanges, EntryShow, EntryView, HostRoster, Key, NoteCounts, NoteInput, Pick, PickOptions,
    TASK_KINDS,
};
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::friction::{friction_caller, stamp};
use super::root::Ctx;
use super::sessions::tab_align;

// `agent roster`

pub const DOC: Doc = Doc {
    use_line: "roster",
    short: "Which harness and model to spawn: this host's roster, its notes, and pick",
    long: r#"Roster is where to look before a spawn: which harness and model this host
can run, what each is good at, how fast and costly it is, whether it is
available now, and what past runs showed. It lists this host's entries:

  ENTRY                    AVAILABLE              SPEED  COST  TASKS      NOTES
  claude/claude-haiku-5-5  yes                    fast   low   edit,test  3 good (2 sessions)
  agy/gemini-3-pro         no: agy isn't on PATH  slow   high  review     -

An entry is HARNESS/MODEL[@EFFORT]: the backend spawn takes, the model
spawn --model takes (empty for the harness's default: claude/), and an
effort for harnesses that take one (the same model at another effort is
its own entry). It is available when it is enabled and its harness's CLI
is on this host's PATH.

Notes are evaluations, one per run, each citing the session it came from:
NOTES counts them by verdict and by distinct session, so you can see
whether a claim rests on one run or on five. A note another supersedes
isn't counted.

Task kinds: edit (small, well-specified change), bugfix, feature
(multi-file), long-horizon (hours, many steps), review, research (reading
and summarising), design, test, kernel (performance-critical low-level
code), chat (quick question).

roster (or roster list), show and pick only read; note only adds a note.
set, enable, disable and remove change the entries.

--json prints this host's roster as one JSON document: {"host", "entries":
[...], "unlisted": {...}}, each entry with its fields, "key", "available",
"unavailable" (why not), "notes" (counts by verdict) and "by_task"."#,
    example: r#"  agent roster
  agent roster pick --task edit
  agent roster show claude/claude-opus-5-5
  agent roster set claude/claude-haiku-5-5 --speed fast --cost low --tasks edit,test --good-at "small precise edits"
  agent roster note claude/claude-haiku-5-5 --task edit --verdict good --session claude:3f3a9c "did the rename across 12 files cleanly""#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster` and `agent roster list`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "json", help = "Print the roster as one JSON document")]
    pub json: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    Def {
        subs: vec![
            roster_list_def(),
            roster_show_def(),
            roster_pick_def(),
            roster_note_def(),
            roster_set_def(),
            roster_enable_def(),
            roster_disable_def(),
            roster_remove_def(),
        ],
        ..leaf!(DOC, Args, run)
    }
}

/// This host's roster: what agent roster prints, and what a host will
/// serve its peers.
pub(crate) fn local_roster(ctx: &Ctx) -> anyhow::Result<HostRoster> {
    agent_roster::host_view(ctx.store()?, &ctx.cfg().host)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let view = local_roster(ctx)?;
    if args.json {
        return ctx.print_json(&view);
    }
    if view.entries.is_empty() && view.unlisted.is_empty() {
        ctx.write_out(&format!("The roster on {} is empty.\n", view.host))?;
        ctx.hint(
            "agent roster set HARNESS/MODEL --tasks KINDS --speed ... --cost ... adds an entry",
        );
        return Ok(());
    }
    ctx.write_out(&render_roster(&view))?;
    if !view.entries.iter().any(|e| e.available) && !view.entries.is_empty() {
        ctx.hint("no entry is available: agent roster enable ENTRY, or put its CLI on PATH");
    }
    Ok(())
}

/// The roster as a table, then the notes on harnesses and models it has
/// no entry for.
fn render_roster(view: &HostRoster) -> String {
    let mut out = String::new();
    if !view.entries.is_empty() {
        let mut rows = vec![vec![
            "ENTRY".to_string(),
            "AVAILABLE".into(),
            "SPEED".into(),
            "COST".into(),
            "TASKS".into(),
            "NOTES".into(),
        ]];
        for e in &view.entries {
            rows.push(vec![
                e.key.clone(),
                availability(e),
                dash(&e.entry.speed),
                dash(&e.entry.cost),
                dash(&e.entry.tasks.join(",")),
                counts(&e.notes),
            ]);
        }
        out.push_str(&tab_align(&rows, 2));
    }
    if !view.unlisted.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Notes on harnesses and models with no entry:\n");
        for (key, c) in &view.unlisted {
            let _ = writeln!(out, "  {key}  {}", counts(c));
        }
    }
    out
}

fn availability(e: &EntryView) -> String {
    if e.available {
        "yes".into()
    } else {
        format!("no: {}", e.unavailable)
    }
}

fn dash(s: &str) -> String {
    if s.is_empty() { "-".into() } else { s.into() }
}

/// Note counts in words: "3 good (2 sessions), 1 bad", "-" for none.
fn counts(c: &NoteCounts) -> String {
    let mut parts = Vec::new();
    for (name, t) in [("good", &c.good), ("mixed", &c.mixed), ("bad", &c.bad)] {
        if t.notes == 0 {
            continue;
        }
        if t.sessions == t.notes {
            parts.push(format!("{} {name}", t.notes));
        } else {
            let s = if t.sessions == 1 { "session" } else { "sessions" };
            parts.push(format!("{} {name} ({} {s})", t.notes, t.sessions));
        }
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(", ")
    }
}

// `agent roster list`

pub const ROSTER_LIST: Doc = Doc {
    use_line: "list",
    short: "List this host's roster (what agent roster prints)",
    long: r#"List prints this host's roster, as "agent roster" does: each entry, whether
it is available, its speed, cost and tasks, and its notes counted by verdict
and session."#,
    example: r#"  agent roster list
  agent roster list --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

pub fn roster_list_def() -> Def {
    leaf!(ROSTER_LIST, Args, run)
}

/// The entry an ENTRY argument names.
fn entry_key(word: &str) -> anyhow::Result<Key> {
    Key::parse(word)
}

// `agent roster show`

pub const ROSTER_SHOW: Doc = Doc {
    use_line: "show HARNESS/MODEL[@EFFORT]",
    short: "Show one roster entry with all its notes",
    long: r#"Show prints one entry: whether it is available, its tasks, speed, cost,
limits, what it is good at and what to avoid, who set it, and its notes
counted. Then every note on it, oldest first: its task kind, verdict, who
wrote it, the session it cites, what happened and why. A superseded note is
listed, marked, and not counted.

It also shows the notes on a harness and model that have no entry.

--json prints {"host", "key", "entry", "notes": [...]}."#,
    example: r#"  agent roster show claude/claude-opus-5-5
  agent roster show claude/claude-haiku-5-5@low --json
  agent roster show agy/"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster show`.
#[derive(clap::Args, Debug, Clone)]
pub struct RosterShowArgs {
    #[arg(long = "json", help = "Print the entry and its notes as one JSON document")]
    pub json: bool,
    /// The positional arguments, checked against ROSTER_SHOW.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn roster_show_def() -> Def {
    leaf!(ROSTER_SHOW, RosterShowArgs, run_roster_show)
}

pub fn run_roster_show(ctx: &Ctx, args: &RosterShowArgs) -> anyhow::Result<()> {
    let key = entry_key(&args.args[0])?;
    let show = agent_roster::show(ctx.store()?, &ctx.cfg().host, &key)?;
    if args.json {
        return ctx.print_json(&show);
    }
    ctx.write_out(&render_show(&show))
}

fn render_show(s: &EntryShow) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} on {}", s.key, s.host);
    match &s.entry {
        None => out.push_str("  no entry: only notes (agent roster set adds one)\n"),
        Some(e) => {
            let _ = writeln!(out, "  available: {}", availability(e));
            let _ = writeln!(out, "  tasks:     {}", dash(&e.entry.tasks.join(", ")));
            let _ = writeln!(
                out,
                "  speed:     {}   cost: {}",
                dash(&e.entry.speed),
                dash(&e.entry.cost)
            );
            for (label, v) in [
                ("good at:  ", &e.entry.good_at),
                ("avoid:    ", &e.entry.avoid),
                ("limits:   ", &e.entry.limits),
            ] {
                if !v.is_empty() {
                    let _ = writeln!(out, "  {label} {v}");
                }
            }
            let _ = writeln!(
                out,
                "  set by:    {}, {}",
                dash(&e.entry.set_by),
                stamp(&e.entry.updated_at)
            );
            let _ = writeln!(out, "  notes:     {}", counts(&e.notes));
        }
    }
    if s.notes.is_empty() {
        out.push_str("\nNo notes yet (agent roster note adds one).\n");
        return out;
    }
    out.push_str("\nNotes:\n");
    for nv in &s.notes {
        let n = &nv.note;
        let mut head = format!(
            "  #{} {}  {}  {}  by {}  from {}",
            n.id,
            stamp(&n.created_at),
            n.task,
            n.verdict,
            n.by,
            n.source
        );
        if n.supersedes != 0 {
            let _ = write!(head, "  supersedes #{}", n.supersedes);
        }
        if nv.superseded_by != 0 {
            let _ = write!(head, "  (superseded by #{})", nv.superseded_by);
        }
        out.push_str(&head);
        out.push('\n');
        for line in n.text.lines() {
            let _ = writeln!(out, "      {line}");
        }
        if !n.why.is_empty() {
            let _ = writeln!(out, "      why: {}", n.why);
        }
    }
    out
}

// `agent roster pick`

pub const ROSTER_PICK: Doc = Doc {
    use_line: "pick --task KIND",
    short: "Pick the best available harness and model for a task, as a spawn line",
    long: r#"Pick ranks this host's available entries for a task kind and prints the
best as a ready spawn line, then the next two, each with one line of why.
It never spawns: replace NAME and run the line yourself.

Ranking: an entry whose tasks include the kind scores 2; each distinct
session with a good note on that kind adds 1, each with a bad one takes 1
away (superseded notes don't count). --prefer breaks ties: fast (by speed),
cheap (by cost) or strong (dearest first). Then more good sessions win.

An entry at an effort is picked like any other; spawn has no --effort yet,
so the why line says which effort the notes were about.

Task kinds: edit, bugfix, feature, long-horizon, review, research, design,
test, kernel, chat.

--json prints {"task", "prefer", "pick", "runners_up": [...]}, each with
"host", "key", "harness", "model", "effort", "score", "command" and "why"."#,
    example: r#"  agent roster pick --task edit
  agent roster pick --task long-horizon --prefer fast
  agent roster pick --task review --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster pick`.
#[derive(clap::Args, Debug, Clone)]
pub struct RosterPickArgs {
    #[arg(long = "json", help = "Print the pick and runners-up as one JSON document")]
    pub json: bool,
    #[arg(
        long = "prefer",
        value_name = "string",
        default_value = "",
        help = "Break ties: fast, cheap, or strong"
    )]
    pub prefer: String,
    // The help is "Task kind: " and TASK_KINDS joined (a test checks).
    #[arg(
        long = "task",
        value_name = "string",
        default_value = "",
        help = "Task kind: edit, bugfix, feature, long-horizon, review, research, design, test, kernel, chat"
    )]
    pub task: String,
    /// The positional arguments, checked against ROSTER_PICK.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn roster_pick_def() -> Def {
    leaf!(ROSTER_PICK, RosterPickArgs, run_roster_pick)
}

/// What pick --json prints.
#[derive(Serialize)]
struct PickResult<'a> {
    task: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    prefer: &'a str,
    pick: &'a Pick,
    runners_up: &'a [Pick],
}

pub fn run_roster_pick(ctx: &Ctx, args: &RosterPickArgs) -> anyhow::Result<()> {
    if args.task.trim().is_empty() {
        anyhow::bail!(
            "--task is required: name the kind of work ({})",
            TASK_KINDS.join(", ")
        );
    }
    let view = local_roster(ctx)?;
    let host = view.host.clone();
    let opts = PickOptions {
        task: args.task.trim().to_string(),
        prefer: args.prefer.trim().to_string(),
    };
    let picks = agent_roster::pick(std::slice::from_ref(&view), &host, &opts)
        .map_err(|e| e.context(format!("picking for --task {}", opts.task)))?;
    let (first, rest) = picks.split_first().expect("pick returns one or more");
    let rest = &rest[..rest.len().min(2)];
    if args.json {
        return ctx.print_json(&PickResult {
            task: &opts.task,
            prefer: &opts.prefer,
            pick: first,
            runners_up: rest,
        });
    }
    let mut out = String::new();
    let _ = writeln!(out, "{}", first.command);
    let _ = writeln!(out, "  {}: {}", first.key, first.why);
    if !rest.is_empty() {
        out.push_str("\nRunners-up:\n");
        for p in rest {
            let _ = writeln!(out, "  {}", p.command);
            let _ = writeln!(out, "    {}: {}", p.key, p.why);
        }
    }
    ctx.write_out(&out)?;
    if first.score <= 0 {
        ctx.hint(format!(
            "nothing on record suits {}: agent roster set ENTRY --tasks {} says which entries do",
            opts.task, opts.task
        ));
    }
    Ok(())
}

// `agent roster note`

pub const ROSTER_NOTE: Doc = Doc {
    use_line: "note HARNESS/MODEL[@EFFORT] \"what happened\"",
    short: "Record how a harness and model did on a task, citing the session",
    long: r#"Note records one run's evaluation of a harness and model: the task kind it
was tried on, a verdict (good, mixed or bad), what happened, and why.

--session is required: the session the note comes from, as HARNESS:ID
(claude:3f3a9c...); the start of an id is enough when it names one
session. It must be a session on this host, as "agent sessions" lists
them, so a note lives on the host its session ran on. Notes are never
edited: --supersedes ID records a newer note in place of an older one on
the same entry, which then stops counting.

The entry needn't exist yet: notes on a harness and model with no entry
are listed under the roster. --by names the writer (default: you, or
"user" for a person; a dream writes dream-<id>)."#,
    example: r#"  agent roster note claude/claude-haiku-5-5@low --task edit --verdict good --session claude:3f3a9c "did the rename across 12 files cleanly"
  agent roster note agy/gemini-3-pro --task review --verdict mixed --session agy:8d1e --why "missed the race" "found 3 of 4 issues"
  agent roster note claude/claude-opus-5-5 --task bugfix --verdict bad --session claude:77aa --supersedes 4 "the fix regressed""#,
    args: ArgRule::Exact(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster note`.
#[derive(clap::Args, Debug, Clone)]
pub struct RosterNoteArgs {
    #[arg(
        long = "by",
        value_name = "string",
        default_value = "",
        help = "Who wrote it (default: your agent name, or \"user\")"
    )]
    pub by: String,
    #[arg(
        long = "session",
        value_name = "string",
        default_value = "",
        help = "The session it came from, as HARNESS:ID (agent sessions lists them)"
    )]
    pub session: String,
    #[arg(
        long = "supersedes",
        value_name = "int",
        default_value_t = 0,
        help = "The id of an earlier note on this entry that this one replaces"
    )]
    pub supersedes: i64,
    // The help is "Task kind: " and TASK_KINDS joined (a test checks).
    #[arg(
        long = "task",
        value_name = "string",
        default_value = "",
        help = "Task kind: edit, bugfix, feature, long-horizon, review, research, design, test, kernel, chat"
    )]
    pub task: String,
    #[arg(
        long = "verdict",
        value_name = "string",
        default_value = "",
        help = "Verdict: good, mixed, or bad"
    )]
    pub verdict: String,
    #[arg(long = "why", value_name = "string", default_value = "", help = "Why it went that way")]
    pub why: String,
    /// The positional arguments, checked against ROSTER_NOTE.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn roster_note_def() -> Def {
    leaf!(ROSTER_NOTE, RosterNoteArgs, run_roster_note)
}

pub fn run_roster_note(ctx: &Ctx, args: &RosterNoteArgs) -> anyhow::Result<()> {
    let key = entry_key(&args.args[0])?;
    for (flag, v) in [
        ("--task", &args.task),
        ("--verdict", &args.verdict),
        ("--session", &args.session),
    ] {
        if v.trim().is_empty() {
            let what = match flag {
                "--task" => format!("the task kind it was tried on ({})", TASK_KINDS.join(", ")),
                "--verdict" => "good, mixed or bad".to_string(),
                _ => "the session it came from, as HARNESS:ID (agent sessions lists them)"
                    .to_string(),
            };
            anyhow::bail!("{flag} is required: {what}");
        }
    }
    let by = if args.by.trim().is_empty() {
        friction_caller(ctx).0
    } else {
        args.by.trim().to_string()
    };
    let n = agent_roster::add_note(
        ctx.store()?,
        ctx.cfg(),
        NoteInput {
            harness: key.harness.clone(),
            model: key.model.clone(),
            effort: key.effort.clone(),
            task: args.task.clone(),
            verdict: args.verdict.clone(),
            text: args.args[1].clone(),
            why: args.why.clone(),
            source: args.session.clone(),
            by,
            supersedes: args.supersedes,
        },
    )?;
    let mut line = format!(
        "Noted #{} on {key}: {} on {}, from {}",
        n.id, n.verdict, n.task, n.source
    );
    if n.supersedes != 0 {
        let _ = write!(line, " (supersedes #{})", n.supersedes);
    }
    line.push_str(".\n");
    ctx.write_out(&line)?;
    if ctx
        .store()?
        .get_roster_entry(&key.harness, &key.model, &key.effort)?
        .is_none()
    {
        ctx.hint(format!(
            "{key} has no roster entry, so pick won't offer it: agent roster set {key} adds one"
        ));
    }
    Ok(())
}

// `agent roster set`

pub const ROSTER_SET: Doc = Doc {
    use_line: "set HARNESS/MODEL[@EFFORT]",
    short: "Add a roster entry, or change one",
    long: r#"Set adds an entry for a harness and model this host can run, or changes
the fields given on one that exists, leaving the rest. A new entry is
enabled. Give a flag an empty value ("") to clear it.

The harness is the backend spawn takes (claude, agy, aider, or a custom
backend's name, which is also looked for on PATH); the model is what spawn
--model takes, empty for the harness's default (claude/)."#,
    example: r#"  agent roster set claude/claude-haiku-5-5 --speed fast --cost low --tasks edit,test --good-at "small precise edits" --avoid "deep debugging"
  agent roster set claude/claude-opus-5-5@max --tasks long-horizon,design --limits "weekly cap"
  agent roster set agy/ --tasks review"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster set`.
#[derive(clap::Args, Debug, Clone)]
pub struct RosterSetArgs {
    #[arg(long = "avoid", value_name = "string", default_value = "", help = "What not to give it, in short")]
    pub avoid: String,
    #[arg(long = "cost", value_name = "string", default_value = "", help = "Cost: low, medium, or high")]
    pub cost: String,
    #[arg(long = "good-at", value_name = "string", default_value = "", help = "What it does well, in short")]
    pub good_at: String,
    #[arg(
        long = "limits",
        value_name = "string",
        default_value = "",
        help = "Its limits, in short (a weekly cap, a context size)"
    )]
    pub limits: String,
    #[arg(long = "speed", value_name = "string", default_value = "", help = "Speed: fast, medium, or slow")]
    pub speed: String,
    #[arg(
        long = "tasks",
        value_name = "string",
        default_value = "",
        help = "The task kinds it suits, comma-separated (agent help roster lists them)"
    )]
    pub tasks: String,
    /// The positional arguments, checked against ROSTER_SET.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn roster_set_def() -> Def {
    leaf!(ROSTER_SET, RosterSetArgs, run_roster_set)
}

pub fn run_roster_set(ctx: &Ctx, args: &RosterSetArgs) -> anyhow::Result<()> {
    let key = entry_key(&args.args[0])?;
    let given = |flag: &str, v: &str| ctx.changed(flag).then(|| v.to_string());
    let changes = EntryChanges {
        good_at: given("good-at", &args.good_at),
        avoid: given("avoid", &args.avoid),
        tasks: given("tasks", &args.tasks),
        speed: given("speed", &args.speed),
        cost: given("cost", &args.cost),
        limits: given("limits", &args.limits),
    };
    let (e, created) =
        agent_roster::set_entry(ctx.store()?, &key, &changes, &friction_caller(ctx).0)?;
    let verb = if created { "Added" } else { "Updated" };
    ctx.write_out(&format!("{verb} {}.\n", Key::of_entry(&e)))?;
    if created && e.tasks.is_empty() {
        ctx.hint(format!(
            "pick matches entries by task: agent roster set {key} --tasks KINDS says which it suits"
        ));
    }
    Ok(())
}

// `agent roster enable`, `agent roster disable`, `agent roster remove`

pub const ROSTER_ENABLE: Doc = Doc {
    use_line: "enable HARNESS/MODEL[@EFFORT]",
    short: "Switch a roster entry back on",
    long: r#"Enable switches an entry back on, so pick offers it again when its CLI is
on PATH."#,
    example: "  agent roster enable agy/gemini-3-pro",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

pub const ROSTER_DISABLE: Doc = Doc {
    use_line: "disable HARNESS/MODEL[@EFFORT]",
    short: "Switch a roster entry off, keeping it and its notes",
    long: r#"Disable switches an entry off: it stays listed, with its fields and notes,
and reads as unavailable, so pick doesn't offer it."#,
    example: "  agent roster disable agy/gemini-3-pro",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

pub const ROSTER_REMOVE: Doc = Doc {
    use_line: "remove HARNESS/MODEL[@EFFORT]",
    short: "Delete a roster entry (its notes stay)",
    long: r#"Remove deletes an entry. Its notes stay, listed under the roster as notes
on a harness and model with no entry; "agent roster set" brings the entry
back. To keep it but stop pick offering it, use disable."#,
    example: "  agent roster remove agy/gemini-3-pro",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The arguments of enable, disable and remove, which have no flags.
#[derive(clap::Args, Debug, Clone)]
pub struct RosterEntryArgs {
    /// The positional arguments: the entry.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn roster_enable_def() -> Def {
    leaf!(ROSTER_ENABLE, RosterEntryArgs, run_roster_enable)
}

pub fn roster_disable_def() -> Def {
    leaf!(ROSTER_DISABLE, RosterEntryArgs, run_roster_disable)
}

pub fn roster_remove_def() -> Def {
    leaf!(ROSTER_REMOVE, RosterEntryArgs, run_roster_remove)
}

fn switch(ctx: &Ctx, args: &RosterEntryArgs, on: bool) -> anyhow::Result<()> {
    let key = entry_key(&args.args[0])?;
    let e = agent_roster::set_enabled(ctx.store()?, &key, on, &friction_caller(ctx).0)?;
    let state = if on { "Enabled" } else { "Disabled" };
    ctx.write_out(&format!("{state} {}.\n", Key::of_entry(&e)))
}

pub fn run_roster_enable(ctx: &Ctx, args: &RosterEntryArgs) -> anyhow::Result<()> {
    switch(ctx, args, true)
}

pub fn run_roster_disable(ctx: &Ctx, args: &RosterEntryArgs) -> anyhow::Result<()> {
    switch(ctx, args, false)
}

pub fn run_roster_remove(ctx: &Ctx, args: &RosterEntryArgs) -> anyhow::Result<()> {
    let key = entry_key(&args.args[0])?;
    agent_roster::remove_entry(ctx.store()?, &key)?;
    ctx.write_out(&format!("Removed {key}; its notes stay.\n"))
}

#[cfg(test)]
#[path = "roster_test.rs"]
pub(crate) mod roster_test;

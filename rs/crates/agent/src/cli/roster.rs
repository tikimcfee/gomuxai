//! `agent roster` and its subcommands (no Go file: new in the Rust build;
//! docs/ROSTER.md). The rules are agent_roster's; this file parses flags and
//! prints. This host only for now: reads over the bus come later.
//!
//! Every output explains itself (docs/ROSTER.md, "Self-explaining output"):
//! a header line saying what it is, availability and note counts in words,
//! and a last "Next:" line saying what to run next.
use std::fmt::Write as _;

use agent_roster::{
    EntryChanges, EntryShow, EntryView, HostRoster, Key, NoteInput, Pick, PickOptions,
    RosterPick, SaveOutcome, SaveResult, SavedCheck, TASK_KINDS,
};
use serde::Serialize;

use std::time::Duration;

use agent_bus::{Context, RpcClient};

use super::command::{ArgRule, Def, Doc};
use super::friction::{friction_caller, stamp};
use super::remote::{
    call_remote, canonical_host, is_no_responders, is_remote_host, trusted_hosts,
};
use super::root::Ctx;
use super::sessions::tab_align;

// `agent roster`

pub const DOC: Doc = Doc {
    use_line: "roster",
    short: "Which harness and model to spawn: this host's roster, its notes, and pick",
    long: r#"Roster is where to look before a spawn: which harness and model this host
can run, what each is good at, how fast and costly it is, whether it is
available now, and what past runs showed. It lists this host's entries,
then every linked host's, asked at once over the bus (3s each):

  Roster on hub: harnesses and models this host can run
  ENTRY                    AVAILABLE            SPEED  COST  TASKS      NOTES
  claude/claude-haiku-5-5  available            fast   low   edit,test  3 good from 2 sessions
  agy/gemini-3-pro         off: agy not on PATH  slow   high  review     no notes

An entry is HARNESS/MODEL[@EFFORT]: the backend spawn takes, the model
spawn --model takes (empty for the harness's default: claude/), and an
effort for harnesses that take one (the same model at another effort is
its own entry). It is available when it is enabled, its harness's CLI is
on this host's PATH, and its plan isn't at its limit.

Limits come from the host's own llm-exp usage snapshot
(~/.cache/llm-exp/usage-snapshot.json), when it is under 10 minutes old:
for claude the fullest of the session, the week, and the named model's
week when the entry is that model; for agy its fullest window. At 90% an
entry reads "near limit: week 93%, resets Fri 04:00" and pick ranks it
down by 2; at 100% it is "off: at limit until ...". With no fresh
snapshot, show says "limits not known".

Notes are evaluations, one per run, each citing the session it came from:
NOTES counts them by verdict and says how many distinct sessions they come
from, so you can see whether a claim rests on one run or on five. A note
another supersedes isn't counted.

Task kinds: edit (small, well-specified change), bugfix, feature
(multi-file), long-horizon (hours, many steps), review, research (reading
and summarising), design, test, kernel (performance-critical low-level
code), chat (quick question).

roster (or roster list), show, picks and pick only read (pick --save adds a
saved pick's version); note only adds a note. set, enable, disable and
remove change the entries.

A linked host whose bus doesn't answer roster (it is down, or runs an
older agent, or its --remote-ops leaves roster out) is listed as not read,
with the fix to run there. --host HOST reads one host.

--json prints {"hosts": [...]}, each host {"host", "entries": [...],
"unlisted": {...}}, or {"host", "error", "fix"} when it wasn't read; each
entry has its fields, "key", "available", "unavailable" (why not), "notes"
(counts by verdict, and "sessions"), "by_task", and, when the host's usage
snapshot is fresh, "limit" ({"window", "pct", "reset_at", "captured_at"}),
"near_limit" and "limits" (in words); each host also has "limits_as_of"."#,
    example: r#"  agent roster
  agent roster pick --task edit
  agent roster --host hub
  agent roster show claude/claude-opus-5-5
  agent roster set claude/claude-haiku-5-5 --speed fast --cost low --tasks edit,test --good-at "small precise edits"
  agent roster note claude/claude-haiku-5-5 --task edit --verdict good --session claude:3f3a9c "did the rename across 12 files cleanly"
  agent roster pick --task edit --save quick-edit
  agent roster picks"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster` and `agent roster list`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Read only this host (default: this host and every linked host)"
    )]
    pub host: String,
    #[arg(long = "json", help = "Print the rosters as one JSON document")]
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
            roster_picks_def(),
            roster_note_def(),
            roster_set_def(),
            roster_enable_def(),
            roster_disable_def(),
            roster_remove_def(),
        ],
        ..leaf!(DOC, Args, run)
    }
}

/// This host's roster: what agent roster prints first, and what this
/// host's bus answers the roster op with.
pub(crate) fn local_roster(ctx: &Ctx) -> anyhow::Result<HostRoster> {
    let usage = agent_manage::llm_usage::read_snapshot();
    agent_roster::host_view(ctx.store()?, &ctx.cfg().host, usage.as_ref())
}

/// The fix for a host whose bus doesn't answer roster.
fn serve_fix(host: &str) -> String {
    format!(
        "on {host}: agent bus restart --remote-ops <its list>,roster (agent bus status there shows its list); an older agent there needs updating first"
    )
}

/// One peer's roster through rpc; a peer that doesn't answer is listed as
/// not read, with the fix.
fn peer_roster(rpc: &RpcClient, host: &str) -> HostRoster {
    match rpc.roster(&Context::background(), host) {
        Ok(r) => r,
        Err(e) if agent_bus::is_unanswered(&e) => HostRoster::unread(
            host,
            "its bus doesn't answer roster: it is down, not linked, runs an older agent, or leaves roster out of its --remote-ops",
            serve_fix(host),
        ),
        Err(e) => HostRoster::unread(host, format!("{e:#}"), String::new()),
    }
}

/// The rosters to read: host alone when given (this one, or a linked one
/// through the bus), else this host's and every trusted peer's, asked at
/// once. A peer that doesn't answer is listed as not read, never an error;
/// with no bus running, each peer is, and a hint says so.
pub(crate) fn gather_rosters(ctx: &Ctx, host: &str) -> anyhow::Result<Vec<HostRoster>> {
    if !host.is_empty() {
        agent_bus::validate_host(host)?;
        let host = canonical_host(ctx, host);
        if !is_remote_host(ctx, &host) {
            return Ok(vec![local_roster(ctx)?]);
        }
        let trusted = trusted_hosts(ctx).iter().any(|p| p.eq_ignore_ascii_case(&host));
        return match call_remote(ctx, &host, |c| c.roster(&Context::background(), &host)) {
            Ok(r) => Ok(vec![r]),
            // A linked host that doesn't serve roster is a row; one nobody
            // knows is the command's error, naming the hosts there are.
            Err(e) if trusted && (agent_bus::is_unanswered(&e) || is_no_responders(&e)) => {
                Ok(vec![HostRoster::unread(
                    &host,
                    "its bus doesn't answer roster: it is down, not linked, runs an older agent, or leaves roster out of its --remote-ops",
                    serve_fix(&host),
                )])
            }
            Err(e) => Err(e),
        };
    }
    let mut hosts = vec![local_roster(ctx)?];
    let local = hosts[0].host.to_ascii_lowercase();
    let peers: Vec<String> = trusted_hosts(ctx)
        .into_iter()
        .filter(|p| !p.is_empty() && p.to_ascii_lowercase() != local)
        .collect();
    if peers.is_empty() {
        return Ok(hosts);
    }
    let nc = match ctx.bus_connect() {
        Ok(nc) => nc,
        Err(e) if agent_bus::is_no_bus(&e) => {
            ctx.hint(format!(
                "no agent bus runs here, so this is this host only, without {} (agent bus start)",
                peers.join(", ")
            ));
            hosts.extend(peers.iter().map(|p| {
                HostRoster::unread(p, "no agent bus runs here", "here: agent bus start")
            }));
            return Ok(hosts);
        }
        Err(e) => return Err(e.context("connecting to the agent bus")),
    };
    let rpc = RpcClient::new(nc.clone(), Duration::ZERO);
    let read: Vec<HostRoster> = std::thread::scope(|s| {
        let handles: Vec<_> = peers
            .iter()
            .map(|p| {
                let rpc = &rpc;
                s.spawn(move || peer_roster(rpc, p))
            })
            .collect();
        handles
            .into_iter()
            .zip(&peers)
            .map(|(h, p)| {
                h.join()
                    .unwrap_or_else(|_| HostRoster::unread(p, "asking it failed", String::new()))
            })
            .collect()
    });
    nc.close();
    hosts.extend(read);
    Ok(hosts)
}

/// What roster --json prints.
#[derive(Serialize)]
struct RostersResult<'a> {
    hosts: &'a [HostRoster],
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let hosts = gather_rosters(ctx, args.host.trim())?;
    if args.json {
        return ctx.print_json(&RostersResult { hosts: &hosts });
    }
    ctx.write_out(&render_rosters(&hosts, &ctx.cfg().host))
}

/// The rosters, host by host: a header, the entries as a table, the notes
/// on harnesses and models with no entry; a host not read says why and the
/// fix. Then what to run next.
fn render_rosters(hosts: &[HostRoster], local_host: &str) -> String {
    let mut out = String::new();
    for (i, view) in hosts.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !view.read() {
            let _ = writeln!(out, "Roster on {}: not read: {}", view.host, view.error);
            if !view.fix.is_empty() {
                let _ = writeln!(out, "  fix: {}", view.fix);
            }
            continue;
        }
        let whose = if view.host.eq_ignore_ascii_case(local_host) {
            "this host".to_string()
        } else {
            view.host.clone()
        };
        let _ = writeln!(
            out,
            "Roster on {}: harnesses and models {whose} can run",
            view.host
        );
        if view.entries.is_empty() {
            out.push_str("It has no entries yet.\n");
        } else {
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
                    e.notes.describe(),
                ]);
            }
            out.push_str(&tab_align(&rows, 2));
        }
        if !view.unlisted.is_empty() {
            out.push_str(
                "Notes on harnesses and models with no entry (pick doesn't offer them):\n",
            );
            for (key, c) in &view.unlisted {
                let _ = writeln!(out, "  {key}  {}", c.describe());
            }
        }
    }
    out.push('\n');
    let entries = hosts.iter().flat_map(|h| &h.entries);
    let (mut any, mut available) = (false, false);
    for e in entries {
        any = true;
        available |= e.available;
    }
    if !any {
        out.push_str(
            "Next: agent roster set HARNESS/MODEL --tasks KINDS --speed fast|medium|slow --cost low|medium|high adds an entry.\n",
        );
    } else if !available {
        out.push_str(
            "Next: no entry is available; agent roster enable ENTRY switches one on, or put its CLI on PATH.\n",
        );
    } else {
        out.push_str(
            "Next: agent roster pick --task KIND prints a spawn line; agent roster show ENTRY lists an entry's notes.\n",
        );
    }
    out
}

/// An entry's availability in words, as the roster lists it: "available",
/// "near limit: week 93%, resets Fri 04:00", "off: disabled", "off: at
/// limit until Fri 04:00". (Limits that aren't known aren't said here; show
/// says so.)
pub(crate) fn availability(e: &EntryView) -> String {
    if e.available && e.near_limit {
        e.limits.clone()
    } else if e.available {
        "available".into()
    } else {
        format!("off: {}", e.unavailable)
    }
}

fn dash(s: &str) -> String {
    if s.is_empty() { "-".into() } else { s.into() }
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
limits (its notes, and its plan's usage now, or "limits not known"), what
it is good at and what to avoid, who set it, and its notes counted. Then every note on it, oldest first: its task kind, verdict, who
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
    let show = agent_roster::show(
        ctx.store()?,
        &ctx.cfg().host,
        &key,
        agent_manage::llm_usage::read_snapshot().as_ref(),
    )?;
    if args.json {
        return ctx.print_json(&show);
    }
    ctx.write_out(&render_show(&show))
}

fn render_show(s: &EntryShow) -> String {
    let mut out = String::new();
    match &s.entry {
        None => {
            let _ = writeln!(
                out,
                "Roster entry {} on {}: none, only notes (pick doesn't offer it)",
                s.key, s.host
            );
        }
        Some(e) => {
            let _ = writeln!(out, "Roster entry {} on {}", s.key, s.host);
            let _ = writeln!(out, "  availability: {}", availability(e));
            let _ = writeln!(out, "  tasks:        {}", dash(&e.entry.tasks.join(", ")));
            let _ = writeln!(out, "  speed:        {}", dash(&e.entry.speed));
            let _ = writeln!(out, "  cost:         {}", dash(&e.entry.cost));
            for (label, v) in [
                ("good at:     ", &e.entry.good_at),
                ("avoid:       ", &e.entry.avoid),
                ("limits:      ", &e.entry.limits),
            ] {
                if !v.is_empty() {
                    let _ = writeln!(out, "  {label} {v}");
                }
            }
            let _ = writeln!(
                out,
                "  set by:       {}, {}",
                dash(&e.entry.set_by),
                stamp(&e.entry.updated_at)
            );
            let _ = writeln!(out, "  limits:       {}", e.limits);
            let _ = writeln!(
                out,
                "  notes:        {} (superseded notes don't count)",
                e.notes.describe()
            );
        }
    }
    if s.notes.is_empty() {
        out.push_str("\nIt has no notes yet.\n");
    } else {
        let _ = writeln!(out, "\nNotes on {}, oldest first:", s.key);
        for nv in &s.notes {
            let n = &nv.note;
            let mut head = format!(
                "  #{} {}  {} on {}  by {}  from {}",
                n.id,
                stamp(&n.created_at),
                n.verdict,
                n.task,
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
    }
    let next = if s.entry.is_none() {
        format!("agent roster set {} --tasks KINDS adds it as an entry", s.key)
    } else {
        "agent roster pick --task KIND ranks it among the others".to_string()
    };
    let _ = writeln!(
        out,
        "\nNext: agent roster note {} --task KIND --verdict good|mixed|bad --session HARNESS:ID \"what happened\" adds a note; {next}.",
        s.key
    );
    out
}

// `agent roster pick`

pub const ROSTER_PICK: Doc = Doc {
    use_line: "pick --task KIND",
    short: "Pick the best available harness and model for a task, as a spawn line",
    long: r#"Pick ranks the available entries of this host and every linked host (as
agent roster reads them; --host HOST for one) for a task kind and prints the
best as a ready spawn line, then the next two, each with one line of why.
An entry on another host spawns there (NAME@HOST). It never spawns: replace
NAME and run the line yourself. A linked host whose roster couldn't be read
is named, with the fix.

Ranking: an entry whose tasks include the kind scores 2; each distinct
session with a good note on that kind adds 1, each with a bad one takes 1
away (superseded notes don't count); one near its plan's limit loses 2,
and one at its limit isn't available. --prefer breaks ties: fast (by speed),
cheap (by cost) or strong (dearest first). Then more good sessions win,
then this host's entries.

An entry at an effort is picked like any other; spawn has no --effort yet,
so the why line says which effort the notes were about.

--save NAME keeps the chosen command as a saved pick: version 1 for a new
name, a new version only when the command differs from its newest one.
--saved NAME[@VERSION] prints a saved command again, exactly, whatever the
roster says now, and says whether its entry is still available (asking
its host, if that is another). "agent roster picks" lists them.

Task kinds: edit, bugfix, feature, long-horizon, review, research, design,
test, kernel, chat.

--json prints {"task", "prefer", "pick", "runners_up": [...], "saved",
"unread": [...]},
each pick with "host", "key", "harness", "model", "effort", "score",
"command", "argv" and "why"; with --saved, {"pick", "available",
"status"}."#,
    example: r#"  agent roster pick --task edit
  agent roster pick --task long-horizon --prefer fast
  agent roster pick --task review --host hub
  agent roster pick --task edit --save quick-edit
  agent roster pick --saved quick-edit@1
  agent roster pick --task review --json"#,
    args: ArgRule::None,
    exclusive: &[
        &["saved", "task"],
        &["saved", "save"],
        &["saved", "prefer"],
        &["saved", "host"],
    ],
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster pick`.
#[derive(clap::Args, Debug, Clone)]
pub struct RosterPickArgs {
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Pick from only this host's roster (default: this host and every linked host)"
    )]
    pub host: String,
    #[arg(long = "json", help = "Print the pick and runners-up as one JSON document")]
    pub json: bool,
    #[arg(
        long = "prefer",
        value_name = "string",
        default_value = "",
        help = "Break ties: fast, cheap, or strong"
    )]
    pub prefer: String,
    #[arg(
        long = "save",
        value_name = "string",
        default_value = "",
        help = "Keep the chosen command as this saved pick (a new version if it changed)"
    )]
    pub save: String,
    #[arg(
        long = "saved",
        value_name = "string",
        default_value = "",
        help = "Print a saved pick's command again: NAME[@VERSION] (default: its newest)"
    )]
    pub saved: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    saved: Option<&'a SaveResult>,
    /// The hosts whose roster couldn't be read, with why and the fix.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    unread: &'a [HostRoster],
}

pub fn run_roster_pick(ctx: &Ctx, args: &RosterPickArgs) -> anyhow::Result<()> {
    if !args.saved.trim().is_empty() {
        return replay_saved(ctx, args);
    }
    if args.task.trim().is_empty() {
        anyhow::bail!(
            "--task is required: name the kind of work ({}), or --saved NAME for a saved pick",
            TASK_KINDS.join(", ")
        );
    }
    let save = if args.save.trim().is_empty() {
        None
    } else {
        Some(agent_roster::check_pick_name(&args.save)?)
    };
    let opts = PickOptions {
        task: args.task.trim().to_string(),
        prefer: args.prefer.trim().to_string(),
    };
    // The kinds and preference are checked before any host is asked.
    agent_roster::check_task(&opts.task)?;
    if !opts.prefer.is_empty() {
        agent_roster::check_prefer(&opts.prefer)?;
    }
    let (read, unread): (Vec<HostRoster>, Vec<HostRoster>) = gather_rosters(ctx, args.host.trim())?
        .into_iter()
        .partition(HostRoster::read);
    let local_host = ctx.cfg().host.clone();
    let picks = agent_roster::pick(&read, &local_host, &opts).map_err(|e| {
        let mut e = e.context(format!("picking for --task {}", opts.task));
        if !unread.is_empty() {
            let names: Vec<&str> = unread.iter().map(|h| h.host.as_str()).collect();
            e = e.context(format!(
                "not read: {} (agent roster says why, and the fix)",
                names.join(", ")
            ));
        }
        e
    })?;
    let (first, rest) = picks.split_first().expect("pick returns one or more");
    let rest = &rest[..rest.len().min(2)];
    let saved = match &save {
        None => None,
        Some(name) => Some(agent_roster::save_pick(
            ctx.store()?,
            name,
            first,
            &opts.task,
            &friction_caller(ctx).0,
        )?),
    };
    if args.json {
        return ctx.print_json(&PickResult {
            task: &opts.task,
            prefer: &opts.prefer,
            pick: first,
            runners_up: rest,
            saved: saved.as_ref(),
            unread: &unread,
        });
    }
    let mut out = String::new();
    let _ = writeln!(out, "Picked for {}: {} on {}", opts.task, first.key, first.host);
    let _ = writeln!(out, "  {}", first.command);
    let _ = writeln!(out, "  why: {}", first.why);
    if !rest.is_empty() {
        out.push_str("\nRunners-up:\n");
        for (i, p) in rest.iter().enumerate() {
            let _ = writeln!(out, "  {}. {} on {}: {}", i + 2, p.key, p.host, p.command);
            let _ = writeln!(out, "     why: {}", p.why);
        }
    }
    if first.score <= 0 {
        let _ = writeln!(
            out,
            "\nNothing on record suits {} (no entry lists it, no good notes on it): this is a guess.",
            opts.task
        );
    }
    if !unread.is_empty() {
        out.push_str("\nNot read, so not in the running:\n");
        for h in &unread {
            let _ = writeln!(out, "  {}: {}", h.host, h.error);
            if !h.fix.is_empty() {
                let _ = writeln!(out, "    fix: {}", h.fix);
            }
        }
    }
    if let Some(s) = &saved {
        out.push('\n');
        out.push_str(&describe_save(s));
    }
    let keep = if saved.is_some() {
        String::new()
    } else {
        "; add --save NAME to keep this command".to_string()
    };
    let show = if first.host.eq_ignore_ascii_case(&local_host) {
        format!("agent roster show {} lists its notes", first.key)
    } else {
        format!("agent roster show {} on {} lists its notes", first.key, first.host)
    };
    let _ = writeln!(
        out,
        "\nNext: run the spawn line with your agent's name for NAME; {show}{keep}."
    );
    ctx.write_out(&out)
}

/// What saving did, in a line.
fn describe_save(s: &SaveResult) -> String {
    let p = &s.pick;
    match s.outcome {
        SaveOutcome::New => format!(
            "Saved as {}@{} (a new saved pick): agent roster pick --saved {} prints it again.\n",
            p.name, p.version, p.name
        ),
        SaveOutcome::NewVersion => format!(
            "Saved as {}@{}: the command changed from @{}, which is kept.\n",
            p.name,
            p.version,
            p.version - 1
        ),
        SaveOutcome::Unchanged => format!(
            "Not saved: {}@{} already has this command.\n",
            p.name, p.version
        ),
    }
}

/// pick --saved: a saved command again, and whether its entry is
/// available now.
fn replay_saved(ctx: &Ctx, args: &RosterPickArgs) -> anyhow::Result<()> {
    let check = check_saved_ref(ctx, &args.saved)?;
    if args.json {
        return ctx.print_json(&check);
    }
    ctx.write_out(&render_saved(&check))
}

/// The saved pick NAME[@VERSION] names, checked against its own host's
/// roster: this one's, or a linked one's through the bus (none read, if
/// that fails: check_saved says it wasn't checked). pick --saved prints it;
/// spawn --saved spawns it.
pub(crate) fn check_saved_ref(ctx: &Ctx, saved_ref: &str) -> anyhow::Result<SavedCheck> {
    let (name, version) = agent_roster::parse_saved_ref(saved_ref)?;
    let p = agent_roster::saved(ctx.store()?, &name, version)?;
    let hosts = if is_remote_host(ctx, &p.host) {
        match ctx.bus_connect() {
            Ok(nc) => {
                let rpc = RpcClient::new(nc.clone(), Duration::ZERO);
                let r = peer_roster(&rpc, &p.host);
                nc.close();
                if r.read() { vec![r] } else { Vec::new() }
            }
            Err(_) => Vec::new(),
        }
    } else {
        vec![local_roster(ctx)?]
    };
    Ok(agent_roster::check_saved(p, &hosts))
}

fn render_saved(c: &SavedCheck) -> String {
    let p = &c.pick;
    let key = Key::new(&p.harness, &p.model, &p.effort);
    let mut out = format!(
        "Saved pick {}@{}: for {}, saved by {} {}\n",
        p.name,
        p.version,
        p.task,
        p.saved_by,
        stamp(&p.saved_at)
    );
    let _ = writeln!(out, "  {}", p.argv.join(" "));
    if !p.why.is_empty() {
        let _ = writeln!(out, "  why, when saved: {}", p.why);
    }
    let _ = writeln!(out, "Its entry {key} on {} now: {}.", p.host, c.status);
    let next = match c.available {
        Some(false) => format!(
            "it may not start; agent roster pick --task {} picks again",
            p.task
        ),
        _ => format!("agent roster picks {} lists its versions", p.name),
    };
    let _ = writeln!(
        out,
        "\nNext: run the line with your agent's name for NAME; {next}."
    );
    out
}

// `agent roster picks`

pub const ROSTER_PICKS: Doc = Doc {
    use_line: "picks [NAME]",
    short: "List the saved picks, or one saved pick's versions",
    long: r#"Picks lists this host's saved picks: startup commands kept with "agent
roster pick --save NAME", the newest version of each, with the task it was
picked for, its entry, who saved it and when. Given a NAME, it lists every
version of that one, newest first.

"agent roster pick --saved NAME[@VERSION]" prints a saved command again.

--json prints {"host", "picks": [...]}."#,
    example: r#"  agent roster picks
  agent roster picks quick-edit
  agent roster picks --json"#,
    args: ArgRule::Max(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent roster picks`.
#[derive(clap::Args, Debug, Clone)]
pub struct RosterPicksArgs {
    #[arg(long = "json", help = "Print the saved picks as one JSON document")]
    pub json: bool,
    /// The positional arguments, checked against ROSTER_PICKS.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn roster_picks_def() -> Def {
    leaf!(ROSTER_PICKS, RosterPicksArgs, run_roster_picks)
}

/// What picks --json prints.
#[derive(Serialize)]
struct PicksResult<'a> {
    host: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    name: &'a str,
    picks: &'a [RosterPick],
}

pub fn run_roster_picks(ctx: &Ctx, args: &RosterPicksArgs) -> anyhow::Result<()> {
    let host = ctx.cfg().host.clone();
    let name = match args.args.first() {
        None => String::new(),
        Some(n) => agent_roster::check_pick_name(n)?,
    };
    if !name.is_empty() {
        // Says what there is when the name isn't saved.
        agent_roster::saved(ctx.store()?, &name, 0)?;
    }
    let picks = ctx.store()?.list_roster_picks(&name)?;
    if args.json {
        return ctx.print_json(&PicksResult {
            host: &host,
            name: &name,
            picks: &picks,
        });
    }
    let mut out = if name.is_empty() {
        format!(
            "Saved picks on {host}: startup commands kept with pick --save (the newest version of each)\n"
        )
    } else {
        format!("Saved pick {name} on {host}: every version, newest first\n")
    };
    if picks.is_empty() {
        out.push_str("There are none yet.\n\nNext: agent roster pick --task KIND --save NAME keeps a pick's command.\n");
        return ctx.write_out(&out);
    }
    let first = if name.is_empty() { "NAME" } else { "VERSION" };
    let mut rows = vec![vec![
        first.to_string(),
        "TASK".into(),
        "ENTRY".into(),
        "SAVED".into(),
        "COMMAND".into(),
    ]];
    for p in &picks {
        let id = if name.is_empty() {
            format!("{}@{}", p.name, p.version)
        } else {
            format!("@{}", p.version)
        };
        let mut entry = Key::new(&p.harness, &p.model, &p.effort).to_string();
        if !p.host.eq_ignore_ascii_case(&host) {
            let _ = write!(entry, " on {}", p.host);
        }
        rows.push(vec![
            id,
            p.task.clone(),
            entry,
            format!("{} by {}", stamp(&p.saved_at), p.saved_by),
            p.argv.join(" "),
        ]);
    }
    out.push_str(&tab_align(&rows, 2));
    out.push_str("\nNext: agent roster pick --saved NAME[@VERSION] prints one again, saying whether its entry is still available");
    if name.is_empty() {
        out.push_str("; agent roster picks NAME lists a pick's versions");
    }
    out.push_str(".\n");
    ctx.write_out(&out)
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
    let by = if args.by.trim().is_empty() {
        friction_caller(ctx).0
    } else {
        args.by.trim().to_string()
    };
    roster_note_by(ctx, args, by)
}

/// Records the note args describe as written by `by` (agent dream roster
/// note fills it from the dream).
pub(crate) fn roster_note_by(ctx: &Ctx, args: &RosterNoteArgs, by: String) -> anyhow::Result<()> {
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
    let mut out = format!(
        "Noted #{} on {key}: {} on {}, from {}",
        n.id, n.verdict, n.task, n.source
    );
    if n.supersedes != 0 {
        let _ = write!(out, " (supersedes #{}, which stops counting)", n.supersedes);
    }
    out.push_str(".\n");
    let has_entry = ctx
        .store()?
        .get_roster_entry(&key.harness, &key.model, &key.effort)?
        .is_some();
    if has_entry {
        let _ = writeln!(out, "Next: agent roster show {key} lists its notes.");
    } else {
        let _ = writeln!(
            out,
            "{key} has no roster entry, so pick won't offer it.\nNext: agent roster set {key} --tasks KINDS adds one."
        );
    }
    ctx.write_out(&out)
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
    let key = Key::of_entry(&e);
    let mut out = format!("{verb} {key}.\n");
    if e.tasks.is_empty() {
        let _ = writeln!(
            out,
            "Next: pick matches entries by task; agent roster set {key} --tasks KINDS says which it suits."
        );
    } else {
        let _ = writeln!(out, "Next: agent roster show {key} shows it as pick sees it.");
    }
    ctx.write_out(&out)
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
    let key = Key::of_entry(&e);
    let out = if on {
        format!("Enabled {key}: pick offers it when its CLI is on PATH.\n")
    } else {
        format!("Disabled {key}: it stays listed, with its notes, and pick doesn't offer it.\nNext: agent roster enable {key} switches it back on.\n")
    };
    ctx.write_out(&out)
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
    ctx.write_out(&format!(
        "Removed {key}; its notes stay, listed under the roster.\nNext: agent roster set {key} brings it back.\n"
    ))
}

#[cfg(test)]
#[path = "roster_test.rs"]
pub(crate) mod roster_test;

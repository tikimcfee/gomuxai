//! `agent friction`, `agent friction add`, `agent friction +1`, `agent friction list`, `agent friction show`, `agent friction accept`, `agent friction resolve`, `agent friction export` (Go: internal/cli/friction.go).
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;

use agent_base::buildinfo;
use agent_base::gostd::{os, strconv};
use agent_base::util::{self, shell_join};
use agent_service::friction_is_open;
use agent_store::{
    AttachedCommand, CommandFilter, FRICTION_ACCEPTED, FRICTION_DUPLICATE, FRICTION_OPEN,
    FRICTION_RESOLVED, FRICTION_WONTFIX, Friction, FrictionFilter, FrictionLink, FrictionVote,
    Time, gojson,
};
use chrono::TimeDelta;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::exit::ExitError;
use super::friction_links::{
    LinkView, format_export_links, format_friction_links, format_link_marker, links_of,
    resolve_linked,
};
use super::history::clip;
use super::root::{self, Ctx};

#[path = "friction_similar.rs"]
pub mod friction_similar;
use friction_similar::similar_friction;

/// The kinds an entry can have; the first is the default for none given.
/// brief, handover and skill are new in the Rust build (docs/FLEET.md).
pub const FRICTION_KINDS: &[&str] = &[
    "other", "help", "error", "missing", "bug", "docs", "brief", "handover", "skill",
];

/// The statuses in lifecycle order; export groups by them.
pub const FRICTION_STATUSES: &[&str] = &[
    FRICTION_OPEN,
    FRICTION_ACCEPTED,
    FRICTION_RESOLVED,
    FRICTION_WONTFIX,
    FRICTION_DUPLICATE,
];

/// add's exit code when it doesn't file because the summary looks like an
/// open entry's.
pub const FRICTION_SIMILAR_EXIT_CODE: i32 = 3;

/// How far back add and +1 look for the caller's failed command.
const FRICTION_ATTACH_WINDOW: TimeDelta = TimeDelta::minutes(15);

// `agent friction`

pub const DOC: Doc = Doc {
    use_line: "friction",
    short: "File what got in your way using agent, and read what others filed",
    long: r#"Friction is this host's box of what got in the way of using agent. If agent
confuses you, gets in your way, or lacks something you need, file it, then
carry on with your work. The people working on the tool fix what's filed
and resolve the entries, so resolved entries record what changed and who
asked.

Kinds (--kind): help (a command's help confused you or left something out),
error (an error that didn't say what went wrong or what to do), missing (a
feature you needed), bug, docs (README, briefing), brief (a brief was
unclear), handover (a handover lost something), skill (the skill or
briefing misled), other (the default).

Lifecycle: open -> accepted (someone means to fix it) -> resolved (fixed;
the note says how), wontfix, or duplicate (its votes count toward the
original).

Check "agent friction list" first, and +1 an entry that's already there.
add won't file a summary close to an open or accepted entry's: it lists
the matches and exits 3, and --new files it anyway. add and +1 attach your
last failed agent command from the past 15 minutes; --no-command skips it.

Each host has its own box. The same problem filed on two hosts is linked
with "agent friction link 4 34@monolith1": both entries stay open and count
their own votes, show and list name the other, and resolving either
resolves both, over the agent bus. "agent friction links" checks every link
and says which have gone stale (one end resolved without the other, a
missing entry, a host that doesn't answer), with what to run."#,
    example: r#"  agent friction list
  agent friction add "wait --timeout doesn't say what status it last saw" --kind error
  agent friction +1 12 "hit it again with --next"
  agent friction show 12
  agent friction link 12 34@monolith1
  agent friction resolve 12 --note "wait names the last status (abc1234)"
  agent friction links --dangling"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![
            friction_add_def(),
            friction_vote_def(),
            friction_list_def(),
            friction_show_def(),
            friction_accept_def(),
            friction_resolve_def(),
            friction_export_def(),
            super::friction_links::friction_link_def(),
            super::friction_links::friction_unlink_def(),
            super::friction_links::def(),
        ],
    )
}

// `agent friction add`

pub const FRICTION_ADD: Doc = Doc {
    use_line: "add \"summary\"",
    short: "File a friction entry",
    long: r#"Add files a friction entry: a one-line summary of what got in your way,
optionally with a kind and more detail.

If your last failed agent command (same AGENT_NAME, or for a person, same
directory) ran in the past 15 minutes, it's copied into the entry, and add
says so; --no-command skips it.

If the summary is close to an open or accepted entry's, add doesn't file:
it lists the matches and exits 3. "agent friction +1 ID" adds your vote to
one of them; --new files yours anyway."#,
    example: r#"  agent friction add "send's help doesn't say the message is typed into the terminal" --kind help
  agent friction add "no way to wait for several agents at once" --kind missing --detail "..."
  agent friction add "status says unknown for aider agents" --kind bug --detail-file notes.md --new"#,
    args: ArgRule::Exact(1),
    exclusive: &[&["detail", "detail-file"]],
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction add`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionAddArgs {
    #[arg(long = "detail", value_name = "string", default_value = "", help = "More detail")]
    pub detail: String,
    #[arg(
        long = "detail-file",
        value_name = "string",
        default_value = "",
        help = "Read the detail from a file (\"-\" for stdin)",
    )]
    pub detail_file: String,
    // The help is "Kind: " and FRICTION_KINDS joined (a test checks).
    #[arg(
        long = "kind",
        value_name = "string",
        default_value = "other",
        help = "Kind: other, help, error, missing, bug, docs, brief, handover, skill",
    )]
    pub kind: String,
    #[arg(long = "new", help = "File even if it looks like an open entry")]
    pub new: bool,
    #[arg(long = "no-command", help = "Don't attach your last failed command")]
    pub no_command: bool,
    /// The positional arguments, checked against FRICTION_ADD.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_add_def() -> Def {
    leaf!(FRICTION_ADD, FrictionAddArgs, run_friction_add)
}

/// Who is running the command, as friction records it: the agent's name, or
/// "user" for a person; and the agent's recorded parent (Go: frictionCaller).
pub fn friction_caller(ctx: &Ctx) -> (String, String) {
    let name = ctx.non_interactive_caller_agent();
    if name.is_empty() {
        return ("user".to_string(), String::new());
    }
    let parent = match ctx.store().map(|s| s.get_agent(&name)) {
        Ok(Ok(Some(a))) => a.spawned_by,
        _ => String::new(),
    };
    (name, parent)
}

/// An id unique across hosts, so boxes can be merged (Go: newFrictionUID).
fn new_friction_uid(host: &str) -> String {
    let mut b = [0u8; 6];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{host}-{hex}")
}

/// The caller's newest failed agent command from the past
/// FRICTION_ATTACH_WINDOW (by AGENT_NAME, or for a person, by directory), or
/// None. Friction's own commands don't count: add exits non-zero when it
/// doesn't file (Go: lastFailedCommand).
fn last_failed_command(ctx: &Ctx, no_command: bool) -> Option<AttachedCommand> {
    if no_command {
        return None;
    }
    let mut f = CommandFilter {
        by_agent: true,
        agent: ctx.non_interactive_caller_agent(),
        failed: true,
        since: Time::now() - FRICTION_ATTACH_WINDOW,
        limit: 20,
        ..Default::default()
    };
    if f.agent.is_empty() {
        f.cwd = os::getwd().unwrap_or_default();
    }
    let recs = ctx.store().ok()?.list_commands(&f).ok()?;
    recs.into_iter()
        .find(|r| !is_friction_command(&r.args))
        .map(|r| AttachedCommand {
            args: r.args,
            exit_code: r.exit_code,
            error: r.error,
            at: r.started_at,
        })
}

/// Whether a command line (without "agent") runs friction or one of its
/// subcommands.
fn is_friction_command(args: &[String]) -> bool {
    let (cmd, _) = root::find(root::tree(), args);
    let path = cmd.path_below_root();
    path == "friction" || path.starts_with("friction ")
}

/// What was attached, for add's and +1's line, e.g. ", with your failed
/// command from 3m ago: agent wait x --for idel (exit 1)".
fn describe_attached(c: Option<&AttachedCommand>) -> String {
    let Some(c) = c else {
        return String::new();
    };
    let a = age(Time::now() - c.at);
    let when = if a == "now" {
        "just now".to_string()
    } else {
        format!("{a} ago")
    };
    format!(
        ", with your failed command from {when}: {} (exit {})",
        clip(&format!("agent {}", shell_join(&c.args)), 80),
        c.exit_code
    )
}

pub fn run_friction_add(ctx: &Ctx, args: &FrictionAddArgs) -> anyhow::Result<()> {
    let summary = args.args[0].trim().to_string();
    if summary.is_empty() {
        anyhow::bail!("the summary is empty: say in one line what got in your way");
    }
    if summary.contains(['\r', '\n']) {
        anyhow::bail!(
            "the summary must be one line; put the rest in --detail or --detail-file"
        );
    }
    if !FRICTION_KINDS.contains(&args.kind.as_str()) {
        return Err(unknown_choice("kind", &args.kind, FRICTION_KINDS));
    }
    let mut detail = args.detail.clone();
    if !args.detail_file.is_empty() {
        let data = if args.detail_file == "-" {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| anyhow::anyhow!("reading --detail-file: read /dev/stdin: {e}"))?;
            buf
        } else {
            os::read_file(&args.detail_file)
                .map_err(|e| anyhow::anyhow!("reading --detail-file: {e}"))?
        };
        detail = String::from_utf8_lossy(&data).into_owned();
    }
    let detail = detail.trim().to_string();
    let store = ctx.store()?;

    if !args.new {
        let open = store.list_friction(&FrictionFilter {
            statuses: vec![FRICTION_OPEN.to_string(), FRICTION_ACCEPTED.to_string()],
            ..Default::default()
        })?;
        let similar = similar_friction(&summary, &open);
        if !similar.is_empty() {
            let mut out = String::from("Similar entries:\n");
            for f in &similar {
                let _ = writeln!(
                    out,
                    "  #{} {} +{} {}",
                    f.id,
                    f.kind,
                    f.votes,
                    clip(&f.summary, 100)
                );
            }
            ctx.write_out(&out)?;
            let id = similar[0].id;
            return Err(anyhow::Error::new(ExitError::new(
                FRICTION_SIMILAR_EXIT_CODE,
                anyhow::anyhow!(
                    "not filed: it looks like #{id}; if it's the same, agent friction +1 {id} \"your note\"; if not, re-run with --new"
                ),
            )));
        }
    }

    let (who, parent) = friction_caller(ctx);
    let now = Time::now();
    let mut f = Friction {
        uid: new_friction_uid(&ctx.cfg().host),
        host: ctx.cfg().host.clone(),
        filed_by: who,
        parent,
        cwd: os::getwd().unwrap_or_default(),
        build: buildinfo::id(),
        created_at: now,
        updated_at: now,
        kind: args.kind.clone(),
        summary,
        detail,
        command: last_failed_command(ctx, args.no_command),
        status: FRICTION_OPEN.to_string(),
        ..Default::default()
    };
    store.add_friction(&mut f)?;
    ctx.write_out(&format!(
        "Filed friction #{} ({}){}.\n",
        f.id,
        f.kind,
        describe_attached(f.command.as_ref())
    ))
}

// `agent friction +1`

pub const FRICTION_VOTE: Doc = Doc {
    use_line: "+1 <id> [\"note\"]",
    aliases: &["vote"],
    short: "Add your vote to a friction entry",
    long: r#"+1 records that an entry got in your way too, with an optional note, and
your last failed agent command from the past 15 minutes (--no-command skips
it). A vote on a duplicate goes to its original."#,
    example: r#"  agent friction +1 12
  agent friction +1 12 "same with --next""#,
    args: ArgRule::Range(1, 2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction +1`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionVoteArgs {
    #[arg(long = "no-command", help = "Don't attach your last failed command")]
    pub no_command: bool,
    /// The positional arguments, checked against FRICTION_VOTE.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_vote_def() -> Def {
    leaf!(FRICTION_VOTE, FrictionVoteArgs, run_friction_vote)
}

pub fn run_friction_vote(ctx: &Ctx, args: &FrictionVoteArgs) -> anyhow::Result<()> {
    let mut f = get_friction(ctx, &args.args[0])?;
    let mut dup_note = String::new();
    if f.status == FRICTION_DUPLICATE {
        dup_note = format!("; #{} is a duplicate of it", f.id);
        f = get_friction(ctx, &f.duplicate_of.to_string())?;
    }
    if f.status == FRICTION_RESOLVED || f.status == FRICTION_WONTFIX {
        anyhow::bail!(
            "friction #{} is closed as {} by {} ({}){dup_note}; if it still gets in your way, file it again with agent friction add",
            f.id,
            f.status,
            f.resolved_by,
            clip(&f.resolution, 80)
        );
    }

    let (who, parent) = friction_caller(ctx);
    let mut v = FrictionVote {
        friction_id: f.id,
        uid: new_friction_uid(&ctx.cfg().host),
        host: ctx.cfg().host.clone(),
        voter: who,
        parent,
        build: buildinfo::id(),
        created_at: Time::now(),
        command: last_failed_command(ctx, args.no_command),
        ..Default::default()
    };
    if let Some(note) = args.args.get(1) {
        v.note = note.trim().to_string();
    }
    ctx.store()?.add_friction_vote(&mut v)?;
    ctx.write_out(&format!(
        "Voted on friction #{} ({}, now +{}{dup_note}){}.\n",
        f.id,
        f.kind,
        f.votes + 1,
        describe_attached(v.command.as_ref())
    ))
}

// `agent friction list`

pub const FRICTION_LIST: Doc = Doc {
    use_line: "list",
    short: "List friction entries",
    long: r#"List shows friction entries, one line each: id, status (unless open), kind,
votes, age, who filed it, and the summary. Most votes first, then newest.
By default it lists open and accepted entries.

An entry linked to the same problem on another host ends with the links,
checked over the bus: [= #34@monolith1], and a stale one is flagged, e.g.
[= #34@monolith1 !out of step] (agent friction show says what to run).

--status takes open, accepted, resolved, wontfix, duplicate, or all, or
several separated by commas."#,
    example: r#"  agent friction list
  agent friction list --status all --kind help
  agent friction list --by lead --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction list`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionListArgs {
    #[arg(
        long = "by",
        value_name = "string",
        default_value = "",
        help = "Only entries filed by this agent (\"user\" for people)",
    )]
    pub by: String,
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    #[arg(long = "kind", value_name = "string", default_value = "", help = "Only this kind")]
    pub kind: String,
    #[arg(
        long = "status",
        value_name = "string",
        default_value = "",
        help = "open, accepted, resolved, wontfix, duplicate, all; comma-separated (default: list open,accepted; export all)",
    )]
    pub status: String,
    /// The positional arguments, checked against FRICTION_LIST.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_list_def() -> Def {
    leaf!(FRICTION_LIST, FrictionListArgs, run_friction_list)
}

/// The flags list and export share (Go: frictionStatus, frictionKind,
/// frictionBy).
struct ListFilter<'a> {
    status: &'a str,
    kind: &'a str,
    by: &'a str,
}

/// An entry as list --json prints it.
#[derive(Serialize)]
struct FrictionListEntry<'a> {
    #[serde(flatten)]
    friction: &'a Friction,
    #[serde(skip_serializing_if = "Option::is_none")]
    links: Option<&'a Vec<LinkView>>,
}

pub fn run_friction_list(ctx: &Ctx, args: &FrictionListArgs) -> anyhow::Result<()> {
    let filter = ListFilter {
        status: &args.status,
        kind: &args.kind,
        by: &args.by,
    };
    let entries = list_friction(ctx, &filter, Some(&[FRICTION_OPEN, FRICTION_ACCEPTED]))?;
    let links = links_of(ctx, &entries)?;
    if args.json {
        let listed: Vec<FrictionListEntry> = entries
            .iter()
            .map(|f| FrictionListEntry {
                friction: f,
                links: links.get(&f.id),
            })
            .collect();
        return ctx.write_out(&format!("{}\n", gojson::marshal(&listed)?));
    }
    if entries.is_empty() {
        let all = ctx.store()?.list_friction(&FrictionFilter::default())?;
        if all.is_empty() {
            ctx.write_out(&format!(
                "The friction box on host {} is empty.\n",
                ctx.cfg().host
            ))?;
            ctx.hint(
                r#"if agent gets in your way, file it: agent friction add "what happened" --kind help|error|missing|bug|docs"#,
            );
            return Ok(());
        }
        let in_all = if all.len() == 1 {
            "1 entry in all".to_string()
        } else {
            format!("{} entries in all", all.len())
        };
        return ctx.write_out(&format!(
            "No {} ({in_all}: agent friction list --status all).\n",
            describe_friction_filter(&filter)
        ));
    }
    ctx.write_out(&format_friction_list(&entries, &links, &Time::now()))
}

/// Names the entries list's flags select, e.g. "open or accepted help entries
/// filed by lead".
fn describe_friction_filter(f: &ListFilter) -> String {
    let statuses = if f.status.is_empty() {
        "open or accepted".to_string()
    } else {
        let parts: Vec<&str> = f.status.split(',').collect();
        util::or_list(&parts, None)
    };
    let mut s = statuses + " ";
    if !f.kind.is_empty() {
        s += f.kind;
        s += " ";
    }
    s += "entries";
    if !f.by.is_empty() {
        s += " filed by ";
        s += f.by;
    }
    s
}

/// The error for a flag value that isn't one of choices, suggesting close
/// ones (Go: unknownChoice).
pub fn unknown_choice(what: &str, value: &str, choices: &[&str]) -> anyhow::Error {
    let similar = util::similar(value, choices);
    if !similar.is_empty() {
        return anyhow::anyhow!(
            "unknown {what} {}; did you mean {}? (want {})",
            strconv::quote(value),
            util::or_list(&similar, None),
            choices.join(", ")
        );
    }
    anyhow::anyhow!(
        "unknown {what} {} (want {})",
        strconv::quote(value),
        choices.join(", ")
    )
}

/// The entries the list and export flags select, with these statuses if
/// --status isn't given (None: all).
fn list_friction(
    ctx: &Ctx,
    f: &ListFilter,
    default_statuses: Option<&[&str]>,
) -> anyhow::Result<Vec<Friction>> {
    let mut statuses: Vec<String> = default_statuses
        .unwrap_or_default()
        .iter()
        .map(|s| s.to_string())
        .collect();
    if !f.status.is_empty() {
        statuses = parse_friction_statuses(f.status)?;
    }
    if !f.kind.is_empty() && !FRICTION_KINDS.contains(&f.kind) {
        return Err(unknown_choice("kind", f.kind, FRICTION_KINDS));
    }
    ctx.store()?.list_friction(&FrictionFilter {
        statuses,
        kind: f.kind.to_string(),
        filed_by: f.by.to_string(),
        ..Default::default()
    })
}

/// Reads a comma-separated --status; "all" is every status (empty).
fn parse_friction_statuses(s: &str) -> anyhow::Result<Vec<String>> {
    let mut statuses = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part == "all" {
            return Ok(Vec::new());
        }
        if FRICTION_STATUSES.contains(&part) {
            statuses.push(part.to_string());
        } else {
            let mut choices = FRICTION_STATUSES.to_vec();
            choices.push("all");
            return Err(unknown_choice("status", part, &choices));
        }
    }
    Ok(statuses)
}

/// Renders entries one per line under a header, with their links (by entry
/// id) after the summary. The STATUS column only appears if some entry isn't
/// open.
pub fn format_friction_list(
    entries: &[Friction],
    links: &HashMap<i64, Vec<LinkView>>,
    now: &Time,
) -> String {
    let mut id_w = "ID".len();
    let mut by_w = "BY".len();
    let mut show_status = false;
    for f in entries {
        id_w = id_w.max(format!("#{}", f.id).len());
        by_w = by_w.max(f.filed_by.len());
        show_status = show_status || f.status != FRICTION_OPEN;
    }
    let mut b = String::new();
    let mut row = |id: &str, status: &str, kind: &str, votes: &str, age: &str, by: &str, summary: &str| {
        let _ = write!(b, "{id:<id_w$} ");
        if show_status {
            let _ = write!(b, "{status:<9} ");
        }
        let _ = writeln!(b, "{kind:<7} {votes:>5} {age:>4} {by:<by_w$} {summary}");
    };
    row("ID", "STATUS", "KIND", "VOTES", "AGE", "BY", "SUMMARY");
    for f in entries {
        let status = if f.status == FRICTION_OPEN { "" } else { f.status.as_str() };
        let summary = clip(&f.summary, 100) + &format_link_marker(links.get(&f.id));
        row(
            &format!("#{}", f.id),
            status,
            &f.kind,
            &format!("+{}", f.votes),
            &age(*now - f.created_at),
            &f.filed_by,
            &summary,
        );
    }
    b
}

/// Renders a duration as a short age: "now", "5m", "3h", "12d".
pub fn age(d: TimeDelta) -> String {
    if d < TimeDelta::minutes(1) {
        "now".to_string()
    } else if d < TimeDelta::hours(1) {
        format!("{}m", d.num_minutes())
    } else if d < TimeDelta::hours(48) {
        format!("{}h", d.num_hours())
    } else {
        format!("{}d", d.num_hours() / 24)
    }
}

// `agent friction show`

pub const FRICTION_SHOW: Doc = Doc {
    use_line: "show <id>",
    short: "Show everything about a friction entry",
    long: r##"Show prints a friction entry in full: who filed it, where, and with which
build, its detail, attached command, votes, duplicates, and resolution.

Its links to the same problem on other hosts are checked over the bus and
listed with that entry's kind (in parentheses), status and votes there, e.g.
"#34@monolith1 (bug) open, +2 there: ...". A link that has gone stale is
flagged with "!" and what to run: out of step (one end closed, the other
open), one-sided (the other host has no link back), not found (no such
entry there, or a different one under its id), or unreachable (what was
last seen)."##,
    example: r#"  agent friction show 12
  agent friction show 12 --json"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction show`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionShowArgs {
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    /// The positional arguments, checked against FRICTION_SHOW.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_show_def() -> Def {
    leaf!(FRICTION_SHOW, FrictionShowArgs, run_friction_show)
}

/// Everything show prints about an entry.
#[derive(Serialize)]
pub struct FrictionDetails {
    #[serde(flatten)]
    pub friction: Friction,
    pub vote_list: Vec<FrictionVote>,
    pub duplicates: Vec<Friction>,
    /// Its links to the same problem on other hosts, checked.
    pub links: Vec<LinkView>,
}

pub fn run_friction_show(ctx: &Ctx, args: &FrictionShowArgs) -> anyhow::Result<()> {
    let f = get_friction(ctx, &args.args[0])?;
    let store = ctx.store()?;
    let vote_list = store.list_friction_votes(f.id)?;
    let duplicates = store.list_friction(&FrictionFilter {
        duplicate_of: f.id,
        ..Default::default()
    })?;
    let mut links = links_of(ctx, std::slice::from_ref(&f))?;
    let d = FrictionDetails {
        links: links.remove(&f.id).unwrap_or_default(),
        friction: f,
        vote_list,
        duplicates,
    };
    if args.json {
        return ctx.write_out(&format!("{}\n", gojson::marshal(&d)?));
    }
    ctx.write_out(&format_friction_show(&d))
}

fn format_friction_show(d: &FrictionDetails) -> String {
    let f = &d.friction;
    let mut b = String::new();
    let _ = writeln!(b, "#{} {}, {}, +{}: {}", f.id, f.kind, f.status, f.votes, f.summary);
    let _ = writeln!(
        b,
        "Filed by {}{} on {}, {}, build {}, in {}",
        f.filed_by,
        parent_note(&f.parent),
        f.host,
        stamp(&f.created_at),
        f.build,
        f.cwd
    );
    if let Some(c) = &f.command {
        let _ = writeln!(b, "Command: {}", format_attached(c, None));
    }
    if !f.detail.is_empty() {
        let _ = write!(b, "\n{}\n\n", f.detail);
    }
    if !f.accepted_by.is_empty() {
        let _ = writeln!(
            b,
            "Accepted by {}, {}{}",
            f.accepted_by,
            stamp(&f.accepted_at),
            note_part(&f.accept_note)
        );
    }
    match f.status.as_str() {
        FRICTION_RESOLVED => {
            let _ = writeln!(
                b,
                "Resolved by {}, {}{}",
                f.resolved_by,
                stamp(&f.resolved_at),
                note_part(&f.resolution)
            );
        }
        FRICTION_WONTFIX => {
            let _ = writeln!(
                b,
                "Won't fix, by {}, {}{}",
                f.resolved_by,
                stamp(&f.resolved_at),
                note_part(&f.resolution)
            );
        }
        FRICTION_DUPLICATE => {
            let _ = writeln!(
                b,
                "Duplicate of #{}, by {}, {}{}",
                f.duplicate_of,
                f.resolved_by,
                stamp(&f.resolved_at),
                note_part(&f.resolution)
            );
        }
        _ => {}
    }
    if !d.vote_list.is_empty() {
        b.push_str("Votes:\n");
        for v in &d.vote_list {
            let _ = writeln!(
                b,
                "  {} {}{}, build {}{}",
                stamp(&v.created_at),
                v.voter,
                parent_note(&v.parent),
                v.build,
                note_part(&v.note)
            );
            if let Some(c) = &v.command {
                let _ = writeln!(b, "    command: {}", format_attached(c, None));
            }
        }
    }
    if !d.duplicates.is_empty() {
        b.push_str("Duplicates:\n");
        for dup in &d.duplicates {
            let _ = writeln!(
                b,
                "  #{} +{} by {}: {}",
                dup.id, dup.votes, dup.filed_by, dup.summary
            );
        }
    }
    b.push_str(&format_friction_links(f, &d.links));
    b
}

pub fn parent_note(parent: &str) -> String {
    if parent.is_empty() {
        return String::new();
    }
    format!(" (helper of {parent})")
}

pub fn note_part(note: &str) -> String {
    if note.is_empty() {
        return String::new();
    }
    format!(": {note}")
}

/// A time as friction prints it, in local time: "2026-10-24 14:02".
pub fn stamp(t: &Time) -> String {
    t.local().0.format("%Y-%m-%d %H:%M").to_string()
}

/// Renders an attached command, e.g. "agent wait x --for idel (exit 1,
/// 2026-10-24 14:02): invalid status", with the command line wrapped in code
/// (Markdown backticks), if given.
fn format_attached(c: &AttachedCommand, code: Option<fn(&str) -> String>) -> String {
    let mut line = format!("agent {}", shell_join(&c.args));
    if let Some(code) = code {
        line = code(&line);
    }
    let mut s = format!("{line} (exit {}, {})", c.exit_code, stamp(&c.at));
    if !c.error.is_empty() {
        s += ": ";
        s += &c.error;
    }
    s
}

// `agent friction accept`

pub const FRICTION_ACCEPT: Doc = Doc {
    use_line: "accept <id>",
    short: "Mark an open friction entry as accepted",
    long: r#"Accept marks an open entry as accepted: someone means to fix it. It records
who accepted it, and an optional note."#,
    example: r#"  agent friction accept 12 --note "will fold into the wait rework""#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction accept`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionAcceptArgs {
    #[arg(
        long = "note",
        value_name = "string",
        default_value = "",
        help = "Why, or what the plan is",
    )]
    pub note: String,
    /// The positional arguments, checked against FRICTION_ACCEPT.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_accept_def() -> Def {
    leaf!(FRICTION_ACCEPT, FrictionAcceptArgs, run_friction_accept)
}

pub fn run_friction_accept(ctx: &Ctx, args: &FrictionAcceptArgs) -> anyhow::Result<()> {
    let mut f = get_friction(ctx, &args.args[0])?;
    if f.status != FRICTION_OPEN {
        anyhow::bail!(
            "friction #{} is already {}{}; accept takes open entries",
            f.id,
            f.status,
            by_whom(&f)
        );
    }
    let now = Time::now();
    f.status = FRICTION_ACCEPTED.to_string();
    f.updated_at = now;
    f.accepted_by = friction_caller(ctx).0;
    f.accepted_at = now;
    f.accept_note = args.note.trim().to_string();
    ctx.store()?.update_friction(&f)?;
    ctx.write_out(&format!(
        "Accepted friction #{} ({}): {}\n",
        f.id,
        f.kind,
        clip(&f.summary, 100)
    ))
}

/// Says who moved f to its status, e.g. " (by lead)", if anyone did.
fn by_whom(f: &Friction) -> String {
    let by = if f.status == FRICTION_ACCEPTED {
        &f.accepted_by
    } else {
        &f.resolved_by
    };
    if by.is_empty() {
        return String::new();
    }
    format!(" (by {by})")
}

// `agent friction resolve`

pub const FRICTION_RESOLVE: Doc = Doc {
    use_line: "resolve <id> (--note TEXT | --wontfix --note TEXT | --dup OTHER_ID | --linked)",
    short: "Close a friction entry: fixed, won't fix, or duplicate",
    long: r#"Resolve closes an open or accepted entry, recording who closed it:

  --note TEXT              fixed; the note says how (a commit, a command)
  --wontfix --note TEXT    not going to change; the note says why
  --dup OTHER_ID           the same as another entry here, whose votes
                           then include this one's (and which takes over
                           its links to other hosts)

Fixed or won't fix, it closes the entries linked to it on other hosts the
same way, over the agent bus, and theirs in turn; --only closes just this
one. A host that can't be reached is listed with what to run: once its bus
answers, --linked passes a closed entry's resolution on to its linked
entries still open."#,
    example: r#"  agent friction resolve 12 --note "wait names the last status (abc1234)"
  agent friction resolve 13 --wontfix --note "keys is raw by design; use send"
  agent friction resolve 14 --dup 12
  agent friction resolve 12 --linked"#,
    args: ArgRule::Exact(1),
    exclusive: &[&["wontfix", "dup"]],
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction resolve`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionResolveArgs {
    #[arg(
        long = "dup",
        value_name = "string",
        default_value = "",
        help = "Close it as a duplicate of this entry",
    )]
    pub dup: String,
    #[arg(long = "linked", help = "Pass a closed entry's resolution on to its linked entries")]
    pub linked: bool,
    #[arg(
        long = "note",
        value_name = "string",
        default_value = "",
        help = "How it was fixed, or why it won't be",
    )]
    pub note: String,
    #[arg(long = "only", help = "Resolve only this entry, not its linked entries on other hosts")]
    pub only: bool,
    #[arg(long = "wontfix", help = "Close it as won't fix")]
    pub wontfix: bool,
    /// The positional arguments, checked against FRICTION_RESOLVE.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_resolve_def() -> Def {
    leaf!(FRICTION_RESOLVE, FrictionResolveArgs, run_friction_resolve)
}

pub fn run_friction_resolve(ctx: &Ctx, args: &FrictionResolveArgs) -> anyhow::Result<()> {
    let note = args.note.trim().to_string();
    if args.linked {
        return resolve_linked_only(ctx, args, &note);
    }
    if note.is_empty() && args.dup.is_empty() {
        anyhow::bail!(
            "--note is required: say how it was fixed, or with --wontfix, why it won't be (or close it with --dup ID)"
        );
    }
    let mut f = get_friction(ctx, &args.args[0])?;
    if f.status != FRICTION_OPEN && f.status != FRICTION_ACCEPTED {
        anyhow::bail!(
            "friction #{} is already closed as {}{}",
            f.id,
            f.status,
            by_whom(&f)
        );
    }

    let mut original: Option<Friction> = None;
    if !args.dup.is_empty() {
        let mut o = get_friction(ctx, &args.dup)?;
        // Duplicates stay one level deep: point at the original's original.
        if o.status == FRICTION_DUPLICATE {
            o = get_friction(ctx, &o.duplicate_of.to_string())?;
        }
        if o.id == f.id {
            anyhow::bail!("friction #{} can't be a duplicate of itself", f.id);
        }
        original = Some(o);
    }

    let store = ctx.store()?;
    let mut had_links = false;
    if original.is_some() {
        had_links = !store.list_friction_links(f.id)?.is_empty();
    }

    let now = Time::now();
    f.updated_at = now;
    f.resolved_at = now;
    f.resolution = note;
    f.resolved_by = friction_caller(ctx).0;
    if let Some(o) = &original {
        f.status = FRICTION_DUPLICATE.to_string();
        f.duplicate_of = o.id;
    } else if args.wontfix {
        f.status = FRICTION_WONTFIX.to_string();
    } else {
        f.status = FRICTION_RESOLVED.to_string();
    }
    store.update_friction(&f)?;

    if let Some(o) = &original {
        let votes = match store.get_friction(o.id) {
            Ok(Some(updated)) => updated.votes,
            _ => o.votes,
        };
        let mut out = format!(
            "Closed friction #{} as a duplicate of #{} (now +{votes}).\n",
            f.id, o.id
        );
        if had_links {
            let _ = writeln!(out, "Its links to other hosts' entries moved to #{}.", o.id);
        }
        return ctx.write_out(&out);
    }
    if f.status == FRICTION_WONTFIX {
        ctx.write_out(&format!(
            "Closed friction #{} ({}) as wontfix.\n",
            f.id, f.kind
        ))?;
    } else {
        ctx.write_out(&format!("Resolved friction #{} ({}).\n", f.id, f.kind))?;
    }
    if args.only {
        return Ok(());
    }
    resolve_linked(ctx, &f)
}

/// resolve --linked: passes a closed entry's resolution on to its linked
/// entries still open, e.g. once a host that was unreachable when the entry
/// was resolved answers again.
fn resolve_linked_only(ctx: &Ctx, args: &FrictionResolveArgs, note: &str) -> anyhow::Result<()> {
    if !note.is_empty() || args.wontfix || !args.dup.is_empty() || args.only {
        anyhow::bail!(
            "--linked takes no --note, --wontfix, --dup or --only: it passes the entry's own resolution on to its linked entries"
        );
    }
    let f = get_friction(ctx, &args.args[0])?;
    if f.status == FRICTION_DUPLICATE {
        anyhow::bail!(
            "friction #{} is a duplicate of #{}, which holds its links: agent friction resolve {} --linked",
            f.id,
            f.duplicate_of,
            f.duplicate_of
        );
    }
    if friction_is_open(&f.status) {
        anyhow::bail!(
            "friction #{} is still {}: agent friction resolve {} --note \"...\" resolves it and its linked entries",
            f.id,
            f.status,
            f.id
        );
    }
    let links = ctx.store()?.list_friction_links(f.id)?;
    if links.is_empty() {
        anyhow::bail!(
            "friction #{} has no links to other hosts' entries (agent friction link {} ID@HOST makes one)",
            f.id,
            f.id
        );
    }
    resolve_linked(ctx, &f)
}

/// Reads the entry an id argument ("12" or "#12") names, or says why it
/// can't.
pub fn get_friction(ctx: &Ctx, arg: &str) -> anyhow::Result<Friction> {
    let id = arg
        .strip_prefix('#')
        .unwrap_or(arg)
        .parse::<i64>()
        .ok()
        .filter(|id| *id >= 1);
    let Some(id) = id else {
        anyhow::bail!(
            "{} isn't a friction id: ids are numbers, like 12 or #12 (agent friction list shows them)",
            strconv::quote(arg)
        );
    };
    let store = ctx.store()?;
    if let Some(f) = store.get_friction(id)? {
        return Ok(f);
    }
    let last = store.last_friction_id()?;
    if last == 0 {
        anyhow::bail!("no friction #{id}: nothing has been filed on this host yet");
    }
    anyhow::bail!("no friction #{id} (latest is #{last}); agent friction list --status all lists them")
}

// `agent friction export`

pub const FRICTION_EXPORT: Doc = Doc {
    use_line: "export",
    short: "Write the friction box as Markdown",
    long: r#"Export writes friction entries as Markdown to stdout, one section per
entry, grouped by status. The resolved section is the tool's record of what
changed and who asked. By default it writes every entry; --status as for
list."#,
    example: r#"  agent friction export
  agent friction export --status resolved"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction export`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionExportArgs {
    #[arg(
        long = "by",
        value_name = "string",
        default_value = "",
        help = "Only entries filed by this agent (\"user\" for people)",
    )]
    pub by: String,
    #[arg(long = "kind", value_name = "string", default_value = "", help = "Only this kind")]
    pub kind: String,
    #[arg(
        long = "status",
        value_name = "string",
        default_value = "",
        help = "open, accepted, resolved, wontfix, duplicate, all; comma-separated (default: list open,accepted; export all)",
    )]
    pub status: String,
    /// The positional arguments, checked against FRICTION_EXPORT.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_export_def() -> Def {
    leaf!(FRICTION_EXPORT, FrictionExportArgs, run_friction_export)
}

pub fn run_friction_export(ctx: &Ctx, args: &FrictionExportArgs) -> anyhow::Result<()> {
    let filter = ListFilter {
        status: &args.status,
        kind: &args.kind,
        by: &args.by,
    };
    let entries = list_friction(ctx, &filter, None)?;
    let links = ctx.store()?.list_friction_links(0)?;
    let mut by_entry: HashMap<i64, Vec<FrictionLink>> = HashMap::new();
    for l in links {
        by_entry.entry(l.friction_id).or_default().push(l);
    }
    ctx.write_out(&format_friction_export(&entries, &by_entry, &ctx.cfg().host))
}

/// Wraps s in a code span, with a fence of more backticks than s has in a
/// row.
pub fn markdown_code(s: &str) -> String {
    let mut fence = "`".to_string();
    while s.contains(&fence) {
        fence.push('`');
    }
    let s = if s.starts_with('`') || s.ends_with('`') {
        format!(" {s} ")
    } else {
        s.to_string()
    };
    format!("{fence}{s}{fence}")
}

fn friction_status_heading(status: &str) -> &'static str {
    match status {
        FRICTION_OPEN => "Open",
        FRICTION_ACCEPTED => "Accepted",
        FRICTION_RESOLVED => "Resolved",
        FRICTION_WONTFIX => "Won't fix",
        FRICTION_DUPLICATE => "Duplicates",
        _ => "",
    }
}

/// Renders entries as Markdown, a section per status in lifecycle order,
/// keeping the entries' order within each, with their links (by entry id) as
/// last seen.
pub fn format_friction_export(
    entries: &[Friction],
    links: &HashMap<i64, Vec<FrictionLink>>,
    host: &str,
) -> String {
    let mut b = String::new();
    let _ = writeln!(b, "# Friction on {host}");
    if entries.is_empty() {
        b.push_str("\nNo entries.\n");
    }
    for status in FRICTION_STATUSES {
        let group: Vec<&Friction> = entries.iter().filter(|f| f.status == *status).collect();
        if group.is_empty() {
            continue;
        }
        let _ = writeln!(b, "\n## {}", friction_status_heading(status));
        for f in group {
            let _ = write!(b, "\n### #{} {}\n\n", f.id, f.summary);
            let _ = writeln!(
                b,
                "{}, +{}. Filed by {}{}, {}, build {}.",
                f.kind,
                f.votes,
                f.filed_by,
                parent_note(&f.parent),
                stamp(&f.created_at),
                f.build
            );
            if !f.accepted_by.is_empty() {
                let _ = writeln!(
                    b,
                    "Accepted by {}, {}{}.",
                    f.accepted_by,
                    stamp(&f.accepted_at),
                    note_part(&f.accept_note)
                );
            }
            match f.status.as_str() {
                FRICTION_RESOLVED => {
                    let _ = writeln!(
                        b,
                        "Resolved by {}, {}{}.",
                        f.resolved_by,
                        stamp(&f.resolved_at),
                        note_part(&f.resolution)
                    );
                }
                FRICTION_WONTFIX => {
                    let _ = writeln!(
                        b,
                        "Won't fix ({}, {}){}.",
                        f.resolved_by,
                        stamp(&f.resolved_at),
                        note_part(&f.resolution)
                    );
                }
                FRICTION_DUPLICATE => {
                    let _ = writeln!(
                        b,
                        "Duplicate of #{} ({}, {}){}.",
                        f.duplicate_of,
                        f.resolved_by,
                        stamp(&f.resolved_at),
                        note_part(&f.resolution)
                    );
                }
                _ => {}
            }
            b.push_str(&format_export_links(links.get(&f.id)));
            if let Some(c) = &f.command {
                let _ = writeln!(b, "\nCommand: {}", format_attached(c, Some(markdown_code)));
            }
            if !f.detail.is_empty() {
                let _ = writeln!(b, "\n{}", f.detail);
            }
        }
    }
    b
}

#[cfg(test)]
#[path = "friction_test.rs"]
pub(crate) mod friction_test;

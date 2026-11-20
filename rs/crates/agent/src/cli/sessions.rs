//! `agent sessions` (Go: internal/cli/sessions.go).
use std::collections::{HashMap, HashSet};
use std::io::Write;
#[cfg(test)]
use std::sync::{Arc, LazyLock, Mutex};

use agent_base::gostd::{filepath, os as goos, time as gotime};
use agent_base::types::Status;
use agent_bus::Context;
use agent_discovery::{self as discovery, Dir, Query, Session};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::export::export_deps;
use super::flags;
use super::output::plural;
use super::root::Ctx;
use super::transcript::{live_now, session_dirs, utc_or_none};

// `agent sessions`

pub const DOC: Doc = Doc {
    use_line: "sessions",
    short: "List the agent sessions on this host, spawned by agent or not: the newest 25, or filtered",
    long: r#"Sessions lists the agent sessions this host can see, newest first: the
transcripts Claude Code, agy and Kimi Code keep of their sessions, whether
"agent spawn" started the session (a workgroup agent) or a person ran the
CLI in a terminal. Both kinds have the same fields; WORKGROUP names the
agent that owns a session, or is "-". A subagent's session (a Claude Task,
an agy or Kimi subagent) is a session too, listed under the one that
started it.

  WHEN              HARNESS  PROJECT  STATUS  WORKGROUP  TITLE
  2026-10-25 13:35  claude   gomuxai  idle    lead-api   Fix the auth tests
  2026-10-25 13:30  claude   gomuxai  ended (quit)      -          Plan the export
  └ 10-25 13:31     claude   gomuxai  ended (finished)  -          Explore internal/export

The table shows the newest 25 sessions, each with its subagents, and a
hint on stderr says how many older ones it left out: --limit N shows the
newest N, --all every one. --json prints every session unless --limit
says how many, so a script or display reading it is never cut short.

Filters (they combine, and apply to --json too):
  --since WHEN     written since then: a duration back from now (30m, 2h,
                   3d, 1w) or a local date and time (2026-11-01,
                   "2026-11-01 13:30", 2026-11-01T13:30:00Z)
  --project P      a path (it has a slash, or is ., .. or ~): run in that
                   directory or one inside it, symlinks resolved; anything
                   else: PROJECT contains P, ignoring case
  --harness H      claude, agy, kimi, or a workgroup agent's backend that
                   keeps no transcript (aider)
  --workgroup      owned by a workgroup agent; --local: not
A filter and the limit pick sessions by the one at the top of their
group: a subagent goes with the session that started it. A live agent's
session with no transcript yet counts as written now.

WHEN is the transcript's last write, in local time. PROJECT is the last
part of the working directory the session recorded. TITLE is the newest
title the harness recorded: for Claude, a custom title (/rename) over the
AI-written one; agy's annotation title; Kimi's state.json title; a Claude
subagent's description when it has none.

STATUS (idle, processing, waiting, error, ended, unknown; an ended session
shows its end reason, as in "ended (gone)"; "agent statuses" says what each
means):
  - a workgroup agent's current session: its last report, reconciled with
    Claude Code's session registry for Claude by the rule agent status uses:
    the registry's status when a running process holds the session (except
    a "waiting" within 5s of a deny), ended (gone) when none does but the
    report says it is alive. agy: ended (gone) once agy no longer holds the
    conversation open. agent status also reads the screen; sessions doesn't;
  - an agent's earlier sessions (before /clear, a resume, a respawn, or a
    CLI started anew in its pane): ended (replaced); its last one, once
    cleanup removed it: ended (closed);
  - Claude, no workgroup: the registry's status, or ended (gone) if no
    running process holds the session (unknown if the registry can't be
    read);
  - agy, no workgroup: unknown while agy holds the conversation open (its
    presence lock; idle or busy can't be told), else ended (gone);
  - Kimi, which records nothing about running: unknown if its transcript
    changed in the last 10 minutes, else ended (gone);
  - a subagent: ended (finished) if it recorded finishing, ended (parent)
    if its parent session ended; else unknown.
Subagents are listed under their parent, oldest last write first.

Where it looks (each harness's own default dirs that exist):
  claude  $CLAUDE_CONFIG_DIR/projects, else ~/.claude/projects
  agy     ~/.gemini/antigravity/brain (desktop), ~/.gemini/antigravity-cli/brain (CLI)
  kimi    ~/.kimi-code/sessions
In the JSON app config (--backend-config), claude_projects_dir,
agy_brain_dir and kimi_sessions_dir replace a harness's dirs with one path,
and "" turns the harness off. A workgroup agent's transcript elsewhere is
listed too. --dirs prints the directories it scans (with --json: each one's
harness, path, origin, config or default, and whether it exists).

It sorts by the files' last writes from directory listings, then reads
only the transcripts it shows (their head and tail), and for --project the
Claude ones whose project folder, named for the directory the session
started in, could match.

Workgroup agents come from this binary's store and the stores its termd
sessions point at, live and departed, as agent export --all reads them.
This host only. --json prints each session as docs/EVENTS.md describes a
Session: the "session" line agent transcript opens with."#,
    example: r#"  agent sessions
  agent sessions --all
  agent sessions --since 2h --project .
  agent sessions --project gomuxai --limit 5
  agent sessions --workgroup
  agent sessions --local --harness claude
  agent sessions --json --since 1d"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent sessions`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "all",
        help = "Every session, however many (the table shows the newest 25 without it)",
    )]
    pub all: bool,
    #[arg(
        long = "dirs",
        help = "Print the directories sessions scans instead of the sessions (--json: harness, path, origin, exists)",
    )]
    pub dirs: bool,
    #[arg(
        long = "harness",
        value_name = "string",
        default_value = "",
        help = "Only this harness's sessions: claude, agy, kimi, or the backend of a workgroup agent with no transcript (aider)",
    )]
    pub harness: String,
    #[arg(long = "json", help = "Print the sessions as a JSON array")]
    pub json: bool,
    #[arg(
        long = "limit",
        short = 'n',
        value_name = "int",
        default_value = "0",
        value_parser = flags::parse_int,
        help = "Only the newest N sessions, each with its subagents (the table's default: 25; --json's: all)",
    )]
    pub limit: i64,
    #[arg(long = "local", help = "Only sessions no workgroup agent owns")]
    pub local: bool,
    #[arg(
        long = "project",
        value_name = "string",
        default_value = "",
        help = "Only sessions run in this directory or inside it (a path: ., ~/src/app, /srv/x), or whose PROJECT contains this text, ignoring case",
    )]
    pub project: String,
    #[arg(
        long = "since",
        value_name = "string",
        default_value = "",
        help = r#"Only sessions written since then: a duration back from now (30m, 2h, 3d, 1w) or a local date and time (2026-11-01, "2026-11-01 13:30")"#,
    )]
    pub since: String,
    #[arg(long = "workgroup", help = "Only sessions a workgroup agent owns")]
    pub workgroup: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// How many sessions the table shows without --limit or --all.
const SESSIONS_DEFAULT_LIMIT: usize = 25;

/// Lists the Claude sessions running on this host (Go: claudeSessions, a
/// package variable for tests). A test puts in a fake for its data dir
/// (set_claude_sessions); without one, a test reads the isolated (empty)
/// session registry testenv sets up, never this machine's.
pub(crate) fn claude_sessions(ctx: &Ctx) -> anyhow::Result<HashMap<String, Status>> {
    #[cfg(test)]
    {
        let fake = CLAUDE_SESSIONS_FAKES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ctx.cfg().database_path)
            .cloned();
        if let Some(f) = fake {
            return f();
        }
        crate::cli::testenv::isolate();
    }
    #[cfg(not(test))]
    let _ = ctx;
    agent_backend::namedagents::claude_sessions()
}

/// A stand-in for claude_sessions in tests.
#[cfg(test)]
pub(crate) type ClaudeSessionsFn = Arc<dyn Fn() -> anyhow::Result<HashMap<String, Status>> + Send + Sync>;

/// Each test's claude_sessions, by its database path.
#[cfg(test)]
static CLAUDE_SESSIONS_FAKES: LazyLock<Mutex<HashMap<String, ClaudeSessionsFn>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Makes claude_sessions call f for the store at database_path (tests).
#[cfg(test)]
pub(crate) fn set_claude_sessions(database_path: &str, f: ClaudeSessionsFn) {
    CLAUDE_SESSIONS_FAKES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(database_path.to_string(), f);
}

/// A directory agent sessions scans, as --dirs --json prints it (Go:
/// sessionDir).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(crate) struct SessionDir {
    #[serde(flatten)]
    pub dir: Dir,
    pub exists: bool,
}

/// Prints the directories agent sessions scans: the line its table opens
/// with, or a JSON array (Go: printDirs).
fn print_dirs(ctx: &Ctx, json: bool, dirs: &[Dir]) -> anyhow::Result<()> {
    if json {
        let list: Vec<SessionDir> = dirs
            .iter()
            .map(|d| SessionDir {
                dir: d.clone(),
                exists: std::fs::metadata(&d.path).is_ok_and(|i| i.is_dir()),
            })
            .collect();
        return ctx.print_json(&list);
    }
    let home = goos::user_home_dir().unwrap_or_default();
    ctx.write_out(&format!("Looked in: {}\n", discovery::describe_dirs(dirs, &home)))
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    if args.workgroup && args.local {
        anyhow::bail!("--workgroup and --local each leave out what the other keeps: use one (or neither, for all)");
    }
    let q = sessions_query(ctx, args, Local::now())?;
    let dirs = session_dirs(ctx);
    if args.dirs {
        return print_dirs(ctx, args.json, &dirs);
    }
    let agents = workgroup_agents(ctx)?;
    let (live, claude_err) = live_now(ctx);
    let mut index = discovery::new_index(&dirs, &agents);
    // A workgroup agent with no transcript is listed under its backend's name
    // (aider, say), so that is a harness to filter on too.
    let harnesses = index.harnesses();
    if !q.harness.is_empty() && !harnesses.contains(&q.harness) {
        anyhow::bail!(
            "--harness {}: no session has that harness; use {}",
            q.harness,
            harnesses.join(", ")
        );
    }
    let (picked, left) = index.select(&q);
    let sessions = index.resolve(&picked, &live);
    if let Some(e) = &claude_err
        && sessions.iter().any(|s| s.harness == discovery::HARNESS_CLAUDE)
    {
        ctx.hint(format!("Claude sessions' status is unknown or their last report: {e:#}"));
    }
    if left > 0 {
        ctx.hint(format!(
            "showing the newest {} sessions and their subagents; {left} older left out: --all lists every one, --limit N the newest N",
            q.limit
        ));
    }
    if args.json {
        return ctx.print_json(&sessions);
    }
    let home = goos::user_home_dir().unwrap_or_default();
    let mut out = Vec::new();
    print_sessions(&mut out, &sessions, &discovery::describe_dirs(&dirs, &home))?;
    ctx.out().write_all(&out)?;
    Ok(())
}

/// What agent sessions' flags pick, at now (Go: sessionsQuery).
pub(crate) fn sessions_query(ctx: &Ctx, args: &Args, now: DateTime<Local>) -> anyhow::Result<Query> {
    let mut q = Query {
        harness: args.harness.clone(),
        workgroup: args.workgroup,
        local: args.local,
        ..Query::default()
    };
    let limit_set = ctx.changed("limit");
    if args.all && limit_set {
        anyhow::bail!("--all and --limit don't go together: --all lists every session, --limit N the newest N");
    } else if limit_set && args.limit < 1 {
        anyhow::bail!("--limit {}: give 1 or more (--all lists every session)", args.limit);
    } else if limit_set {
        q.limit = args.limit as usize;
    } else if !args.all && !args.json {
        // Scripts and displays read --json: it is never cut unasked.
        q.limit = SESSIONS_DEFAULT_LIMIT;
    }
    if !args.since.is_empty() {
        q.since = Some(parse_since(&args.since, now)?);
    }
    let mut p = args.project.clone();
    if !p.is_empty() {
        if looks_like_dir(&p) {
            let home = goos::user_home_dir().unwrap_or_default();
            if p == "~" {
                p = home;
            } else if let Some(rest) = p.strip_prefix("~/")
                && !home.is_empty()
            {
                p = filepath::join(&[home.as_str(), rest]);
            }
            p = filepath::abs(&p).map_err(|e| anyhow::anyhow!("--project {}: {e:#}", args.project))?;
        }
        q.project = p;
    }
    Ok(q)
}

/// Whether a --project value is a directory rather than part of a project
/// name: it has a slash, or is ., .. or ~ (Go: looksLikeDir).
fn looks_like_dir(p: &str) -> bool {
    p.contains('/') || p == "." || p == ".." || p == "~"
}

/// Reads --since: a duration back from now (a Go duration, or a whole number
/// of days or weeks: 3d, 1w), or a local date, or date and time (Go:
/// parseSince).
pub(crate) fn parse_since(v: &str, now: DateTime<Local>) -> anyhow::Result<DateTime<Utc>> {
    let v = v.trim();
    let n = v.trim_end_matches(['d', 'w']);
    let unit = v.trim_start_matches(|c: char| c.is_ascii_digit());
    if unit.len() == 1
        && !n.is_empty()
        && n != v
        && let Ok(days) = go_atoi(n)
    {
        let days = if unit == "w" { days * 7 } else { days };
        return Ok(add_days(now, -days).with_timezone(&Utc));
    }
    if let Ok(d) = gotime::parse_duration_nanos(v) {
        if d < 0 {
            anyhow::bail!("--since {v}: give how long ago without a minus sign (2h, 3d)");
        }
        return Ok(now.with_timezone(&Utc) - chrono::Duration::nanoseconds(d));
    }
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d")
        && go_digits(v, "2006-01-02")
        && let Some(t) = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap_or_default()).earliest()
    {
        return Ok(t.with_timezone(&Utc));
    }
    for (layout, go) in [
        ("%Y-%m-%d %H:%M", "2006-01-02 15:04"),
        ("%Y-%m-%dT%H:%M", "2006-01-02T15:04"),
        ("%Y-%m-%d %H:%M:%S", "2006-01-02 15:04:05"),
        ("%Y-%m-%dT%H:%M:%S", "2006-01-02T15:04:05"),
    ] {
        if go_digits(v, go)
            && let Ok(t) = NaiveDateTime::parse_from_str(v, layout)
            && let Some(t) = Local.from_local_datetime(&t).earliest()
        {
            return Ok(t.with_timezone(&Utc));
        }
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Ok(t.with_timezone(&Utc));
    }
    anyhow::bail!(
        "--since {v}: give a duration back from now (30m, 2h, 3d, 1w) or a local date and time (2026-11-01, \"2026-11-01 13:30\")"
    )
}

/// Whether v has the shape of Go's layout: a digit wherever the layout has one,
/// and the same other characters (Go's fixed-width fields take no more or
/// fewer digits, as chrono's do).
fn go_digits(v: &str, layout: &str) -> bool {
    v.len() == layout.len()
        && v
            .bytes()
            .zip(layout.bytes())
            .all(|(a, b)| if b.is_ascii_digit() { a.is_ascii_digit() } else { a == b })
}

/// Go's strconv.Atoi.
fn go_atoi(s: &str) -> Result<i64, ()> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    s.parse::<i64>().map_err(|_| ())
}

/// Go's t.AddDate(0, 0, days): the same wall clock time, days later.
fn add_days(t: DateTime<Local>, days: i64) -> DateTime<Local> {
    let naive = t.naive_local() + chrono::Duration::days(days);
    Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap_or_else(|| t + chrono::Duration::days(days))
}

/// Reads every run export --all sees on this host, with the transcript
/// history each name recorded (Go: workgroupAgents).
pub(crate) fn workgroup_agents(ctx: &Ctx) -> anyhow::Result<Vec<discovery::Agent>> {
    let data_dir = ctx.data_dir();
    let mut tree = agent_export::Tree::new(export_deps(ctx), std::slice::from_ref(&data_dir), &[], true);
    let snap = tree.snapshot(&Context::background());
    tree.close();
    let snap = snap?;
    let own = filepath::clean(&data_dir);
    // By store and name: one list per name.
    let mut history: HashMap<String, Vec<String>> = HashMap::new();
    let mut agents = Vec::new();
    for a in snap.agents {
        let key = format!("{}\0{}", a.store, a.name);
        if !history.contains_key(&key) {
            // Another binary's store keeps the list where its default config
            // does: beside its database, under logs/.
            let list = if a.store == own {
                ctx.cfg().transcripts_path(&a.name)
            } else {
                filepath::join(&[a.store.as_str(), "logs", &format!("{}.transcripts", a.name)])
            };
            let refs: Vec<String> = agent_service::read_transcript_refs(&list)
                .into_iter()
                .map(|r| r.path)
                .collect();
            history.insert(key.clone(), refs);
        }
        agents.push(discovery::Agent {
            name: a.name,
            parent: a.parent,
            store: a.store,
            host: a.host,
            backend: a.backend,
            live: a.live,
            created_at: utc_or_none(&a.created_at),
            ended_at: utc_or_none(&a.ended_at),
            status: a.status,
            end_reason: a.end_reason,
            event: a.event,
            status_at: utc_or_none(&a.status_at),
            session_id: a.session_id,
            transcript: a.transcript,
            history: history.get(&key).cloned().unwrap_or_default(),
        });
    }
    Ok(agents)
}

/// Prints the sessions for a person: one line each, newest first, with each
/// one's subagents right under it (Go: printSessions).
pub(crate) fn print_sessions(out: &mut dyn Write, sessions: &[Session], looked: &str) -> std::io::Result<()> {
    writeln!(out, "Looked in: {looked}")?;
    if sessions.is_empty() {
        writeln!(out, "No sessions.")?;
        return Ok(());
    }
    let key = |harness: &str, id: &str| format!("{harness}\0{id}");
    let ids: HashSet<String> = sessions.iter().map(|s| key(&s.harness, &s.session_id)).collect();
    let mut subs: HashMap<String, Vec<&Session>> = HashMap::new();
    let mut top: Vec<&Session> = Vec::new();
    for s in sessions {
        if let Some(p) = &s.parent_session
            && ids.contains(&key(&s.harness, &p.session_id))
        {
            subs.entry(key(&s.harness, &p.session_id)).or_default().push(s);
            continue;
        }
        top.push(s);
    }
    writeln!(out)?;
    let mut rows: Vec<Vec<String>> = vec![
        ["WHEN", "HARNESS", "PROJECT", "STATUS", "WORKGROUP", "TITLE"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    ];
    // The lines in print order: each top session, then its subagents.
    let mut order: Vec<(String, &Session)> = Vec::new();
    let mut printed: HashSet<String> = HashSet::new();
    fn kids_of<'a>(
        s: &Session,
        depth: usize,
        subs: &HashMap<String, Vec<&'a Session>>,
        printed: &mut HashSet<String>,
        order: &mut Vec<(String, &'a Session)>,
    ) {
        let key = format!("{}\0{}", s.harness, s.session_id);
        if !printed.insert(key.clone()) {
            return;
        }
        let mut kids: Vec<&Session> = subs.get(&key).cloned().unwrap_or_default();
        kids.reverse(); // sessions are newest first; kids read oldest last write first
        for k in kids {
            order.push((
                format!("{}└ {}", "  ".repeat(depth), session_when(k.mtime, "%m-%d %H:%M")),
                k,
            ));
            kids_of(k, depth + 1, subs, printed, order);
        }
    }
    for s in &top {
        order.push((session_when(s.mtime, "%Y-%m-%d %H:%M"), s));
        kids_of(s, 0, &subs, &mut printed, &mut order);
    }
    let mut owned = 0;
    for (when, s) in order {
        let mut group = "-".to_string();
        if let Some(w) = &s.workgroup {
            group = w.name.clone();
            owned += 1;
        }
        let mut title = s.title.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            title = if s.path.is_empty() {
                "(no transcript yet)".to_string()
            } else {
                format!("({})", s.session_id)
            };
        }
        let mut status = s.status.clone();
        if !s.end_reason.is_empty() {
            status += &format!(" ({})", s.end_reason);
        }
        let project = if s.project.is_empty() { "-".to_string() } else { s.project.clone() };
        rows.push(vec![when, s.harness.clone(), project, status, group, title]);
    }
    out.write_all(tab_align(&rows, 2).as_bytes())?;
    writeln!(out, "\n{}, {owned} in a workgroup.", plural(sessions.len(), "session"))
}

/// Go's text/tabwriter with minwidth 0, tabwidth 0, padding pad, padchar ' '
/// and no flags, for rows of cells that each end in a tab but the last: each
/// column but the last is as wide as its widest cell (in runes) plus pad.
pub(crate) fn tab_align(rows: &[Vec<String>], pad: usize) -> String {
    let cols = rows.iter().map(|r| r.len().saturating_sub(1)).max().unwrap_or(0);
    let mut widths = vec![0; cols];
    for r in rows {
        for (j, c) in r.iter().take(r.len().saturating_sub(1)).enumerate() {
            widths[j] = widths[j].max(c.chars().count() + pad);
        }
    }
    let mut out = String::new();
    for r in rows {
        for (j, c) in r.iter().enumerate() {
            out.push_str(c);
            if j + 1 < r.len() {
                out.push_str(&" ".repeat(widths[j] - c.chars().count()));
            }
        }
        out.push('\n');
    }
    out
}

fn session_when(t: Option<DateTime<Utc>>, layout: &str) -> String {
    match t {
        None => "-".to_string(),
        Some(t) => t.with_timezone(&Local).format(layout).to_string(),
    }
}

#[cfg(test)]
#[path = "sessions_test.rs"]
pub(crate) mod sessions_test;

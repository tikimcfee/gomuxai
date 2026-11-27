//! `agent transcript` (Go: internal/cli/transcript.go, and
//! transcript_poll.go in transcript_poll.rs).
use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_base::gostd::os::PathError;
use agent_base::gostd::{filepath, strconv};
use agent_bus::Context;
use agent_discovery::{self as discovery, Agent as DiscoveryAgent, Index, Live, Report, Session};
use agent_service::{STATE_KEY_EVENT, STATE_KEY_RESULT, STATE_KEY_SESSION_ID, STATE_KEY_STATUS, STATE_KEY_TRANSCRIPT};
use agent_store::{Store, Time};
use agent_transcript::{self as transcript, Event, Finals, Parser, Reader, TurnRef};
use chrono::{DateTime, FixedOffset, Utc};

use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::logs::{abs_path, typed_json_line};
use super::root::{Ctx, CtxShare};
use super::sessions::{claude_sessions, workgroup_agents};

#[path = "transcript_poll.rs"]
mod transcript_poll;
pub(crate) use transcript_poll::reader_gone;

// `agent transcript`

pub const DOC: Doc = Doc {
    use_line: "transcript <name|session-id|path>",
    short: "Print one session's events as JSON lines; --follow streams them as they happen",
    long: r#"Transcript prints one agent session's events as JSON lines, as
docs/EVENTS.md describes them: a "session" line (the session as agent
sessions --json lists it), then an "event" line for each prompt, thinking
block, reply, tool call and harness record, in transcript order. An event
that changes later (a tool's result lands) is sent again, whole, with the
same id and "op":"update". Displays and scripts read this; agent logs is
the view for a person.

The session is one of:
  - a workgroup agent's name: its current transcript (after cleanup, its
    last one);
  - a session id, or the start of one that only one session's id starts
    with (agent sessions lists them, agent sessions --json their ids),
    with or without its harness first (claude:3f3a9c, as dreams cite one);
  - a transcript's path; its harness is told from the path, or from its
    first lines.

--talk sends only prompts, replies and tool calls, each call without its
output (its result's status only), for a reader that wants what was said.
--results whole|once|none says how much of a tool's result an event
carries: whole (the default) has its text and the harness's structured
copy, which often repeats the text; once keeps only the text (or the
structured copy when there is no text); none keeps only the status.
--talk implies none unless --results says otherwise.

--scrub drops from each event what may hold a secret or someone else's
words: pasted and tagged blocks, code fences, quotes, key blocks, and
lines with a credential, an email address or a sign-in URL, from its text
and tool output; a tool's input, structured result and meta; a prompt's
images. A dream's own agents always read it scrubbed, and so should
anything that mines other sessions' transcripts.

--from <byte> resumes after the line that starts at that byte offset: pass
the "at" of the last event you hold. That line's events and those before it
aren't sent again. The ones after have the seq, line and turn a whole read
gives them, and updates to earlier events still come (whole, so you needn't
hold what they update). -1, the default, reads from the start.
--until <byte> leaves out the lines that start at or after it, so --from A
--until B reads the lines after A's and before B. The rest is still read,
unsent: an event of the window that a prompt past it abandons (an edited
prompt sent again) comes with "abandoned": true.

--follow keeps reading as the transcript grows, until Ctrl-C or until the
reader closes the pipe (both exit 0; a closed pipe is noticed within
250ms even when there is nothing to write). It polls the file every
250ms, and never reads a line that isn't whole yet. A transcript that is
removed, replaced by another file, or rewritten in place ends it with an
error. It goes on after the session ends: a resumed session writes on.
A running session's transcript that isn't written yet (Claude Code writes
it at the first prompt) is waited for, as tail -F does, and streamed once
it is there; the wait ends with an error if the session exits first.

A new session line comes whenever anything but the size and times
changed. The title, model, version, branch and agent name come from the
lines the stream read, so after the initial read a session line may
correct what the opening one had from the transcript's head and tail; a
known value never becomes empty. Status: for an agent of this store, its
last report, read every 2s; otherwise, and to reconcile with the
harness (Claude Code's session registry), a full check like agent sessions' every
30s, backing off to 5 minutes while the transcript doesn't grow.

Claude Code writes a turn's closing reply only after its Stop hook has run,
and the hook has the text already. For a Claude agent of this store,
transcript sends that reply at once, as a provisional say (id
"<turn id>#final", "provisional":true, "at" where its line will land), and
the transcript's own copy as an update of it once it lands. A provisional
reply isn't in the transcript, so it takes no seq of its own: it has the
seq of the event that comes next. Don't resume --from its "at", and drop
the provisional replies you hold when you resume: one still waiting for
its copy is sent again.

--requests adds the request channel: what each model call was sent (Claude
Code's api-request lines). Request events have their own seq, so the
conversation's seq is the same with or without it.

This host only; for another host's agent, agent capture name@host shows its
screen."#,
    example: r#"  agent transcript lead-api
  agent transcript lead-api --follow
  agent transcript 3f3a9c --from 18342 --until 40960
  agent transcript ~/.claude/projects/-home-user-shop/3f3a9c1e-7a2b-4c4d-9e0f-1a2b3c4d5e6f.jsonl --requests"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent transcript`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "follow", help = "Keep reading as the transcript grows, until interrupted")]
    pub follow: bool,
    #[arg(
        long = "from",
        value_name = "int",
        default_value = "-1",
        value_parser = flags::parse_int,
        help = "Resume after the line starting at this byte offset (the at of the last event you hold)",
    )]
    pub from: i64,
    #[arg(long = "requests", help = "Add the request channel: what each model call was sent")]
    pub requests: bool,
    #[arg(
        long = "talk",
        help = "Only prompts, replies and tool calls, the calls without their output (--results none)"
    )]
    pub talk: bool,
    #[arg(
        long = "results",
        value_name = "string",
        default_value = "whole",
        help = "A tool result: whole (its text and the harness's structured copy), once (its text only), or none (its status only)"
    )]
    pub results: String,
    #[arg(
        long = "scrub",
        help = "Drop pasted blocks, code, keys, credentials, sign-in URLs and tool inputs (always, for a dream's agent)"
    )]
    pub scrub: bool,
    #[arg(
        long = "until",
        value_name = "int",
        default_value = "-1",
        value_parser = flags::parse_int,
        help = "Leave out the lines starting at or after this byte offset (not with --follow)",
    )]
    pub until: i64,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// How often --follow reads what the transcript appended (and looks for a
/// provisional reply, and whether the reader has gone); reads the last report
/// of the agent of this store that owns the session; and resolves the
/// session's status in full, at first and at most (see follow). Go's package
/// variables, which its tests change; here StreamOptions carries them.
#[derive(Debug, Clone, Copy)]
pub struct Intervals {
    pub poll: Duration,
    pub session: Duration,
    pub status: Duration,
    pub status_max: Duration,
}

impl Default for Intervals {
    fn default() -> Self {
        Intervals {
            poll: Duration::from_millis(250),
            session: Duration::from_secs(2),
            status: Duration::from_secs(30),
            status_max: Duration::from_secs(5 * 60),
        }
    }
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let mut out = ctx.out();
    let gone: ReaderGone = Arc::new(|| reader_gone(libc::STDOUT_FILENO));
    run_transcript(ctx, args, &mut **out, Some(gone), Intervals::default())
}

/// run, writing the stream to out, and asking reader_gone whether out's
/// reader has closed its end (Go: runTranscript, on os.Stdout).
pub(crate) fn run_transcript(
    ctx: &Ctx,
    args: &Args,
    out: &mut dyn Write,
    reader_gone: Option<ReaderGone>,
    intervals: Intervals,
) -> anyhow::Result<()> {
    let mut opts = StreamOptions {
        from: args.from,
        until: -1,
        follow: args.follow,
        requests: args.requests,
        reader_gone: None,
        intervals,
        shape: Shape {
            talk: args.talk,
            results: Results::parse(&args.results, args.talk && !ctx.changed("results"))?,
            scrub: args.scrub || super::logs::in_dream(ctx),
        },
    };
    if opts.shape.scrub && opts.requests {
        anyhow::bail!(
            "--requests sends what each model call was sent, whole, which can't be scrubbed: leave it out (a dream's agent reads transcripts scrubbed)"
        );
    }
    if opts.from < -1 {
        anyhow::bail!(
            "--from {}: give the byte offset of a line's start (the at of an event), or -1 for the start",
            opts.from
        );
    }
    if ctx.changed("until") {
        if opts.follow {
            anyhow::bail!(
                "--until ends the read at an offset, and --follow reads on as the transcript grows: use one"
            );
        } else if args.until < 0 {
            anyhow::bail!(
                "--until {}: give a byte offset, 0 or more (leave --until out to read to the end)",
                args.until
            );
        } else if args.until <= opts.from {
            anyhow::bail!(
                "--until {} is not after --from {}: the window is the lines after --from's and before --until",
                args.until,
                opts.from
            );
        }
        opts.until = args.until;
    }
    // A reader closing the pipe is how a display says it's done. With
    // SIGPIPE ignored, a write to a closed pipe fails with EPIPE instead of
    // killing the process (as it kills every other command: output::GoStdio):
    // on stdout that ends the stream, and a notice on a closed stderr is just
    // lost. While there is nothing to write, --follow asks the pipe whether
    // its reader has gone.
    super::output::ignore_sigpipe();
    let (bctx, cancel) = Context::with_cancel(&Context::background());
    let _interrupt = Interrupt::notify(cancel);
    opts.reader_gone = reader_gone;
    let arg = &args.args[0];
    let mut target = resolve_transcript(ctx, arg)?;
    if target.unwritten {
        if !opts.follow {
            anyhow::bail!(
                "session {} has no transcript yet: {} isn't written (Claude Code writes it at the first prompt); --follow waits for it",
                target.session.session_id,
                target.path
            );
        }
        ctx.notice(format!(
            "Waiting for {} (session {}, {}). Ctrl-C stops.",
            target.path, target.session.session_id, target.session.status
        ));
        match await_transcript(ctx, &bctx, arg, target, opts.reader_gone.as_ref(), &opts.intervals)? {
            Some(t) => target = t,
            None => return Ok(()),
        }
    }
    if opts.follow {
        ctx.notice(format!(
            "Following {} (session {}). Ctrl-C stops.",
            target.path, target.session.session_id
        ));
    }
    match stream_transcript(ctx, &bctx, &target, out, &opts) {
        Err(e) if is_broken_pipe(&e) || bctx.is_done() => Ok(()),
        res => res,
    }
}

/// Go's errors.Is(err, syscall.EPIPE).
fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

/// Reports whether the stream's reader has closed its end.
pub(crate) type ReaderGone = Arc<dyn Fn() -> bool + Send + Sync>;

/// The transcript agent transcript reads, and its session (Go:
/// transcriptTarget).
#[derive(Clone)]
pub(crate) struct TranscriptTarget {
    pub path: String,
    pub reader: &'static dyn Reader,
    /// The session as agent sessions lists it.
    pub session: Session,
    /// The transcripts of the sessions above a subagent's, nearest first: its
    /// status follows theirs.
    pub above: Vec<String>,
    /// There is no file at path yet, and its session hasn't exited (a Claude
    /// session before its first prompt). --follow waits for it
    /// (await_transcript).
    pub unwritten: bool,
}

/// Finds the transcript arg names: a workgroup agent's current one, a
/// session's by id (or the start of one), or a path (Go: resolveTranscript).
pub(crate) fn resolve_transcript(ctx: &Ctx, arg: &str) -> anyhow::Result<TranscriptTarget> {
    let mut path = transcript_path(ctx, arg)?;
    // The session is found by its path, or its id in the file names a
    // listing gives; only it and the sessions above it are read.
    let mut index = session_index(ctx)?;
    let found: Option<Session>;
    if path.is_empty() {
        // A dream's items and citations name a session as harness:id.
        let (harness, id) = match arg.split_once(':') {
            Some((h, id))
                if [discovery::HARNESS_CLAUDE, discovery::HARNESS_AGY, discovery::HARNESS_KIMI].contains(&h) =>
            {
                (Some(h), id)
            }
            _ => (None, arg),
        };
        let sessions: Vec<Session> = index
            .sessions()
            .into_iter()
            .filter(|s| harness.is_none_or(|h| s.harness == h))
            .collect();
        let s = session_by_id(&sessions, id)?;
        path = s.path.clone();
        found = Some(s);
    } else {
        found = index.lookup(&path);
    }
    let missing = matches!(std::fs::metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    let mut sessions = Vec::new();
    if let Some(f) = &found {
        let live = live_for(ctx, &f.harness);
        sessions = index.resolve(std::slice::from_ref(f), &live);
    }
    let mut session;
    let mut above = Vec::new();
    if let Some(i) = session_at(&sessions, &path) {
        session = sessions[i].clone();
        above = sessions_above(&sessions, &session);
    } else if missing {
        return Err(no_transcript_file(&path));
    } else {
        session = unlisted_session(&path)?;
    }
    let mut unwritten = false;
    if missing {
        // A session is listed with its transcript's path before the file is
        // there: its harness names it first. Only an exited one's is lost.
        if session.status == discovery::STATUS_ENDED {
            anyhow::bail!(
                "no transcript at {path}: there is no such file, and its session {} has ended",
                session.session_id
            );
        }
        unwritten = true;
    }
    let reader = match harness_reader(&session.harness) {
        Some(r) => r,
        None => {
            let head = read_head(&path)?;
            let r = transcript::detect(&head);
            session.harness = r.harness().to_string();
            r
        }
    };
    Ok(TranscriptTarget {
        path,
        reader,
        session,
        above,
        unwritten,
    })
}

/// The transcript a path or a workgroup agent's name names, or "" for an
/// argument that can only be a session id (Go: transcriptPath).
fn transcript_path(ctx: &Ctx, arg: &str) -> anyhow::Result<String> {
    if arg.is_empty() {
        anyhow::bail!(
            "give an agent's name, a session id or a transcript's path (agent sessions lists the sessions)"
        );
    }
    if arg.contains('/') || arg.ends_with(".jsonl") {
        return match std::fs::metadata(arg) {
            // resolve_transcript tells a session's unwritten transcript from none
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(abs_path(arg)),
            Err(e) => Err(anyhow::Error::new(PathError::new("stat", arg, e))
                .context(format!("can't read the transcript at {arg}"))),
            Ok(info) if info.is_dir() => Err(anyhow::anyhow!(
                "{arg} is a directory: give a transcript file (agent sessions --json lists their paths)"
            )),
            Ok(_) => Ok(abs_path(arg)),
        };
    }
    let (r, remote) = super::remote::parse_target(ctx, arg)?;
    if remote {
        anyhow::bail!(
            "transcripts are read per host, and {r}'s is on {}: agent capture {r} shows its screen",
            strconv::quote(&r.host)
        );
    }
    if agent_service::validate_agent_name(arg).is_err() {
        return Ok(String::new());
    }
    let svc = ctx.service()?;
    if let Some(last) = svc.transcripts(arg).last() {
        return Ok(last.path.clone());
    }
    if svc.agent(arg).is_ok() {
        anyhow::bail!(
            "agent {arg} has no transcript yet: a claude or agy agent's hooks name it at its first prompt, and other backends keep none (agent logs {arg} --terminal prints its terminal log)"
        );
    }
    Ok(String::new())
}

/// The session whose id is id, or the one session whose id starts with it
/// (Go: sessionByID).
fn session_by_id(sessions: &[Session], id: &str) -> anyhow::Result<Session> {
    let mut matches: Vec<&Session> = Vec::new();
    for s in sessions {
        if s.session_id == id {
            matches = vec![s];
            break;
        }
        if s.session_id.starts_with(id) {
            matches.push(s);
        }
    }
    match matches.len() {
        0 => anyhow::bail!(
            "no agent or session matches {}: agent list lists the agents, and agent sessions --json the sessions and their ids",
            strconv::quote(id)
        ),
        1 => {}
        n => {
            let ids: Vec<&str> = matches[..n.min(5)].iter().map(|s| s.session_id.as_str()).collect();
            let more = if n > 5 { format!(" and {} more", n - 5) } else { String::new() };
            anyhow::bail!(
                "{} starts the ids of {n} sessions ({}{more}): give more of the id",
                strconv::quote(id),
                ids.join(", ")
            );
        }
    }
    let s = matches[0].clone();
    if s.path.is_empty() {
        let name = s.workgroup.as_ref().map(|w| w.name.as_str()).unwrap_or("");
        anyhow::bail!(
            "session {} has no transcript yet (agent {name}'s CLI hasn't named one; agent logs {name} --terminal prints its terminal log)",
            s.session_id
        );
    }
    Ok(s)
}

/// The error for a path no file is at, and no session lists (Go:
/// noTranscriptFile).
fn no_transcript_file(path: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no transcript at {path}: there is no such file (agent sessions lists the sessions, and agent sessions --json their paths)"
    )
}

/// Waits for a running session's transcript to be written, as tail -F does:
/// it looks for the file every poll interval, and resolves arg again every
/// status interval and once the file is there, returning the target then. It
/// ends with resolve_transcript's error once the session has exited with no
/// transcript, and with None when bctx is done or the reader has gone (Go:
/// awaitTranscript).
fn await_transcript(
    ctx: &Ctx,
    bctx: &Context,
    arg: &str,
    mut target: TranscriptTarget,
    reader_gone: Option<&ReaderGone>,
    iv: &Intervals,
) -> anyhow::Result<Option<TranscriptTarget>> {
    let mut due = Instant::now() + iv.status;
    loop {
        if bctx.sleep_or_done(iv.poll).is_some() {
            return Ok(None);
        }
        if reader_gone.is_some_and(|f| f()) {
            return Ok(None);
        }
        let missing = match std::fs::metadata(&target.path) {
            Ok(_) => false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                return Err(anyhow::Error::new(PathError::new("stat", &target.path, e))
                    .context(format!("can't read the transcript {}", target.path)));
            }
        };
        if missing && Instant::now() < due {
            continue;
        }
        let next = resolve_transcript(ctx, arg)?;
        if !next.unwritten {
            return Ok(Some(next));
        }
        target = next;
        due = Instant::now() + iv.status;
    }
}

/// The index of the session whose transcript is path, or None (Go:
/// sessionAt).
pub(crate) fn session_at(sessions: &[Session], path: &str) -> Option<usize> {
    sessions
        .iter()
        .position(|s| !s.path.is_empty() && same_path(&s.path, path))
}

/// Whether a and b name the same file (Go: samePath).
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    if filepath::clean(a) == filepath::clean(b) {
        return true;
    }
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ai), Ok(bi)) => ai.dev() == bi.dev() && ai.ino() == bi.ino(),
        _ => false,
    }
}

/// The transcripts of the sessions above a subagent's, nearest first (Go:
/// sessionsAbove).
fn sessions_above(sessions: &[Session], s: &Session) -> Vec<String> {
    let mut above = Vec::new();
    let mut seen = HashSet::new();
    let mut p = s.parent_session.clone();
    while let Some(parent) = p {
        if !seen.insert(parent.session_id.clone()) {
            break;
        }
        let Some(c) = sessions
            .iter()
            .find(|c| c.harness == s.harness && c.session_id == parent.session_id && !c.path.is_empty())
        else {
            break;
        };
        above.push(c.path.clone());
        p = c.parent_session.clone();
    }
    above
}

/// The session of a transcript discovery can't read (an agy log outside a
/// conversation folder): its file, and an unknown status (Go:
/// unlistedSession).
fn unlisted_session(path: &str) -> anyhow::Result<Session> {
    let info = std::fs::metadata(path).map_err(|e| transcript_gone(path, e))?;
    let mtime = info.modified().ok().map(DateTime::<Utc>::from);
    Ok(Session {
        session_id: transcript::session_id_from_path(path),
        harness: discovery::harness_of(path).to_string(),
        path: path.to_string(),
        size: info.len() as i64,
        mtime,
        status: discovery::STATUS_UNKNOWN.to_string(),
        ..Session::default()
    })
}

/// The reader of a harness's transcripts, or None (Go: harnessReader).
fn harness_reader(harness: &str) -> Option<&'static dyn Reader> {
    match harness {
        discovery::HARNESS_CLAUDE => Some(&transcript::Claude),
        discovery::HARNESS_AGY => Some(&transcript::Agy),
        discovery::HARNESS_KIMI => Some(&transcript::Kimi),
        _ => None,
    }
}

/// How much of a transcript read_head reads to detect its format.
const TRANSCRIPT_HEAD: usize = 256 << 10;

fn read_head(path: &str) -> anyhow::Result<Vec<u8>> {
    let f = std::fs::File::open(path).map_err(|e| transcript_gone(path, e))?;
    let mut head = vec![0u8; TRANSCRIPT_HEAD];
    let (n, err) = read_at_full(&f, &mut head, 0);
    if let Some(e) = err {
        return Err(anyhow::Error::new(PathError::new("read", path, e))
            .context(format!("reading the transcript {path}")));
    }
    head.truncate(n);
    Ok(head)
}

/// Go's os.File.ReadAt: reads until buf is full, the file ends (no error), or
/// a read fails (the error, with what was read).
fn read_at_full(f: &std::fs::File, buf: &mut [u8], off: u64) -> (usize, Option<std::io::Error>) {
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], off + n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return (n, Some(e)),
        }
    }
    (n, None)
}

/// The error for a transcript that can't be opened (Go: transcriptGone).
fn transcript_gone(path: &str, err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        return anyhow::anyhow!(
            "the transcript {path} is gone (agent sessions lists the sessions there are)"
        );
    }
    anyhow::Error::new(PathError::new("open", path, err)).context(format!("can't read the transcript {path}"))
}

/// The directories agent sessions scans (Go: sessionDirs).
pub(crate) fn session_dirs(ctx: &Ctx) -> Vec<discovery::Dir> {
    let cfg = ctx.cfg();
    discovery::dirs(&discovery::Overrides {
        claude: cfg.claude_projects_dir.clone(),
        agy: cfg.agy_brain_dir.clone(),
        kimi: cfg.kimi_sessions_dir.clone(),
    })
}

/// The sessions agent sessions would list, joined to the workgroup agents,
/// reading no transcript yet (Go: sessionIndex).
pub(crate) fn session_index(ctx: &Ctx) -> anyhow::Result<Index> {
    let agents = workgroup_agents(ctx)?;
    Ok(discovery::new_index(&session_dirs(ctx), &agents))
}

/// What is running on this host, for resolving a session of harness: it asks
/// Claude Code only for a Claude session (Go: liveFor).
pub(crate) fn live_for(ctx: &Ctx, harness: &str) -> Live {
    if harness == discovery::HARNESS_CLAUDE {
        return live_now(ctx).0;
    }
    Live {
        now: Some(Utc::now()),
        ..Live::default()
    }
}

/// Joins sessions to the workgroup agents and sets their status, as agent
/// sessions does. It asks Claude Code what is running only when a session is
/// Claude's (Go: resolveSessions).
fn resolve_sessions(ctx: &Ctx, sessions: Vec<Session>) -> anyhow::Result<Vec<Session>> {
    let agents = workgroup_agents(ctx)?;
    let live = if sessions.iter().any(|s| s.harness == discovery::HARNESS_CLAUDE) {
        live_now(ctx).0
    } else {
        Live {
            now: Some(Utc::now()),
            ..Live::default()
        }
    };
    Ok(discovery::resolve(sessions, agents, live))
}

/// What the harnesses say is running on this host now. The error says why
/// Claude's sessions are missing from it (Go: liveNow).
pub(crate) fn live_now(ctx: &Ctx) -> (Live, Option<anyhow::Error>) {
    let mut live = Live {
        now: Some(Utc::now()),
        reports: HashMap::new(),
        claude: None,
    };
    if let Ok(svc) = ctx.service()
        && let Ok(reports) = svc.session_reports()
    {
        for (id, r) in reports {
            live.reports.insert(
                id,
                Report {
                    status: r.status,
                    end_reason: r.end_reason,
                    at: utc_or_none(&r.at),
                },
            );
        }
    }
    match claude_sessions(ctx) {
        Err(e) => (live, Some(e)),
        Ok(statuses) => {
            live.claude = Some(statuses.into_iter().map(|(id, st)| (id, st.to_string())).collect());
            (live, None)
        }
    }
}

/// t in UTC, or None for Go's zero time.
pub(crate) fn utc_or_none(t: &Time) -> Option<DateTime<Utc>> {
    if t.is_zero() {
        None
    } else {
        Some(t.0.with_timezone(&Utc))
    }
}

/// What agent transcript's flags choose (Go: streamOptions).
#[derive(Clone, Default)]
pub(crate) struct StreamOptions {
    /// from and until bound the window: the lines after the one at from (-1:
    /// all) and before until (-1: no end).
    pub from: i64,
    pub until: i64,
    pub follow: bool,
    pub requests: bool,
    /// Reports whether the stream's reader has closed its end, so --follow
    /// ends while it has nothing to write; None if that can't be told.
    pub reader_gone: Option<ReaderGone>,
    pub intervals: Intervals,
    /// Which events, and how much of a tool's result, it writes.
    pub shape: Shape,
}

/// What --talk and --results leave out of the stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Shape {
    /// Only prompts, replies and tool calls.
    pub talk: bool,
    pub results: Results,
    /// Scrub each event (logs::scrub_event).
    pub scrub: bool,
}

/// How much of a tool's result an event carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Results {
    /// Its text and the harness's structured copy, as the transcript has them.
    #[default]
    Whole,
    /// Its text, or the structured copy when there is no text: each result
    /// once.
    Once,
    /// Its status only.
    None,
}

impl Results {
    /// --results' value; --talk alone means none.
    pub(crate) fn parse(v: &str, talk_default: bool) -> anyhow::Result<Results> {
        if talk_default {
            return Ok(Results::None);
        }
        match v {
            "whole" => Ok(Results::Whole),
            "once" => Ok(Results::Once),
            "none" => Ok(Results::None),
            _ => anyhow::bail!("--results {}: give whole, once or none", strconv::quote(v)),
        }
    }
}

impl Shape {
    /// e as the stream writes it, or None for an event it leaves out.
    pub(crate) fn apply(&self, mut e: Event) -> Option<Event> {
        if self.talk && e.prompt.is_none() && e.say.is_none() && e.tool.is_none() {
            return None;
        }
        if self.scrub {
            super::logs::scrub_event(&mut e);
        }
        if self.talk
            && let Some(t) = &mut e.tool
        {
            // A call in one line: what it was about, not all it was sent.
            t.input = None;
        }
        if let Some(r) = e.tool.as_mut().and_then(|t| t.result.as_mut()) {
            match self.results {
                Results::Whole => {}
                Results::Once => {
                    if !r.output.is_empty() {
                        r.structured = None;
                    }
                }
                Results::None => {
                    r.output.clear();
                    r.structured = None;
                }
            }
        }
        Some(e)
    }
}

/// How many of the last bytes read --follow keeps, to tell a transcript
/// rewritten in place (truncated and grown again between two polls) from one
/// that grew.
const TRANSCRIPT_TAIL: usize = 64;

/// Writes one transcript's event stream (Go: transcriptStream).
struct TranscriptStream<'a, 'w> {
    opts: &'a StreamOptions,
    ctx: &'a Ctx,
    target: &'a TranscriptTarget,
    f: std::fs::File,
    p: Parser,
    finals: Finals,
    w: BufWriter<&'w mut dyn Write>,
    buf: Vec<u8>,
    /// The bytes read but not yet parsed, a line not yet whole; it starts at
    /// p.pos().at. tail holds the last bytes read.
    pending: Vec<u8>,
    tail: Vec<u8>,
    /// The session as the stream knows it, and written what the last session
    /// line said of it (session_meta).
    session: Session,
    written: String,
    /// The turn of the latest event, and turn_at when its first event
    /// happened.
    turn: Option<TurnRef>,
    turn_at: Option<DateTime<FixedOffset>>,
    /// The workgroup agent of this store that owns the session, or "". For a
    /// Claude agent, its Stop replies are sent provisionally; stop_at is when
    /// the latest one looked at was stored.
    owner: String,
    stop_at: Time,
    /// With --until, window holds the window's events until the read has gone
    /// on to the transcript's end, and dead the ids a rewind abandoned there:
    /// the window's events of a branch abandoned past its end are sent
    /// abandoned (docs/EVENTS.md, --until).
    window: Vec<Event>,
    dead: Option<HashSet<String>>,
}

/// Writes target's event stream to out: the session line, the events in the
/// window, and with follow, what the transcript appends until bctx is done
/// (Go: streamTranscript).
pub(crate) fn stream_transcript(
    ctx: &Ctx,
    bctx: &Context,
    target: &TranscriptTarget,
    out: &mut dyn Write,
    opts: &StreamOptions,
) -> anyhow::Result<()> {
    let f = std::fs::File::open(&target.path).map_err(|e| transcript_gone(&target.path, e))?;
    let mut popts = transcript::file_options(target.reader.harness(), &target.path);
    popts.requests = opts.requests;
    let p = target.reader.new_parser(popts);
    let mut owner = String::new();
    if let Some(g) = &target.session.workgroup
        && same_path(&g.store, &ctx.data_dir())
    {
        owner = g.name.clone();
    }

    // The initial read ends where the session line says the transcript ends.
    let mut end = target.session.size;
    if let Ok(info) = f.metadata()
        && (end <= 0 || end > info.len() as i64)
    {
        end = info.len() as i64;
    }
    check_parseable(&f, end).map_err(|e| {
        anyhow::anyhow!(
            "the transcript {} is not one agent sessions can read: {e:#}",
            target.path
        )
    })?;
    check_line_start(&f, opts.from, end)?;

    let mut s = TranscriptStream {
        opts,
        ctx,
        target,
        f,
        p,
        finals: Finals::new(),
        w: BufWriter::with_capacity(64 << 10, out),
        buf: vec![0u8; 1 << 20],
        pending: Vec::new(),
        tail: Vec::new(),
        session: target.session.clone(),
        written: String::new(),
        turn: None,
        turn_at: None,
        owner,
        stop_at: Time::zero(),
        window: Vec::new(),
        dead: None,
    };
    s.session.size = end;
    s.write_session()?;
    if opts.until >= 0 {
        s.dead = Some(HashSet::new()); // read on past the window, for its abandoned events
    }
    s.read(end)?;
    s.flush_window()?;
    if opts.until < 0 {
        // The opening line has what agent sessions read of the transcript's
        // head and tail; the parser has read every line.
        s.take_meta();
        s.write_session_if_changed()?;
        s.provisional()?;
    }
    s.w.flush()?;
    if !opts.follow {
        return Ok(());
    }
    s.follow(bctx)
}

impl TranscriptStream<'_, '_> {
    /// Writes the session line (Go: writeSession).
    fn write_session(&mut self) -> anyhow::Result<()> {
        self.written = session_meta(&self.session);
        let line = typed_json_line("session", &self.session)?;
        self.w.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Writes the session line if anything but the session's size and times
    /// changed since the last one (Go: writeSessionIfChanged).
    fn write_session_if_changed(&mut self) -> anyhow::Result<()> {
        if session_meta(&self.session) == self.written {
            return Ok(());
        }
        self.write_session()
    }

    /// Takes what the session says about itself from the stream's own parser,
    /// which has read every line where agent sessions reads a transcript's head
    /// and tail: a value it found replaces the session's, and a known value
    /// never becomes empty (Go: takeMeta).
    fn take_meta(&mut self) {
        let m = self.p.session();
        let s = &mut self.session;
        for (field, v) in [
            (&mut s.cwd, m.cwd),
            (&mut s.project, m.project),
            (&mut s.title, m.title),
            (&mut s.agent_name, m.agent_name),
            (&mut s.model, m.model),
            (&mut s.version, m.version),
            (&mut s.git_branch, m.git_branch),
            (&mut s.continued_in, m.continued_in),
        ] {
            if !v.is_empty() {
                *field = v;
            }
        }
    }

    /// Parses the transcript up to end, past a --until too (flush_window), and
    /// writes the events in the window (Go: read).
    fn read(&mut self, end: i64) -> anyhow::Result<()> {
        let mut off = self.read_to();
        while off < end {
            let want = (self.buf.len() as i64).min(end - off) as usize;
            let (n, err) = read_at_full(&self.f, &mut self.buf[..want], off as u64);
            off += n as i64;
            self.pending.extend_from_slice(&self.buf[..n]);
            self.tail.extend_from_slice(&self.buf[n.saturating_sub(TRANSCRIPT_TAIL)..n]);
            let drop = self.tail.len().saturating_sub(TRANSCRIPT_TAIL);
            self.tail.drain(..drop);
            let start = self.p.pos().at;
            let events = self.p.feed(&self.pending);
            for e in events {
                self.emit(e)?;
            }
            let consumed = (self.p.pos().at - start) as usize;
            self.pending.drain(..consumed);
            if let Some(e) = err {
                return Err(anyhow::Error::new(PathError::new("read", &self.target.path, e))
                    .context(format!("reading the transcript {}", self.target.path)));
            }
            if n == 0 {
                break;
            }
        }
        Ok(())
    }

    /// How far the stream has read the transcript: its whole lines, and a last
    /// line not yet whole (Go: readTo).
    fn read_to(&self) -> i64 {
        self.p.pos().at + self.pending.len() as i64
    }

    /// Writes a --until window's events, once the read has gone on, silently,
    /// to the transcript's end: those of a branch a later prompt abandoned,
    /// abandoned, as a whole read's fold (transcript::latest) has them.
    /// Nothing past the window is sent (Go: flushWindow).
    fn flush_window(&mut self) -> anyhow::Result<()> {
        for mut e in std::mem::take(&mut self.window) {
            e.abandoned = e.abandoned || self.dead.as_ref().is_some_and(|d| d.contains(&e.id));
            self.write_event(e)?;
        }
        Ok(())
    }

    /// Writes an event line, shaped by --talk and --results.
    fn write_event(&mut self, e: Event) -> anyhow::Result<()> {
        let Some(e) = self.opts.shape.apply(e) else { return Ok(()) };
        let line = typed_json_line("event", &e)?;
        self.w.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Writes one event of the transcript's, if its line is in the window.
    /// Every event goes through Finals and the turn, so what the window holds
    /// is merged and turned as a whole read's is (Go: emit).
    fn emit(&mut self, e: Event) -> anyhow::Result<()> {
        let e = self.finals.merge(e);
        if e.kind != transcript::KIND_REQUEST
            && let Some(turn) = &e.turn
            && self.turn.as_ref().is_none_or(|t| t.id != turn.id)
        {
            self.turn = Some(turn.clone());
            self.turn_at = e.time();
        }
        if e.abandoned
            && let Some(dead) = &mut self.dead
        {
            dead.insert(e.id.clone());
        }
        if (self.opts.from >= 0 && e.at <= self.opts.from) || (self.opts.until >= 0 && e.at >= self.opts.until) {
            return Ok(());
        }
        if self.opts.until >= 0 {
            self.window.push(e);
            return Ok(());
        }
        self.write_event(e)
    }

    /// Writes the closing reply of the turn the transcript is in, if the
    /// agent's Stop hook has reported it and the transcript doesn't hold it
    /// yet. The hook stores it as the agent's _result; a new turn, and a Stop
    /// that delivers held messages (the turn goes on), empty it (Go:
    /// provisional).
    fn provisional(&mut self) -> anyhow::Result<()> {
        if self.owner.is_empty() || self.target.session.harness != discovery::HARNESS_CLAUDE {
            return Ok(());
        }
        let Some(turn) = self.turn.clone() else { return Ok(()) };
        let Ok(store) = self.ctx.store() else { return Ok(()) };
        let Ok(entries) = store.list_state(&self.owner) else {
            return Ok(()); // the next poll asks again
        };
        let mut result = None;
        let mut current = false;
        for e in entries {
            if e.key == STATE_KEY_RESULT {
                result = Some(e);
            } else if e.key == STATE_KEY_TRANSCRIPT {
                current = same_path(&e.value, &self.target.path);
            }
        }
        let Some(result) = result else { return Ok(()) };
        if result.value.is_empty() || !current || !result.updated_at.after(&self.stop_at) {
            return Ok(());
        }
        self.stop_at = result.updated_at;
        if let Some(turn_at) = self.turn_at
            && result.updated_at.0 < turn_at
        {
            return Ok(()); // an earlier turn's
        }
        let pos = self.p.pos();
        let ts = result.updated_at.utc().format_rfc3339_nano();
        let Some(mut e) = self.finals.provisional(&turn.id, &result.value, &ts, pos) else {
            return Ok(());
        };
        // Not the transcript's: it shares the seq of the event that comes next.
        e.seq = pos.seq;
        self.write_event(e)
    }

    /// Writes what the transcript appends, and the session line again when the
    /// session changes, until bctx is done or the reader has gone.
    ///
    /// Status comes from checks that run beside the poll, one at a time, so a
    /// slow one never holds it up. A full check resolves the session as agent
    /// sessions does (every store, and the session registry for Claude): every
    /// status interval, the wait doubling up to the most while the transcript
    /// doesn't grow. For a session an agent of this store owns, a report check
    /// reads that agent's record and last report from the store alone, every
    /// session interval, and counts only when the agent has reported since: so
    /// a status the full check reconciled with the harness isn't undone by a
    /// report it already weighed (Go: follow).
    fn follow(&mut self, bctx: &Context) -> anyhow::Result<()> {
        let iv = self.opts.intervals;
        let now = Instant::now();
        let (mut wait, mut full_due, mut report_due) = (iv.status, now + iv.status, now + iv.session);
        // The owner's last report the session's status reflects.
        let mut reported: Option<DateTime<Utc>> = None;
        if !self.owner.is_empty()
            && let Ok(store) = self.ctx.store()
            && let Some(a) = own_agent(store, self.ctx, &self.owner, self.own_history())
        {
            reported = a.status_at;
        }
        let mut grown = self.read_to(); // how far the transcript had grown at the last full check
        let mut checking: Option<mpsc::Receiver<StatusCheck>> = None;
        let res = (|| -> anyhow::Result<()> {
            loop {
                if bctx.sleep_or_done(iv.poll).is_some() {
                    return Ok(());
                }
                let done = checking.as_ref().and_then(|rx| rx.try_recv().ok());
                if let Some(c) = done {
                    checking = None;
                    let mut ok = c.ok;
                    if c.full {
                        wait = iv.status;
                        if self.read_to() == grown {
                            wait = (2 * c.wait).min(iv.status_max);
                        }
                        grown = self.read_to();
                        full_due = Instant::now() + wait;
                    } else {
                        report_due = Instant::now() + iv.session;
                        ok = ok && after(c.reported, reported);
                    }
                    if after(c.reported, reported) {
                        reported = c.reported;
                    }
                    if ok {
                        self.session.status = c.status;
                        self.session.status_at = c.status_at;
                    }
                } else {
                    if self.opts.reader_gone.as_ref().is_some_and(|f| f()) {
                        return Ok(());
                    }
                    self.poll()?;
                    if checking.is_none() {
                        let now = Instant::now();
                        if now >= full_due {
                            checking = Some(self.start_check(true, wait)?);
                        } else if !self.owner.is_empty() && now >= report_due {
                            checking = Some(self.start_check(false, Duration::ZERO)?);
                        }
                    }
                }
                self.session.size = self.read_to();
                self.write_session_if_changed()?;
                self.w.flush()?;
            }
        })();
        // A check is finished before the stream returns, so none outlives it.
        if let Some(rx) = checking {
            let _ = rx.recv();
        }
        res
    }

    /// Reads what the transcript appended since the last poll, once it has
    /// checked the file is still the one the stream has read: not removed,
    /// replaced, or rewritten in place (Go: poll).
    fn poll(&mut self) -> anyhow::Result<()> {
        let path = &self.target.path;
        let info = match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("the transcript {path} was removed: the stream ends here");
            }
            Err(e) => {
                return Err(anyhow::Error::new(PathError::new("stat", path, e))
                    .context(format!("reading the transcript {path}")));
            }
            Ok(info) => info,
        };
        match self.f.metadata() {
            Ok(open) if open.dev() == info.dev() && open.ino() == info.ino() => {}
            _ => anyhow::bail!(
                "the transcript {path} was replaced by another file: the stream ends here; run agent transcript again to read the new one"
            ),
        }
        let read_to = self.read_to();
        let shrank = |size: u64| {
            anyhow::anyhow!(
                "the transcript {path} shrank from {read_to} to {size} bytes: it was rewritten, so the events sent may no longer match it; run agent transcript again"
            )
        };
        if (info.len() as i64) < read_to {
            return Err(shrank(info.len()));
        }
        if self.rewritten() {
            // It may have shrunk since the stat above: say so if it did.
            if let Ok(now) = self.f.metadata()
                && (now.len() as i64) < read_to
            {
                return Err(shrank(now.len()));
            }
            anyhow::bail!(
                "the transcript {path} was rewritten: the bytes the stream read last are no longer there, so the events sent may no longer match it; run agent transcript again"
            );
        }
        self.read(info.len() as i64)?;
        self.session.mtime = info.modified().ok().map(DateTime::<Utc>::from);
        self.take_meta();
        self.provisional()
    }

    /// Whether the last bytes the stream read have changed: the file was
    /// truncated and grew again between two polls (Go: rewritten).
    fn rewritten(&self) -> bool {
        let mut got = vec![0u8; self.tail.len()];
        let off = self.read_to() - self.tail.len() as i64;
        let (n, _) = read_at_full(&self.f, &mut got, off.max(0) as u64);
        n != got.len() || got != self.tail
    }

    /// The transcript, for an owner's report check: the agent's earlier
    /// session if it has moved on to another (/clear, a respawn). A subagent's
    /// session belongs to its parent's instead (Go: ownHistory).
    fn own_history(&self) -> Vec<String> {
        if self.target.session.parent_session.is_some() {
            return Vec::new();
        }
        vec![self.target.path.clone()]
    }

    /// Starts a status check, full or of the owner's report, and returns where
    /// its result arrives. It reads nothing the stream changes (Go:
    /// startCheck).
    fn start_check(&self, full: bool, wait: Duration) -> anyhow::Result<mpsc::Receiver<StatusCheck>> {
        let (tx, rx) = mpsc::channel();
        let share: CtxShare = self.ctx.share()?;
        let path = self.target.path.clone();
        let above = self.target.above.clone();
        let owner = self.owner.clone();
        let history = self.own_history();
        std::thread::spawn(move || {
            let ctx = share.quiet_ctx();
            let _ = tx.send(status_check(&ctx, full, wait, &path, &above, &owner, history));
        });
        Ok(rx)
    }
}

/// a after b, where None is Go's zero time.
fn after(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

/// What a status check found: the session's status and when it was set (ok
/// false if it couldn't be told), and the owner's last report at the time (Go:
/// statusCheck).
struct StatusCheck {
    full: bool,
    /// A full check's wait before it.
    wait: Duration,
    ok: bool,
    status: String,
    status_at: Option<DateTime<Utc>>,
    reported: Option<DateTime<Utc>>,
}

/// The body of start_check's thread.
fn status_check(
    ctx: &Ctx,
    full: bool,
    wait: Duration,
    path: &str,
    above: &[String],
    owner: &str,
    history: Vec<String>,
) -> StatusCheck {
    let mut check = StatusCheck {
        full,
        wait,
        ok: false,
        status: String::new(),
        status_at: None,
        reported: None,
    };
    let mut agents = Vec::new();
    if !owner.is_empty() {
        let Ok(store) = ctx.store() else { return check };
        let Some(a) = own_agent(store, ctx, owner, history) else {
            return check;
        };
        check.reported = a.status_at;
        agents.push(a);
    }
    let probed: Vec<Session> = std::iter::once(path.to_string())
        .chain(above.iter().cloned())
        .filter_map(|p| discovery::probe(&p))
        .collect();
    let sessions = if full {
        match resolve_sessions(ctx, probed) {
            Ok(s) => s,
            Err(_) => return check,
        }
    } else {
        discovery::resolve(
            probed,
            agents,
            Live {
                now: Some(Utc::now()),
                ..Live::default()
            },
        )
    };
    if let Some(i) = session_at(&sessions, path) {
        check.ok = true;
        check.status = sessions[i].status.clone();
        check.status_at = sessions[i].status_at;
    }
    check
}

/// The named agent of this store as the status rule (discovery::resolve)
/// needs it, read from the store alone, as export reads it: its record, and
/// the state its last report wrote. With no record it has departed. history is
/// the transcripts its name used. None if the store can't be read (Go:
/// ownAgent).
fn own_agent(st: &Store, ctx: &Ctx, name: &str, history: Vec<String>) -> Option<DiscoveryAgent> {
    let cfg = ctx.cfg();
    let mut a = DiscoveryAgent {
        name: name.to_string(),
        store: filepath::dir(&cfg.database_path),
        host: cfg.host.clone(),
        history,
        ..DiscoveryAgent::default()
    };
    let rec = st.get_agent(name).ok()?;
    let Some(rec) = rec else { return Some(a) };
    a.live = true;
    a.parent = rec.spawned_by;
    a.backend = rec.backend;
    a.created_at = utc_or_none(&rec.created_at);
    let entries = st.list_state(name).ok()?;
    for e in entries {
        match e.key.as_str() {
            STATE_KEY_STATUS => {
                a.status = e.value;
                a.status_at = utc_or_none(&e.updated_at);
            }
            STATE_KEY_EVENT => a.event = e.value,
            STATE_KEY_SESSION_ID => a.session_id = e.value,
            STATE_KEY_TRANSCRIPT => a.transcript = e.value,
            _ => {}
        }
    }
    Some(a)
}

/// What a session line says, but for the size and times that change with
/// every write (Go: sessionMeta).
fn session_meta(s: &Session) -> String {
    let mut s = s.clone();
    s.size = 0;
    s.mtime = None;
    s.status_at = None;
    serde_json::to_string(&s).unwrap_or_default()
}

/// An error unless the whole lines of the transcript's head (the first end
/// bytes, at most TRANSCRIPT_HEAD of them) hold a JSON object: no transcript
/// format any reader knows (Go: checkParseable).
fn check_parseable(f: &std::fs::File, end: i64) -> anyhow::Result<()> {
    let mut head = vec![0u8; (end.max(0) as usize).min(TRANSCRIPT_HEAD)];
    let (n, err) = read_at_full(f, &mut head, 0);
    if let Some(e) = err {
        return Err(e.into());
    }
    let head = &head[..n];
    let whole = head.iter().rposition(|&b| b == b'\n').unwrap_or(0);
    let mut lines = 0;
    for line in head[..whole].split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() {
            continue;
        }
        if line[0] == b'{' {
            return Ok(());
        }
        lines += 1;
    }
    if lines > 0 {
        anyhow::bail!("none of its first {lines} lines is a JSON object");
    }
    Ok(())
}

/// An error unless from is -1 or the start of a whole line of the
/// transcript's first end bytes, as --from needs (Go: checkLineStart).
fn check_line_start(f: &std::fs::File, from: i64, end: i64) -> anyhow::Result<()> {
    if from < 0 {
        return Ok(());
    }
    if from >= end {
        anyhow::bail!(
            "--from {from} is past the transcript's end ({end} bytes): give the at of an event it sent"
        );
    }
    if from > 0 {
        let mut b = [0u8; 1];
        let (n, err) = read_at_full(f, &mut b, (from - 1) as u64);
        if let Some(e) = err {
            return Err(anyhow::Error::new(e).context("reading the transcript"));
        }
        if n == 0 {
            return Err(anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
                .context("reading the transcript"));
        }
        if b[0] != b'\n' {
            anyhow::bail!(
                "--from {from} is not the start of a line of the transcript: give the at of an event it sent"
            );
        }
    }
    let mut buf = vec![0u8; 64 << 10];
    let mut off = from;
    while off < end {
        let want = (buf.len() as i64).min(end - off) as usize;
        let (n, err) = read_at_full(f, &mut buf[..want], off as u64);
        if buf[..n].contains(&b'\n') {
            return Ok(());
        }
        off += n as i64;
        if let Some(e) = err {
            return Err(anyhow::Error::new(e).context("reading the transcript"));
        }
        if n == 0 {
            break;
        }
    }
    anyhow::bail!("--from {from}: the line there has no end yet; resume from an earlier event's at")
}

/// Turns SIGINT and SIGTERM into a cancelled Context while it is held, as
/// Go's signal.NotifyContext does; dropping it restores what was there.
pub(crate) struct Interrupt {
    old: [(libc::c_int, libc::sigaction); 2],
    stop: Arc<std::sync::atomic::AtomicBool>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

impl Interrupt {
    /// Cancels through cancel when SIGINT or SIGTERM arrives.
    pub(crate) fn notify(cancel: agent_bus::CancelFn) -> Interrupt {
        use std::sync::atomic::Ordering;
        INTERRUPTED.store(false, Ordering::SeqCst);
        let mut old = [(libc::SIGINT, unsafe { std::mem::zeroed::<libc::sigaction>() }), (libc::SIGTERM, unsafe {
            std::mem::zeroed::<libc::sigaction>()
        })];
        for (sig, prev) in old.iter_mut() {
            // SAFETY: a plain sigaction call with a handler that only stores to
            // an atomic, which is async-signal-safe.
            unsafe {
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = on_interrupt as *const () as usize;
                act.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut act.sa_mask);
                libc::sigaction(*sig, &act, prev);
            }
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watching = Arc::clone(&stop);
        let watcher = std::thread::spawn(move || {
            while !watching.load(Ordering::SeqCst) {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    cancel.cancel();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Interrupt {
            old,
            stop,
            watcher: Some(watcher),
        }
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(w) = self.watcher.take() {
            let _ = w.join();
        }
        for (sig, prev) in &self.old {
            // SAFETY: restores the action saved in notify.
            unsafe {
                libc::sigaction(*sig, prev, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(test)]
#[path = "transcript_test.rs"]
mod transcript_test;

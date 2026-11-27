//! `agent logs` (Go: internal/cli/logs.go), and what the logs op the bus
//! serves to other hosts prints (Go: serveLogs, in remote_ops.go).
use std::collections::HashSet;
use std::io::Write as _;

use agent_base::gostd::filepath;
use agent_base::gostd::os::{self as goos, PathError};
use agent_base::gostd::{strconv, time as gotime};
use agent_base::util;
use agent_bus::{Context, LogsRequest, LogsResponse, MAX_LOGS_REPLY};
use agent_service::AgentService;
use agent_transcript::{self as transcript, ErrUnparseable, Event, PromptEvent, SystemEvent, ToolEvent};
use chrono::Local;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::output::plural;
use super::remote_ops::{LogsChunk, fit_logs, json_size};
use super::root::Ctx;

// `agent logs`

pub const DOC: Doc = Doc {
    use_line: "logs <name|session-id|path>",
    short: "Print what an agent or session did (prompts, replies, tool calls), even after cleanup",
    long: r#"Logs prints an agent's last events, oldest first, one per line:

  12:04:31 prompt run the tests
  12:04:40 tool   Bash: go test ./... → ok (34 lines)
  12:05:02 say    All tests pass.
  12:05:02 system turn took 31.2s

Kinds: prompt (a message it got; one that didn't come from a person
typing says where it came from: queued, delivered, task-notification,
peer, command, system), think (its reasoning; a bare "think" where the CLI
kept none of its text), say (text it wrote), tool (a call and its result:
ok, error, interrupted, running in the background, or no result yet),
system (a record of the CLI's own: turn timing, compaction, API errors,
interrupts). Times are local. Text longer than a line is cut and marked
"…", a command or path in its middle so its end shows; --full prints it
whole, with each tool's output.

Instead of an agent's name, give a session id (or the start of one only
one session's id starts with; agent sessions lists them) or a
transcript's path, for any session on this host, an agent's or not: logs
then reads that one transcript.

For a long session, read it in parts rather than whole:
  --talk      only prompts, replies and tool calls, each call on one line
              and its output left out (--full then prints prompts and
              replies whole, still not tool output);
  --grep TEXT only the events whose text, tool call or output contains
              TEXT (ignoring case);
  --start N   from the Nth event (of those --talk and --grep keep), --lines
              of them, instead of the last --lines.
With any of these, each line starts with its number (#N), and a hint
says what part this is and the --start that reads on; --start N --lines
1 --full reads one event whole.

--scrub redacts secrets in place, as agent transcript --scrub does
(tokens, keyed secrets, Authorization values, sign-in URLs, emails,
private keys become [redacted: ...]); nothing else is removed, and pasted
and harness-inserted blocks are labelled as data. With --full and more
than one event, a piece over 4000 characters is cut with a marker naming
the command that reads its event whole (--start N --lines 1 --full is
never cut). A dream's own agents always read logs this way.

--json prints the events as docs/EVENTS.md describes them, one JSON
object per line: for each transcript, a session line, then its events
(a tool's result is an update of its call, sent again whole).

The events come from the transcript the agent's CLI keeps of its session,
whose path the CLI's hooks report (claude and agy agents):
  claude: ~/.claude/projects/<project>/<session id>.jsonl
  agy:    ~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript_full.jsonl
Each path an agent used is listed in <name>.transcripts beside its terminal
log, so logs finds them after cleanup, and across /clear and respawns under
the same name (a line "--- session <id>" marks where one begins).

--terminal prints the terminal log instead, as does logs when there is no
transcript to read (another backend, no hook has fired yet, the file is
gone or unparseable; a hint says which). That log is everything the
agent's terminal showed, cleaned into text: escapes dropped, blank lines
and screen redraws left out; it has no timestamps. Log files are in the
data dir next to the agent binary, not in the working directory; here:
  {log_file}

For an agent on another host, agent logs name@host prints what agent logs
prints there, with the same flags, through the agent bus (its logs op,
which a host serves to peers by default). A bus message holds at most
512 KB: a longer answer keeps the newest events (whole events, whole JSON
lines) and says on stderr how much was left out; --lines asks for fewer.
Terminal controls are stripped from the text, as capture's are. For the
live screen, use agent capture."#,
    example: r#"  agent logs researcher
  agent logs 3f3a9c --talk --lines 200
  agent logs 3f3a9c --grep "cargo test" --lines 20
  agent logs 3f3a9c --talk --start 201 --lines 200
  agent logs researcher@hub --lines 20
  agent logs researcher --lines 10 --full
  agent logs researcher --terminal --lines 100"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent logs`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "full", help = "Print each event's text and tool output whole")]
    pub full: bool,
    #[arg(long = "json", help = "Print the sessions and their events as JSON lines")]
    pub json: bool,
    #[arg(
        long = "lines",
        value_name = "int",
        default_value = "50",
        value_parser = flags::parse_int,
        help = "Number of events (or terminal lines) to print",
    )]
    pub lines: i64,
    #[arg(long = "terminal", help = "Print the terminal log instead of the transcript")]
    pub terminal: bool,
    #[arg(long = "talk", help = "Only prompts, replies and one-line tool calls, without tool output")]
    pub talk: bool,
    #[arg(
        long = "scrub",
        help = "Redact secrets in place, and cap long pieces under --full with a pointer to the whole (always, for a dream's agent)"
    )]
    pub scrub: bool,
    #[arg(
        long = "grep",
        value_name = "string",
        default_value = "",
        help = "Only events whose text, tool call or output contains this (ignoring case)"
    )]
    pub grep: String,
    #[arg(
        long = "start",
        value_name = "int",
        default_value = "0",
        value_parser = flags::parse_int,
        help = "Print from the Nth event (1 is the first) instead of the last --lines",
    )]
    pub start: i64,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let arg = &args.args[0];
    // A transcript's path is a path, '@' in it or not; name@host otherwise.
    let (name, remote) = if is_path(arg) {
        (arg.clone(), None)
    } else {
        let (r, remote) = super::remote::parse_target(ctx, arg)?;
        (r.name.clone(), remote.then_some(r))
    };
    let opts = LogsOptions {
        lines: args.lines,
        full: args.full,
        json: args.json,
        terminal: args.terminal,
        talk: args.talk,
        grep: args.grep.clone(),
        start: args.start,
        sessions: true,
        scrub: args.scrub || in_dream(ctx),
    };
    opts.check()?;
    if remote.is_some() && opts.scrub {
        anyhow::bail!(
            "agent logs {arg} would read another host's transcript unscrubbed: --scrub (and a dream's agent) reads this host's only"
        );
    }
    if let Some(r#ref) = remote {
        if opts.parts() {
            anyhow::bail!(
                "--talk, --grep and --start read a transcript here, not over the bus: run agent logs on {}'s host",
                r#ref.name
            );
        }
        let req = LogsRequest {
            agent: String::new(),
            lines: opts.lines,
            full: opts.full,
            json: opts.json,
            terminal: opts.terminal,
        };
        return super::remote_ops::run_remote_logs(ctx, &r#ref, req);
    }
    let (chunks, why) = build_logs(ctx, &name, &opts)?;
    if !why.is_empty() {
        ctx.hint(&why);
    }
    let mut out = ctx.out();
    for c in &chunks {
        out.write_all(c.text.as_bytes())?;
    }
    Ok(())
}

/// agent logs' flags (Go: logsOptions).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogsOptions {
    pub lines: i64,
    pub full: bool,
    pub json: bool,
    pub terminal: bool,
    /// Only prompts, replies and one-line tool calls.
    pub talk: bool,
    /// Only events containing this, ignoring case ("" for all).
    pub grep: String,
    /// Print from this event (1-based, of those kept), 0 for the last lines.
    pub start: i64,
    /// The argument may be a session id or a transcript's path, not only an
    /// agent's name: this host's own agent logs, never the bus's logs op.
    pub sessions: bool,
    /// Scrub what it prints (scrub_event): what a dream's agent, or a
    /// person mining transcripts, reads.
    pub scrub: bool,
}

impl LogsOptions {
    pub fn check(&self) -> anyhow::Result<()> {
        if self.lines < 1 {
            anyhow::bail!("--lines must be at least 1");
        }
        if self.start < 0 {
            anyhow::bail!("--start must be 1 or more (1 is the first event)");
        }
        if self.json && (self.terminal || self.full) {
            anyhow::bail!("--json prints transcript events whole; it doesn't go with --terminal or --full");
        }
        if self.scrub && self.terminal {
            anyhow::bail!("--terminal prints the raw terminal log, which can't be scrubbed: read the transcript instead");
        }
        if self.parts() && (self.json || self.terminal) {
            anyhow::bail!(
                "--talk, --grep and --start pick events to print as lines; they don't go with --json or --terminal"
            );
        }
        Ok(())
    }

    /// Whether it reads a transcript in parts (--talk, --grep, --start):
    /// it reads the whole transcript then, and numbers the lines.
    pub fn parts(&self) -> bool {
        self.talk || !self.grep.is_empty() || self.start > 0
    }

    /// Whether long pieces are cut, each with a marker naming the command
    /// that reads its event whole: --full read for mining (--scrub, or a
    /// dream's agent), more than one event at a time.
    pub fn caps(&self) -> bool {
        self.scrub && self.full && self.lines > 1
    }
}

/// A chunk of a line of text: an event's line(s), a line of the terminal log,
/// or a message (Go: logsChunk{text: ...}).
fn text_chunk(text: String) -> LogsChunk {
    LogsChunk { text, session: false }
}

/// Works out what agent logs prints for name on this host: the chunks for
/// stdout, and a hint for stderr ("" for none). The bus's logs op serves the
/// same, so agent logs name@host prints what logs prints there (Go:
/// buildLogs).
pub fn build_logs(ctx: &Ctx, name: &str, o: &LogsOptions) -> anyhow::Result<(Vec<LogsChunk>, String)> {
    let svc: &AgentService = ctx.service()?;
    let an_agent = agent_service::validate_agent_name(name).is_ok()
        && (!svc.transcripts(name).is_empty() || svc.agent(name).is_ok() || log_exists(svc, name));
    if !an_agent && o.sessions {
        match session_logs(ctx, name, o) {
            // A name no agent and no session has: the agent's error, which
            // says what it might have meant.
            Err(e)
                if agent_service::validate_agent_name(name).is_ok()
                    && format!("{e:#}").starts_with("no agent or session matches") => {}
            // The start of several sessions' ids: say no agent has it either.
            Err(e) if agent_service::validate_agent_name(name).is_ok() && !is_path(name) => {
                return Err(anyhow::anyhow!(
                    "no agent named {} here (agent list lists them), and as a session: {e:#}",
                    strconv::quote(name)
                ));
            }
            res => return res,
        }
    }
    // The name becomes a path component.
    agent_service::validate_agent_name(name)?;
    if o.terminal {
        return Ok((terminal_log_chunks(svc, name, o.lines)?, String::new()));
    }

    let read = if o.parts() || o.caps() { ALL_EVENTS as i64 } else { o.lines };
    let (mut sessions, why) = read_transcripts(svc, name, read);
    if o.scrub {
        redact_sessions(&mut sessions);
    }
    if !why.is_empty() {
        if o.json {
            anyhow::bail!(
                "no transcript to print for {name}: {why}; agent logs {name} --terminal prints its terminal log"
            );
        }
        if !log_exists(svc, name) && svc.agent(name).is_err() {
            return Err(no_log(svc, name));
        }
        let chunks = terminal_log_chunks(svc, name, o.lines)?;
        return Ok((chunks, format!("printing {name}'s terminal log: {why}")));
    }
    if o.json {
        return Ok((event_line_chunks(&sessions)?, String::new()));
    }
    if sessions.len() == 1 && sessions[0].events.is_empty() {
        return Ok((
            vec![text_chunk(format!(
                "Agent {name}'s transcript has no events yet ({}).\n",
                sessions[0].file.session.path
            ))],
            String::new(),
        ));
    }
    if o.parts() {
        return Ok(part_chunks(&sessions, o));
    }
    Ok((tail_chunks(&sessions, o), String::new()))
}

/// The last --lines events of the sessions, oldest first, each session
/// after the first marked where it begins (and the first too when the tail
/// starts in a later one).
fn tail_chunks(sessions: &[LogsSession], o: &LogsOptions) -> Vec<LogsChunk> {
    let all: Vec<(usize, usize, &Event)> = sessions
        .iter()
        .enumerate()
        .flat_map(|(i, s)| s.events.iter().enumerate().map(move |(k, e)| (i, k, e)))
        .collect();
    let from = all.len().saturating_sub(o.lines as usize);
    let mut chunks = Vec::new();
    let mut session = None;
    for &(i, k, e) in &all[from..] {
        if session.is_some_and(|s| s != i) || (session.is_none() && i > 0) {
            chunks.push(session_chunk(&sessions[i]));
        }
        session = Some(i);
        chunks.push(text_chunk(event_line(e, o.full, o, &sessions[i], k) + "\n"));
    }
    chunks
}

/// Whether the caller is a worker of an open dream: what it reads of
/// transcripts is scrubbed, whatever it asks (Ivan's rule: anything that
/// mines other sessions' transcripts scrubs them first).
///
/// It fails closed: only a caller that is clearly no dream's worker (a
/// person, or an agent the store says has no dream) reads unscrubbed. A
/// store it can't read, or a worker whose dream has just closed, scrubs.
pub(crate) fn in_dream(ctx: &Ctx) -> bool {
    let caller = ctx.caller_agent();
    if caller.is_empty() {
        return false;
    }
    let Ok(store) = ctx.store() else { return true };
    match store.get_state(&caller, super::dream::STATE_KEY_DREAM) {
        Ok(Some(_)) => return true,
        Ok(None) => {}
        Err(_) => return true,
    }
    !matches!(store.open_dream_worker(&caller), Ok(None))
}

/// Whether an argument names a transcript by its path rather than an agent
/// or a session id.
fn is_path(arg: &str) -> bool {
    arg.contains('/') || arg.ends_with(".jsonl")
}

/// Redacts every event a read holds: the stream --json prints, and the
/// events the lines print.
fn redact_sessions(sessions: &mut [LogsSession]) {
    for s in sessions {
        s.file.session.title = redact_line(&s.file.session.title);
        s.file.events.iter_mut().for_each(redact_event);
        s.events.iter_mut().for_each(redact_event);
    }
}

/// An event as whoever mines transcripts reads it (a dream's agent, or
/// --scrub), by Ivan's rule as he set it on 2026-11-27: don't delete text a
/// dreamer reads; redact the secrets in it. Each secret (a token, a keyed
/// secret, an Authorization value, a sign-in URL, an email, a private key)
/// is replaced in place by a marker (agent_dream::scrub::redact), in its
/// text, tool call, input, output and structured result alike; a prompt's
/// pasted and harness-inserted blocks are labelled as data, never
/// instructions. Nothing else is removed.
pub(crate) fn redact_event(e: &mut Event) {
    use agent_dream::scrub::{label_blocks, redact};
    if let Some(p) = &mut e.prompt {
        p.text = redact(&label_blocks(&p.text));
        p.command = redact(&p.command);
    }
    if let Some(t) = &mut e.think {
        t.text = redact(&t.text);
    }
    if let Some(s) = &mut e.say {
        s.text = redact(&s.text);
    }
    if let Some(t) = &mut e.tool {
        t.tool_summary.target = redact(&t.tool_summary.target);
        t.tool_summary.detail = redact(&t.tool_summary.detail);
        t.input = t.input.take().and_then(redact_json);
        t.meta = t.meta.take().and_then(redact_json);
        if let Some(r) = &mut t.result {
            r.output = redact(&r.output);
            r.structured = r.structured.take().and_then(redact_json);
        }
    }
    if let Some(s) = &mut e.system {
        s.text = redact(&s.text);
        s.data = s.data.take().and_then(redact_json);
    }
    if let Some(r) = e.request.take() {
        // What a model call was sent: its messages, redacted as JSON.
        e.request = serde_json::to_string(&r)
            .ok()
            .and_then(|j| serde_json::from_str(&agent_dream::scrub::redact(&j)).ok());
    }
}

/// A JSON value redacted as text: a marker holds no quote or backslash, so
/// it stays JSON; None in the unlikely case it doesn't.
fn redact_json(j: agent_transcript::RawJson) -> Option<agent_transcript::RawJson> {
    agent_transcript::RawJson::from_json_string(agent_dream::scrub::redact(j.as_str())).ok()
}

/// One line's text redacted (a session's title, a recorded command's
/// words): agent_dream::scrub::redact, nothing cut.
pub(crate) fn redact_line(s: &str) -> String {
    agent_dream::scrub::redact(s)
}

/// How many characters of one piece of an event (a prompt, a reply, a
/// tool's output) a capped read prints.
pub const CAP_CHARS: usize = 4000;

/// An event's line as logs prints it: format_event, with each long piece
/// cut and marked when the read caps (LogsOptions::caps). The marker names
/// the command that reads the event whole: `k` is its place in `s`'s
/// events, which hold the whole session when the read caps.
fn event_line(e: &Event, full: bool, o: &LogsOptions, s: &LogsSession, k: usize) -> String {
    if !(o.caps() && full) {
        return format_event(e, full);
    }
    // By its path, which always resolves (an id needs the session listed).
    let whole = format!(
        "agent logs {} --start {} --lines 1 --full",
        s.file.session.path,
        k + 1
    );
    let cap = |t: &mut String| {
        let n = t.chars().count();
        if n > CAP_CHARS {
            let kept: String = t.chars().take(CAP_CHARS).collect();
            *t = format!("{kept}\n[… {} more characters cut; the whole event: {whole}]", n - CAP_CHARS);
        }
    };
    let mut e = e.clone();
    if let Some(p) = &mut e.prompt {
        cap(&mut p.text);
    }
    if let Some(t) = &mut e.think {
        cap(&mut t.text);
    }
    if let Some(x) = &mut e.say {
        cap(&mut x.text);
    }
    if let Some(r) = e.tool.as_mut().and_then(|t| t.result.as_mut()) {
        cap(&mut r.output);
    }
    format_event(&e, full)
}

/// How many events a read in parts asks for: all of them.
const ALL_EVENTS: usize = 1 << 40;

/// The line that marks where a session begins, after the first.
fn session_chunk(s: &LogsSession) -> LogsChunk {
    LogsChunk {
        text: format!(
            "--- session {} ({})\n",
            s.file.session.session_id, s.file.session.harness
        ),
        session: true,
    }
}

/// What agent logs prints for one session named by its id or its
/// transcript's path (agent transcript's resolve_transcript finds it): any
/// session on this host, an agent's or not.
fn session_logs(ctx: &Ctx, arg: &str, o: &LogsOptions) -> anyhow::Result<(Vec<LogsChunk>, String)> {
    if o.terminal {
        anyhow::bail!(
            "--terminal prints an agent's terminal log, and {} isn't an agent here: give the agent's name (agent list)",
            strconv::quote(arg)
        );
    }
    let target = super::transcript::resolve_transcript(ctx, arg)?;
    if target.unwritten {
        anyhow::bail!(
            "session {} has no transcript yet: {} isn't written (Claude Code writes it at the first prompt)",
            target.session.session_id,
            target.path
        );
    }
    let read = if o.parts() || o.caps() { ALL_EVENTS } else { o.lines as usize };
    let f = transcript::read_file(&target.path, Some(target.reader), read)
        .map_err(|e| anyhow::anyhow!("can't read {}: {}", target.path, read_error_text(&target.path, &e)))?;
    let mut events = transcript::latest(&f.events);
    if !o.parts() && !o.caps() && events.len() > o.lines as usize {
        events.drain(..events.len() - o.lines as usize);
    }
    let mut sessions = vec![LogsSession { file: f, events }];
    if o.scrub {
        redact_sessions(&mut sessions);
    }
    if o.json {
        return Ok((event_line_chunks(&sessions)?, String::new()));
    }
    if sessions[0].events.is_empty() {
        return Ok((
            vec![text_chunk(format!(
                "Session {}'s transcript has no events yet ({}).\n",
                target.session.session_id, target.path
            ))],
            String::new(),
        ));
    }
    if o.parts() {
        return Ok(part_chunks(&sessions, o));
    }
    Ok((tail_chunks(&sessions, o), String::new()))
}

/// A transcript read in parts: the events --talk and --grep keep, numbered
/// from 1, and of those, --lines from --start (or the last --lines); with a
/// hint saying which part this is and how to read on.
fn part_chunks(sessions: &[LogsSession], o: &LogsOptions) -> (Vec<LogsChunk>, String) {
    let needle = o.grep.to_lowercase();
    let mut kept: Vec<(usize, usize, &Event)> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        for (k, e) in s.events.iter().enumerate() {
            if o.talk && !is_talk(e) {
                continue;
            }
            if !needle.is_empty() && !event_text(e).to_lowercase().contains(&needle) {
                continue;
            }
            kept.push((i, k, e));
        }
    }
    let total = kept.len();
    let lines = o.lines as usize;
    let first = if o.start > 0 {
        (o.start as usize - 1).min(total)
    } else {
        total.saturating_sub(lines)
    };
    let last = (first + lines).min(total);
    let mut chunks = Vec::new();
    let mut session = None;
    for (n, (i, k, e)) in kept[first..last].iter().enumerate() {
        // A window that starts in a later session says which one it is.
        if session.is_some_and(|s| s != *i) || (session.is_none() && *i > 0) {
            chunks.push(session_chunk(&sessions[*i]));
        }
        session = Some(*i);
        // --talk never prints tool output, --full or not.
        let full = o.full && !(o.talk && e.tool.is_some());
        let line = event_line(e, full, o, &sessions[*i], *k);
        chunks.push(text_chunk(format!("#{} {line}\n", first + n + 1)));
    }
    let what = match (o.talk, needle.is_empty()) {
        (true, true) => "talk events".to_string(),
        (true, false) => format!("talk events matching {}", strconv::quote(&o.grep)),
        (false, false) => format!("events matching {}", strconv::quote(&o.grep)),
        (false, true) => "events".to_string(),
    };
    let hint = if total == 0 {
        format!("no {what}")
    } else if first >= total {
        format!("--start {} is past the last of {total} {what}", o.start)
    } else if last < total {
        format!(
            "{what} {}-{last} of {total}; --start {} reads on",
            first + 1,
            last + 1
        )
    } else {
        format!("{what} {}-{last} of {total}", first + 1)
    };
    (chunks, hint)
}

/// Whether --talk keeps an event: what was said, and the calls made.
fn is_talk(e: &Event) -> bool {
    e.prompt.is_some() || e.say.is_some() || e.tool.is_some()
}

/// The text --grep looks in: an event's text, and a tool call's name,
/// target, detail and output.
fn event_text(e: &Event) -> String {
    if let Some(p) = &e.prompt {
        return p.text.clone();
    }
    if let Some(t) = &e.think {
        return t.text.clone();
    }
    if let Some(s) = &e.say {
        return s.text.clone();
    }
    if let Some(t) = &e.tool {
        let out = t.result.as_ref().map(|r| r.output.as_str()).unwrap_or("");
        return format!("{}\n{}\n{}\n{out}", t.name, t.target, t.detail);
    }
    if let Some(s) = &e.system {
        return s.text.clone();
    }
    String::new()
}

/// A transcript read for logs: the stream its lines make, and the events logs
/// prints, each in its latest state (Go: logsSession).
pub struct LogsSession {
    pub file: transcript::File,
    pub events: Vec<Event>,
}

/// Reads the last n events of the named agent's transcripts, newest
/// transcript first, going back to older ones while it has fewer than n. It
/// returns the sessions that hold them, oldest first, or why there is no
/// transcript to read: the newest must be readable, while an older one that
/// isn't just ends the search (Go: readTranscripts).
fn read_transcripts(svc: &AgentService, name: &str, n: i64) -> (Vec<LogsSession>, String) {
    let refs = svc.transcripts(name);
    if refs.is_empty() {
        return (Vec::new(), "no transcript recorded for it".to_string());
    }
    let mut sessions: Vec<LogsSession> = Vec::new();
    let mut total: i64 = 0;
    for r in refs.iter().rev() {
        if total >= n {
            break;
        }
        let reader = svc.transcript_reader(&r.backend);
        let f = match transcript::read_file(&r.path, reader.as_deref(), (n - total) as usize) {
            Ok(f) => f,
            Err(e) => {
                if !sessions.is_empty() {
                    break;
                }
                if goos::is_not_exist(&e) {
                    return (Vec::new(), format!("its transcript {} is gone", r.path));
                }
                return (
                    Vec::new(),
                    format!("its transcript {} can't be read: {}", r.path, read_error_text(&r.path, &e)),
                );
            }
        };
        let mut events = transcript::latest(&f.events);
        let keep = (n - total) as usize;
        if events.len() > keep {
            events.drain(..events.len() - keep);
        }
        total += events.len() as i64;
        let empty = events.is_empty();
        sessions.push(LogsSession { file: f, events });
        if empty && sessions.len() > 1 {
            sessions.pop();
        }
    }
    sessions.reverse();
    (sessions, String::new())
}

/// The Go text of an error transcript::read_file returned: the unparseable
/// error's own message (Go wraps ErrUnparseable with %w, so its text comes
/// first), and an open's error as os.Open says it.
pub(crate) fn read_error_text(path: &str, e: &anyhow::Error) -> String {
    if e.downcast_ref::<ErrUnparseable>().is_some() || e.chain().any(|c| c.is::<ErrUnparseable>()) {
        return e.to_string();
    }
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return format!("open {path}: {}", goos::errno_text(io));
    }
    format!("{e:#}")
}

/// Renders each session as docs/EVENTS.md frames it: a session line, then its
/// events, updates included, for the events logs would print (Go:
/// eventLineChunks).
fn event_line_chunks(sessions: &[LogsSession]) -> anyhow::Result<Vec<LogsChunk>> {
    let mut chunks = Vec::new();
    for s in sessions {
        chunks.push(LogsChunk {
            text: typed_json_line("session", &s.file.session)?,
            session: true,
        });
        let kept: HashSet<&str> = s.events.iter().map(|e| e.id.as_str()).collect();
        for e in &s.file.events {
            if !kept.contains(e.id.as_str()) {
                continue;
            }
            chunks.push(text_chunk(typed_json_line("event", e)?));
        }
    }
    Ok(chunks)
}

/// A line of an event stream: v's JSON object with a "type" field first, as
/// Go encodes a struct embedding v after a Type field, then a newline (Go:
/// sessionLine, eventLine, and their json.Encoder with SetEscapeHTML(false)).
pub(crate) fn typed_json_line<T: Serialize + ?Sized>(typ: &str, v: &T) -> anyhow::Result<String> {
    let body = go_json(v)?;
    let rest = body
        .strip_prefix('{')
        .ok_or_else(|| anyhow::anyhow!("a {typ} line that isn't a JSON object"))?;
    if rest == "}" {
        return Ok(format!("{{\"type\":{}}}\n", go_json(typ)?));
    }
    Ok(format!("{{\"type\":{},{rest}\n", go_json(typ)?))
}

/// v as Go's json.Encoder with SetEscapeHTML(false) writes it, without the
/// newline (agent_base::gostd::json: U+2028 and U+2029 escaped, <, > and &
/// left as they are, floats as Go formats them). Every JSON line a display
/// reads (logs, transcript, export, fleet, focus) goes through here.
pub(crate) fn go_json<T: Serialize + ?Sized>(v: &T) -> anyhow::Result<String> {
    Ok(agent_base::gostd::json::to_string(v, false)?)
}

// Widths of an event line: the head ("15:04:05 prompt "), and how much text a
// cut line keeps.
const EVENT_HEAD_WIDTH: usize = "15:04:05 prompt ".len();
const EVENT_TEXT_WIDTH: usize = 120;
const TOOL_TEXT_WIDTH: usize = 80;

/// Renders an event as one line, or with full, as the whole text and tool
/// output, continuation lines indented under the text (Go: formatEvent).
pub fn format_event(e: &Event, full: bool) -> String {
    let ts = match e.time() {
        Some(t) => t.with_timezone(&Local).format("%H:%M:%S").to_string(),
        None => "--:--:--".to_string(),
    };
    let mut head = format!("{ts} {:<6} ", e.kind);
    if e.abandoned {
        // A branch the conversation left: a prompt sent again edited.
        head.push_str("(abandoned) ");
    }
    let text = |s: &str| -> String {
        if full {
            indent(s)
        } else {
            cut_line(s, EVENT_TEXT_WIDTH)
        }
    };
    if let Some(p) = &e.prompt {
        let mut s = text(&p.text);
        let summary = notification_summary(p);
        if !summary.is_empty() && !full {
            // A task notification's markup says nothing at a glance.
            s = cut_line(&summary, EVENT_TEXT_WIDTH);
        }
        if let Some((_, messages)) = p.text.split_once('\n')
            && p.origin == transcript::ORIGIN_DELIVERED
            && !full
        {
            // Under gomuxai's head line, the messages delivered.
            s = cut_line(messages, EVENT_TEXT_WIDTH);
        }
        if p.origin != transcript::ORIGIN_HUMAN && p.origin != transcript::ORIGIN_COMMAND {
            s = format!("({}) {s}", p.origin);
        }
        if !p.images.is_empty() {
            s.push_str(&format!(" [{}]", plural(p.images.len(), "image")));
        }
        return head + &s;
    }
    if let Some(t) = &e.think {
        if t.redacted {
            // Its text isn't kept: the line just says it thought.
            return head.trim_end_matches(' ').to_string();
        }
        return head + &text(&t.text);
    }
    if let Some(s) = &e.say {
        return head + &text(&s.text);
    }
    if let Some(t) = &e.tool {
        return head + &format_tool(t, full);
    }
    if let Some(s) = &e.system {
        return head + &format_system(s, &text);
    }
    if let Some(r) = &e.request {
        return head + &format!("{}, {}", r.model, plural(r.tail.len(), "new message"));
    }
    head
}

/// The <summary> of each task notification a prompt holds, joined, or ""
/// (Go: notificationSummary).
fn notification_summary(p: &PromptEvent) -> String {
    if p.origin != transcript::ORIGIN_TASK_NOTIFICATION {
        return String::new();
    }
    let summaries: Vec<String> = transcript::task_notifications(&p.text)
        .into_iter()
        .filter(|n| !n.summary.is_empty())
        .map(|n| n.summary)
        .collect();
    summaries.join("; ")
}

/// Renders a tool call and its result: what it called, on what, and how it
/// went (Go: formatTool).
fn format_tool(c: &ToolEvent, full: bool) -> String {
    let mut call = c.name.clone();
    let about = if c.target.is_empty() { &c.detail } else { &c.target };
    if !about.is_empty() {
        if full {
            call += &format!(": {}", indent(about));
        } else {
            call += &format!(": {}", elide(about, TOOL_TEXT_WIDTH));
        }
    }
    let m = tool_meta(c);
    if !m.is_empty() {
        call += &format!(" ({m})");
    }
    let (status, output) = match &c.result {
        Some(r) => (r.status.as_str(), r.output.trim_end_matches('\n')),
        None => (transcript::STATUS_PENDING, ""),
    };
    let lines = if output.trim().is_empty() {
        0
    } else {
        output.matches('\n').count() + 1
    };
    let result = match status {
        transcript::STATUS_PENDING => "no result yet",
        transcript::STATUS_BACKGROUND => "running in the background",
        s => s,
    };
    if full && lines > 0 {
        return format!("{call} → {result} ({})\n{}", plural(lines, "line"), indent_all(output));
    }
    if status == transcript::STATUS_PENDING || status == transcript::STATUS_BACKGROUND || lines == 0 {
        return format!("{call} → {result}");
    }
    if lines == 1 || status == transcript::STATUS_ERROR {
        // An error's first line usually says what failed.
        let mut s = format!("{call} → {result}: {}", cut_line(output, TOOL_TEXT_WIDTH));
        if lines > 1 {
            s += &format!(" ({})", plural(lines, "line"));
        }
        return s;
    }
    format!("{call} → {result} ({})", plural(lines, "line"))
}

/// Decodes data into the named fields of a Go struct, as encoding/json does:
/// keys match a field's name ignoring case, the last one wins, and None means
/// the data isn't one (it doesn't decode, or a field's value isn't of its
/// type). null decodes as no fields.
fn go_fields(data: Option<&str>, names: &[&str]) -> Option<Vec<Option<serde_json::Value>>> {
    let v: serde_json::Value = serde_json::from_str(data?).ok()?;
    let mut out = vec![None; names.len()];
    match v {
        serde_json::Value::Null => {}
        serde_json::Value::Object(m) => {
            for (k, val) in m {
                if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(&k)) {
                    out[i] = Some(val);
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

/// A Go int field's value: absent or null is 0; anything but a whole number
/// is an error (None).
fn go_int(v: &Option<serde_json::Value>) -> Option<i64> {
    match v {
        None | Some(serde_json::Value::Null) => Some(0),
        Some(serde_json::Value::Number(n)) => n.as_i64(),
        _ => None,
    }
}

/// A Go float64 field's value.
fn go_float(v: &Option<serde_json::Value>) -> Option<f64> {
    match v {
        None | Some(serde_json::Value::Null) => Some(0.0),
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// A Go string field's value.
fn go_string(v: &Option<serde_json::Value>) -> Option<String> {
    match v {
        None | Some(serde_json::Value::Null) => Some(String::new()),
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The numbers of an edit worth a glance, "+3 -1", or "" (Go: toolMeta).
fn tool_meta(c: &ToolEvent) -> String {
    let Some(meta) = &c.meta else { return String::new() };
    if c.action != "edit" || meta.as_str().is_empty() {
        return String::new();
    }
    let Some(f) = go_fields(Some(meta.as_str()), &["added", "removed"]) else {
        return String::new();
    };
    match (go_int(&f[0]), go_int(&f[1])) {
        (Some(a), Some(r)) => format!("+{a} -{r}"),
        _ => String::new(),
    }
}

/// Renders a harness record: its text, or what its subtype says (Go:
/// formatSystem).
fn format_system(s: &SystemEvent, text: &dyn Fn(&str) -> String) -> String {
    let data = s.data.as_ref().map(|d| d.as_str());
    match s.subtype.as_str() {
        "turn_duration" => {
            if let Some(f) = go_fields(data, &["durationMs"])
                && let Some(ms) = go_float(&f[0])
                && ms > 0.0
            {
                let d = (ms * 1e6) as i64;
                return format!("turn took {}", gotime::format_duration_nanos(round_nanos(d, 100_000_000)));
            }
        }
        "interrupt" | "api_error" => return text(&s.text),
        "model_fallback" => {
            if let Some(f) = go_fields(data, &["From", "To"])
                && let (Some(from), Some(to)) = (go_string(&f[0]), go_string(&f[1]))
                && !from.is_empty()
                && !to.is_empty()
            {
                return format!("model fallback: {from} → {to}");
            }
        }
        _ => {}
    }
    if s.text.is_empty() {
        return s.subtype.clone();
    }
    format!("{}: {}", s.subtype, text(&s.text))
}

/// Go's Duration.Round(m) for a duration d of nanoseconds, m > 0: halfway
/// values round away from zero.
fn round_nanos(d: i64, m: i64) -> i64 {
    let mut r = d % m;
    if d < 0 {
        r = -r;
        if r + r < m {
            return d + r;
        }
        return d.checked_sub(m - r).unwrap_or(i64::MIN);
    }
    if r + r < m {
        return d - r;
    }
    d.checked_add(m - r).unwrap_or(i64::MAX)
}

/// s on one line (its lines and runs of spaces joined by one space), cut to
/// width runes in the middle, so a command's or a path's head and its telling
/// tail both show: "go test ./… -run TestSpin" (Go: elide).
pub fn elide(s: &str, width: usize) -> String {
    let r: Vec<char> = s.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    if r.len() <= width {
        return r.into_iter().collect();
    }
    let tail = (width - 1) / 2;
    let head: String = r[..width - 1 - tail].iter().collect();
    let end: String = r[r.len() - tail..].iter().collect();
    format!("{}…{}", head.trim(), end.trim())
}

/// The first non-blank line of s, cut to width runes; a cut (or dropped
/// lines) is marked "…" (Go: cutLine).
pub fn cut_line(s: &str, width: usize) -> String {
    let s = s.trim();
    let (line, rest) = s.split_once('\n').unwrap_or((s, ""));
    let mut line = line.trim().to_string();
    let mut cut = !rest.trim().is_empty();
    let r: Vec<char> = line.chars().collect();
    if r.len() > width {
        line = r[..width - 1].iter().collect::<String>().trim().to_string();
        cut = true;
    }
    if cut {
        line.push('…');
    }
    line
}

/// Indents the second and later lines of s under an event's text (Go:
/// indent).
fn indent(s: &str) -> String {
    s.trim().replace('\n', &format!("\n{}", " ".repeat(EVENT_HEAD_WIDTH)))
}

/// Indents every line of s under an event's text (Go: indentAll).
fn indent_all(s: &str) -> String {
    " ".repeat(EVENT_HEAD_WIDTH) + &indent(s)
}

/// Whether the named agent has a terminal log (Go: logExists).
fn log_exists(svc: &AgentService, name: &str) -> bool {
    std::fs::metadata(svc.config().log_path(name)).is_ok()
}

/// The last n lines of the agent's terminal log, or a line saying it has
/// logged nothing yet (Go: terminalLogChunks).
fn terminal_log_chunks(svc: &AgentService, name: &str, n: i64) -> anyhow::Result<Vec<LogsChunk>> {
    let cfg = svc.config();
    let n_us = n.max(0) as usize;
    let mut res = tail_log_lines(&cfg.log_path(name), n_us);
    let empty = |res: &anyhow::Result<Vec<String>>| match res {
        Err(e) => goos::is_not_exist(e),
        Ok(lines) => lines.is_empty(),
    };
    if empty(&res)
        && let Ok(captured) = svc.capture(name, n.clamp(0, i32::MAX as i64) as i32, false)
    {
        let live = util::terminal_lines(&captured);
        if !live.is_empty() {
            res = Ok(live[live.len().saturating_sub(n_us)..].to_vec());
        }
    }
    if empty(&res) {
        if svc.agent(name).is_err() {
            return Err(no_log(svc, name));
        }
        return Ok(vec![text_chunk(format!(
            "Agent {name} has logged no output yet ({}).\n",
            abs_path(&cfg.log_path(name))
        ))]);
    }
    Ok(res?.into_iter().map(|line| text_chunk(line + "\n")).collect())
}

/// The error for a name with neither a log nor an agent: it suggests close
/// names among both (Go: noLog).
fn no_log(svc: &AgentService, name: &str) -> anyhow::Error {
    let cfg = svc.config();
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&cfg.log_dir) {
        let mut files: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        for f in files {
            if let Some(n) = f.strip_suffix(".log") {
                names.push(n.to_string());
            }
        }
    }
    if let Ok(agents) = svc.list() {
        for a in agents {
            if !names.contains(&a.name) {
                names.push(a.name);
            }
        }
    }
    let msg = format!(
        "no log for {}: no agent by that name has run on host {}",
        strconv::quote(name),
        cfg.host
    );
    let similar = util::similar(name, &names);
    if !similar.is_empty() {
        return anyhow::anyhow!("{msg}; did you mean {}?", util::or_list(&similar, None));
    }
    anyhow::anyhow!("{msg} (logs are in {})", abs_path(&cfg.log_dir))
}

/// path made absolute, or as it is if that fails (Go: absPath).
pub(crate) fn abs_path(path: &str) -> String {
    filepath::abs(path).unwrap_or_else(|_| path.to_string())
}

/// How much of the end of a log file tail_log_lines reads first. Logs grow to
/// megabytes, mostly escape sequences; the window doubles until it holds
/// enough lines of text.
const LOG_TAIL_CHUNK: u64 = 256 << 10;

/// The last n lines of text in the terminal log at path (Go: tailLogLines).
pub fn tail_log_lines(path: &str, n: usize) -> anyhow::Result<Vec<String>> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).map_err(|e| anyhow::Error::new(PathError::new("open", path, e)))?;
    let size = f
        .metadata()
        .map_err(|e| anyhow::Error::new(PathError::new("stat", path, e)))?
        .len();
    let mut chunk = LOG_TAIL_CHUNK;
    loop {
        let off = size.saturating_sub(chunk);
        let mut buf = vec![0u8; (size - off) as usize];
        let mut read = 0;
        while read < buf.len() {
            match f.read_at(&mut buf[read..], off + read as u64) {
                Ok(0) => break,
                Ok(k) => read += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(anyhow::Error::new(PathError::new("read", path, e)).context("reading log"));
                }
            }
        }
        buf.truncate(read);
        let mut lines = util::terminal_lines(&String::from_utf8_lossy(&buf));
        if off > 0 && !lines.is_empty() {
            lines.remove(0); // may start mid-line, or mid-sequence
        }
        if lines.len() >= n || off == 0 {
            let from = lines.len().saturating_sub(n);
            return Ok(lines.split_off(from));
        }
        chunk *= 2;
    }
}

/// Answers another host's agent logs name@host: what agent logs prints here,
/// cut to fit a bus message (Go: serveLogs, in remote_ops.go). The bus's
/// handler (remote_ops::bus_handler) calls it with a quiet Ctx of its own.
pub fn serve_logs(ctx: &Ctx, _c: &Context, req: LogsRequest) -> anyhow::Result<LogsResponse> {
    // An agent's name only: a peer reads no session or path here.
    let opts = LogsOptions {
        lines: req.lines,
        full: req.full,
        json: req.json,
        terminal: req.terminal,
        ..Default::default()
    };
    opts.check()?;
    let (chunks, why) = build_logs(ctx, &req.agent, &opts)?;
    let budget = MAX_LOGS_REPLY as i64 - json_size(&why) as i64 - 128;
    let (output, truncated) = fit_logs(&chunks, budget, !opts.json);
    Ok(LogsResponse {
        output,
        hint: why,
        truncated: truncated as i64,
    })
}

#[cfg(test)]
#[path = "logs_test.rs"]
pub(crate) mod logs_test;

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
    use_line: "logs <name>",
    short: "Print what an agent did (prompts, replies, tool calls), even after cleanup",
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
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (r#ref, remote) = super::remote::parse_target(ctx, &args.args[0])?;
    let opts = LogsOptions {
        lines: args.lines,
        full: args.full,
        json: args.json,
        terminal: args.terminal,
    };
    opts.check()?;
    if remote {
        let req = LogsRequest {
            agent: String::new(),
            lines: opts.lines,
            full: opts.full,
            json: opts.json,
            terminal: opts.terminal,
        };
        return super::remote_ops::run_remote_logs(ctx, &r#ref, req);
    }
    let (chunks, why) = build_logs(ctx, &r#ref.name, &opts)?;
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
}

impl LogsOptions {
    pub fn check(&self) -> anyhow::Result<()> {
        if self.lines < 1 {
            anyhow::bail!("--lines must be at least 1");
        }
        if self.json && (self.terminal || self.full) {
            anyhow::bail!("--json prints transcript events whole; it doesn't go with --terminal or --full");
        }
        Ok(())
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
    // The name becomes a path component.
    agent_service::validate_agent_name(name)?;
    let svc: &AgentService = ctx.service()?;
    if o.terminal {
        return Ok((terminal_log_chunks(svc, name, o.lines)?, String::new()));
    }

    let (sessions, why) = read_transcripts(svc, name, o.lines);
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
    let mut chunks = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        if i > 0 {
            chunks.push(LogsChunk {
                text: format!(
                    "--- session {} ({})\n",
                    s.file.session.session_id, s.file.session.harness
                ),
                session: true,
            });
        }
        for e in &s.events {
            chunks.push(text_chunk(format_event(e, o.full) + "\n"));
        }
    }
    Ok((chunks, String::new()))
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
    let opts = LogsOptions {
        lines: req.lines,
        full: req.full,
        json: req.json,
        terminal: req.terminal,
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

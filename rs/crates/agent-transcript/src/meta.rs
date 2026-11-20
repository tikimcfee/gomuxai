//! What a session says about itself: its working directory, title, agent name,
//! model, version and git branch, and the session it continued in, from its
//! transcript's records and the files the harness keeps beside it. These are
//! the one set of rules for them. The readers apply them to every line as they
//! parse; `agent sessions` (`agent-discovery`) applies them to the head and
//! tail of a transcript it reads, never the whole, its tail backwards.
//!
//! So a value is placed by its line's byte offset, not by when it was
//! harvested: each Harvest function takes the line's offset, and the newest
//! line's value wins (the oldest's, for the working directory), whatever order
//! the lines come in. A value from the files beside a transcript counts as from
//! offset 0.

use std::collections::HashMap;

use agent_base::gostd::{filepath, json};
use serde::Deserialize;

use crate::events::{ParentSession, Session, trim_bytes_space};
use crate::kimi::kimi_object;

/// Holds the byte offset of the line each of a [`Session`]'s harvested fields
/// came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct MetaAt {
    pub(crate) cwd: i64,
    pub(crate) title: i64,
    pub(crate) agent_name: i64,
    pub(crate) model: i64,
    pub(crate) version: i64,
    pub(crate) continued_in: i64,
}

/// Reports whether a value from the line at `at` replaces the one a field holds
/// from the line at `was`: it does if the field has none yet (`have` is false)
/// or the line is newer.
fn newer(have: bool, at: i64, was: i64) -> bool {
    !have || at > was
}

/// A session's project: the last element of the working directory it recorded
/// (a `file://` URI too), by one rule for every harness. It is never the name
/// of the folder the harness stores the session in.
pub fn project_of(cwd: &str) -> String {
    let p = cwd
        .strip_prefix("file://")
        .unwrap_or(cwd)
        .trim_end_matches('/');
    if p.is_empty() {
        return String::new();
    }
    filepath::base(p)
}

impl Session {
    /// Records the working directory a line at `at` names: the session's is the
    /// oldest one recorded, the directory it started in. `project` is its last
    /// element.
    pub(crate) fn set_cwd(&mut self, cwd: &str, at: i64) {
        if cwd.is_empty() || (!self.cwd.is_empty() && at >= self.at.cwd) {
            return;
        }
        self.cwd = cwd.to_string();
        self.project = project_of(cwd);
        self.at.cwd = at;
    }

    /// Records a title: a custom one (a person's rename) wins over every other,
    /// and among titles of a kind the newest wins.
    pub(crate) fn set_title(&mut self, title: &str, custom: bool, at: i64) {
        if title.is_empty() || (self.custom_title && !custom) {
            return;
        }
        if custom != self.custom_title || newer(!self.title.is_empty(), at, self.at.title) {
            self.title = title.to_string();
            self.custom_title = custom;
            self.at.title = at;
        }
    }

    /// Reports whether `title` is a custom title, which no other kind replaces:
    /// a reader of a transcript's end need look no further for one.
    pub fn title_is_custom(&self) -> bool {
        self.custom_title
    }

    pub(crate) fn set_model(&mut self, model: &str, at: i64) {
        if !model.is_empty() && newer(!self.model.is_empty(), at, self.at.model) {
            self.model = model.to_string();
            self.at.model = at;
        }
    }

    /// Records the harness version a line at `at` names, and the git branch the
    /// same line names: the newest line with a version sets both, even an empty
    /// branch (a session that left its repository).
    pub(crate) fn set_version(&mut self, version: &str, branch: &str, at: i64) {
        if !version.is_empty() && newer(!self.version.is_empty(), at, self.at.version) {
            self.version = version.to_string();
            self.git_branch = branch.to_string();
            self.at.version = at;
        }
    }

    pub(crate) fn set_agent_name(&mut self, name: &str, at: i64) {
        if !name.is_empty() && newer(!self.agent_name.is_empty(), at, self.at.agent_name) {
            self.agent_name = name.to_string();
            self.at.agent_name = at;
        }
    }

    pub(crate) fn set_continued_in(&mut self, id: &str, at: i64) {
        if !id.is_empty() && newer(!self.continued_in.is_empty(), at, self.at.continued_in) {
            self.continued_in = id.to_string();
            self.at.continued_in = at;
        }
    }

    /// Records in `self` what a Claude subagent's `meta.json` says about it:
    /// the call that started it, and its description as the title, since a
    /// subagent's transcript records none.
    pub fn harvest_claude_subagent(&mut self, meta: &ClaudeSubagentMeta) {
        if let Some(ref mut parent) = self.parent_session
            && parent.tool_id.is_empty()
        {
            parent.tool_id = meta.tool_use_id.clone();
        }
        if self.title.is_empty() {
            self.title = meta.description.clone();
        }
    }

    /// Records in `self`, the conversation `self.session_id`, what agy keeps
    /// beside its transcript: its workspace (`history.jsonl`'s, else a
    /// subagent's own), the conversation that started a subagent, and its
    /// title: the one `annotations/<id>.pbtxt` holds, else a subagent's role.
    pub fn harvest_agy(&mut self, f: &AgyFiles) {
        self.set_cwd(&f.workspace, 0);
        if let Some(ref sub) = f.subagent {
            self.parent_session = Some(ParentSession {
                session_id: sub.parent.clone(),
                tool_id: String::new(),
            });
            self.set_cwd(&sub.workspace, 0);
            self.set_title(&sub.role, false, 0);
        }
        let pbtxt = filepath::join(&[
            f.home.as_str(),
            "annotations",
            &format!("{}.pbtxt", self.session_id),
        ]);
        if let Ok(data) = std::fs::read(pbtxt) {
            let t = agy_annotation_title(&data);
            if !t.is_empty() {
                self.title = t;
            }
        }
    }
}

/// The part of a Claude Code record that says something about its session.
/// Decoded with Go's rules (`gostd::json`), from the same object as the
/// record it is part of.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ClaudeMeta {
    #[serde(rename = "type", default)]
    pub(crate) r#type: String,
    #[serde(rename = "sessionId", default)]
    pub(crate) session_id: String,
    #[serde(rename = "agentId", default)]
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) cwd: String,
    #[serde(default)]
    pub(crate) version: String,
    #[serde(rename = "gitBranch", default)]
    pub(crate) git_branch: String,
    #[serde(rename = "aiTitle", default)]
    pub(crate) ai_title: String,
    #[serde(rename = "customTitle", default)]
    pub(crate) custom_title: String,
    #[serde(rename = "agentName", default)]
    pub(crate) agent_name: String,
    #[serde(rename = "continuedInSessionId", default)]
    pub(crate) continued_in: String,
    #[serde(rename = "isSidechain", default)]
    pub(crate) is_sidechain: bool,
}

const CLAUDE_META_RECORDS: &[&[u8]] = &[
    br#""ai-title""#,
    br#""custom-title""#,
    br#""agent-name""#,
    br#""continued-in""#,
];

/// Reports, from a line's bytes alone, whether it may be one of the records of
/// a Claude Code transcript that carry only session metadata: a title, the
/// agent name, the session it continued in. Every other record says at most the
/// working directory, model, version and git branch, so a reader that has those
/// need decode no other line.
pub fn claude_meta_record(line: &[u8]) -> bool {
    CLAUDE_META_RECORDS
        .iter()
        .any(|t| line.windows(t.len()).any(|w| w == *t))
}

/// Records in `s` what the line of a Claude Code transcript at byte offset `at`
/// says about its session. Lines may come in any order; one that isn't a JSON
/// object says nothing.
pub fn harvest_claude(s: &mut Session, line: &[u8], at: i64) {
    // The Go's struct embeds claudeMeta beside the message: both are decoded
    // from the one object, and the line says nothing if either fails.
    #[derive(Deserialize)]
    struct Record {
        #[serde(default)]
        message: Option<MsgModel>,
    }
    #[derive(Deserialize)]
    struct MsgModel {
        #[serde(default)]
        model: String,
    }
    let Ok(doc) = json::parse(line) else {
        return;
    };
    let (Ok(meta), Ok(r)) = (doc.decode::<ClaudeMeta>(), doc.decode::<Record>()) else {
        return;
    };
    let model = r.message.as_ref().map(|m| m.model.as_str()).unwrap_or("");
    harvest_claude_meta(s, &meta, model, at);
}

pub(crate) fn harvest_claude_meta(s: &mut Session, r: &ClaudeMeta, model: &str, at: i64) {
    if r.is_sidechain && r.agent_id.is_empty() {
        return;
    }
    match r.r#type.as_str() {
        "ai-title" => s.set_title(&r.ai_title, false, at),
        "custom-title" => s.set_title(&r.custom_title, true, at),
        "agent-name" => s.set_agent_name(&r.agent_name, at),
        "continued-in" => s.set_continued_in(&r.continued_in, at),
        "assistant" if model != "<synthetic>" => {
            s.set_model(model, at);
        }
        _ => {}
    }
    if !r.agent_id.is_empty() {
        // A subagent's own transcript: its session is named after it, and its
        // records name the session that started it.
        s.session_id = format!("agent-{}", r.agent_id);
        if !r.session_id.is_empty() {
            let parent = s.parent_session.get_or_insert_with(ParentSession::default);
            parent.session_id = r.session_id.clone();
        }
    } else if !r.session_id.is_empty() {
        s.session_id = r.session_id.clone();
    }
    s.set_cwd(&r.cwd, at);
    if matches!(
        r.r#type.as_str(),
        "user" | "assistant" | "attachment" | "system"
    ) {
        s.set_version(&r.version, &r.git_branch, at);
    }
}

/// The `meta.json` Claude Code keeps beside a subagent's transcript
/// (`<session id>/subagents/agent-<id>.meta.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ClaudeSubagentMeta {
    #[serde(rename = "agentType", default)]
    pub agent_type: String,
    #[serde(default)]
    pub description: String,
    /// The call that started the subagent.
    #[serde(rename = "toolUseId", default)]
    pub tool_use_id: String,
    /// Whether it finished; `None` if the file doesn't say.
    #[serde(default)]
    pub finished: Option<bool>,
}

/// Returns the session folder (`<session id>`, beside the session's
/// `<session id>.jsonl`) that holds the Claude subagent transcript at `path`:
/// `<session id>/subagents/agent-<id>.jsonl` for an agent the session started,
/// or `<session id>/subagents/workflows/<run>/agent-<id>.jsonl` for one of a
/// workflow's agents. Returns `""` for any other path.
pub fn claude_subagent_session(path: &str) -> String {
    let base = filepath::base(path);
    if !base.starts_with("agent-") || !base.ends_with(".jsonl") {
        return String::new();
    }
    let mut dir = filepath::dir(path);
    if filepath::base(&dir) != "subagents" {
        let workflows = filepath::dir(&dir);
        if filepath::base(&workflows) != "workflows"
            || filepath::base(&filepath::dir(&workflows)) != "subagents"
        {
            return String::new();
        }
        dir = filepath::dir(&workflows);
    }
    filepath::dir(&dir)
}

/// Reads the `meta.json` beside the Claude subagent transcript at `path`.
/// Returns `None` if there is none, or it isn't JSON that Go decodes into the
/// struct (`null` is, an array isn't).
pub fn read_claude_subagent_meta(path: &str) -> Option<ClaudeSubagentMeta> {
    let meta_path = format!("{}.meta.json", path.strip_suffix(".jsonl").unwrap_or(path));
    let data = std::fs::read(meta_path).ok()?;
    json::from_slice(&data).ok()
}

/// Records in `s` what the line of a Kimi Code wire log at byte offset `at`
/// says about its session: the newest `llm.request`'s model. Lines may come in
/// any order; a line that isn't an `llm.request` is passed over without being
/// decoded. (The title and working directory are in `state.json`:
/// [`crate::file_options`].)
pub fn harvest_kimi(s: &mut Session, line: &[u8], at: i64) {
    if !line
        .windows(b"\"llm.request\"".len())
        .any(|w| w == b"\"llm.request\"")
    {
        return;
    }
    let r = kimi_object(line);
    if r.str("type") == "llm.request" {
        s.set_model(&r.str("model"), at);
    }
}

/// agy's record of a subagent conversation,
/// `brain/<parent>/.system_generated/subagents/<conversation id>.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgySubagent {
    pub conversation_id: String,
    /// The conversation that started it.
    pub parent: String,
    pub role: String,
    /// agy's `SUBAGENT_STATE_*`.
    pub state: String,
    /// The first of its workspace URIs that is a `file://` one, as a path.
    pub workspace: String,
}

/// Reads the subagent descriptor at `path`,
/// `brain/<parent>/.system_generated/subagents/<id>.json`. Returns `None` if it
/// isn't one.
pub fn read_agy_subagent(path: &str) -> Option<AgySubagent> {
    let data = std::fs::read(path).ok()?;
    #[derive(Default, Deserialize)]
    struct Descriptor {
        #[serde(default)]
        role: String,
    }
    // agy is Go, and writes a nil slice as null: null is a field's zero value.
    #[derive(Deserialize)]
    struct Doc {
        #[serde(rename = "conversationId", default)]
        conversation_id: String,
        #[serde(rename = "subagentDescriptor", default)]
        descriptor: Descriptor,
        #[serde(default)]
        state: String,
        #[serde(rename = "workspaceUris", default)]
        workspace_uris: Vec<String>,
    }
    let d: Doc = json::from_slice(&data).ok()?;
    if d.conversation_id.is_empty() {
        return None;
    }
    let parent = filepath::base(&filepath::dir(&filepath::dir(&filepath::dir(path))));
    let mut workspace = String::new();
    for u in &d.workspace_uris {
        let w = file_uri_path(u);
        if !w.is_empty() {
            workspace = w;
            break;
        }
    }
    Some(AgySubagent {
        conversation_id: d.conversation_id,
        parent,
        role: d.descriptor.role,
        state: d.state,
        workspace,
    })
}

/// Maps a `file://` URI to a clean local path, or `""`: Go's `url.Parse`, a
/// scheme of `file` and a path that isn't empty, then `filepath.Clean`. A path
/// whose escapes decode to bytes that aren't UTF-8 reads as Go writes such a
/// string in JSON, each bad byte a U+FFFD.
pub(crate) fn file_uri_path(uri: &str) -> String {
    match url_file_path(uri.as_bytes()) {
        Some(p) if !p.is_empty() => filepath::clean(&json::lossy(&p)),
        _ => String::new(),
    }
}

/// The path `url.Parse` reads from `uri`, decoded, if it parses and its
/// scheme is `file`. This is the part of Go's `net/url` a `file` URI goes
/// through: the fragment cut off first and checked, the scheme, the query cut
/// off, a rootless (opaque) rest, the authority (user info, host, port)
/// checked, and the path's escapes.
fn url_file_path(uri: &[u8]) -> Option<Vec<u8>> {
    if uri.iter().any(|&b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let (u, frag) = match uri.iter().position(|&b| b == b'#') {
        Some(i) => (&uri[..i], Some(&uri[i + 1..])),
        None => (uri, None),
    };
    // getScheme: letters, then letters, digits, "+", "-" or "." up to a ":".
    let mut scheme_end = None;
    for (i, &c) in u.iter().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i > 0 => {}
            b':' if i > 0 => {
                scheme_end = Some(i);
                break;
            }
            _ => return None,
        }
    }
    let i = scheme_end?;
    if !u[..i].eq_ignore_ascii_case(b"file") {
        return None;
    }
    let mut rest = &u[i + 1..];
    let questions = rest.iter().filter(|&&b| b == b'?').count();
    if rest.ends_with(b"?") && questions == 1 {
        rest = &rest[..rest.len() - 1];
    } else if let Some(q) = rest.iter().position(|&b| b == b'?') {
        rest = &rest[..q];
    }
    if !rest.starts_with(b"/") {
        // A rootless path is opaque: the URL has no path.
        return None;
    }
    if let Some(after) = rest.strip_prefix(b"//") {
        let end = after.iter().position(|&b| b == b'/').unwrap_or(after.len());
        url_authority(&after[..end])?;
        rest = &after[end..];
    }
    let path = url_unescape(rest, UrlPart::Path)?;
    if let Some(f) = frag
        && !f.is_empty()
    {
        url_unescape(f, UrlPart::Fragment)?;
    }
    Some(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UrlPart {
    Path,
    Fragment,
    Host,
    Zone,
    UserPassword,
}

/// Go's `parseAuthority`: `[userinfo@]host`, the user info of allowed
/// characters only and its escapes well formed.
fn url_authority(a: &[u8]) -> Option<()> {
    let (user, host) = match a.iter().rposition(|&b| b == b'@') {
        Some(i) => (Some(&a[..i]), &a[i + 1..]),
        None => (None, a),
    };
    url_host(host)?;
    if let Some(user) = user {
        let allowed = |c: &u8| c.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(c);
        if !user.iter().all(allowed) {
            return None;
        }
        url_unescape(user, UrlPart::UserPassword)?;
    }
    Some(())
}

/// Go's `parseHost`, for a scheme other than http(s): an IPv6 literal in
/// brackets, or a name of host characters, either with an optional `:port`.
fn url_host(h: &[u8]) -> Option<()> {
    if let Some(open) = h.iter().rposition(|&b| b == b'[') {
        if open > 0 {
            return None;
        }
        let close = h.iter().rposition(|&b| b == b']')?;
        let port = &h[close + 1..];
        if !url_valid_port(port) {
            return None;
        }
        url_unescape(port, UrlPart::Host)?;
        let name = &h[1..close];
        let name = match name.windows(3).position(|w| w == b"%25") {
            Some(z) => {
                let mut n = url_unescape(&name[..z], UrlPart::Host)?;
                n.extend(url_unescape(&name[z..], UrlPart::Zone)?);
                n
            }
            None => url_unescape(name, UrlPart::Host)?,
        };
        // netip.ParseAddr, and only an IPv6 address will do.
        let name = std::str::from_utf8(&name).ok()?;
        let (addr, zone) = match name.split_once('%') {
            Some((a, z)) => (a, Some(z)),
            None => (name, None),
        };
        if zone == Some("") {
            return None;
        }
        addr.parse::<std::net::Ipv6Addr>().ok()?;
        return Some(());
    }
    if let Some(first) = h.iter().position(|&b| b == b':') {
        // More than one colon: the port is after the last.
        let last = h.iter().rposition(|&b| b == b':').unwrap_or(first);
        if !url_valid_port(&h[last..]) {
            return None;
        }
    }
    url_unescape(h, UrlPart::Host)?;
    Some(())
}

/// Go's `validOptionalPort`: empty, or a colon and digits.
fn url_valid_port(p: &[u8]) -> bool {
    p.is_empty() || (p[0] == b':' && p[1..].iter().all(u8::is_ascii_digit))
}

/// Go's `shouldEscape` for a host (and a zone): what a host may not hold
/// unescaped.
fn url_host_escapes(c: u8) -> bool {
    !(c.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:[]<>\"".contains(&c))
}

/// Go's `url.unescape`: every `%` followed by two hex digits (in a host, only
/// a non-ASCII byte or `%25` may be escaped), a host's other ASCII characters
/// allowed ones; the bytes the escapes stand for.
fn url_unescape(s: &[u8], part: UrlPart) -> Option<Vec<u8>> {
    let unhex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            if i + 2 >= s.len() || !s[i + 1].is_ascii_hexdigit() || !s[i + 2].is_ascii_hexdigit() {
                return None;
            }
            let pct25 = &s[i..i + 3] == b"%25";
            if part == UrlPart::Host && unhex(s[i + 1]) < 8 && !pct25 {
                return None;
            }
            if part == UrlPart::Zone {
                let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
                if !pct25 && v != b' ' && url_host_escapes(v) {
                    return None;
                }
            }
            i += 3;
        } else {
            if matches!(part, UrlPart::Host | UrlPart::Zone)
                && s[i] < 0x80
                && url_host_escapes(s[i])
            {
                return None;
            }
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            out.push((unhex(s[i + 1]) << 4) | unhex(s[i + 2]));
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Reads agy's `history.jsonl` in `home` (the dir holding `brain/`), one
/// `{"workspace","conversationId"?}` line per typed prompt: each conversation's
/// workspace, the first one recorded. Missing or malformed files and lines
/// contribute nothing.
pub fn read_agy_history(home: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(data) = std::fs::read(filepath::join(&[home, "history.jsonl"])) else {
        return out;
    };
    #[derive(Deserialize)]
    struct Entry {
        #[serde(rename = "conversationId", default)]
        conversation_id: String,
        #[serde(default)]
        workspace: String,
    }
    for line in data.split(|&b| b == b'\n') {
        // The line is decoded whole: JSON's whitespace is space, tab, CR
        // and LF only, so a line a form feed ends doesn't read.
        if trim_bytes_space(line).is_empty() {
            continue;
        }
        let Ok(e) = json::from_slice::<Entry>(line) else {
            continue;
        };
        if e.conversation_id.is_empty() || e.workspace.is_empty() {
            continue;
        }
        out.entry(e.conversation_id)
            .or_insert_with(|| filepath::clean(&e.workspace));
    }
    out
}

/// What agy keeps beside a conversation's transcript.
#[derive(Debug, Clone, Default)]
pub struct AgyFiles {
    /// The dir holding `brain/`, and `annotations/` beside it.
    pub home: String,
    /// The conversation's workspace as `history.jsonl` records it
    /// ([`read_agy_history`]).
    pub workspace: String,
    /// The conversation's descriptor, if it is a subagent.
    pub subagent: Option<AgySubagent>,
}

/// Pulls the `title:"..."` field out of an annotation file (protobuf text
/// format, whose string escapes are a subset of Go's), or `""`.
pub(crate) fn agy_annotation_title(data: &[u8]) -> String {
    let Some(i) = data.windows(7).position(|w| w == b"title:\"") else {
        return String::new();
    };
    let Some(unquoted) = unquote_go_prefix(&data[i + "title:".len()..]) else {
        return String::new();
    };
    // Go's string holds the bytes the escapes make, UTF-8 or not, and its
    // JSON encoder writes each bad byte as U+FFFD.
    json::lossy(trim_bytes_space(&unquoted)).into_owned()
}

/// Decodes a leading Go double-quoted string literal (`strconv.QuotedPrefix` +
/// `strconv.Unquote`), including octal `\ooo` and hex `\xNN` byte escapes that
/// protobuf text format writes for UTF-8, into bytes. As in Go, an unescaped
/// byte that isn't UTF-8 becomes U+FFFD then, before the escapes' bytes join
/// it.
fn unquote_go_prefix(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            return Some(out);
        }
        if b == b'\n' {
            return None;
        }
        if b >= 0x80 {
            // One UTF-8 character, or one bad byte read as U+FFFD.
            let n = match b {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            match bytes.get(i..i + n).map(std::str::from_utf8) {
                Some(Ok(s)) => {
                    out.extend_from_slice(s.as_bytes());
                    i += n;
                }
                _ => {
                    out.extend_from_slice("\u{FFFD}".as_bytes());
                    i += 1;
                }
            }
            continue;
        }
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            return None;
        }
        match bytes[i] {
            b'a' => {
                out.push(0x07);
                i += 1;
            }
            b'b' => {
                out.push(0x08);
                i += 1;
            }
            b'f' => {
                out.push(0x0c);
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'v' => {
                out.push(0x0b);
                i += 1;
            }
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'"' => {
                out.push(b'"');
                i += 1;
            }
            b'0'..=b'7' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let d0 = (bytes[i] as char).to_digit(8)?;
                let d1 = (bytes[i + 1] as char).to_digit(8)?;
                let d2 = (bytes[i + 2] as char).to_digit(8)?;
                let v = (d0 << 6) | (d1 << 3) | d2;
                if v > 255 {
                    return None;
                }
                out.push(v as u8);
                i += 3;
            }
            b'x' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push(((hi << 4) | lo) as u8);
                i += 3;
            }
            b'u' => {
                if i + 4 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 5]).ok()?;
                let cp = u32::from_str_radix(hex, 16).ok()?;
                let ch = char::from_u32(cp)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                i += 5;
            }
            b'U' => {
                if i + 8 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 9]).ok()?;
                let cp = u32::from_str_radix(hex, 16).ok()?;
                let ch = char::from_u32(cp)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                i += 9;
            }
            _ => return None,
        }
    }
    None
}

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use agent_base::gostd::json;
use regex::Regex;
use serde::Deserialize;

use crate::claude_rewind::ClaudeNode;
use crate::events::{
    Event, Image, KIND_PROMPT, KIND_REQUEST, KIND_SAY, KIND_SYSTEM, KIND_THINK, KIND_TOOL,
    LineReader, OP_ADD, OP_UPDATE, ORIGIN_COMMAND, ORIGIN_DELIVERED, ORIGIN_HUMAN, ORIGIN_PEER,
    ORIGIN_QUEUED, ORIGIN_SYSTEM, ORIGIN_TASK_NOTIFICATION, Options, Parser, PromptEvent, RawJson,
    Reader, RequestEvent, STATUS_BACKGROUND, STATUS_ERROR, STATUS_INTERRUPTED, STATUS_OK,
    STATUS_PENDING, SayEvent, Session, SystemEvent, ThinkEvent, ToolEvent, ToolResult, TurnRef,
    TurnsFn, deserialize_present_raw, new_parser, trim_bytes_space,
};
use crate::meta::{ClaudeMeta, claude_subagent_session, harvest_claude_meta};
use crate::tools::summarize_tool;
use crate::transcript::{READ_CHUNK, read_at_some};

/// Reads Claude Code session transcripts
/// (`~/.claude/projects/<project>/<session id>.jsonl`, a subagent's
/// `<session id>/subagents/agent-<agent id>.jsonl`, and a workflow agent's
/// `<session id>/subagents/workflows/<run>/agent-<agent id>.jsonl`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Claude;

impl Reader for Claude {
    fn harness(&self) -> &'static str {
        "claude"
    }

    fn new_parser(&self, opts: Options) -> Parser {
        let requests = opts.requests;
        let c = ClaudeLines {
            opts,
            sess: Session::default(),
            turn: None,
            index: 0,
            numbered: false,
            prompts: 0,
            tools: HashMap::new(),
            pending: Vec::new(),
            blobs: if requests { Some(HashMap::new()) } else { None },
            closing: None,
            nodes: HashMap::new(),
            asked: HashMap::new(),
            latest: HashMap::new(),
            abandoned: HashMap::new(),
        };
        new_parser("claude", Box::new(c))
    }
}

/// The part of a transcript line the parser reads. Decoded with Go's rules
/// ([`ClaudeRecord::decode`]): `null` is a field's zero value.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaudeRecord {
    /// The session's metadata: type, sessionId, agentId, cwd, version, ...
    /// (embedded in the Go's struct, decoded from the same object).
    #[serde(skip)]
    pub(crate) meta: ClaudeMeta,
    #[serde(default)]
    pub(crate) subtype: String,
    #[serde(default)]
    pub(crate) uuid: String,
    /// The record's `parentUuid`: a uuid, or `null` for the first record of a
    /// conversation (`claude_rewind.rs`).
    #[serde(
        rename = "parentUuid",
        default,
        deserialize_with = "deserialize_present_raw"
    )]
    pub(crate) parent: Option<RawJson>,
    #[serde(default)]
    pub(crate) timestamp: String,

    #[serde(rename = "promptId", default)]
    pub(crate) prompt_id: String,
    #[serde(rename = "turnPosition", default)]
    pub(crate) turn_position: Option<TurnPosition>,
    #[serde(default)]
    pub(crate) origin: Option<ClaudeOrigin>,
    #[serde(rename = "promptSource", default)]
    pub(crate) prompt_source: String,

    #[serde(rename = "isMeta", default)]
    pub(crate) is_meta: bool,
    #[serde(rename = "isCompactSummary", default)]
    pub(crate) is_compact_summary: bool,
    #[serde(rename = "isApiErrorMessage", default)]
    pub(crate) is_api_error_message: bool,

    /// A system record's text.
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    pub(crate) content: Option<RawJson>,
    #[serde(
        rename = "toolUseResult",
        default,
        deserialize_with = "deserialize_present_raw"
    )]
    pub(crate) tool_use_result: Option<RawJson>,
    #[serde(default)]
    pub(crate) attachment: Option<ClaudeAttachment>,
    #[serde(default)]
    pub(crate) message: Option<ClaudeMessage>,

    // api-request and api-request-blob lines.
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    pub(crate) params: Option<RawJson>,
    #[serde(default)]
    pub(crate) base: String,
    #[serde(default)]
    pub(crate) tail: Vec<String>,
    #[serde(default)]
    pub(crate) hash: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TurnPosition {
    #[serde(rename = "promptIndex", default)]
    pub(crate) prompt_index: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaudeOrigin {
    #[serde(default)]
    pub(crate) kind: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaudeAttachment {
    #[serde(rename = "type", default)]
    pub(crate) r#type: String,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    pub(crate) prompt: Option<RawJson>,
    #[serde(rename = "commandMode", default)]
    pub(crate) command_mode: String,
    #[serde(default)]
    pub(crate) origin: Option<ClaudeOrigin>,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    pub(crate) content: Option<RawJson>,
    #[serde(rename = "hookEvent", default)]
    pub(crate) hook_event: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaudeMessage {
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    pub(crate) content: Option<RawJson>,
    #[serde(default)]
    pub(crate) stop_reason: String,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeBlock {
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    input: Option<RawJson>,
    #[serde(default)]
    tool_use_id: String,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    content: Option<RawJson>,
    #[serde(default)]
    is_error: bool,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    from: Option<RawJson>,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    to: Option<RawJson>,
    #[serde(default)]
    source: Option<ClaudeImageSource>,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeImageSource {
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    data: String,
}

#[derive(Default)]
struct ClaudeContent {
    text: String,
    images: Vec<Image>,
    block: usize,
    found: bool,
}

/// Reads a message's content: a string or blocks (Go: readClaudeContent). A
/// `null` content is a string, empty, as Go's `json.Unmarshal` into a string
/// reads it.
fn read_claude_content(raw: Option<&RawJson>) -> (ClaudeContent, Vec<ClaudeBlock>) {
    let mut c = ClaudeContent::default();
    let Some(raw) = raw else {
        return (c, Vec::new());
    };
    let Ok(doc) = json::parse(raw.as_bytes()) else {
        return (c, Vec::new());
    };
    if let Ok(s) = doc.decode::<String>() {
        c.text = s;
        c.found = true;
        return (c, Vec::new());
    }
    let Ok(blocks) = doc.decode::<Vec<ClaudeBlock>>() else {
        return (c, Vec::new());
    };
    let mut parts = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        match b.r#type.as_str() {
            "text" => parts.push(b.text.as_str()),
            "image" if b.source.is_some() => {
                let src = b.source.as_ref().unwrap();
                c.images.push(Image {
                    media_type: src.media_type.clone(),
                    data: src.data.clone(),
                });
            }
            _ => continue,
        }
        if !c.found {
            c.block = i;
            c.found = true;
        }
    }
    c.text = parts.join("\n");
    (c, blocks)
}

impl ClaudeRecord {
    /// Decodes a line as Go's `json.Unmarshal` into the record (its embedded
    /// metadata included) does, or `None` where that fails.
    pub(crate) fn decode(line: &[u8]) -> Option<ClaudeRecord> {
        let doc = json::parse(line).ok()?;
        let mut r: ClaudeRecord = doc.decode().ok()?;
        r.meta = doc.decode().ok()?;
        Some(r)
    }

    pub(crate) fn block_id(&self, line: usize, i: usize) -> String {
        if self.uuid.is_empty() {
            format!("L{line}#{i}")
        } else {
            format!("{}#{i}", self.uuid)
        }
    }
}

pub(crate) struct ClaudeLines {
    pub(crate) opts: Options,
    pub(crate) sess: Session,
    pub(crate) turn: Option<TurnRef>,
    pub(crate) index: usize,
    pub(crate) numbered: bool,
    pub(crate) prompts: usize,
    pub(crate) tools: HashMap<String, Event>,
    pub(crate) pending: Vec<String>,
    pub(crate) blobs: Option<HashMap<String, RawJson>>,
    pub(crate) closing: Option<Event>,
    pub(crate) nodes: HashMap<String, ClaudeNode>,
    pub(crate) asked: HashMap<String, String>,
    pub(crate) latest: HashMap<String, Event>,
    pub(crate) abandoned: HashMap<String, bool>,
}

impl LineReader for ClaudeLines {
    fn session(&self) -> Session {
        self.sess.clone()
    }

    fn read(&mut self, line: &[u8], at: i64, n: usize) -> (Vec<Event>, bool) {
        let Some(r) = ClaudeRecord::decode(line) else {
            return (Vec::new(), false);
        };
        // Before subagents had their own files (agentId), their records were
        // sidechains of the main transcript: another session's.
        if r.meta.is_sidechain && r.meta.agent_id.is_empty() {
            return (Vec::new(), true);
        }
        let model = if r.meta.r#type == "assistant" {
            r.message.as_ref().map(|m| m.model.as_str()).unwrap_or("")
        } else {
            ""
        };
        harvest_claude_meta(&mut self.sess, &r.meta, model, at);
        if !r.prompt_id.is_empty() {
            self.enter(&r);
        }

        let prev_closing_id = self.closing.as_ref().map(|e| e.id.clone());
        let mut l = ClaudeLine {
            c: self,
            r: &r,
            raw: line,
            at,
            n,
            adds: Vec::new(),
            updates: Vec::new(),
        };
        match r.meta.r#type.as_str() {
            "user" => {
                if r.message.is_some() {
                    l.user();
                }
            }
            "assistant" => {
                if r.message.is_some() {
                    l.assistant();
                }
            }
            "attachment" => {
                if r.attachment.is_some() {
                    l.attachment();
                }
            }
            "system" => l.system(),
            "api-request-blob" => {
                #[derive(Deserialize)]
                struct Blob {
                    #[serde(default, deserialize_with = "deserialize_present_raw")]
                    message: Option<RawJson>,
                }
                if let Some(ref mut blobs) = l.c.blobs
                    && !r.hash.is_empty()
                    && let Ok(blob) = json::from_slice::<Blob>(line)
                    && let Some(msg) = blob.message
                    && !msg.is_empty()
                {
                    blobs.insert(r.hash.clone(), msg);
                }
            }
            "api-request" if l.c.opts.requests => {
                l.request();
            }
            _ => {}
        }
        // A new turn closes the last one.
        if let Some(ref prev_id) = prev_closing_id
            && l.c.closing.as_ref().is_some_and(|c| &c.id == prev_id)
            && let Some(ref cur_turn) = l.c.turn
            && l.c
                .closing
                .as_ref()
                .unwrap()
                .turn
                .as_ref()
                .is_none_or(|t| t.id != cur_turn.id)
        {
            l.close();
        }
        (l.tree(), true)
    }
}

pub(crate) struct ClaudeLine<'a> {
    pub(crate) c: &'a mut ClaudeLines,
    pub(crate) r: &'a ClaudeRecord,
    pub(crate) raw: &'a [u8],
    pub(crate) at: i64,
    pub(crate) n: usize,
    pub(crate) adds: Vec<Event>,
    pub(crate) updates: Vec<Event>,
}

impl ClaudeLines {
    fn enter(&mut self, r: &ClaudeRecord) {
        let mut index = self.index;
        if let Some(pi) = r.turn_position.as_ref().and_then(|tp| tp.prompt_index) {
            index = pi;
            self.index = index;
            self.numbered = true;
        } else if let Some(ref turns_fn) = self.opts.turns {
            if let Some(i) = turns_fn(&r.prompt_id) {
                index = i;
                self.index = i;
            }
        } else if !self.numbered {
            index = self.prompts;
        }
        self.set_turn(&r.prompt_id, index, false);
    }

    pub(crate) fn set_turn(&mut self, id: &str, index: usize, derived: bool) {
        let same = self
            .turn
            .as_ref()
            .is_some_and(|t| t.id == id && t.index == index && t.derived == derived);
        if !same {
            self.turn = Some(TurnRef {
                id: id.to_string(),
                index,
                derived,
            });
        }
    }

    pub(crate) fn counted(&self, r: &ClaudeRecord) -> bool {
        r.prompt_id.is_empty()
            || (!self.numbered
                && self.opts.turns.is_none()
                && r.turn_position
                    .as_ref()
                    .and_then(|tp| tp.prompt_index)
                    .is_none())
    }

    pub(crate) fn settled(&mut self, id: &str, status: &str) {
        if status == STATUS_PENDING {
            return;
        }
        if status != STATUS_BACKGROUND {
            self.tools.remove(id);
        }
        if let Some(i) = self.pending.iter().position(|p| p == id) {
            self.pending.remove(i);
        }
    }
}

const CLAUDE_INTERRUPTS: &[&str] = &[
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
];

const CLAUDE_ECHO_TAGS: &[&str] = &[
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<bash-stdout>",
    "<bash-stderr>",
];

// The regexes keep RE2's classes: its \s is [\t\n\f\r ] and its \d [0-9],
// where the regex crate's are Unicode's.
static CLAUDE_PLAIN_COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(/[a-z][a-z0-9_:-]*)(?:[\t\n\f\r ]|$)").unwrap());

static CLAUDE_DELIVERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(A message arrived|[0-9]+ messages arrived) while you were working").unwrap()
});

impl ClaudeLine<'_> {
    fn event(&self, id: String, kind: &str) -> Event {
        Event {
            id,
            op: OP_ADD.to_string(),
            at: self.at,
            line: self.n,
            ts: self.r.timestamp.clone(),
            turn: self.c.turn.clone(),
            agent: self.r.meta.agent_id.clone(),
            kind: kind.to_string(),
            ..Event::default()
        }
    }

    fn add(&mut self, e: Event) {
        self.adds.push(e);
    }

    fn close(&mut self) {
        let Some(mut e) = self.c.closing.take() else {
            return;
        };
        let text = e.say.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        e.op = OP_UPDATE.to_string();
        e.at = self.at;
        e.line = self.n;
        e.ts = self.r.timestamp.clone();
        e.say = Some(SayEvent {
            text,
            r#final: true,
            provisional: false,
        });
        self.updates.push(e);
    }

    fn user(&mut self) {
        let msg = self.r.message.as_ref().unwrap();
        let (content, blocks) = read_claude_content(msg.content.as_ref());
        let mut interrupted = false;
        if content.found {
            interrupted = self.user_content(content);
        }
        let results: Vec<&ClaudeBlock> = blocks
            .iter()
            .filter(|b| b.r#type == "tool_result" && !b.tool_use_id.is_empty())
            .collect();
        let own = results.len() == 1;
        for b in results {
            self.result(b, own);
        }
        if interrupted {
            self.interrupt();
        }
    }

    fn user_content(&mut self, content: ClaudeContent) -> bool {
        let r = self.r;
        let id = r.block_id(self.n, content.block);
        let text = content.text.trim().to_string();
        if text.is_empty() && content.images.is_empty() {
            return false;
        }
        if r.is_compact_summary {
            let mut e = self.event(id, KIND_SYSTEM);
            e.system = Some(SystemEvent {
                subtype: "compact_summary".to_string(),
                text: content.text,
                data: None,
            });
            self.add(e);
            return false;
        }
        if r.is_meta && r.origin.is_none() && r.prompt_source != "system" {
            return false;
        }
        let marked = r.prompt_source.is_empty();
        if marked {
            if CLAUDE_INTERRUPTS.contains(&text.as_str()) {
                let mut e = self.event(id, KIND_SYSTEM);
                e.system = Some(SystemEvent {
                    subtype: "interrupt".to_string(),
                    text: content.text,
                    data: None,
                });
                self.add(e);
                self.c.closing = None; // the turn didn't end with that reply
                return true;
            }
            if CLAUDE_ECHO_TAGS.iter().any(|tag| text.starts_with(tag)) {
                let mut e = self.event(id, KIND_SYSTEM);
                e.system = Some(SystemEvent {
                    subtype: "local_command".to_string(),
                    text: content.text,
                    data: None,
                });
                self.add(e);
                return false;
            }
        }

        let mut p = PromptEvent {
            text: content.text,
            origin: claude_origins(r).to_string(),
            command: String::new(),
            images: content.images,
        };
        if marked {
            if let Some((name, args)) = slash_command(&text) {
                p.origin = ORIGIN_COMMAND.to_string();
                p.command = name.clone();
                p.text = if args.is_empty() {
                    name
                } else {
                    format!("{name} {args}")
                };
            } else if let Some(cmd) = shell_escape(&text) {
                p.origin = ORIGIN_COMMAND.to_string();
                p.command = "!".to_string();
                p.text = format!("! {cmd}");
                self.escaped();
            } else if r.origin.is_none()
                && r.meta.agent_id.is_empty()
                && let Some(caps) = CLAUDE_PLAIN_COMMAND.captures(&text)
            {
                p.origin = ORIGIN_COMMAND.to_string();
                p.command = caps[1].to_string();
            }
        }
        self.number(&id, &p.origin);
        let is_task_notification = p.origin == ORIGIN_TASK_NOTIFICATION;
        let mut e = self.event(id, KIND_PROMPT);
        e.prompt = Some(p);
        self.add(e);
        if is_task_notification {
            self.notification(&text);
        }
        false
    }

    fn escaped(&mut self) {
        let r = self.r;
        if !self.c.numbered
            || self.c.opts.turns.is_some()
            || r.prompt_id.is_empty()
            || r.turn_position
                .as_ref()
                .and_then(|tp| tp.prompt_index)
                .is_some()
        {
            return;
        }
        self.c.index += 1;
        let index = self.c.index;
        self.c.set_turn(&r.prompt_id, index, false);
    }

    fn number(&mut self, id: &str, origin: &str) {
        let r = self.r;
        if !self.c.counted(r) {
            return;
        }
        let human = matches!(origin, ORIGIN_HUMAN | ORIGIN_COMMAND | ORIGIN_QUEUED);
        if human {
            self.c.prompts += 1;
        } else if !r.prompt_id.is_empty() || origin != ORIGIN_TASK_NOTIFICATION {
            return;
        } else if self.c.prompts == 0 {
            self.c.prompts = 1;
        }
        let prompts = self.c.prompts;
        if !r.prompt_id.is_empty() {
            self.c.set_turn(&r.prompt_id, prompts, false);
        } else {
            self.c.set_turn(id, prompts, true);
        }
    }

    fn result(&mut self, b: &ClaudeBlock, own: bool) {
        let Some(call) = self.c.tools.get(&b.tool_use_id).cloned() else {
            return;
        };
        let mut res = ToolResult {
            status: STATUS_OK.to_string(),
            output: result_text(b.content.as_ref()),
            structured: None,
        };
        #[derive(Default, Deserialize)]
        struct Structured {
            #[serde(default)]
            status: String,
            #[serde(rename = "backgroundTaskId", default)]
            background_task_id: String,
            #[serde(rename = "agentId", default)]
            agent_id: String,
        }
        let mut structured = Structured::default();
        if own && let Some(ref raw_tur) = self.r.tool_use_result {
            let trimmed = trim_bytes_space(raw_tur.as_bytes());
            if trimmed.first() == Some(&b'{') {
                res.structured = Some(raw_tur.clone());
                // The Go ignores the error: a field of another type (a null
                // agentId, a numeric status) leaves that field empty and the
                // others read.
                structured = json::from_slice_partial::<Structured>(trimmed).0;
            }
        }
        if b.is_error {
            res.status = STATUS_ERROR.to_string();
        } else if structured.status == "async_launched" || !structured.background_task_id.is_empty()
        {
            res.status = STATUS_BACKGROUND.to_string();
        }
        // While a call runs, the table has no response to read.
        let response = if res.status == STATUS_BACKGROUND {
            None
        } else {
            res.structured.as_ref().map(|r| r.as_bytes().to_vec())
        };
        let subs = if !structured.agent_id.is_empty() {
            vec![format!("agent-{}", structured.agent_id)]
        } else {
            Vec::new()
        };
        self.update(&call, res, response.as_deref(), subs);
    }

    fn update(
        &mut self,
        call: &Event,
        res: ToolResult,
        response: Option<&[u8]>,
        subs: Vec<String>,
    ) {
        let mut t = call.tool.clone().unwrap_or_default();
        let status = res.status.clone();
        let input_bytes = t.input.as_ref().map_or(&[][..], RawJson::as_bytes);
        t.tool_summary = summarize_tool(&t.name, input_bytes, response, &self.c.sess.cwd);
        t.result = Some(res);
        if !subs.is_empty() {
            t.sub_sessions = subs;
        }
        let mut e = call.clone();
        e.op = OP_UPDATE.to_string();
        e.at = self.at;
        e.line = self.n;
        e.ts = self.r.timestamp.clone();
        e.tool = Some(t);
        self.updates.push(e.clone());
        self.c.tools.insert(call.id.clone(), e);
        self.c.settled(&call.id, &status);
    }

    fn interrupt(&mut self) {
        let pending = self.c.pending.clone();
        for id in pending {
            if let Some(call) = self.c.tools.get(&id).cloned()
                && call
                    .tool
                    .as_ref()
                    .and_then(|t| t.result.as_ref())
                    .is_some_and(|r| r.status == STATUS_PENDING)
            {
                self.update(
                    &call,
                    ToolResult {
                        status: STATUS_INTERRUPTED.to_string(),
                        output: String::new(),
                        structured: None,
                    },
                    None,
                    Vec::new(),
                );
            }
        }
    }

    fn notification(&mut self, text: &str) {
        for n in task_notifications(text) {
            let Some(call) = self.c.tools.get(&n.tool_use_id).cloned() else {
                continue;
            };
            let is_bg = call
                .tool
                .as_ref()
                .and_then(|t| t.result.as_ref())
                .is_some_and(|r| r.status == STATUS_BACKGROUND);
            if !is_bg {
                continue;
            }
            let Some(status) = claude_task_status(&n.status) else {
                continue;
            };
            let output = if !n.result.is_empty() {
                n.result
            } else {
                n.summary
            };
            let res = ToolResult {
                status: status.to_string(),
                output,
                structured: None,
            };
            let response_vec = if !n.usage.is_empty() {
                let mut m = BTreeMap::new();
                for (tag, key) in [
                    ("tool_uses", "totalToolUseCount"),
                    ("subagent_tokens", "totalTokens"),
                    ("duration_ms", "totalDurationMs"),
                ] {
                    if let Some(s) = tag_text(&n.usage, tag)
                        && let Ok(v) = json::from_str::<i64>(s)
                    {
                        m.insert(key, v);
                    }
                }
                json::marshal(&m).ok()
            } else {
                None
            };
            self.update(&call, res, response_vec.as_deref(), Vec::new());
        }
    }

    fn assistant(&mut self) {
        let r = self.r;
        let msg = r.message.as_ref().unwrap();
        let (content, blocks) = read_claude_content(msg.content.as_ref());
        // More from the model: the last reply didn't close its turn.
        self.c.closing = None;
        if r.is_api_error_message {
            let mut e = self.event(r.block_id(self.n, 0), KIND_SYSTEM);
            let data = object_fields(self.raw, |key| key == "error" || key == "apiErrorStatus");
            e.system = Some(SystemEvent {
                subtype: "api_error".to_string(),
                text: content.text,
                data,
            });
            self.add(e);
            return;
        }
        let mut last_text: Option<usize> = None;
        let mut calls = false;
        for (i, b) in blocks.iter().enumerate() {
            if b.r#type == "text" && !b.text.trim().is_empty() {
                last_text = Some(i);
            } else if b.r#type == "tool_use" {
                calls = true;
            }
        }
        for (i, b) in blocks.iter().enumerate() {
            match b.r#type.as_str() {
                "thinking" | "redacted_thinking" => {
                    let mut e = self.event(r.block_id(self.n, i), KIND_THINK);
                    let redacted = b.thinking.is_empty();
                    e.think = Some(ThinkEvent {
                        text: b.thinking.clone(),
                        redacted,
                    });
                    self.add(e);
                }
                "text" => {
                    if b.text.trim().is_empty() {
                        continue;
                    }
                    let mut e = self.event(r.block_id(self.n, i), KIND_SAY);
                    let stop = msg.stop_reason.as_str();
                    let is_final =
                        last_text == Some(i) && (stop == "end_turn" || stop == "stop_sequence");
                    e.say = Some(SayEvent {
                        text: b.text.clone(),
                        r#final: is_final,
                        provisional: false,
                    });
                    if last_text == Some(i)
                        && stop.is_empty()
                        && !calls
                        && !r.meta.agent_id.is_empty()
                    {
                        self.c.closing = Some(e.clone());
                    }
                    self.add(e);
                }
                "fallback" => {
                    let mut e = self.event(r.block_id(self.n, i), KIND_SYSTEM);
                    e.system = Some(SystemEvent {
                        subtype: "model_fallback".to_string(),
                        text: String::new(),
                        data: model_fallback(b),
                    });
                    self.add(e);
                }
                "tool_use" => {
                    let mut e = self.event(b.id.clone(), KIND_TOOL);
                    let input_bytes = b.input.as_ref().map_or(&[][..], RawJson::as_bytes);
                    let summary = summarize_tool(&b.name, input_bytes, None, &self.c.sess.cwd);
                    e.tool = Some(ToolEvent {
                        name: b.name.clone(),
                        input: b.input.clone(),
                        result: Some(ToolResult {
                            status: STATUS_PENDING.to_string(),
                            output: String::new(),
                            structured: None,
                        }),
                        tool_summary: summary,
                        sub_sessions: Vec::new(),
                    });
                    self.c.tools.insert(b.id.clone(), e.clone());
                    self.c.pending.push(b.id.clone());
                    self.add(e);
                }
                _ => {}
            }
        }
    }

    fn attachment(&mut self) {
        let a = self.r.attachment.as_ref().unwrap();
        let id = self.r.block_id(self.n, 0);
        let mut p = PromptEvent::default();
        match a.r#type.as_str() {
            "queued_command" => {
                let (content, _) = read_claude_content(a.prompt.as_ref());
                if !content.found {
                    return;
                }
                p.text = content.text;
                p.images = content.images;
                p.origin = ORIGIN_QUEUED.to_string();
                let kind = a.origin.as_ref().map(|o| o.kind.as_str()).unwrap_or("");
                if a.command_mode == "task-notification" {
                    p.origin = ORIGIN_TASK_NOTIFICATION.to_string();
                } else if matches!(kind, "peer" | "channel" | "coordinator") {
                    p.origin = ORIGIN_PEER.to_string();
                }
            }
            "hook_additional_context" => {
                let Some(ref raw) = a.content else {
                    return;
                };
                let Ok(parts) = json::from_str::<Vec<String>>(raw.as_str()) else {
                    return;
                };
                if parts.is_empty() {
                    return;
                }
                p.text = parts.join("\n");
                p.origin = if a.hook_event == "Stop" && CLAUDE_DELIVERED.is_match(&p.text) {
                    ORIGIN_DELIVERED.to_string()
                } else {
                    ORIGIN_SYSTEM.to_string()
                };
            }
            _ => return,
        }
        let notify_text = if p.origin == ORIGIN_TASK_NOTIFICATION {
            Some(p.text.trim().to_string())
        } else {
            None
        };
        let mut e = self.event(id, KIND_PROMPT);
        e.prompt = Some(p);
        self.add(e);
        if let Some(t) = notify_text {
            self.notification(&t);
        }
    }

    fn system(&mut self) {
        let mut e = self.event(self.r.block_id(self.n, 0), KIND_SYSTEM);
        let text = self
            .r
            .content
            .as_ref()
            .and_then(|c| json::from_str::<String>(c.as_str()).ok())
            .unwrap_or_default();
        let data = object_fields(self.raw, |key| !is_claude_envelope(key));
        e.system = Some(SystemEvent {
            subtype: self.r.subtype.clone(),
            text,
            data,
        });
        self.add(e);
        if self.c.closing.is_some()
            && matches!(
                self.r.subtype.as_str(),
                "turn_duration" | "stop_hook_summary"
            )
        {
            self.close(); // the turn ended
        }
    }

    fn request(&mut self) {
        let r = self.r;
        let mut e = self.event(r.id.clone(), KIND_REQUEST);
        #[derive(Default, Deserialize)]
        struct Params {
            #[serde(default)]
            model: String,
        }
        // The Go ignores the error, as from_slice_partial does.
        let model = r
            .params
            .as_ref()
            .map(|p| json::from_slice_partial::<Params>(p.as_bytes()).0.model)
            .unwrap_or_default();
        let mut blobs = BTreeMap::new();
        if let Some(ref known) = self.c.blobs {
            for hash in &r.tail {
                if let Some(blob) = known.get(hash) {
                    blobs.insert(hash.clone(), blob.clone());
                }
            }
        }
        e.request = Some(RequestEvent {
            model,
            params: r.params.clone(),
            base: r.base.clone(),
            tail: r.tail.clone(),
            blobs,
        });
        self.add(e);
    }
}

fn claude_origins(r: &ClaudeRecord) -> &'static str {
    if r.prompt_source == "queued" {
        return ORIGIN_QUEUED;
    }
    if r.prompt_source == "sdk" {
        return ORIGIN_HUMAN;
    }
    if r.prompt_source == "system" && r.origin.is_none() {
        return ORIGIN_SYSTEM;
    }
    let kind = r.origin.as_ref().map(|o| o.kind.as_str()).unwrap_or("");
    match kind {
        "human" => ORIGIN_HUMAN,
        "task-notification" => ORIGIN_TASK_NOTIFICATION,
        "peer" | "channel" | "coordinator" => ORIGIN_PEER,
        "" => {
            if !r.meta.agent_id.is_empty() {
                ORIGIN_PEER
            } else {
                ORIGIN_HUMAN
            }
        }
        _ => ORIGIN_SYSTEM,
    }
}

fn slash_command(text: &str) -> Option<(String, String)> {
    if !text.starts_with("<command-name>") && !text.starts_with("<command-message>") {
        return None;
    }
    let mut name = tag_text(text, "command-name")?.to_string();
    if name.is_empty() {
        return None;
    }
    if !name.starts_with('/') {
        name.insert(0, '/');
    }
    let args = tag_text(text, "command-args").unwrap_or("").to_string();
    Some((name, args))
}

fn shell_escape(text: &str) -> Option<&str> {
    if !text.starts_with("<bash-input>") {
        return None;
    }
    tag_text(text, "bash-input")
}

/// Returns the trimmed text between `<name>` and `</name>` in `s`.
pub(crate) fn tag_text<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let (_, rest) = s.split_once(&open)?;
    let (inner, _) = rest.split_once(&close)?;
    Some(inner.trim())
}

fn claude_task_status(status: &str) -> Option<&'static str> {
    match status {
        "completed" => Some(STATUS_OK),
        "failed" => Some(STATUS_ERROR),
        "killed" | "stopped" => Some(STATUS_INTERRUPTED),
        _ => None,
    }
}

/// One `<task-notification>` block of a Claude Code message: a background task
/// (a command, a subagent) reporting back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskNotification {
    pub task_id: String,
    pub tool_use_id: String,
    pub status: String,
    pub summary: String,
    pub result: String,
    /// The `<usage>` block's inner markup, if any.
    pub usage: String,
}

/// Returns every `<task-notification>` block in `text`, in order. Claude Code
/// puts several in one message when several tasks finish while the agent works.
pub fn task_notifications(mut text: &str) -> Vec<TaskNotification> {
    let mut out = Vec::new();
    while let Some((_, rest)) = text.split_once("<task-notification>") {
        let (block, after) = match rest.split_once("</task-notification>") {
            Some((b, a)) => (b, a),
            None => (rest, ""),
        };
        out.push(TaskNotification {
            task_id: tag_text(block, "task-id").unwrap_or("").to_string(),
            tool_use_id: tag_text(block, "tool-use-id").unwrap_or("").to_string(),
            status: tag_text(block, "status").unwrap_or("").to_string(),
            summary: tag_text(block, "summary").unwrap_or("").to_string(),
            result: tag_text(block, "result").unwrap_or("").to_string(),
            usage: tag_text(block, "usage").unwrap_or("").to_string(),
        });
        text = after;
    }
    out
}

fn model_fallback(b: &ClaudeBlock) -> Option<RawJson> {
    let mut out = String::from("{");
    for (key, val) in [("from", b.from.as_ref()), ("to", b.to.as_ref())] {
        let Some(v) = val else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        if out.len() > 1 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(key).unwrap());
        out.push(':');
        out.push_str(v.as_str());
    }
    out.push('}');
    RawJson::from_json_string(out).ok()
}

fn is_claude_envelope(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "subtype"
            | "content"
            | "parentUuid"
            | "logicalParentUuid"
            | "isSidechain"
            | "uuid"
            | "timestamp"
            | "userType"
            | "entrypoint"
            | "cwd"
            | "sessionId"
            | "session_id"
            | "version"
            | "gitBranch"
            | "slug"
            | "isMeta"
            | "level"
            | "sessionKind"
            | "agentId"
            | "promptId"
    )
}

/// Returns the fields of the JSON object `line` that `keep` accepts, in its own
/// order (a name that comes twice, twice, as the Go's `json.Decoder` reads
/// tokens), or `None` if there are none. Names are written as `json.Marshal`
/// writes them, HTML-escaped.
fn object_fields<F: Fn(&str) -> bool>(line: &[u8], keep: F) -> Option<RawJson> {
    let members = json::parse(line).ok()?.members()?;
    let mut out = String::from("{");
    for (key, val) in members {
        if !keep(&key) {
            continue;
        }
        if out.len() > 1 {
            out.push(',');
        }
        out.push_str(&json::to_string(&key, true).ok()?);
        out.push(':');
        out.push_str(&val);
    }
    if out.len() == 1 {
        return None;
    }
    out.push('}');
    RawJson::from_json_string(out).ok()
}

/// Returns, for the Claude subagent transcript at `path`
/// ([`crate::claude_subagent_session`]), the lookup of its parent's turn
/// indexes by prompt id that [`Options::turns`] takes. Returns `None` for any
/// other path.
pub fn claude_subagent_turns(path: &str) -> Option<TurnsFn> {
    let session = claude_subagent_session(path);
    if session.is_empty() {
        return None;
    }
    let t = Mutex::new(ParentTurns {
        path: format!("{session}.jsonl"),
        p: Claude.new_parser(Options::default()),
        pending: Vec::new(),
        turns: HashMap::new(),
    });
    Some(Arc::new(move |prompt_id: &str| {
        let mut guard = t.lock().unwrap();
        guard.lookup(prompt_id)
    }))
}

struct ParentTurns {
    path: String,
    p: Parser,
    pending: Vec<u8>,
    turns: HashMap<String, usize>,
}

impl ParentTurns {
    fn lookup(&mut self, prompt_id: &str) -> Option<usize> {
        if let Some(&i) = self.turns.get(prompt_id) {
            return Some(i);
        }
        self.read();
        self.turns.get(prompt_id).copied()
    }

    fn read(&mut self) {
        let Ok(f) = std::fs::File::open(&self.path) else {
            return;
        };
        let mut off = self.p.pos().at + self.pending.len() as i64;
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let n = match read_at_some(&f, &mut buf, off as u64) {
                Ok(n) => n,
                Err(_) => return,
            };
            off += n as i64;
            self.pending.extend_from_slice(&buf[..n]);
            let start = self.p.pos().at;
            for e in self.p.feed(&self.pending) {
                if let Some(t) = e.turn
                    && !t.derived
                {
                    self.turns.insert(t.id, t.index);
                }
            }
            let consumed = (self.p.pos().at - start) as usize;
            self.pending.drain(..consumed);
            if n == 0 {
                return;
            }
        }
    }
}

fn result_text(content: Option<&RawJson>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    let Ok(doc) = json::parse(content.as_bytes()) else {
        return String::new();
    };
    if let Ok(s) = doc.decode::<String>() {
        return s;
    }
    let Ok(blocks) = doc.decode::<Vec<ClaudeBlock>>() else {
        return String::new();
    };
    blocks
        .into_iter()
        .filter(|b| b.r#type == "text")
        .map(|b| b.text)
        .collect::<Vec<_>>()
        .join("\n")
}

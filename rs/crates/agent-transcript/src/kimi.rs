use std::collections::HashMap;
use std::sync::LazyLock;

use agent_base::gostd::{filepath, json};
use chrono::DateTime;
use regex::Regex;
use serde_json::{Map, Value};

use crate::agy::numbered_text;
use crate::events::{
    Event, KIND_PROMPT, KIND_SAY, KIND_THINK, KIND_TOOL, LineReader, OP_ADD, OP_UPDATE,
    ORIGIN_HUMAN, ORIGIN_PEER, ORIGIN_QUEUED, Options, ParentSession, Parser, PromptEvent, RawJson,
    Reader, STATUS_ERROR, STATUS_OK, STATUS_PENDING, SayEvent, Session, ThinkEvent, ToolEvent,
    ToolResult, TurnRef, new_parser,
};
use crate::tools::{count_lines, marshal_sorted_value, parse_object, summarize_tool};
use crate::transcript::session_id_from_path;

/// Reads Kimi Code's session logs
/// (`~/.kimi-code/sessions/<workspace>/session_<uuid>/agents/main/wire.jsonl`,
/// and a subagent's `agents/<agent>/wire.jsonl`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Kimi;

impl Reader for Kimi {
    fn harness(&self) -> &'static str {
        "kimi"
    }

    fn new_parser(&self, opts: Options) -> Parser {
        let agent = if opts.session.parent_session.is_some() {
            opts.session.session_id.clone()
        } else {
            String::new()
        };
        let k = KimiLines {
            sess: opts.session,
            agent,
            turn: opts.turn,
            turns: 0,
            tools: HashMap::new(),
            calls: HashMap::new(),
            early: HashMap::new(),
        };
        new_parser("kimi", Box::new(k))
    }
}

/// A JSON object's fields, read one at a time.
#[derive(Debug, Clone, Default)]
pub(crate) struct KimiFields(HashMap<String, RawJson>);

pub(crate) fn kimi_object(raw: &[u8]) -> KimiFields {
    if raw.is_empty() {
        return KimiFields::default();
    }
    json::from_slice::<HashMap<String, RawJson>>(raw)
        .map(KimiFields)
        .unwrap_or_default()
}

impl KimiFields {
    pub(crate) fn get(&self, key: &str) -> Option<&RawJson> {
        self.0.get(key)
    }

    /// Returns the field `key`'s string, or `""` if it isn't one.
    pub(crate) fn str(&self, key: &str) -> String {
        self.0
            .get(key)
            .and_then(|raw| json::from_str::<String>(raw.as_str()).ok())
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct KimiEvent {
    typ: String,
    part: KimiFields,
    call_id: String,
    name: String,
    cwd: String,
    args: Option<RawJson>,
    result: Option<RawJson>,
}

fn read_kimi_event(raw: Option<&RawJson>) -> KimiEvent {
    let f = raw.map_or_else(KimiFields::default, |r| kimi_object(r.as_bytes()));
    let part = f
        .get("part")
        .map_or_else(KimiFields::default, |r| kimi_object(r.as_bytes()));
    let cwd = f
        .get("display")
        .map_or_else(KimiFields::default, |r| kimi_object(r.as_bytes()))
        .str("cwd");
    let mut call_id = f.str("toolCallId");
    if call_id.is_empty() {
        call_id = f.str("uuid");
    }
    KimiEvent {
        typ: f.str("type"),
        part,
        call_id,
        name: f.str("name"),
        cwd,
        args: f.get("args").cloned(),
        result: f.get("result").cloned(),
    }
}

struct KimiLines {
    sess: Session,
    agent: String,
    turn: Option<TurnRef>,
    turns: usize,
    tools: HashMap<String, Event>,
    calls: HashMap<String, String>,
    early: HashMap<String, KimiEvent>,
}

impl LineReader for KimiLines {
    fn session(&self) -> Session {
        self.sess.clone()
    }

    fn read(&mut self, line: &[u8], at: i64, n: usize) -> (Vec<Event>, bool) {
        let Ok(map) = json::from_slice::<HashMap<String, RawJson>>(line) else {
            return (Vec::new(), false);
        };
        let r = KimiFields(map);
        let ts = r
            .get("time")
            .and_then(|raw| json::from_str::<f64>(raw.as_str()).ok())
            .and_then(|ms| DateTime::from_timestamp_millis(ms as i64))
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
            .unwrap_or_default();

        let typ = r.str("type");
        let mut events = Vec::new();
        match typ.as_str() {
            "llm.request" => {
                self.sess.set_model(&r.str("model"), at);
            }
            "turn.prompt" | "turn.steer" => {
                let text = kimi_prompt(&r);
                if !text.is_empty() {
                    let mut e = self.make_event("", KIND_PROMPT, at, n, &ts);
                    let mut origin = if !self.agent.is_empty() {
                        ORIGIN_PEER
                    } else {
                        ORIGIN_HUMAN
                    };
                    if typ == "turn.steer" {
                        origin = ORIGIN_QUEUED;
                    } else {
                        self.turns += 1;
                        let turn = TurnRef {
                            id: e.id.clone(),
                            index: self.turns,
                            derived: true,
                        };
                        self.turn = Some(turn.clone());
                        e.turn = Some(turn);
                    }
                    e.prompt = Some(PromptEvent {
                        text,
                        origin: origin.to_string(),
                        command: String::new(),
                        images: Vec::new(),
                    });
                    events.push(e);
                }
            }
            "context.append_loop_event" => {
                events = self.loop_event(read_kimi_event(r.get("event")), at, n, &ts);
            }
            _ => {}
        }
        (events, true)
    }
}

impl KimiLines {
    fn make_event(&self, id: &str, kind: &str, at: i64, n: usize, ts: &str) -> Event {
        let id = if id.is_empty() {
            format!("L{n}#0")
        } else {
            id.to_string()
        };
        Event {
            id,
            op: OP_ADD.to_string(),
            at,
            line: n,
            ts: ts.to_string(),
            turn: self.turn.clone(),
            agent: self.agent.clone(),
            kind: kind.to_string(),
            ..Event::default()
        }
    }

    fn loop_event(&mut self, ev: KimiEvent, at: i64, n: usize, ts: &str) -> Vec<Event> {
        match ev.typ.as_str() {
            "content.part" => {
                let part_type = ev.part.str("type");
                let think = ev.part.str("think");
                let text = ev.part.str("text");
                if part_type == "think" && !think.trim().is_empty() {
                    let mut e = self.make_event("", KIND_THINK, at, n, ts);
                    e.think = Some(ThinkEvent {
                        text: think,
                        redacted: false,
                    });
                    return vec![e];
                }
                if part_type == "text" && !text.trim().is_empty() {
                    let mut e = self.make_event("", KIND_SAY, at, n, ts);
                    e.say = Some(SayEvent {
                        text,
                        r#final: false,
                        provisional: false,
                    });
                    return vec![e];
                }
            }
            "tool.call" => {
                self.sess.set_cwd(&ev.cwd, at);
                let mut id = ev.call_id.clone();
                if !id.is_empty() && self.tools.contains_key(&id) {
                    id = format!("{id}@L{n}");
                }
                let mut e = self.make_event(&id, KIND_TOOL, at, n, ts);
                let input_bytes = ev.args.as_ref().map_or(&[][..], RawJson::as_bytes);
                let summary = summarize_tool(&ev.name, input_bytes, None, &self.sess.cwd);
                e.tool = Some(ToolEvent {
                    name: ev.name,
                    input: ev.args,
                    result: Some(ToolResult {
                        status: STATUS_PENDING.to_string(),
                        output: String::new(),
                        structured: None,
                    }),
                    tool_summary: summary,
                    sub_sessions: Vec::new(),
                });
                self.tools.insert(e.id.clone(), e.clone());
                if ev.call_id.is_empty() {
                    return vec![e];
                }
                self.calls.insert(ev.call_id.clone(), e.id.clone());
                let mut events = vec![e.clone()];
                if let Some(res) = self.early.remove(&ev.call_id) {
                    events.push(self.result(&e, res, at, n, ts));
                }
                return events;
            }
            "tool.result" => {
                let call = self
                    .calls
                    .get(&ev.call_id)
                    .and_then(|cid| self.tools.get(cid))
                    .cloned();
                let is_pending = call
                    .as_ref()
                    .and_then(|c| c.tool.as_ref())
                    .and_then(|t| t.result.as_ref())
                    .is_some_and(|r| r.status == STATUS_PENDING);
                if !is_pending {
                    if !ev.call_id.is_empty() {
                        self.early.insert(ev.call_id.clone(), ev);
                    }
                    return Vec::new();
                }
                let call = call.unwrap();
                return vec![self.result(&call, ev, at, n, ts)];
            }
            _ => {}
        }
        Vec::new()
    }

    fn result(&mut self, call: &Event, ev: KimiEvent, at: i64, n: usize, ts: &str) -> Event {
        let r = ev
            .result
            .as_ref()
            .map_or_else(KimiFields::default, |raw| kimi_object(raw.as_bytes()));
        let output = kimi_output(r.get("output"));
        let is_err = r
            .get("isError")
            .and_then(|raw| json::from_str::<bool>(raw.as_str()).ok())
            .unwrap_or(false);
        let status = if is_err { STATUS_ERROR } else { STATUS_OK };
        let res = ToolResult {
            status: status.to_string(),
            output: output.clone(),
            structured: None,
        };
        let mut t = call.tool.clone().unwrap_or_default();
        let input_bytes = t.input.as_ref().map_or(&[][..], RawJson::as_bytes);
        let result_bytes = ev.result.as_ref().map_or(&[][..], RawJson::as_bytes);
        let resp = kimi_response(&t.name, input_bytes, result_bytes, &output);
        t.tool_summary = summarize_tool(&t.name, input_bytes, Some(&resp), &self.sess.cwd);
        t.result = Some(res);
        let mut e = call.clone();
        e.op = OP_UPDATE.to_string();
        e.at = at;
        e.line = n;
        e.ts = ts.to_string();
        e.tool = Some(t);
        self.tools.insert(call.id.clone(), e.clone());
        e
    }
}

fn kimi_prompt(r: &KimiFields) -> String {
    let mut parts = Vec::new();
    if let Some(raw_input) = r.get("input") {
        if let Ok(input) = json::from_str::<Vec<RawJson>>(raw_input.as_str()) {
            for raw in input {
                let p = kimi_object(raw.as_bytes());
                let text = p.str("text");
                if p.str("type") == "text" && !text.is_empty() {
                    parts.push(text);
                }
            }
        } else {
            let s = r.str("input");
            if !s.is_empty() {
                parts.push(s);
            }
        }
    }
    if parts.is_empty() {
        return r.str("prompt").trim().to_string();
    }
    parts.join("\n").trim().to_string()
}

fn kimi_output(raw: Option<&RawJson>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    if raw.is_empty() || raw.as_str() == "null" {
        return String::new();
    }
    if let Ok(s) = json::from_str::<String>(raw.as_str()) {
        return s;
    }
    if let Ok(parts) = json::from_str::<Vec<RawJson>>(raw.as_str()) {
        let mut texts = Vec::new();
        for p in parts {
            let f = kimi_object(p.as_bytes());
            if f.str("type") == "text" {
                texts.push(f.str("text"));
            }
        }
        return texts.join("\n");
    }
    raw.as_str().to_string()
}

pub(crate) static KIMI_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\t\n\f\r ]*([0-9]+)\t(.*)$").unwrap());

fn kimi_response(name: &str, input: &[u8], result: &[u8], output: &str) -> Vec<u8> {
    let Some(mut r) = parse_object(result) else {
        return result.to_vec();
    };
    if name != "Read" || r.get("file").is_some() {
        return result.to_vec();
    }
    let mut start = 1usize;
    if let Some(in_obj) = parse_object(input)
        && let Some(n) = in_obj.num("line_offset")
        && n > 0
    {
        start = n as usize;
    }
    let n = count_lines(output);
    let mut f = Map::new();
    f.insert("numLines".to_string(), Value::Number((n as i64).into()));
    f.insert(
        "startLine".to_string(),
        Value::Number((start as i64).into()),
    );
    if let Some(text) = numbered_text(output, &KIMI_LINE, start, n) {
        f.insert("content".to_string(), Value::String(text));
    }
    r.0.insert("file".to_string(), Value::Object(f));
    marshal_sorted_value(&Value::Object(r.0))
        .map(|raw| raw.as_bytes().to_vec())
        .unwrap_or_else(|| result.to_vec())
}

pub(crate) fn kimi_file_options(path: &str) -> Options {
    let agent_dir = filepath::dir(path);
    if filepath::base(path) != "wire.jsonl"
        || filepath::base(&filepath::dir(&agent_dir)) != "agents"
    {
        return Options::default();
    }
    let session_dir = filepath::dir(&filepath::dir(&agent_dir));
    let mut s = Session {
        session_id: session_id_from_path(path),
        ..Session::default()
    };
    let main = filepath::base(&agent_dir) == "main";
    if !main {
        let base_sd = filepath::base(&session_dir);
        let parent_id = base_sd.strip_prefix("session_").unwrap_or(&base_sd);
        s.parent_session = Some(ParentSession {
            session_id: parent_id.to_string(),
            tool_id: String::new(),
        });
    }
    if let Ok(data) = std::fs::read(filepath::join(&[&session_dir, "state.json"])) {
        let state = kimi_object(&data);
        if main {
            s.title = state.str("title").trim().to_string();
            if s.title.is_empty() {
                s.title = state.str("lastPrompt").trim().to_string();
            }
        }
        let cwd = state.str("workDir").trim().to_string();
        if !cwd.is_empty() {
            s.set_cwd(&cwd, 0);
        } else {
            s.set_cwd(state.str("cwd").trim(), 0);
        }
    }
    Options {
        session: s,
        ..Options::default()
    }
}

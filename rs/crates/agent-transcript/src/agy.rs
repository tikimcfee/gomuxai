use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use agent_base::gostd::{filepath, json};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::claude::tag_text;
use crate::events::{
    Event, KIND_PROMPT, KIND_SAY, KIND_SYSTEM, KIND_THINK, KIND_TOOL, LineReader, OP_ADD,
    OP_UPDATE, ORIGIN_DELIVERED, ORIGIN_HUMAN, ORIGIN_PEER, ORIGIN_SYSTEM,
    ORIGIN_TASK_NOTIFICATION, Options, Parser, PromptEvent, RawJson, Reader, STATUS_BACKGROUND,
    STATUS_ERROR, STATUS_INTERRUPTED, STATUS_OK, STATUS_PENDING, SayEvent, Session, SystemEvent,
    ThinkEvent, ToolEvent, ToolResult, TurnRef, deserialize_present_raw, new_parser,
};
use crate::meta::{AgyFiles, read_agy_history, read_agy_subagent};
use crate::tools::{JsonObj, marshal_sorted_value, parse_object, summarize_tool};

/// Reads the Antigravity CLI's conversation transcripts
/// (`~/.gemini/antigravity-cli/brain/<conversation id>/.system_generated/logs/transcript_full.jsonl`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Agy;

#[derive(Debug, Default, Deserialize)]
struct AgyStep {
    #[serde(default)]
    step_index: i64,
    #[serde(default)]
    source: String,
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    tool_calls: Vec<AgyToolCall>,
}

#[derive(Debug, Default, Deserialize)]
struct AgyToolCall {
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "deserialize_present_raw")]
    args: Option<RawJson>,
}

impl Reader for Agy {
    fn harness(&self) -> &'static str {
        "agy"
    }

    fn new_parser(&self, opts: Options) -> Parser {
        let encoded = filepath::base(&opts.session.path) == "transcript.jsonl";
        let agent = if opts.session.parent_session.is_some() {
            opts.session.session_id.clone()
        } else {
            String::new()
        };
        let mut sess = opts.session;
        sess.path.clear();
        let a = AgyLines {
            sess,
            agent,
            turn: opts.turn,
            turns: 0,
            encoded,
            prev: -1,
            highest: -1,
            rewinds: 0,
            tools: HashMap::new(),
            planner: 0,
            calls: Vec::new(),
            running: Vec::new(),
        };
        new_parser("agy", Box::new(a))
    }
}

struct AgyRunning {
    task: String,
    call: String,
}

struct AgyLines {
    sess: Session,
    agent: String,
    turn: Option<TurnRef>,
    turns: usize,
    encoded: bool,
    prev: i64,
    highest: i64,
    rewinds: usize,
    tools: HashMap<String, Event>,
    planner: i64,
    calls: Vec<String>,
    running: Vec<AgyRunning>,
}

impl LineReader for AgyLines {
    fn session(&self) -> Session {
        self.sess.clone()
    }

    fn read(&mut self, line: &[u8], at: i64, n: usize) -> (Vec<Event>, bool) {
        let Ok(st) = json::from_slice::<AgyStep>(line) else {
            return (Vec::new(), false);
        };
        self.sess.set_cwd(&st.cwd, at);
        let mut step = st.step_index.to_string();
        if self.prev >= 0 && st.step_index <= self.prev {
            self.rewinds += 1;
        }
        if st.step_index <= self.highest {
            step.push('.');
            step.push_str(&(self.rewinds + 1).to_string());
        }
        self.prev = st.step_index;
        self.highest = self.highest.max(st.step_index);

        let mut l = AgyLine {
            a: self,
            st: &st,
            step,
            at,
            n,
            adds: Vec::new(),
            updates: Vec::new(),
        };
        match st.r#type.as_str() {
            "USER_INPUT" => l.user_input(),
            "PLANNER_RESPONSE" => l.planner(),
            "GENERIC" => l.generic(),
            "SYSTEM_MESSAGE" => l.system_message(),
            "EPHEMERAL_MESSAGE" => {
                let text = st.content.trim();
                if !text.is_empty() {
                    let mut e = l.event(0, KIND_PROMPT);
                    e.prompt = Some(PromptEvent {
                        text: text.to_string(),
                        origin: agy_injected(text).to_string(),
                        command: String::new(),
                        images: Vec::new(),
                    });
                    l.add(e);
                }
            }
            "CHECKPOINT" => {
                if !st.content.trim().is_empty() {
                    let mut e = l.event(0, KIND_SYSTEM);
                    e.system = Some(SystemEvent {
                        subtype: "compact_summary".to_string(),
                        text: st.content.clone(),
                        data: None,
                    });
                    l.add(e);
                }
            }
            "ERROR_MESSAGE" => {
                let mut e = l.event(0, KIND_SYSTEM);
                let data = if !st.error.is_empty() {
                    let mut m = BTreeMap::new();
                    m.insert("error", st.error.as_str());
                    json::to_string(&m, true)
                        .ok()
                        .and_then(|s| RawJson::from_json_string(s).ok())
                } else {
                    None
                };
                e.system = Some(SystemEvent {
                    subtype: "api_error".to_string(),
                    text: st.content.clone(),
                    data,
                });
                l.add(e);
            }
            _ => {}
        }
        let mut out = l.adds;
        out.extend(l.updates);
        (out, true)
    }
}

struct AgyLine<'a> {
    a: &'a mut AgyLines,
    st: &'a AgyStep,
    step: String,
    at: i64,
    n: usize,
    adds: Vec<Event>,
    updates: Vec<Event>,
}

// The regexes keep RE2's classes, which are ASCII: its \s is [\t\n\f\r ] (no
// \v), its \S the rest, and its \d [0-9]; the regex crate's are Unicode's.
static AGY_MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"setting `Model Selection` from .*? to (.+?)\. ").unwrap());
static AGY_TASK_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"background task with task id: ([^\t\n\f\r ]+)").unwrap());
static AGY_KILLED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^Task "([^"]+)" cancelled\."#).unwrap());
static AGY_TASK_SENDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/task-[0-9]+$").unwrap());
static AGY_TASK_DONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^Task id "([^"]+)" (finished|was canceled) with result:\n?((?s:.*))$"#).unwrap()
});
const AGY_RESTART: &str = "[Notice] All your subagents and background tasks have been stopped";
static AGY_HOOK_PREAMBLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:The following system message was injected by an? [0-9A-Za-z_]+ hook:|Stop hook blocked termination:)[\t\n\f\r ]*",
    )
    .unwrap()
});
static AGY_DELIVERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:\[from |\[URGENT INTERRUPT\]\n|[0-9]+ messages arrived while you were working(?:, | \()|\[part [0-9]+/[0-9]+ of the message above\]\n)",
    )
    .unwrap()
});
static AGY_MESSAGE_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\[Message\] (?:timestamp=[^\t\n\f\r ]+ )?sender=([^\t\n\f\r ]+) (?:priority=[^\t\n\f\r ]+ )?content=((?s:.*))$",
    )
    .unwrap()
});
static AGY_MESSAGE_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\[Message\] ").unwrap());
static AGY_CONVERSATION_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""conversationId":[\t\n\f\r ]*"([^"]+)""#).unwrap());
static AGY_EXIT_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(?:The c|C)ommand exited with code ([0-9]+)\.").unwrap());
static AGY_SHOWING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^Showing lines ([0-9]+)[\t\n\f\r ]*(?:-|to)[\t\n\f\r ]*([0-9]+)(?: of ([0-9]+))?",
    )
    .unwrap()
});
static AGY_TOTAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^Total Lines: ([0-9]+)").unwrap());
static AGY_NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^[0-9]+: ").unwrap());
pub(crate) static AGY_NUMBERED_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]+): ?(.*)$").unwrap());
static AGY_REPLACED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^Made 1 replacement in .* starting at line ([0-9]+) \(\+([0-9]+)/-([0-9]+) lines\)",
    )
    .unwrap()
});
static AGY_HUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -([0-9]+)(?:,([0-9]+))? \+([0-9]+)(?:,([0-9]+))? @@").unwrap()
});

impl AgyLines {
    fn running_call(&self, task: &str) -> Option<Event> {
        for r in &self.running {
            if r.task == task {
                return self.tools.get(&r.call).cloned();
            }
        }
        None
    }
}

impl AgyLine<'_> {
    fn event(&self, part: usize, kind: &str) -> Event {
        let id = format!("step_{}#{part}", self.step);
        Event {
            id,
            op: OP_ADD.to_string(),
            at: self.at,
            line: self.n,
            ts: self.st.created_at.clone(),
            turn: self.a.turn.clone(),
            agent: self.a.agent.clone(),
            kind: kind.to_string(),
            ..Event::default()
        }
    }

    fn add(&mut self, e: Event) {
        self.adds.push(e);
    }

    fn user_input(&mut self) {
        if let Some(caps) = AGY_MODEL.captures(&self.st.content) {
            self.a.sess.set_model(&caps[1], self.at);
        }
        let text = agy_prompt(&self.st.content);
        if text.is_empty() {
            return;
        }
        let mut e = self.event(0, KIND_PROMPT);
        e.prompt = Some(PromptEvent {
            text: text.clone(),
            origin: ORIGIN_HUMAN.to_string(),
            command: String::new(),
            images: Vec::new(),
        });
        if self.st.source == "SYSTEM_SDK" {
            if let Some(ref mut p) = e.prompt {
                p.origin = agy_injected(&text).to_string();
            }
            self.add(e);
            return;
        }
        self.a.turns += 1;
        let turn = TurnRef {
            id: e.id.clone(),
            index: self.a.turns,
            derived: true,
        };
        self.a.turn = Some(turn.clone());
        e.turn = Some(turn);
        self.add(e);
        self.abandon();
    }

    fn abandon(&mut self) {
        let calls = std::mem::take(&mut self.a.calls);
        for id in calls {
            if !id.is_empty()
                && let Some(call) = self.a.tools.get(&id).cloned()
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

    fn planner(&mut self) {
        let st = self.st;
        let mut part = 0usize;
        if !st.thinking.trim().is_empty() {
            let mut e = self.event(part, KIND_THINK);
            e.think = Some(ThinkEvent {
                text: st.thinking.clone(),
                redacted: false,
            });
            self.add(e);
            part += 1;
        }
        if !st.content.trim().is_empty() {
            let mut e = self.event(part, KIND_SAY);
            e.say = Some(SayEvent {
                text: st.content.clone(),
                r#final: st.tool_calls.is_empty(),
                provisional: false,
            });
            self.add(e);
        }
        self.abandon();
        self.a.planner = st.step_index;
        for (j, tc) in st.tool_calls.iter().enumerate() {
            let input = tc.args.as_ref().and_then(|raw| {
                let bytes = agy_args(raw.as_bytes(), self.a.encoded);
                String::from_utf8(bytes)
                    .ok()
                    .and_then(|s| RawJson::from_json_string(s).ok())
            });
            if self.a.sess.cwd.is_empty()
                && tc.name == "run_command"
                && let Some(ref inp) = input
            {
                // Go's struct{ Cwd string }: "cwd" matches it too, and the
                // error is ignored.
                #[derive(Default, Deserialize)]
                struct RunArgs {
                    #[serde(rename = "Cwd", default)]
                    cwd: String,
                }
                let args = json::from_slice_partial::<RunArgs>(inp.as_bytes()).0;
                self.a.sess.set_cwd(&args.cwd, self.at);
            }
            let mut e = self.event(0, KIND_TOOL);
            e.id = format!("step_{}_tool_{j}", self.step);
            let input_bytes = input.as_ref().map_or(&[][..], RawJson::as_bytes);
            let summary = summarize_tool(&tc.name, input_bytes, None, &self.a.sess.cwd);
            e.tool = Some(ToolEvent {
                name: tc.name.clone(),
                input,
                result: Some(ToolResult {
                    status: STATUS_PENDING.to_string(),
                    output: String::new(),
                    structured: None,
                }),
                tool_summary: summary,
                sub_sessions: Vec::new(),
            });
            self.a.tools.insert(e.id.clone(), e.clone());
            self.a.calls.push(e.id.clone());
            self.add(e);
        }
    }

    fn generic(&mut self) {
        let j = self.st.step_index - self.a.planner - 1;
        if j < 0 || (j as usize) >= self.a.calls.len() {
            return;
        }
        let j = j as usize;
        if self.a.calls[j].is_empty() {
            return;
        }
        let id = std::mem::take(&mut self.a.calls[j]);
        let Some(call) = self.a.tools.get(&id).cloned() else {
            return;
        };
        let output = agy_output(&self.st.content);
        let status = agy_status(self.st, &output);
        let res = ToolResult {
            status: status.to_string(),
            output: output.clone(),
            structured: None,
        };
        let tool_ref = call.tool.as_ref().unwrap();
        let tool_name = tool_ref.name.clone();
        let input_bytes = tool_ref
            .input
            .as_ref()
            .map_or(Vec::new(), |r| r.as_bytes().to_vec());
        let response: Option<Vec<u8>> = if status == STATUS_BACKGROUND {
            if let Some(caps) = AGY_TASK_ID.captures(&output) {
                self.a.running.push(AgyRunning {
                    task: caps[1].to_string(),
                    call: id,
                });
            }
            None
        } else if self.st.status == "ERROR"
            || self.st.status == "INVALID"
            || !self.st.error.is_empty()
        {
            let mut m = BTreeMap::new();
            m.insert("output", output.as_str());
            json::marshal(&m).ok()
        } else {
            Some(agy_response(&tool_name, &input_bytes, &output))
        };
        let subs = if tool_name == "invoke_subagent" {
            agy_subagent_ids(&output)
        } else {
            Vec::new()
        };
        let res_ok = res.status == STATUS_OK;
        self.update(&call, res, response.as_deref(), subs);
        if tool_name == "manage_task"
            && res_ok
            && let Some(caps) = AGY_KILLED.captures(&output)
        {
            self.end(
                &caps[1],
                ToolResult {
                    status: STATUS_INTERRUPTED.to_string(),
                    output: String::new(),
                    structured: None,
                },
                None,
            );
        }
    }

    fn update(
        &mut self,
        call: &Event,
        res: ToolResult,
        response: Option<&[u8]>,
        subs: Vec<String>,
    ) {
        let mut t = call.tool.clone().unwrap_or_default();
        let input_bytes = t.input.as_ref().map_or(&[][..], RawJson::as_bytes);
        t.tool_summary = summarize_tool(&t.name, input_bytes, response, &self.a.sess.cwd);
        t.result = Some(res);
        if !subs.is_empty() {
            t.sub_sessions = subs;
        }
        let mut e = call.clone();
        e.op = OP_UPDATE.to_string();
        e.at = self.at;
        e.line = self.n;
        e.ts = self.st.created_at.clone();
        e.tool = Some(t);
        self.updates.push(e.clone());
        self.a.tools.insert(call.id.clone(), e);
    }

    fn end(&mut self, task: &str, res: ToolResult, response: Option<&[u8]>) {
        let Some(i) = self.a.running.iter().position(|r| r.task == task) else {
            return;
        };
        let removed = self.a.running.remove(i);
        if let Some(call) = self.a.tools.get(&removed.call).cloned() {
            self.update(&call, res, response, Vec::new());
        }
    }

    fn system_message(&mut self) {
        for (k, m) in agy_system_entries(&self.st.content).into_iter().enumerate() {
            let mut e = self.event(k, KIND_PROMPT);
            let mut prompt_text = m.text.clone();
            let mut origin = ORIGIN_SYSTEM;
            if m.sender.is_empty() {
                if let Some(mat) = AGY_HOOK_PREAMBLE.find(&m.text) {
                    prompt_text = m.text[mat.end()..].to_string();
                    origin = agy_injected(&prompt_text);
                }
            } else if m.sender == "system" {
                origin = ORIGIN_SYSTEM;
            } else if AGY_TASK_SENDER.is_match(&m.sender) {
                origin = ORIGIN_TASK_NOTIFICATION;
            } else {
                origin = ORIGIN_PEER;
            }
            e.prompt = Some(PromptEvent {
                text: prompt_text,
                origin: origin.to_string(),
                command: String::new(),
                images: Vec::new(),
            });
            self.add(e);

            if let Some(done) = AGY_TASK_DONE.captures(&m.text) {
                let Some(call) = self.a.running_call(&done[1]) else {
                    continue;
                };
                let output = done[3].trim().to_string();
                let status = if &done[2] == "was canceled" {
                    STATUS_INTERRUPTED
                } else if agy_failed(&output) {
                    STATUS_ERROR
                } else {
                    STATUS_OK
                };
                let res = ToolResult {
                    status: status.to_string(),
                    output: output.clone(),
                    structured: None,
                };
                let tool_ref = call.tool.as_ref().unwrap();
                let input_bytes = tool_ref.input.as_ref().map_or(&[][..], RawJson::as_bytes);
                let resp = agy_response(&tool_ref.name, input_bytes, &output);
                self.end(&done[1], res, Some(&resp));
                continue;
            }
            if m.sender == "system" && m.text.starts_with(AGY_RESTART) {
                while !self.a.running.is_empty() {
                    let task = self.a.running[0].task.clone();
                    self.end(
                        &task,
                        ToolResult {
                            status: STATUS_INTERRUPTED.to_string(),
                            output: String::new(),
                            structured: None,
                        },
                        None,
                    );
                }
            } else if origin == ORIGIN_TASK_NOTIFICATION
                && let Some(call) = self.a.running_call(&m.sender)
                && call.tool.as_ref().is_some_and(|t| t.name == "schedule")
            {
                self.end(
                    &m.sender,
                    ToolResult {
                        status: STATUS_OK.to_string(),
                        output: m.text,
                        structured: None,
                    },
                    None,
                );
            }
        }
    }
}

fn agy_injected(text: &str) -> &'static str {
    if AGY_DELIVERED.is_match(text) {
        ORIGIN_DELIVERED
    } else {
        ORIGIN_SYSTEM
    }
}

struct AgyMessage {
    sender: String,
    text: String,
}

fn agy_system_entries(content: &str) -> Vec<AgyMessage> {
    let mut body = content;
    if let Some(end) = content.rfind("</SYSTEM_MESSAGE>")
        && let Some(start) = content[..end].rfind("<SYSTEM_MESSAGE>")
    {
        body = &content[start + "<SYSTEM_MESSAGE>".len()..end];
    }
    let mut parts = Vec::new();
    let mut last = 0usize;
    for mat in AGY_MESSAGE_START.find_iter(body) {
        parts.push(&body[last..mat.start()]);
        last = mat.start();
    }
    parts.push(&body[last..]);
    let mut out = Vec::new();
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(caps) = AGY_MESSAGE_HEAD.captures(p) {
            out.push(AgyMessage {
                sender: caps[1].to_string(),
                text: caps[2].trim().to_string(),
            });
        } else {
            out.push(AgyMessage {
                sender: String::new(),
                text: p.to_string(),
            });
        }
    }
    out
}

fn agy_subagent_ids(output: &str) -> Vec<String> {
    AGY_CONVERSATION_ID
        .captures_iter(output)
        .map(|c| c[1].to_string())
        .collect()
}

/// Returns a call's `args` as the call's input. `transcript_full.jsonl` stores
/// them plain, as they are; `transcript.jsonl` (`encoded`) writes every value as
/// a string holding its JSON (`"\"ls\""`, `"5000"`, `"false"`, `"[...]"`), so
/// each such value is decoded once. A value agy cut short there
/// (`truncated_fields`) is no longer valid JSON and stays as it was written.
pub(crate) fn agy_args(args: &[u8], encoded: bool) -> Vec<u8> {
    if !encoded {
        return args.to_vec();
    }
    let Ok(mut fields) = json::from_slice::<BTreeMap<String, RawJson>>(args) else {
        return args.to_vec();
    };
    if fields.is_empty() {
        return args.to_vec();
    }
    for v in fields.values_mut() {
        if let Ok(s) = json::from_str::<String>(v.as_str())
            && json::valid(s.as_bytes())
            && let Ok(decoded) = RawJson::from_json_string(s)
        {
            *v = decoded;
        }
    }
    // json.Marshal: the keys sorted, each raw value compacted, HTML escaped.
    json::marshal(&fields).unwrap_or_else(|_| args.to_vec())
}

fn agy_prompt(content: &str) -> String {
    if let Some(req) = tag_text(content, "USER_REQUEST") {
        return req.to_string();
    }
    content.trim().to_string()
}

fn agy_output(content: &str) -> String {
    let mut rest = content;
    loop {
        let (line, after) = match rest.split_once('\n') {
            Some((l, a)) => (l, Some(a)),
            None => (rest, None),
        };
        if !line.starts_with("Created At:") && !line.starts_with("Completed At:") {
            break;
        }
        match after {
            Some(a) => rest = a,
            None => {
                rest = "";
                break;
            }
        }
    }
    rest.trim().to_string()
}

fn agy_failed(output: &str) -> bool {
    AGY_EXIT_CODE
        .captures(output)
        .is_some_and(|caps| &caps[1] != "0")
}

fn agy_status(st: &AgyStep, output: &str) -> &'static str {
    if st.status == "ERROR" || st.status == "INVALID" || !st.error.is_empty() {
        STATUS_ERROR
    } else if st.status == "RUNNING" {
        STATUS_BACKGROUND
    } else if agy_failed(output) {
        STATUS_ERROR
    } else {
        STATUS_OK
    }
}

/// The structured response the tool table reads from a result agy records only
/// as text, as `glyph3d-js`'s `agyEnrich` reconstructs it.
pub(crate) fn agy_response(name: &str, input: &[u8], output: &str) -> Vec<u8> {
    let mut r = Map::new();
    r.insert("output".to_string(), Value::String(output.to_string()));
    let in_obj = parse_object(input).unwrap_or_default();
    match name {
        "run_command" => {
            r.insert("stdout".to_string(), Value::String(output.to_string()));
        }
        "view_file" | "read_file" => {
            if let Some(f) = agy_read_file(output, &in_obj) {
                r.insert("file".to_string(), Value::Object(f));
            }
        }
        "replace_file_content" => {
            if let Some(p) = agy_diff_patch(output)
                && let Ok(val) = serde_json::to_value(p)
            {
                r.insert("structuredPatch".to_string(), val);
            }
        }
        "edit_file" => {
            if let Some(caps) = AGY_REPLACED.captures(output) {
                let start: i64 = caps[1].parse().unwrap_or(0);
                let added: i64 = caps[2].parse().unwrap_or(0);
                let removed: i64 = caps[3].parse().unwrap_or(0);
                let mut repl = Map::new();
                repl.insert("added".to_string(), Value::Number(added.into()));
                repl.insert("removed".to_string(), Value::Number(removed.into()));
                repl.insert("startLine".to_string(), Value::Number(start.into()));
                r.insert("replacement".to_string(), Value::Object(repl));
            }
        }
        "write_to_file" | "write_file" => {
            let typ = if in_obj.get("Overwrite").and_then(Value::as_bool) == Some(true) {
                "update"
            } else {
                "create"
            };
            r.insert("type".to_string(), Value::String(typ.to_string()));
            if let Some(content) = in_obj.text(&["CodeContent", "Content"]) {
                r.insert("content".to_string(), Value::String(content.to_string()));
            }
        }
        _ => {}
    }
    marshal_sorted_value(&Value::Object(r))
        .map(|raw| raw.as_bytes().to_vec())
        .unwrap_or_default()
}

fn agy_read_file(output: &str, in_obj: &JsonObj) -> Option<Map<String, Value>> {
    let mut f = Map::new();
    if let Some(caps) = AGY_TOTAL.captures(output)
        && let Ok(total) = caps[1].parse::<i64>()
    {
        f.insert("totalLines".to_string(), Value::Number(total.into()));
    }
    let (start, n) = if let Some(caps) = AGY_SHOWING.captures(output) {
        let s: i64 = caps[1].parse().unwrap_or(0);
        let end: i64 = caps[2].parse().unwrap_or(0);
        if let Some(m3) = caps.get(3)
            && !m3.as_str().is_empty()
            && let Ok(total) = m3.as_str().parse::<i64>()
        {
            f.insert("totalLines".to_string(), Value::Number(total.into()));
        }
        let count = (end - s + 1).max(0) as usize;
        (s.max(0) as usize, count)
    } else {
        let count = AGY_NUMBERED.find_iter(output).count();
        if count == 0 {
            return None;
        }
        let mut s = 1usize;
        if let Some(in_start) = in_obj.num("StartLine")
            && in_start > 0
        {
            s = in_start as usize;
        }
        (s, count)
    };
    f.insert(
        "startLine".to_string(),
        Value::Number((start as i64).into()),
    );
    f.insert("numLines".to_string(), Value::Number((n as i64).into()));
    if let Some(text) = numbered_text(output, &AGY_NUMBERED_LINE, start, n) {
        f.insert("content".to_string(), Value::String(text));
    }
    Some(f)
}

/// The text of the `n` lines a tool printed numbered from `start`, each line
/// matching `line` (its number, then its text), with the numbers taken off.
/// Lines before the first are a header and are skipped; `None` unless all `n`
/// are there, in order.
pub(crate) fn numbered_text(
    output: &str,
    line_re: &Regex,
    start: usize,
    n: usize,
) -> Option<String> {
    if n == 0 {
        return None;
    }
    let mut b = String::new();
    let mut want = start;
    for l in output.split('\n') {
        let Some(caps) = line_re.captures(l) else {
            if want > start {
                break;
            }
            continue;
        };
        let num: usize = caps[1].parse().unwrap_or(0);
        if num != want {
            if want > start {
                break;
            }
            continue;
        }
        b.push_str(&caps[2]);
        b.push('\n');
        want += 1;
        if want == start + n {
            return Some(b);
        }
    }
    None
}

#[derive(Serialize)]
struct AgyHunkLines {
    #[serde(rename = "oldStart")]
    old_start: i64,
    #[serde(rename = "oldLines")]
    old_lines: i64,
    #[serde(rename = "newStart")]
    new_start: i64,
    #[serde(rename = "newLines")]
    new_lines: i64,
    lines: Vec<String>,
}

fn agy_diff_patch(output: &str) -> Option<Vec<AgyHunkLines>> {
    if output.contains("middle of the output truncated") {
        return None;
    }
    let (_, rest) = output.split_once("[diff_block_start]\n")?;
    let (diff, _) = rest.split_once("[diff_block_end]")?;
    let count = |s: Option<regex::Match<'_>>| -> i64 {
        match s {
            None => 1,
            Some(m) if m.as_str().is_empty() => 1,
            Some(m) => m.as_str().parse().unwrap_or(0),
        }
    };
    let mut hunks: Vec<AgyHunkLines> = Vec::new();
    for line in diff.strip_suffix('\n').unwrap_or(diff).split('\n') {
        if let Some(caps) = AGY_HUNK.captures(line) {
            hunks.push(AgyHunkLines {
                old_start: count(caps.get(1)),
                old_lines: count(caps.get(2)),
                new_start: count(caps.get(3)),
                new_lines: count(caps.get(4)),
                lines: Vec::new(),
            });
            continue;
        }
        let Some(h) = hunks.last_mut() else {
            continue;
        };
        if line.is_empty() {
            // A blank context line can lose its leading space.
            h.lines.push(" ".to_string());
        } else if matches!(line.as_bytes()[0], b' ' | b'+' | b'-') {
            h.lines.push(line.to_string());
        }
    }
    Some(hunks)
}

pub(crate) fn agy_file_options(path: &str) -> Options {
    let mut opts = Options {
        session: Session {
            path: path.to_string(),
            ..Session::default()
        },
        ..Options::default()
    };
    let conv = agy_conversation_dir(path);
    if conv.is_empty() {
        return opts;
    }
    let id = filepath::base(&conv);
    let brain = filepath::dir(&conv);
    let home = filepath::dir(&brain);
    let workspace = read_agy_history(&home).remove(&id).unwrap_or_default();
    let mut subagent = None;
    if let Ok(entries) = std::fs::read_dir(&brain) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            let desc = filepath::join(&[
                &brain,
                &name,
                ".system_generated",
                "subagents",
                &format!("{id}.json"),
            ]);
            if std::fs::symlink_metadata(&desc).is_ok() {
                if let Some(sub) = read_agy_subagent(&desc) {
                    subagent = Some(sub);
                }
                break;
            }
        }
    }
    let files = AgyFiles {
        home,
        workspace,
        subagent,
    };
    opts.session.session_id = id.clone();
    opts.session.harvest_agy(&files);
    if let Some(ref sub) = files.subagent
        && let Some(calls) = agy_spawn_calls(&filepath::join(&[&brain, &sub.parent]))
        && let Some(call) = calls.get(&id)
    {
        if let Some(ref mut parent_sess) = opts.session.parent_session {
            parent_sess.tool_id = call.id.clone();
        }
        opts.turn = call.turn.clone();
    }
    opts
}

/// Returns the `brain/<conversation id>` dir an agy transcript is in, for either
/// place agy keeps one: `<id>/.system_generated/logs/transcript*.jsonl` or
/// `brain/<id>/transcript*.jsonl`. Returns `""` for a path of neither shape.
pub fn agy_conversation_dir(path: &str) -> String {
    if !filepath::base(path).starts_with("transcript") {
        return String::new();
    }
    let dir = filepath::dir(path);
    if filepath::base(&dir) == "logs" && filepath::base(&filepath::dir(&dir)) == ".system_generated"
    {
        return filepath::dir(&filepath::dir(&dir));
    }
    if filepath::base(&filepath::dir(&dir)) == "brain" {
        return dir;
    }
    String::new()
}

const AGY_TRANSCRIPTS: &[&str] = &[
    ".system_generated/logs/transcript_full.jsonl",
    ".system_generated/logs/transcript.jsonl",
    "transcript_full.jsonl",
    "transcript.jsonl",
];

struct AgySpawnIndex {
    size: u64,
    mtime: Option<SystemTime>,
    calls: HashMap<String, Event>,
}

static AGY_SPAWNS: LazyLock<Mutex<HashMap<String, AgySpawnIndex>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn agy_spawn_calls(conv: &str) -> Option<HashMap<String, Event>> {
    let mut found: Option<(String, std::fs::File)> = None;
    for name in AGY_TRANSCRIPTS {
        let p = filepath::join(&[conv, name]);
        if let Ok(f) = std::fs::File::open(&p) {
            found = Some((p, f));
            break;
        }
    }
    let (path, f) = found?;
    let info = f.metadata().ok()?;
    let size = info.len();
    let mtime = info.modified().ok();

    let mut cache = AGY_SPAWNS.lock().unwrap();
    if let Some(c) = cache.get(&path)
        && c.size == size
        && c.mtime == mtime
    {
        return Some(c.calls.clone());
    }

    let mut calls = HashMap::new();
    let mut p = Agy.new_parser(Options {
        session: Session {
            path: path.clone(),
            ..Session::default()
        },
        ..Options::default()
    });
    let mut br = BufReader::new(f);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = br.read_until(b'\n', &mut line).unwrap_or(0);
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            line.push(b'\n');
        }
        for e in p.feed(&line) {
            if e.op == OP_UPDATE
                && let Some(ref tool) = e.tool
                && tool.name == "invoke_subagent"
                && let Some(ref res) = tool.result
            {
                for id in agy_subagent_ids(&res.output) {
                    calls.insert(id, e.clone());
                }
            }
        }
    }
    cache.insert(
        path,
        AgySpawnIndex {
            size,
            mtime,
            calls: calls.clone(),
        },
    );
    Some(calls)
}

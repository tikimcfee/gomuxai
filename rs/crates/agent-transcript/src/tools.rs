//! The one tool table: it turns a raw tool call into what a reader shows.
//! Ported from `glyph3d-js` `packages/glyph3d-core/src/collections/toolRegistry.js`
//! (`TOOLS`, `normalizeToolCall`, the meta readers, `askDetail`, `fallbackDetail`,
//! `relativize`, the noise list). Its presentation half (highlight decoration,
//! hues, kind buckets) stays with the readers, and so does the result text: a
//! tool event carries its result whole.

use std::collections::{BTreeMap, HashMap};

use agent_base::gostd::json;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::events::{RawJson, deserialize_present_raw, is_false, is_zero_usize};

/// How a reader shows one tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSummary {
    /// What kind of thing the call did: `read`, `edit`, `write`, `bash`,
    /// `grep`, `glob`, `task`, `ask`, `fetch`, `search`, `skill`, `send`, or
    /// [`ACTION_NOISE`]. A tool the table doesn't know keeps its own name,
    /// lowercased.
    #[serde(default)]
    pub action: String,
    /// The file the call is about, relative to the session's working directory
    /// when it's under it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// The call's most telling input, whole: its command, pattern, URL,
    /// question or message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Holds the numbers worth showing, by action:
    ///   - `read`   `{lines, range?: [first, last]}` (`range` on a partial read), or `{bytes}` for an image
    ///   - `edit`   `{added, removed, ranges?: [[first, last], ...], patch?}`
    ///   - `write`  `{kind?, lines}`
    ///   - `bash`   `{lines, interrupted?}`
    ///   - `task`   `{tools?, tokens?, ms?}`
    #[serde(
        default,
        deserialize_with = "deserialize_present_raw",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<RawJson>,
    /// The target's text as the call saw it (a read: the lines it read) or left
    /// it (a write: what it wrote; an edit: the file after it), when the result
    /// holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileText>,
}

/// Part or all of a file's text: its lines from `start` (1-based), of the
/// file's `total` lines when the result says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileText {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub start: usize,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub total: usize,
}

/// The action of the bookkeeping tools (`TodoWrite`, `ToolSearch`, ...). Their
/// events are kept; readers may hide them.
pub const ACTION_NOISE: &str = "noise";

const FILE_KEYS: &[&str] = &["file_path", "path"];

const FALLBACK_KEYS: &[&str] = &[
    "command",
    "pattern",
    "query",
    "url",
    "description",
    "prompt",
    "path",
    "file_path",
    "name",
    "CommandLine",
    "Pattern",
    "Query",
    "Url",
    "AbsolutePath",
    "TargetFile",
    "Path",
    "SearchPath",
    "ToolName",
    "toolSummary",
];

#[derive(Clone, Copy)]
enum DetailKind {
    None,
    Field(&'static [&'static str]),
    Labeled(&'static str, &'static str),
    Joined(&'static str, &'static [&'static str]),
    Ask,
    Subagent,
    Message,
}

type MetaFn = fn(&JsonObj, Option<&JsonObj>) -> Option<RawJson>;
type FileFn = fn(&JsonObj, &JsonObj) -> Option<FileText>;

#[derive(Clone, Copy)]
struct ToolSpec {
    action: &'static str,
    target: &'static [&'static str],
    detail: DetailKind,
    meta: Option<MetaFn>,
    file: Option<FileFn>,
}

fn lookup_tool(name: &str) -> Option<ToolSpec> {
    let spec = match name {
        "Read" => ToolSpec {
            action: "read",
            target: FILE_KEYS,
            detail: DetailKind::None,
            meta: Some(read_meta),
            file: Some(read_file),
        },
        "Edit" | "MultiEdit" => ToolSpec {
            action: "edit",
            target: FILE_KEYS,
            detail: DetailKind::None,
            meta: Some(edit_meta),
            file: Some(edited_file),
        },
        "NotebookEdit" => ToolSpec {
            action: "edit",
            target: &["notebook_path", "file_path", "path"],
            detail: DetailKind::None,
            meta: Some(notebook_meta),
            file: None,
        },
        "Write" => ToolSpec {
            action: "write",
            target: FILE_KEYS,
            detail: DetailKind::None,
            meta: Some(write_meta),
            file: Some(written_file),
        },
        "Bash" => ToolSpec {
            action: "bash",
            target: &[],
            detail: DetailKind::Field(&["command"]),
            meta: Some(bash_meta),
            file: None,
        },
        "Grep" => ToolSpec {
            action: "grep",
            target: &[],
            detail: DetailKind::Field(&["pattern"]),
            meta: None,
            file: None,
        },
        "Glob" => ToolSpec {
            action: "glob",
            target: &[],
            detail: DetailKind::Field(&["pattern"]),
            meta: None,
            file: None,
        },
        "Task" | "Agent" => ToolSpec {
            action: "task",
            target: &[],
            detail: DetailKind::Field(&["subagent_type", "description"]),
            meta: Some(task_meta),
            file: None,
        },
        "AgentSwarm" => ToolSpec {
            action: "task",
            target: &[],
            detail: DetailKind::Field(&["description"]),
            meta: Some(task_meta),
            file: None,
        },
        "Workflow" => ToolSpec {
            action: "task",
            target: &[],
            detail: DetailKind::Labeled("workflow", "name"),
            meta: None,
            file: None,
        },
        "AskUserQuestion" | "ask_question" => ToolSpec {
            action: "ask",
            target: &[],
            detail: DetailKind::Ask,
            meta: None,
            file: None,
        },
        "WebFetch" | "FetchURL" => ToolSpec {
            action: "fetch",
            target: &[],
            detail: DetailKind::Field(&["url"]),
            meta: None,
            file: None,
        },
        "WebSearch" | "search_web" => ToolSpec {
            action: "search",
            target: &[],
            detail: DetailKind::Field(&["query"]),
            meta: None,
            file: None,
        },
        "Skill" => ToolSpec {
            action: "skill",
            target: &[],
            detail: DetailKind::Field(&["skill"]),
            meta: None,
            file: None,
        },
        "TodoWrite" | "TodoList" | "EnterPlanMode" | "ToolSearch" | "TaskGet" | "TaskOutput" => {
            ToolSpec {
                action: ACTION_NOISE,
                target: &[],
                detail: DetailKind::None,
                meta: None,
                file: None,
            }
        }
        // agy: its own names and argument keys. view_file is read_file's older name.
        "view_file" => ToolSpec {
            action: "read",
            target: &["AbsolutePath"],
            detail: DetailKind::None,
            meta: Some(read_meta),
            file: Some(read_file),
        },
        "read_file" => ToolSpec {
            action: "read",
            target: &["Path"],
            detail: DetailKind::None,
            meta: Some(read_meta),
            file: Some(read_file),
        },
        "replace_file_content" => ToolSpec {
            action: "edit",
            target: &["TargetFile"],
            detail: DetailKind::None,
            meta: Some(edit_meta),
            file: None,
        },
        "edit_file" => ToolSpec {
            action: "edit",
            target: &["Path"],
            detail: DetailKind::None,
            meta: Some(replace_meta),
            file: None,
        },
        "write_to_file" => ToolSpec {
            action: "write",
            target: &["TargetFile"],
            detail: DetailKind::None,
            meta: Some(write_meta),
            file: None,
        },
        "write_file" => ToolSpec {
            action: "write",
            target: &["Path"],
            detail: DetailKind::None,
            meta: Some(write_meta),
            file: None,
        },
        "run_command" => ToolSpec {
            action: "bash",
            target: &[],
            detail: DetailKind::Field(&["CommandLine"]),
            meta: Some(bash_meta),
            file: None,
        },
        "invoke_subagent" => ToolSpec {
            action: "task",
            target: &[],
            detail: DetailKind::Subagent,
            meta: None,
            file: None,
        },
        "manage_subagents" => ToolSpec {
            action: "task",
            target: &[],
            detail: DetailKind::Labeled("subagents", "Action"),
            meta: None,
            file: None,
        },
        "manage_task" => ToolSpec {
            action: "task",
            target: &[],
            detail: DetailKind::Joined(" ", &["Action", "TaskId"]),
            meta: None,
            file: None,
        },
        "schedule" => ToolSpec {
            action: "task",
            target: &[],
            detail: DetailKind::Labeled("timer", "Prompt"),
            meta: None,
            file: None,
        },
        "send_message" => ToolSpec {
            action: "send",
            target: &[],
            detail: DetailKind::Message,
            meta: None,
            file: None,
        },
        "read_url_content" => ToolSpec {
            action: "fetch",
            target: &[],
            detail: DetailKind::Field(&["Url"]),
            meta: None,
            file: None,
        },
        _ => return None,
    };
    Some(spec)
}

/// Returns how a reader shows a call of the tool `name` with `input`, given its
/// result's structured `response` (Claude Code's `toolUseResult`, or a
/// harness's equivalent in its shape) or `None` while the call has none. `cwd`
/// is the session's working directory, or `""` to keep paths absolute.
pub fn summarize_tool(name: &str, input: &[u8], response: Option<&[u8]>, cwd: &str) -> ToolSummary {
    let in_obj = parse_object(input).unwrap_or_default();
    let mut s = describe(name, &in_obj, cwd);
    let Some(spec) = lookup_tool(name) else {
        return s;
    };
    if spec.meta.is_none() && spec.file.is_none() {
        return s;
    }
    let out_obj = response.and_then(parse_object);
    if let Some(meta_fn) = spec.meta {
        s.meta = meta_fn(&in_obj, out_obj.as_ref());
    }
    if let (Some(file_fn), Some(out)) = (spec.file, out_obj.as_ref()) {
        s.file = file_fn(&in_obj, out);
    }
    s
}

/// [`summarize_tool`] without `meta`, for a caller that shows only what a call
/// is about (a status line, a log line): it reads no result and counts nothing.
pub fn describe_tool(name: &str, input: &[u8], cwd: &str) -> ToolSummary {
    let in_obj = parse_object(input).unwrap_or_default();
    describe(name, &in_obj, cwd)
}

fn describe(name: &str, in_obj: &JsonObj, cwd: &str) -> ToolSummary {
    let Some(spec) = lookup_tool(name) else {
        let mut action = name.to_lowercase();
        if action.is_empty() {
            action = "act".to_string();
        }
        return ToolSummary {
            action,
            detail: relativize(&in_obj.str(FALLBACK_KEYS), cwd),
            ..ToolSummary::default()
        };
    };
    let mut s = ToolSummary {
        action: spec.action.to_string(),
        target: relativize(&in_obj.str(spec.target), cwd),
        ..ToolSummary::default()
    };
    match spec.detail {
        DetailKind::Field(keys) => s.detail = in_obj.str(keys),
        DetailKind::Labeled(label, key) => {
            let v = in_obj.str(&[key]);
            if v.is_empty() {
                s.detail = label.to_string();
            } else {
                s.detail = format!("{label}: {v}");
            }
        }
        DetailKind::Joined(sep, keys) => {
            let parts: Vec<String> = keys
                .iter()
                .map(|k| in_obj.str(&[*k]))
                .filter(|v| !v.is_empty())
                .collect();
            s.detail = parts.join(sep);
        }
        DetailKind::Ask => s.detail = ask_detail(in_obj),
        DetailKind::Subagent => s.detail = subagent_detail(in_obj),
        DetailKind::Message => s.detail = message_detail(in_obj),
        DetailKind::None => {
            if spec.action == ACTION_NOISE {
                // Noise still says what it was about (ToolSearch's query, say).
                s.detail = in_obj.str(FALLBACK_KEYS);
            }
        }
    }
    s
}

/// Makes a path under `cwd` relative to it; any other stays as it is.
fn relativize(p: &str, cwd: &str) -> String {
    if cwd.is_empty() {
        return p.to_string();
    }
    let prefix = format!("{}/", cwd.trim_end_matches('/'));
    if let Some(rel) = p.strip_prefix(&prefix)
        && !rel.is_empty()
    {
        return rel.to_string();
    }
    p.to_string()
}

// --- details ----------------------------------------------------------------

fn ask_detail(in_obj: &JsonObj) -> String {
    let mut qs = Vec::new();
    for q in in_obj.objects("questions") {
        let v = q.str(&["question"]);
        if !v.is_empty() {
            qs.push(v);
        }
    }
    qs.join("\n")
}

fn subagent_detail(in_obj: &JsonObj) -> String {
    let specs_owned = in_obj.objects("Subagents");
    let specs: Vec<&JsonObj> = if in_obj.list("Subagents").is_none() {
        vec![in_obj]
    } else {
        specs_owned.iter().collect()
    };
    let mut roles = Vec::new();
    for s in specs {
        let v = s.str(&["Role", "TypeName"]);
        if !v.is_empty() {
            roles.push(v);
        }
    }
    roles.join("\n")
}

fn message_detail(in_obj: &JsonObj) -> String {
    let mut parts = Vec::new();
    let r = in_obj.str(&["Recipient"]);
    if !r.is_empty() {
        parts.push(format!("→ {r}"));
    }
    let m = in_obj.str(&["Message"]);
    if !m.is_empty() {
        parts.push(m);
    }
    parts.join("\n")
}

// --- meta -------------------------------------------------------------------

#[derive(Serialize)]
struct ReadStats {
    lines: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<[i64; 2]>,
}

#[derive(Serialize)]
struct SizeStats {
    bytes: i64,
}

#[derive(Serialize)]
struct EditStats {
    added: usize,
    removed: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ranges: Vec<[i64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<RawJson>,
}

#[derive(Serialize)]
struct WriteStats {
    #[serde(skip_serializing_if = "String::is_empty")]
    kind: String,
    lines: usize,
}

#[derive(Serialize)]
struct BashStats {
    lines: usize,
    #[serde(skip_serializing_if = "is_false")]
    interrupted: bool,
}

#[derive(Default, PartialEq, Eq, Serialize)]
struct TaskStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ms: Option<i64>,
}

/// Go's `json.Marshal(v)`, HTML escaping and float formatting included.
fn marshal_raw<T: Serialize>(v: &T) -> Option<RawJson> {
    json::to_string(v, true)
        .ok()
        .and_then(|s| RawJson::from_json_string(s).ok())
}

/// Serializes a JSON value with all object keys sorted lexicographically,
/// matching Go's `json.Marshal` on `map[string]any`.
pub(crate) fn marshal_sorted_value(v: &Value) -> Option<RawJson> {
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Sorted<'a> {
        Null,
        Bool(bool),
        Number(&'a serde_json::Number),
        String(&'a str),
        Array(Vec<Sorted<'a>>),
        Object(BTreeMap<&'a str, Sorted<'a>>),
    }
    fn to_sorted(v: &Value) -> Sorted<'_> {
        match v {
            Value::Null => Sorted::Null,
            Value::Bool(b) => Sorted::Bool(*b),
            Value::Number(n) => Sorted::Number(n),
            Value::String(s) => Sorted::String(s),
            Value::Array(arr) => Sorted::Array(arr.iter().map(to_sorted).collect()),
            Value::Object(map) => {
                let mut b = BTreeMap::new();
                for (k, val) in map {
                    b.insert(k.as_str(), to_sorted(val));
                }
                Sorted::Object(b)
            }
        }
    }
    marshal_raw(&to_sorted(v))
}

fn read_meta(_in_obj: &JsonObj, out_obj: Option<&JsonObj>) -> Option<RawJson> {
    let f = out_obj?.obj("file")?;
    if let Some(n) = f.num("numLines") {
        let mut m = ReadStats {
            lines: n,
            range: None,
        };
        let start = f.num("startLine");
        let total = f.num("totalLines");
        if let Some(start) = start
            && n > 0
            && (start > 1 || total.is_some_and(|t| n < t))
        {
            m.range = Some([start, start + n - 1]);
        }
        return marshal_raw(&m);
    }
    if let Some(n) = f.num("originalSize") {
        return marshal_raw(&SizeStats { bytes: n });
    }
    None
}

fn edit_meta(in_obj: &JsonObj, out_obj: Option<&JsonObj>) -> Option<RawJson> {
    if let Some(out) = out_obj
        && let Some(Value::Array(p)) = out.get("structuredPatch")
    {
        let mut m = EditStats {
            added: 0,
            removed: 0,
            ranges: Vec::new(),
            patch: None,
        };
        for h in p {
            let hunk = match h {
                Value::Object(map) => JsonObj(map.clone()),
                _ => JsonObj::default(),
            };
            let (added, removed, ranges) = count_hunk(&hunk);
            m.added += added;
            m.removed += removed;
            m.ranges.extend(ranges);
        }
        m.patch = marshal_sorted_value(&Value::Array(p.clone()));
        return marshal_raw(&m);
    }
    let (added, removed) = count_replacements(in_obj)?;
    marshal_raw(&EditStats {
        added,
        removed,
        ranges: Vec::new(),
        patch: None,
    })
}

fn count_hunk(hunk: &JsonObj) -> (usize, usize, Vec<[i64; 2]>) {
    let mut line = hunk.num("newStart").filter(|&n| n != 0).unwrap_or(1);
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut ranges = Vec::new();
    let mut run: i64 = 0;
    if let Some(lines) = hunk.list("lines") {
        for l in lines {
            let s = l.as_str().unwrap_or("");
            if s.starts_with('+') {
                added += 1;
                if run == 0 {
                    run = line;
                }
                line += 1;
                continue;
            }
            if run != 0 {
                ranges.push([run, line - 1]);
                run = 0;
            }
            if s.starts_with('-') {
                removed += 1; // a removed line isn't in the new file
            } else {
                line += 1;
            }
        }
    }
    if run != 0 {
        ranges.push([run, line - 1]);
    }
    (added, removed, ranges)
}

fn count_replacements(in_obj: &JsonObj) -> Option<(usize, usize)> {
    let edits_owned = in_obj.objects("edits");
    let edits: Vec<&JsonObj> = if in_obj.list("edits").is_none() {
        vec![in_obj]
    } else {
        edits_owned.iter().collect()
    };
    if edits.is_empty() {
        return None;
    }
    let mut budget = MAX_DIFF_CELLS;
    let mut added = 0usize;
    let mut removed = 0usize;
    for e in edits {
        let mut old = e.get("old_string").and_then(Value::as_str);
        let mut new = e.get("new_string").and_then(Value::as_str);
        let mut all = e
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old.is_none() && new.is_none() {
            old = e.get("TargetContent").and_then(Value::as_str);
            new = e.get("ReplacementContent").and_then(Value::as_str);
            all = e
                .get("AllowMultiple")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        }
        let (Some(old), Some(new)) = (old, new) else {
            return None;
        };
        if all {
            return None;
        }
        let (a, r) = replaced_lines(old, new, &mut budget)?;
        added += a;
        removed += r;
    }
    Some((added, removed))
}

/// Bounds the work `count_replacements` does for one call: the sum, over its
/// edits, of the product of the two sides' line counts after their common ends
/// are dropped.
pub(crate) const MAX_DIFF_CELLS: usize = 1 << 22;

/// Counts the file lines that replacing the text `old` by `new` adds and
/// removes, taking both to be whole lines, as an edit's usually are: a
/// replacement within one line counts it changed (+1 -1). Spends the cells it
/// compares from `budget`, and returns `None` when they don't fit.
pub(crate) fn replaced_lines(old: &str, new: &str, budget: &mut usize) -> Option<(usize, usize)> {
    let (a_full, b_full) = line_ids(&split_lines(old), &split_lines(new));
    let mut a = a_full.as_slice();
    let mut b = b_full.as_slice();
    while !a.is_empty() && !b.is_empty() && a[0] == b[0] {
        a = &a[1..];
        b = &b[1..];
    }
    while !a.is_empty() && !b.is_empty() && a[a.len() - 1] == b[b.len() - 1] {
        a = &a[..a.len() - 1];
        b = &b[..b.len() - 1];
    }
    let cells = a.len().saturating_mul(b.len());
    if cells > *budget {
        return None;
    }
    *budget -= cells;
    let common = lcs_len(a, b);
    Some((b.len() - common, a.len() - common))
}

fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    s.strip_suffix('\n').unwrap_or(s).split('\n').collect()
}

fn line_ids<'a>(a: &[&'a str], b: &[&'a str]) -> (Vec<usize>, Vec<usize>) {
    let mut ids: HashMap<&'a str, usize> = HashMap::new();
    let mut number = |lines: &[&'a str]| -> Vec<usize> {
        let mut out = Vec::with_capacity(lines.len());
        for &l in lines {
            let next = ids.len();
            let id = *ids.entry(l).or_insert(next);
            out.push(id);
        }
        out
    };
    let na = number(a);
    let nb = number(b);
    (na, nb)
}

fn lcs_len(a: &[usize], b: &[usize]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for &ai in a {
        for (j, &bj) in b.iter().enumerate() {
            cur[j + 1] = if ai == bj {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn notebook_meta(in_obj: &JsonObj, out_obj: Option<&JsonObj>) -> Option<RawJson> {
    if let Some(out) = out_obj
        && out.get("structuredPatch").is_some()
    {
        return edit_meta(in_obj, out_obj);
    }
    let src = in_obj.get("new_source")?.as_str()?;
    if in_obj.str(&["edit_mode"]) != "insert" {
        return None;
    }
    marshal_raw(&EditStats {
        added: count_lines(src),
        removed: 0,
        ranges: Vec::new(),
        patch: None,
    })
}

fn replace_meta(_in_obj: &JsonObj, out_obj: Option<&JsonObj>) -> Option<RawJson> {
    let x = out_obj?.obj("replacement")?;
    let start = x.num("startLine").unwrap_or(0);
    let added = x.num("added").unwrap_or(0).max(0) as usize;
    let removed = x.num("removed").unwrap_or(0).max(0) as usize;
    let mut m = EditStats {
        added,
        removed,
        ranges: Vec::new(),
        patch: None,
    };
    if added > 0 {
        m.ranges.push([start, start + added as i64 - 1]);
    }
    marshal_raw(&m)
}

fn write_meta(in_obj: &JsonObj, out_obj: Option<&JsonObj>) -> Option<RawJson> {
    let content = out_obj
        .and_then(|o| o.get("content"))
        .and_then(Value::as_str)
        .or_else(|| in_obj.text(&["content", "CodeContent", "Content"]))?;
    let kind = out_obj
        .and_then(|o| o.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    marshal_raw(&WriteStats {
        kind,
        lines: count_lines(content),
    })
}

fn bash_meta(_in_obj: &JsonObj, out_obj: Option<&JsonObj>) -> Option<RawJson> {
    let out = out_obj?;
    let text = out.text(&["stdout", "output"])?;
    let interrupted = out
        .get("interrupted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    marshal_raw(&BashStats {
        lines: count_lines(text),
        interrupted,
    })
}

fn task_meta(_in_obj: &JsonObj, out_obj: Option<&JsonObj>) -> Option<RawJson> {
    let out = out_obj?;
    let m = TaskStats {
        tools: out.num("totalToolUseCount"),
        tokens: out.num("totalTokens"),
        ms: out.num("totalDurationMs"),
    };
    if m == TaskStats::default() {
        return None;
    }
    marshal_raw(&m)
}

// --- file text --------------------------------------------------------------

fn read_file(_in_obj: &JsonObj, out_obj: &JsonObj) -> Option<FileText> {
    let f = out_obj.obj("file")?;
    let text = f.text(&["content"])?.to_string();
    let start = f.num("startLine").unwrap_or(0).max(1) as usize;
    let total = f.num("totalLines").unwrap_or(0).max(0) as usize;
    Some(FileText { text, start, total })
}

fn written_file(_in_obj: &JsonObj, out_obj: &JsonObj) -> Option<FileText> {
    let text = out_obj.text(&["content"])?.to_string();
    let total = count_lines(&text);
    Some(FileText {
        text,
        start: 1,
        total,
    })
}

fn edited_file(in_obj: &JsonObj, out_obj: &JsonObj) -> Option<FileText> {
    let mut text = out_obj
        .text(&["originalFile", "originalFileContents"])?
        .to_string();
    struct Repl {
        old: String,
        new: String,
        all: bool,
    }
    let mut rs = Vec::new();
    if let Some(old) = out_obj.text(&["oldString"]) {
        let new = out_obj.text(&["newString"]).unwrap_or("");
        let all = out_obj.get("replaceAll").and_then(Value::as_bool) == Some(true);
        rs.push(Repl {
            old: old.to_string(),
            new: new.to_string(),
            all,
        });
    } else {
        let out_edits = out_obj.objects("edits");
        let in_edits = in_obj.objects("edits");
        let edits: Vec<&JsonObj> = if !out_edits.is_empty() {
            out_edits.iter().collect()
        } else if !in_edits.is_empty() {
            in_edits.iter().collect()
        } else {
            vec![in_obj]
        };
        for e in edits {
            let old = e.text(&["old_string"])?;
            let new = e.text(&["new_string"])?;
            let all = e.get("replace_all").and_then(Value::as_bool) == Some(true);
            rs.push(Repl {
                old: old.to_string(),
                new: new.to_string(),
                all,
            });
        }
    }
    for r in rs {
        if r.old.is_empty() {
            if !text.is_empty() {
                return None;
            }
            text = r.new;
        } else if !text.contains(&r.old) {
            return None;
        } else if r.all {
            text = text.replace(&r.old, &r.new);
        } else {
            text = text.replacen(&r.old, &r.new, 1);
        }
    }
    let total = count_lines(&text);
    Some(FileText {
        text,
        start: 1,
        total,
    })
}

/// Counts `s`'s lines; a final newline doesn't start another.
pub(crate) fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    s.strip_suffix('\n')
        .unwrap_or(s)
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

// --- input and result objects -----------------------------------------------

/// A JSON object: a tool's input, or its result's structured response.
#[derive(Debug, Clone, Default)]
pub(crate) struct JsonObj(pub(crate) Map<String, Value>);

pub(crate) fn parse_object(raw: &[u8]) -> Option<JsonObj> {
    if raw.is_empty() {
        return None;
    }
    match json::from_slice::<Value>(raw).ok()? {
        Value::Object(map) => Some(JsonObj(map)),
        _ => None,
    }
}

impl JsonObj {
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Returns the first of `keys` whose value is a string that isn't blank,
    /// trimmed, or `""`.
    pub(crate) fn str(&self, keys: &[&str]) -> String {
        for &k in keys {
            if let Some(v) = self.0.get(k).and_then(Value::as_str) {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
        String::new()
    }

    /// Returns the first of `keys` whose value is a string, whole, even `""`.
    pub(crate) fn text<'a>(&'a self, keys: &[&str]) -> Option<&'a str> {
        for &k in keys {
            if let Some(v) = self.0.get(k).and_then(Value::as_str) {
                return Some(v);
            }
        }
        None
    }

    /// Returns `key`'s value as an integer, if it's a number.
    pub(crate) fn num(&self, key: &str) -> Option<i64> {
        let v = self.0.get(key)?.as_number()?;
        v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
    }

    /// Returns `key`'s value if it's an object, or `None`.
    pub(crate) fn obj(&self, key: &str) -> Option<JsonObj> {
        match self.0.get(key)? {
            Value::Object(map) => Some(JsonObj(map.clone())),
            _ => None,
        }
    }

    /// Returns `key`'s value if it's an array, or `None`.
    pub(crate) fn list(&self, key: &str) -> Option<&[Value]> {
        self.0.get(key)?.as_array().map(Vec::as_slice)
    }

    /// Returns the objects in `key`'s array.
    pub(crate) fn objects(&self, key: &str) -> Vec<JsonObj> {
        let Some(arr) = self.list(key) else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|v| match v {
                Value::Object(map) => Some(JsonObj(map.clone())),
                _ => None,
            })
            .collect()
    }
}

//! What a session's transcript says about the tokens its model calls used.
//!
//! Each harness records them per model call, in its own shape:
//!
//! - Claude Code: `message.usage` on every `assistant` record. One model call
//!   is written as several records (one per content block) that repeat the
//!   same `message.id` and usage, so a call is counted once, by its id.
//! - agy: `input_tokens`, `cache_read_tokens` and `output_tokens` on each
//!   `PLANNER_RESPONSE` step of `transcript_full.jsonl`.
//! - Kimi Code: `usage.record` records in `wire.jsonl` (`inputTokens`,
//!   `outputTokens`).
//!
//! Counting means reading the whole transcript, which `agent sessions` never
//! does. So a [`Scan`] is resumable: it remembers the byte it read up to, and
//! [`Scan::read_file`] reads only what a transcript gained since. A caller
//! keeps the `Scan` (it serializes) beside the file's path.
//!
//! No harness records the model's context window, so [`Usage::context_tokens`]
//! is the size of the last call's prompt, not a share of anything.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use serde::{Deserialize, Serialize};

const HARNESS_CLAUDE: &str = "claude";
const HARNESS_AGY: &str = "agy";
const HARNESS_KIMI: &str = "kimi";

/// Token counts summed over model calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Prompt tokens that were neither read from nor written to a cache.
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub output: i64,
    #[serde(default)]
    pub cache_read: i64,
    /// Prompt tokens written to a cache (Claude only).
    #[serde(default)]
    pub cache_creation: i64,
    /// Model calls counted.
    #[serde(default)]
    pub calls: i64,
}

impl Tokens {
    /// Every token of every call: prompt (cached or not) and output.
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }

    /// The prompt of the calls: what the model was sent.
    pub fn prompt(&self) -> i64 {
        self.input + self.cache_read + self.cache_creation
    }

    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_creation += o.cache_creation;
        self.calls += o.calls;
    }

    fn sub(&mut self, o: &Tokens) {
        self.input -= o.input;
        self.output -= o.output;
        self.cache_read -= o.cache_read;
        self.cache_creation -= o.cache_creation;
        self.calls -= o.calls;
    }
}

/// A transcript's token usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(flatten)]
    pub tokens: Tokens,
    /// The prompt size of the session's latest model call: what its context
    /// holds now. 0 if it made none.
    #[serde(default)]
    pub context_tokens: i64,
    /// Times the harness compacted the conversation (Claude only).
    #[serde(default)]
    pub compactions: i64,
    /// The same counts per model, where the harness names the model of each
    /// call (Claude only).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_model: BTreeMap<String, Tokens>,
}

impl Usage {
    /// Adds another transcript's usage to this one's totals (a subagent's to
    /// its parent's, say). `context_tokens` stays this one's.
    pub fn add_totals(&mut self, o: &Usage) {
        self.tokens.add(&o.tokens);
        self.compactions += o.compactions;
        for (m, t) in &o.by_model {
            self.by_model.entry(m.clone()).or_default().add(t);
        }
    }
}

/// A resumable count of one transcript's usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scan {
    #[serde(default)]
    pub harness: String,
    /// The byte the scan has read up to: the end of its last whole line.
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub usage: Usage,
    /// The Claude model call counted last, so that the records repeating it
    /// replace its counts instead of adding to them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    last_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    last_model: String,
    #[serde(default)]
    last: Tokens,
}

impl Scan {
    pub fn new(harness: &str) -> Scan {
        Scan {
            harness: harness.to_string(),
            ..Scan::default()
        }
    }

    /// Reads what the transcript at `path` holds past `self.offset`, whole
    /// lines only (a line still being written is left for the next read).
    /// A file shorter than the offset was rewritten: it is counted again from
    /// its start. Returns whether anything was read.
    pub fn read_file(&mut self, path: &str) -> std::io::Result<bool> {
        let mut f = std::fs::File::open(path)?;
        let size = f.metadata()?.len();
        if size < self.offset {
            *self = Scan::new(&self.harness);
        }
        if size == self.offset {
            return Ok(false);
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut r = BufReader::with_capacity(1 << 16, f);
        let mut line = Vec::new();
        let mut any = false;
        loop {
            line.clear();
            let n = r.read_until(b'\n', &mut line)?;
            if n == 0 || line.last() != Some(&b'\n') {
                break;
            }
            self.offset += n as u64;
            self.line(&line);
            any = true;
        }
        Ok(any)
    }

    /// Counts one transcript line.
    pub fn line(&mut self, line: &[u8]) {
        match self.harness.as_str() {
            HARNESS_CLAUDE => self.claude(line),
            HARNESS_AGY => self.agy(line),
            HARNESS_KIMI => self.kimi(line),
            _ => {}
        }
    }

    fn claude(&mut self, line: &[u8]) {
        let has_usage = contains(line, b"\"usage\"");
        if !has_usage && !contains(line, b"\"compact_boundary\"") {
            return;
        }
        #[derive(Deserialize)]
        struct Record {
            #[serde(rename = "type", default)]
            r#type: String,
            #[serde(default)]
            subtype: String,
            #[serde(rename = "isSidechain", default)]
            is_sidechain: bool,
            #[serde(rename = "agentId", default)]
            agent_id: String,
            #[serde(default)]
            message: Option<Message>,
        }
        #[derive(Deserialize)]
        struct Message {
            #[serde(default)]
            id: String,
            #[serde(default)]
            model: String,
            #[serde(default)]
            usage: Option<ClaudeUsage>,
        }
        #[derive(Deserialize)]
        struct ClaudeUsage {
            #[serde(default)]
            input_tokens: Option<i64>,
            #[serde(default)]
            output_tokens: Option<i64>,
            #[serde(default)]
            cache_read_input_tokens: Option<i64>,
            #[serde(default)]
            cache_creation_input_tokens: Option<i64>,
        }
        let Ok(r) = serde_json::from_slice::<Record>(line) else {
            return;
        };
        if r.r#type == "system" {
            if r.subtype == "compact_boundary" {
                self.usage.compactions += 1;
            }
            return;
        }
        if r.r#type != "assistant" {
            return;
        }
        let Some(m) = r.message else { return };
        let Some(u) = m.usage else { return };
        if m.model == "<synthetic>" {
            return;
        }
        let t = Tokens {
            input: u.input_tokens.unwrap_or(0),
            output: u.output_tokens.unwrap_or(0),
            cache_read: u.cache_read_input_tokens.unwrap_or(0),
            cache_creation: u.cache_creation_input_tokens.unwrap_or(0),
            calls: 1,
        };
        if !m.id.is_empty() && m.id == self.last_id {
            // The same call, written again with another content block: its
            // newest counts stand.
            let (last, model) = (self.last, self.last_model.clone());
            self.usage.tokens.sub(&last);
            if let Some(b) = self.usage.by_model.get_mut(&model) {
                b.sub(&last);
            }
        }
        self.usage.tokens.add(&t);
        if !m.model.is_empty() {
            self.usage
                .by_model
                .entry(m.model.clone())
                .or_default()
                .add(&t);
        }
        self.last_id = m.id;
        self.last_model = m.model;
        self.last = t;
        // A sidechain record inside a session's own transcript is a subagent's
        // call (older Claude Code wrote them inline): its tokens count, but
        // the session's context is its own last call's.
        if !r.is_sidechain || !r.agent_id.is_empty() {
            self.usage.context_tokens = t.prompt();
        }
    }

    fn agy(&mut self, line: &[u8]) {
        if !contains(line, b"PLANNER_RESPONSE") {
            return;
        }
        #[derive(Deserialize)]
        struct Step {
            #[serde(rename = "type", default)]
            r#type: String,
            #[serde(default)]
            input_tokens: Option<i64>,
            #[serde(default)]
            cache_read_tokens: Option<i64>,
            #[serde(default)]
            output_tokens: Option<i64>,
        }
        let Ok(s) = serde_json::from_slice::<Step>(line) else {
            return;
        };
        if s.r#type != "PLANNER_RESPONSE" || (s.input_tokens.is_none() && s.output_tokens.is_none())
        {
            return;
        }
        let t = Tokens {
            input: s.input_tokens.unwrap_or(0),
            output: s.output_tokens.unwrap_or(0),
            cache_read: s.cache_read_tokens.unwrap_or(0),
            cache_creation: 0,
            calls: 1,
        };
        self.usage.tokens.add(&t);
        self.usage.context_tokens = t.prompt();
    }

    fn kimi(&mut self, line: &[u8]) {
        if !contains(line, b"\"usage.record\"") {
            return;
        }
        #[derive(Deserialize)]
        struct Rec {
            #[serde(rename = "type", default)]
            r#type: String,
            #[serde(rename = "inputTokens", default)]
            input: Option<i64>,
            #[serde(rename = "outputTokens", default)]
            output: Option<i64>,
        }
        let Ok(r) = serde_json::from_slice::<Rec>(line) else {
            return;
        };
        if r.r#type != "usage.record" {
            return;
        }
        let t = Tokens {
            input: r.input.unwrap_or(0),
            output: r.output.unwrap_or(0),
            cache_read: 0,
            cache_creation: 0,
            calls: 1,
        };
        self.usage.tokens.add(&t);
        self.usage.context_tokens = t.prompt();
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

/// Counts the whole transcript at `path`.
pub fn read_usage(harness: &str, path: &str) -> std::io::Result<Usage> {
    let mut s = Scan::new(harness);
    s.read_file(path)?;
    Ok(s.usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn claude_line(id: &str, model: &str, u: [i64; 4]) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{},"output_tokens":{}}}}}}}"#,
            u[0], u[1], u[2], u[3]
        ) + "\n"
    }

    #[test]
    fn claude_counts_a_call_once_and_keeps_its_newest_counts() {
        let mut s = Scan::new(HARNESS_CLAUDE);
        s.line(claude_line("m1", "a", [2, 100, 50, 5]).as_bytes());
        s.line(claude_line("m1", "a", [2, 100, 50, 30]).as_bytes());
        s.line(claude_line("m2", "b", [3, 10, 152, 7]).as_bytes());
        s.line(br#"{"type":"system","subtype":"compact_boundary"}"#);
        s.line(br#"{"type":"assistant","message":{"id":"x","model":"<synthetic>","usage":{"input_tokens":9,"output_tokens":9}}}"#);
        s.line(b"not json \"usage\"");
        let u = &s.usage;
        assert_eq!(
            u.tokens,
            Tokens {
                input: 5,
                output: 37,
                cache_read: 202,
                cache_creation: 110,
                calls: 2
            }
        );
        assert_eq!(u.context_tokens, 165);
        assert_eq!(u.compactions, 1);
        assert_eq!(u.by_model["a"].output, 30);
        assert_eq!(u.by_model["b"].calls, 1);
    }

    #[test]
    fn inline_sidechain_calls_count_but_leave_the_context() {
        let mut s = Scan::new(HARNESS_CLAUDE);
        s.line(claude_line("m1", "a", [1, 0, 99, 1]).as_bytes());
        s.line(br#"{"type":"assistant","isSidechain":true,"message":{"id":"s1","model":"a","usage":{"input_tokens":7,"output_tokens":2}}}"#);
        assert_eq!(s.usage.tokens.calls, 2);
        assert_eq!(s.usage.context_tokens, 100);
    }

    #[test]
    fn agy_and_kimi() {
        let mut s = Scan::new(HARNESS_AGY);
        s.line(br#"{"type":"PLANNER_RESPONSE","input_tokens":11873,"cache_read_tokens":0,"output_tokens":402}"#);
        s.line(br#"{"type":"PLANNER_RESPONSE","input_tokens":2972,"cache_read_tokens":17069,"output_tokens":120}"#);
        s.line(br#"{"type":"USER_INPUT","content":"PLANNER_RESPONSE"}"#);
        assert_eq!(s.usage.tokens.total(), 11873 + 402 + 2972 + 17069 + 120);
        assert_eq!(s.usage.context_tokens, 2972 + 17069);

        let mut s = Scan::new(HARNESS_KIMI);
        s.line(br#"{"type":"usage.record","time":1,"inputTokens":5120,"outputTokens":233}"#);
        assert_eq!(
            s.usage.tokens,
            Tokens {
                input: 5120,
                output: 233,
                calls: 1,
                ..Tokens::default()
            }
        );
        assert_eq!(s.usage.context_tokens, 5120);
    }

    #[test]
    fn a_scan_resumes_and_survives_serializing_mid_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let p = path.to_str().unwrap();
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(claude_line("m1", "a", [1, 0, 0, 5]).as_bytes())
            .unwrap();
        // Half a line: not counted until its newline arrives.
        let second = claude_line("m1", "a", [1, 0, 0, 9]);
        f.write_all(&second.as_bytes()[..20]).unwrap();
        f.flush().unwrap();

        let mut s = Scan::new(HARNESS_CLAUDE);
        assert!(s.read_file(p).unwrap());
        assert_eq!(s.usage.tokens.output, 5);
        assert!(!s.read_file(p).unwrap());

        let mut s: Scan = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        f.write_all(&second.as_bytes()[20..]).unwrap();
        f.flush().unwrap();
        assert!(s.read_file(p).unwrap());
        assert_eq!(s.usage.tokens.output, 9);
        assert_eq!(s.usage.tokens.calls, 1);
        assert_eq!(read_usage(HARNESS_CLAUDE, p).unwrap(), s.usage);

        // A rewritten, shorter file is counted from its start.
        std::fs::write(&path, claude_line("z", "a", [4, 0, 0, 1])).unwrap();
        assert!(s.read_file(p).unwrap());
        assert_eq!(
            s.usage.tokens,
            Tokens {
                input: 4,
                output: 1,
                calls: 1,
                ..Tokens::default()
            }
        );
    }

    #[test]
    fn the_corpus_has_usage_for_every_harness() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/corpus");
        let u = read_usage(HARNESS_CLAUDE, &format!("{root}/claude/basic-turn.jsonl")).unwrap();
        assert!(u.tokens.calls > 0 && u.tokens.total() > 0, "{u:?}");
        let mut found = [false; 2];
        for e in walk(std::path::Path::new(root)) {
            let p = e.to_str().unwrap().to_string();
            if p.ends_with("transcript_full.jsonl") {
                found[0] |= read_usage(HARNESS_AGY, &p).unwrap().tokens.calls > 0;
            } else if p.ends_with("wire.jsonl") {
                found[1] |= read_usage(HARNESS_KIMI, &p).unwrap().tokens.calls > 0;
            }
        }
        assert_eq!(found, [true, true]);
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }
}

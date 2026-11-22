//! The evidence a dream's digest carries (docs/DREAMING.md, "Evidence"): the
//! host's side. It reads the failed `agent` commands run in the dream scope
//! from the store's command log, and the failed tool calls and the messages
//! into the batch's sessions from their transcripts, since the window's start;
//! `agent_dream::evidence` groups and ranks them.

use std::io::Read as _;

use agent_dream::evidence::{self, Evidence, FailedCommand, FailedTool, Prompt};
use agent_dream::gather::{self, Gathered, Transcript};
use agent_store::CommandFilter;
use agent_transcript::{
    Event, KIND_PROMPT, KIND_TOOL, OP_UPDATE, ORIGIN_DELIVERED, ORIGIN_HUMAN, ORIGIN_QUEUED,
    STATUS_ERROR,
};
use chrono::{DateTime, Utc};

use super::dream::read_scope;
use super::root::Ctx;

/// The most bytes read of one transcript: a longer one is read from its start
/// up to here.
const MAX_TRANSCRIPT_BYTES: u64 = 256 << 20;

/// The evidence for the batch g gathered.
pub(crate) fn evidence(ctx: &Ctx, g: &Gathered) -> anyhow::Result<Evidence> {
    let since = g.since.unwrap_or_else(|| Utc::now() - gather::DUE_AFTER);
    let scope = read_scope(ctx)?;
    let rows = ctx.store()?.list_commands(&CommandFilter {
        failed: true,
        since: since.into(),
        ..Default::default()
    })?;
    let cmds: Vec<FailedCommand> = rows
        .into_iter()
        .filter(|r| gather::in_scope(&r.cwd, &scope))
        .map(|r| FailedCommand {
            agent: r.agent,
            args: r.args,
            exit_code: r.exit_code,
            error: r.error,
            at: r.started_at.0.with_timezone(&Utc),
        })
        .collect();
    let tree = super::dream_memory::agent_commands(super::root::tree());
    let (commands, commands_failed) = evidence::group_commands(&cmds, Some(&tree));

    let mut tools = Vec::new();
    let mut prompts = Vec::new();
    let mut read = std::collections::BTreeSet::new();
    for t in &g.transcripts {
        if read_transcript(t, since, &mut tools, &mut prompts).is_ok() {
            read.insert(t.session.clone());
        }
    }
    let (tool_groups, tools_failed) = evidence::group_tools(&tools);
    Ok(Evidence {
        since: Some(since),
        commands_failed,
        commands,
        sessions_read: read.len(),
        tools_failed,
        tools: tool_groups,
        corrections: evidence::corrections(&prompts),
    })
}

/// The shell command a tool call ran: its input's `command` (Claude's Bash),
/// `CommandLine` (agy) or `cmd`; "" for any other tool.
fn shell_command(e: &Event) -> String {
    let Some(input) = e.tool.as_ref().and_then(|t| t.input.as_ref()) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input.as_str()) else {
        return String::new();
    };
    ["command", "CommandLine", "cmd"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|c| c.as_str()))
        .unwrap_or("")
        .to_string()
}

/// Reads one transcript, from its start (what a line means depends on the
/// lines before it), keeping the failed tool calls and the messages from a
/// person or a peer at or after since.
fn read_transcript(
    t: &Transcript,
    since: DateTime<Utc>,
    tools: &mut Vec<FailedTool>,
    prompts: &mut Vec<Prompt>,
) -> anyhow::Result<()> {
    let f = std::fs::File::open(&t.path)?;
    let mut f = f.take(MAX_TRANSCRIPT_BYTES);
    let mut buf = vec![0u8; 1 << 20];
    let mut pending: Vec<u8> = Vec::new();
    let mut parser = None;
    loop {
        let n = f.read(&mut buf)?;
        pending.extend_from_slice(&buf[..n]);
        let p = parser.get_or_insert_with(|| {
            let reader = agent_transcript::detect(&pending);
            reader.new_parser(agent_transcript::file_options(reader.harness(), &t.path))
        });
        let start = p.pos().at;
        for e in p.feed(&pending) {
            keep(&e, t, since, tools, prompts);
        }
        let consumed = (p.pos().at - start) as usize;
        pending.drain(..consumed);
        if n == 0 {
            return Ok(());
        }
    }
}

fn keep(
    e: &Event,
    t: &Transcript,
    since: DateTime<Utc>,
    tools: &mut Vec<FailedTool>,
    prompts: &mut Vec<Prompt>,
) {
    let Some(at) = e.time().map(|t| t.with_timezone(&Utc)) else {
        return;
    };
    if at < since {
        return;
    }
    match e.kind.as_str() {
        KIND_TOOL if e.op == OP_UPDATE => {
            let Some(tool) = e.tool.as_ref() else { return };
            let Some(result) = tool.result.as_ref() else {
                return;
            };
            if result.status != STATUS_ERROR {
                return;
            }
            tools.push(FailedTool {
                session: t.session.clone(),
                tool: tool.name.clone(),
                command: shell_command(e),
                error: result.output.clone(),
                at,
            });
        }
        KIND_PROMPT => {
            let Some(p) = e.prompt.as_ref() else { return };
            if !matches!(
                p.origin.as_str(),
                ORIGIN_HUMAN | ORIGIN_QUEUED | ORIGIN_DELIVERED
            ) {
                return;
            }
            prompts.push(Prompt {
                session: t.session.clone(),
                text: p.text.clone(),
                at,
            });
        }
        _ => {}
    }
}

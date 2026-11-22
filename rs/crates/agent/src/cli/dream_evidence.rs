//! The evidence a dream's digest carries (docs/DREAMING.md, "Evidence"): the
//! host's side. It reads the failed `agent` commands run in the dream scope
//! from the store's command log, and the failed tool calls and the messages
//! into the batch's sessions from their transcripts, since the window's start;
//! `agent_dream::evidence` groups and ranks them.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

use agent_dream::evidence::{self, Evidence, FailedCommand, FailedTool, Prompt};
use agent_dream::gather::{self, Gathered, Transcript};
use agent_store::CommandFilter;
use agent_transcript::{
    Event, KIND_PROMPT, KIND_TOOL, OP_UPDATE, ORIGIN_DELIVERED, ORIGIN_HUMAN, ORIGIN_QUEUED,
    STATUS_ERROR,
};
use chrono::{DateTime, Utc};

use super::dream::read_worktrees;
use super::root::Ctx;

/// The most bytes read of one transcript: a longer one is read from its start
/// up to here.
const MAX_TRANSCRIPT_BYTES: u64 = 256 << 20;

/// The scope dir `cwd` is in: the deepest one it is in, else the repo
/// whose worktree prefix it starts with (`agent dream scope add
/// --worktrees`), if that repo is in scope; "" if none.
fn project_of_cwd(
    cwd: &str,
    scope: &[String],
    worktrees: &std::collections::BTreeMap<String, String>,
) -> String {
    if let Some(d) = scope
        .iter()
        .filter(|d| gather::in_scope(cwd, std::slice::from_ref(d)))
        .max_by_key(|d| d.len())
    {
        return d.clone();
    }
    worktrees
        .iter()
        .find(|(prefix, repo)| {
            !cwd.is_empty() && cwd.starts_with(prefix.as_str()) && scope.contains(repo)
        })
        .map(|(_, repo)| repo.clone())
        .unwrap_or_default()
}

/// The evidence for the batch g gathered over `scope`. Pooled (the global
/// dream's, over several projects), each group names the projects it was
/// seen in.
pub(crate) fn evidence(
    ctx: &Ctx,
    g: &Gathered,
    scope: &[String],
    pooled: bool,
) -> anyhow::Result<Evidence> {
    let since = g.since.unwrap_or_else(|| Utc::now() - gather::DUE_AFTER);
    let worktrees = read_worktrees(ctx)?;
    let rows = ctx.store()?.list_commands(&CommandFilter {
        failed: true,
        since: since.into(),
        ..Default::default()
    })?;
    let mut projects_of: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut cmds: Vec<FailedCommand> = Vec::new();
    for r in rows {
        let project = project_of_cwd(&r.cwd, scope, &worktrees);
        if project.is_empty() {
            continue;
        }
        let source = if r.agent.is_empty() {
            "(person)".to_string()
        } else {
            r.agent.clone()
        };
        projects_of.entry(source).or_default().insert(project);
        cmds.push(FailedCommand {
            agent: r.agent,
            args: r.args,
            exit_code: r.exit_code,
            error: r.error,
            at: r.started_at.0.with_timezone(&Utc),
        });
    }
    let tree = super::dream_memory::agent_commands(super::root::tree());
    let (mut commands, commands_failed) = evidence::group_commands(&cmds, Some(&tree));

    let mut tools = Vec::new();
    let mut prompts = Vec::new();
    let mut read = BTreeSet::new();
    let mut session_projects: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in &g.transcripts {
        session_projects
            .entry(t.session.clone())
            .or_default()
            .insert(t.project.clone());
        if read_transcript(t, since, &mut tools, &mut prompts).is_ok() {
            read.insert(t.session.clone());
        }
    }
    let (mut tool_groups, tools_failed) = evidence::group_tools(&tools);
    if pooled {
        evidence::set_projects(&mut commands, &projects_of);
        evidence::set_projects(&mut tool_groups, &session_projects);
    }
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

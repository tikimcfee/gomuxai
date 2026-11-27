//! `agent dream propose` (a dream's agent) and `agent dream proposals` (a
//! person): changes a dream may only propose, for now to the skip list
//! (docs/DREAMING.md, "Scope").
use agent_store::{PROPOSAL_ACCEPTED, PROPOSAL_REJECTED, PROPOSAL_SKIP, Proposal};

use super::command::{ArgRule, Def, Doc};
use super::dream::{ANY_ROLE, own_worker};
use super::root::Ctx;

// `agent dream propose`

pub const PROPOSE: Doc = Doc {
    use_line: "propose skip add|remove <name>",
    short: "Propose a change only a person may make (a dream's agent)",
    long: r#"Propose records a change a dream may not make itself, for a person to
accept or reject (agent dream proposals). For now that is the skip list,
the directory names no walk of a project goes into (agent index skip
list): propose skip add NAME when build output or a cache was indexed or
gathered (a session's digest full of it, index search returning it), and
skip remove NAME when a name skips what holds real work.

--why says what you saw, in a sentence; --source cites it (a session,
commit, friction entry or dream, as memory does), at least once. The
proposal waits whatever becomes of the dream; the same change pending
already is not recorded twice. Only a dream's own agent runs it."#,
    example: r#"  agent dream propose skip add .next --why "the index lists glyph3d-js's .next build output" --source claude:3f3a9c"#,
    args: ArgRule::Exact(3),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ProposeArgs {
    #[arg(
        long = "why",
        value_name = "string",
        default_value = "",
        help = "What you saw, in a sentence"
    )]
    pub why: String,
    #[arg(
        long = "source",
        value_name = "stringArray",
        help = "A source the proposal rests on (claude:<id>, commit:<hash>, friction:<uid>, dream:<id>); repeatable"
    )]
    pub source: Vec<String>,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn propose_def() -> Def {
    leaf!(PROPOSE, ProposeArgs, run_propose)
}

pub fn run_propose(ctx: &Ctx, args: &ProposeArgs) -> anyhow::Result<()> {
    let (d, w) = own_worker(ctx, "propose", ANY_ROLE)?;
    let (kind, op, name) = (&args.args[0], &args.args[1], &args.args[2]);
    if kind != PROPOSAL_SKIP {
        anyhow::bail!(
            "a dream proposes changes to the skip list (propose skip add|remove NAME), not {kind:?}"
        );
    }
    if op != "add" && op != "remove" {
        anyhow::bail!("propose skip takes add or remove, not {op:?}");
    }
    agent_dream::skip::check_name(name)?;
    let why = args.why.trim();
    if why.is_empty() {
        anyhow::bail!("--why says what you saw: a person decides on it");
    }
    if args.source.is_empty() {
        anyhow::bail!("--source cites what the proposal rests on, at least once");
    }
    for s in &args.source {
        if !s.split_once(':').is_some_and(|(k, v)| {
            !k.is_empty() && !v.is_empty() && !v.contains(char::is_whitespace)
        }) {
            anyhow::bail!(
                "--source {s:?}: cite a source as kind:id (claude:<session>, commit:<hash>, friction:<uid>, dream:<id>)"
            );
        }
    }
    let in_list = agent_dream::skip::dirs().iter().any(|n| n == name);
    if (op == "add") == in_list {
        anyhow::bail!(
            "the skip list {} {name} already (agent index skip list)",
            if in_list { "has" } else { "leaves out" }
        );
    }
    let change = format!("{op} {name}");
    let (id, new) =
        ctx.store()?
            .add_proposal(d.id, &w.agent, PROPOSAL_SKIP, &change, why, &args.source)?;
    if !new {
        return ctx.write_out(&format!(
            "Proposal #{id} (skip {change}) is pending already; a person decides on it.\n"
        ));
    }
    ctx.store()?.append_dream_diary(
        d.id,
        &format!(
            "{} proposed #{id}: skip {change} ({why})",
            super::dream::stamp_now()
        ),
    )?;
    ctx.write_out(&format!(
        "Proposed #{id}: skip {change}. A person accepts or rejects it (agent dream proposals).\n"
    ))
}

// `agent dream proposals`

pub const PROPOSALS: Doc = Doc {
    use_line: "proposals",
    short: "List, accept and reject what dreams proposed (a person)",
    long: r#"Proposals are changes a dream may not make itself and proposed instead,
for now to the skip list (agent index skip). list shows the pending ones
(--all every one), with what was seen and its sources; accept makes the
change (and tells a running index watcher) and records who accepted it;
reject records the no and why. Accepting and rejecting are a person's:
an agent's run of either prompts its person, and a dream's agent is
refused."#,
    example: r#"  agent dream proposals list
  agent dream proposals accept 3
  agent dream proposals reject 4 --note "build/ holds sources in that repo""#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn proposals_def() -> Def {
    Def::group(&PROPOSALS, vec![list_def(), accept_def(), reject_def()])
}

pub const LIST: Doc = Doc {
    use_line: "list",
    short: "List the pending proposals (--all: every one)",
    long: "List prints each pending proposal: its id, the change, the dream and worker that proposed it, why, and its sources. --all includes those decided.",
    example: "  agent dream proposals list\n  agent dream proposals list --all --json",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ListArgs {
    #[arg(long = "all", help = "Every proposal, decided ones too")]
    pub all: bool,
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn list_def() -> Def {
    leaf!(LIST, ListArgs, run_list)
}

fn run_list(ctx: &Ctx, args: &ListArgs) -> anyhow::Result<()> {
    let ps = ctx.store()?.proposals(args.all)?;
    if args.json {
        return ctx.print_json(&ps);
    }
    if ps.is_empty() {
        return ctx.write_out(if args.all {
            "No dream has proposed anything.\n"
        } else {
            "Nothing waits for a decision (--all shows decided proposals).\n"
        });
    }
    let mut out = String::new();
    for p in &ps {
        out += &format_proposal(p);
    }
    ctx.write_out(&out)
}

fn format_proposal(p: &Proposal) -> String {
    let mut s = format!(
        "#{}  {} {}  {}  (dream #{}, {})\n    why: {}\n    sources: {}\n",
        p.id,
        p.kind,
        p.change,
        p.status,
        p.dream_id,
        p.worker,
        p.why,
        p.citations.join(", ")
    );
    if !p.decided_by.is_empty() {
        s += &format!("    {} by {}", p.status, p.decided_by);
        if !p.note.is_empty() {
            s += &format!(": {}", p.note);
        }
        s.push('\n');
    }
    s
}

pub const ACCEPT: Doc = Doc {
    use_line: "accept <id>",
    short: "Accept a proposal: make the change (a person)",
    long: "Accept makes proposal <id>'s change (a skip list add or remove, which tells a running index watcher) and records it accepted, with --note if given.",
    example: "  agent dream proposals accept 3",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct DecideArgs {
    #[arg(
        long = "note",
        value_name = "string",
        default_value = "",
        help = "Why, in a sentence"
    )]
    pub note: String,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn accept_def() -> Def {
    leaf!(ACCEPT, DecideArgs, run_accept)
}

pub const REJECT: Doc = Doc {
    use_line: "reject <id>",
    short: "Reject a proposal (a person)",
    long: "Reject records proposal <id> rejected, with --note saying why, and changes nothing.",
    example: "  agent dream proposals reject 4 --note \"build/ holds sources in that repo\"",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

fn reject_def() -> Def {
    leaf!(REJECT, DecideArgs, run_reject)
}

/// The pending proposal an id names, for a caller that isn't a dream's.
fn pending(ctx: &Ctx, id: &str, verb: &str) -> anyhow::Result<Proposal> {
    if super::logs::in_dream(ctx) {
        anyhow::bail!(
            "a dream proposes; a person decides: agent dream proposals {verb} is refused to a dream's agent"
        );
    }
    let n: i64 = id.trim_start_matches('#').parse().map_err(|_| {
        anyhow::anyhow!("{id:?} isn't a proposal id (agent dream proposals list shows them)")
    })?;
    let p = ctx.store()?.get_proposal(n)?.ok_or_else(|| {
        anyhow::anyhow!("no proposal #{n} (agent dream proposals list --all shows them)")
    })?;
    if p.status != agent_store::PROPOSAL_PENDING {
        anyhow::bail!("proposal #{n} is {} already", p.status);
    }
    Ok(p)
}

/// Who decides: the agent running it, or a person.
fn decider(ctx: &Ctx) -> String {
    let a = ctx.caller_agent();
    if a.is_empty() {
        "a person".to_string()
    } else {
        a
    }
}

fn run_accept(ctx: &Ctx, args: &DecideArgs) -> anyhow::Result<()> {
    let p = pending(ctx, &args.args[0], "accept")?;
    match (p.kind.as_str(), p.change.split_once(' ')) {
        (PROPOSAL_SKIP, Some(("add", name))) => super::index::skip_edit(ctx, name, true)?,
        (PROPOSAL_SKIP, Some(("remove", name))) => super::index::skip_edit(ctx, name, false)?,
        _ => anyhow::bail!(
            "proposal #{} is a change this agent can't make ({} {})",
            p.id,
            p.kind,
            p.change
        ),
    }
    ctx.store()?
        .decide_proposal(p.id, PROPOSAL_ACCEPTED, &decider(ctx), args.note.trim())?;
    ctx.write_out(&format!("Accepted #{}: skip {}.\n", p.id, p.change))
}

fn run_reject(ctx: &Ctx, args: &DecideArgs) -> anyhow::Result<()> {
    let p = pending(ctx, &args.args[0], "reject")?;
    if args.note.trim().is_empty() {
        anyhow::bail!("--note says why: the next dream reads it");
    }
    ctx.store()?
        .decide_proposal(p.id, PROPOSAL_REJECTED, &decider(ctx), args.note.trim())?;
    ctx.write_out(&format!("Rejected #{}: skip {}.\n", p.id, p.change))
}

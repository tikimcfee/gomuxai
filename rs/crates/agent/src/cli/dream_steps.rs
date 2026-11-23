//! The global dream as two agents and code (docs/DREAMING.md, "The global
//! dream: a synthesizer, a critic, and code"): a generator run of the idea
//! engine (the synthesizer) proposes a change set with `agent dream idea`, a
//! critic on the other model judges each idea with `agent dream judge`, and
//! code applies the approved ones and commits. Each step runs when a worker
//! says it is done (`agent dream done`) or when `agent dream run` finds one
//! due, from the dream's state in the store: nothing long-running holds it,
//! so a crash between steps is picked up by the next run.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_dream::ideas::{
    self, OP_ACCEPT, OP_DUP, OP_LINK, OP_PUT, OP_REMOVE, Synthesis, SynthesisIn,
};
use agent_dream::{gather, memory};
use agent_store::{
    CALLER_SYNTHESIS, Dream, DreamIdea, DreamWorker, IDEA_APPLIED, IDEA_APPROVED, IDEA_PROPOSED,
    IDEA_REJECTED, IDEA_SKIPPED, IdeaVote, ROLE_CRITIC, ROLE_GENERATOR, Scores, Verdict,
    WORKER_DONE, WORKER_FAILED, WORKER_RUNNING, gojson,
};
use anyhow::Context as _;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::dream::{
    ANY_ROLE, Known, agent_alive, describe_worker, memory_problems, own_worker, record_memory,
    restore_memory, spawn_worker, stamp_now, triage_accept, triage_dup, triage_link,
};
use super::dream_memory::snapshot_dir_for;
use super::flags;
use super::friction::FrictionAcceptArgs;
use super::friction_links::FrictionLinkArgs;
use super::root::Ctx;

/// The models the global dream's two agents run on, each at its effort: the
/// generator gets one at random and the critic the other, so a critic never
/// judges its own family's sample twice over. ("" is the CLI's default
/// effort: Fable's highest isn't known to the roster yet.)
pub(crate) const MODELS: [(&str, &str); 2] =
    [("claude-opus-5-5", "xhigh"), ("claude-fable-5-2", "")];

/// The global dream's synthesizer.
pub(crate) fn generator_name(id: i64) -> String {
    format!("dream-{id}-synth")
}

/// The global dream's critic.
pub(crate) fn critic_name(id: i64) -> String {
    format!("dream-{id}-critic")
}

/// Something that varies from run to run, for the model pick.
pub(crate) fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    nanos ^ u64::from(std::process::id())
}

/// Dream `id`'s generator, on the model `seed` picks.
pub(crate) fn generator_worker(id: i64, seed: u64) -> DreamWorker {
    let (model, effort) = MODELS[(seed % 2) as usize];
    DreamWorker::new(&generator_name(id), ROLE_GENERATOR, model, effort)
}

/// Dream `id`'s critic, on the model the generator didn't run on.
pub(crate) fn critic_worker(id: i64, generator: &DreamWorker) -> DreamWorker {
    let (model, effort) = MODELS
        .iter()
        .copied()
        .find(|(m, _)| *m != generator.model)
        .unwrap_or(MODELS[0]);
    DreamWorker::new(&critic_name(id), ROLE_CRITIC, model, effort)
}

/// Whether a dream runs as steps (a generator and a critic) rather than as
/// one dreamer.
pub(crate) fn is_staged(workers: &[DreamWorker]) -> bool {
    workers
        .iter()
        .any(|w| w.role == ROLE_GENERATOR || w.role == ROLE_CRITIC)
}

/// What a staged dream does next, from its workers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// A worker is still at work.
    Wait(String),
    /// The generator is done with ideas to judge: start the critic.
    Critic,
    /// Every worker is done: apply the approved ideas and commit.
    Apply,
    /// A worker went away or failed: abandon, marking it failed (the agent
    /// named, if any).
    Abandon { why: String, failed: Option<String> },
}

/// The next step of a staged dream with `workers` and `ideas` ideas, where
/// `alive(agent)` says whether a running worker's agent is still there.
pub(crate) fn next_step(
    workers: &[DreamWorker],
    ideas: usize,
    alive: &dyn Fn(&str) -> bool,
) -> Step {
    // The first running worker decides: steps run one worker at a time.
    if let Some(w) = workers.iter().find(|w| w.status == WORKER_RUNNING) {
        if alive(&w.agent) {
            return Step::Wait(w.agent.clone());
        }
        return Step::Abandon {
            why: format!("its {} {} is gone", w.role, w.agent),
            failed: Some(w.agent.clone()),
        };
    }
    if let Some(w) = workers.iter().find(|w| w.status == WORKER_FAILED) {
        return Step::Abandon {
            why: format!("its {} {} failed", w.role, w.agent),
            failed: None,
        };
    }
    let has = |role: &str| workers.iter().any(|w| w.role == role);
    if !has(ROLE_GENERATOR) {
        return Step::Abandon {
            why: "it has no generator".to_string(),
            failed: None,
        };
    }
    if !has(ROLE_CRITIC) && ideas > 0 {
        return Step::Critic;
    }
    Step::Apply
}

/// What advancing a dream did: whether it is still open, and what to say.
pub(crate) struct Advanced {
    pub(crate) open: bool,
    pub(crate) said: String,
}

/// Runs staged dream d's next step (says what it would do, with dry_run).
pub(crate) fn advance(ctx: &Ctx, d: &Dream, dry_run: bool) -> anyhow::Result<Advanced> {
    let store = ctx.store()?;
    let workers = store.dream_workers(d.id)?;
    let ideas = store.dream_ideas(d.id)?.len();
    let alive = |a: &str| agent_alive(ctx, a).ok().flatten().is_some();
    let step = next_step(&workers, ideas, &alive);
    let would = |s: &str| {
        if dry_run {
            format!("Would {s}")
        } else {
            String::new()
        }
    };
    match step {
        Step::Wait(agent) => {
            let status = agent_alive(ctx, &agent)?.unwrap_or_else(|| "gone".to_string());
            Ok(Advanced {
                open: true,
                said: format!(
                    "Dream #{} is still open: {agent} is {status}. Nothing started.\n",
                    d.id
                ),
            })
        }
        Step::Critic => {
            let generator = workers
                .iter()
                .rev()
                .find(|w| w.role == ROLE_GENERATOR)
                .context("the dream's generator")?;
            let w = critic_worker(d.id, generator);
            if dry_run {
                return Ok(Advanced {
                    open: true,
                    said: format!(
                        "{} dream #{}'s critic {}.\n",
                        would("start"),
                        d.id,
                        describe_worker(&w)
                    ),
                });
            }
            // The critic is the dream starter's helper, as the generator is,
            // not the generator's: whoever's step spawns it.
            let by = store
                .get_agent(&generator.agent)?
                .map(|a| a.spawned_by)
                .unwrap_or_default();
            store.add_dream_worker(d.id, &w)?;
            match spawn_worker(ctx, d, &w, &by) {
                Ok(()) => Ok(Advanced {
                    open: true,
                    said: format!(
                        "Started dream #{}'s critic {}.\n",
                        d.id,
                        describe_worker(&w)
                    ),
                }),
                Err(e) => {
                    let why = format!("abandoned: couldn't start its critic {}: {e:#}", w.agent);
                    store.set_dream_worker_status(d.id, &w.agent, WORKER_FAILED)?;
                    store.abandon_dream(d.id, &why)?;
                    Ok(Advanced {
                        open: false,
                        said: format!("Dream #{} {why}\n", d.id),
                    })
                }
            }
        }
        Step::Apply => {
            if dry_run {
                return Ok(Advanced {
                    open: false,
                    said: format!(
                        "{} dream #{}'s approved ideas and commit it.\n",
                        would("apply"),
                        d.id
                    ),
                });
            }
            Ok(Advanced {
                open: false,
                said: apply(ctx, d)?,
            })
        }
        Step::Abandon { why, failed } => {
            if dry_run {
                return Ok(Advanced {
                    open: false,
                    said: format!("Would close dream #{} as abandoned: {why}.\n", d.id),
                });
            }
            if let Some(a) = &failed {
                store.set_dream_worker_status(d.id, a, WORKER_FAILED)?;
            }
            let mut said = String::new();
            for f in restore_memory(ctx, d)? {
                let _ = writeln!(said, "{f}");
            }
            store.abandon_dream(d.id, &format!("abandoned: {why}"))?;
            let _ = writeln!(said, "Closed dream #{} as abandoned: {why}.", d.id);
            Ok(Advanced { open: false, said })
        }
    }
}

/// The model a worker of dream d ran on, for the diary.
fn model_of(workers: &[DreamWorker], agent: &str) -> String {
    workers
        .iter()
        .find(|w| w.agent == agent)
        .map(|w| {
            if w.model.is_empty() {
                "its default model".to_string()
            } else {
                w.model.clone()
            }
        })
        .unwrap_or_else(|| "an unknown model".to_string())
}

/// Applies dream d's approved ideas in id order, records each idea, its
/// verdict and what came of it in the diary, then commits, or, when the
/// result breaks commit's guards, puts memory back and abandons. Returns
/// what to say.
pub(crate) fn apply(ctx: &Ctx, d: &Dream) -> anyhow::Result<String> {
    let store = ctx.store()?;
    let workers = store.dream_workers(d.id)?;
    let ideas = store.dream_ideas(d.id)?;
    let (mut applied, mut skipped, mut rejected) = (0, 0, 0);
    if ideas.is_empty() {
        store.append_dream_diary(
            d.id,
            &format!(
                "{} no ideas were proposed: nothing to judge or apply",
                stamp_now()
            ),
        )?;
    }
    for i in &ideas {
        let s: Synthesis =
            serde_json::from_str(&i.idea).with_context(|| format!("reading idea #{}", i.id))?;
        let mut line = format!(
            "{} idea #{} {} (by {} on {}): ",
            stamp_now(),
            i.id,
            s.describe(),
            i.worker,
            model_of(&workers, &i.worker)
        );
        let verdict = format!(
            "{} by {} on {}: {}",
            verdict_word(i),
            i.judged_by,
            model_of(&workers, &i.judged_by),
            i.why
        );
        match i.status.as_str() {
            IDEA_APPROVED => match apply_one(ctx, d, &s) {
                Ok(what) => {
                    applied += 1;
                    store.set_idea_outcome(i.id, IDEA_APPLIED, &what)?;
                    let _ = write!(line, "{verdict}; applied: {what}");
                }
                Err(e) => {
                    skipped += 1;
                    let why = format!("{e:#}");
                    store.set_idea_outcome(i.id, IDEA_SKIPPED, &why)?;
                    let _ = write!(line, "{verdict}; skipped: {why}");
                }
            },
            IDEA_REJECTED => {
                rejected += 1;
                let _ = write!(line, "{verdict}");
            }
            // An apply that stopped partway (a crash) and runs again: what
            // it did stands, and isn't done twice.
            IDEA_APPLIED => {
                applied += 1;
                let _ = write!(line, "{verdict}; applied before: {}", i.outcome);
            }
            IDEA_SKIPPED => {
                skipped += 1;
                let _ = write!(line, "{verdict}; skipped before: {}", i.outcome);
            }
            _ => {
                let _ = write!(line, "not judged; not applied");
            }
        }
        store.append_dream_diary(d.id, &line)?;
    }
    let tally = format!(
        "{applied} applied, {skipped} skipped, {rejected} rejected of {} ideas",
        ideas.len()
    );
    let problems = memory_problems(ctx, d)?;
    if !problems.is_empty() {
        let mut said = String::new();
        for f in restore_memory(ctx, d)? {
            let _ = writeln!(said, "{f}");
        }
        let why = format!(
            "abandoned: the applied ideas ({tally}) break {} commit rule{}: {}; memory is put back (friction triage stays)",
            problems.len(),
            if problems.len() == 1 { "" } else { "s" },
            problems.join("; ")
        );
        store.abandon_dream(d.id, &why)?;
        let _ = writeln!(said, "Dream #{} {why}.", d.id);
        return Ok(said);
    }
    store.commit_dream(d.id)?;
    Ok(format!(
        "Applied dream #{}'s ideas ({tally}) and committed it: its {} item{} count as reviewed.\n",
        d.id,
        d.items,
        if d.items == 1 { "" } else { "s" }
    ))
}

/// Applies one approved idea for dream d, through the same calls the
/// dream's verbs make. Returns what it did.
fn apply_one(ctx: &Ctx, d: &Dream, s: &Synthesis) -> anyhow::Result<String> {
    match s {
        Synthesis::Memory {
            dir,
            file,
            op,
            expect_version,
            content,
            ..
        } => {
            let dir = memory_item(ctx, d, dir)?;
            // The snapshot before the first write, as memory put takes it.
            record_memory(ctx, d, &dir)?;
            let snap = snapshot_dir_for(&ctx.data_dir(), &d.id.to_string(), &dir);
            let what = if op == OP_PUT {
                match memory::put(&dir, file, content, expect_version, &snap)? {
                    memory::PutOutcome::Written(v) => format!("wrote {file} (version {v})"),
                    memory::PutOutcome::Unchanged(v) => {
                        format!("{file} already held it (version {v})")
                    }
                }
            } else if op == OP_REMOVE {
                memory::remove(&dir, file, expect_version, &snap)?;
                format!("removed {file}")
            } else {
                anyhow::bail!("{op} is no memory change");
            };
            record_memory(ctx, d, &dir)?;
            Ok(what)
        }
        Synthesis::Friction {
            op,
            id,
            other,
            note,
            ..
        } => {
            let id_s = id.to_string();
            match op.as_str() {
                OP_ACCEPT => triage_accept(
                    ctx,
                    d,
                    &FrictionAcceptArgs {
                        note: note.clone(),
                        args: vec![id_s],
                    },
                )?,
                OP_DUP => triage_dup(ctx, d, &id_s, other, note)?,
                OP_LINK => triage_link(
                    ctx,
                    d,
                    &FrictionLinkArgs {
                        note: note.clone(),
                        args: vec![id_s, other.clone()],
                    },
                )?,
                _ => anyhow::bail!("{op} is no friction triage"),
            }
            Ok(match op.as_str() {
                OP_ACCEPT => format!("accepted friction #{id}"),
                OP_DUP => format!("closed friction #{id} as a duplicate of #{other}"),
                _ => format!("linked friction #{id} to {other}"),
            })
        }
    }
}

/// `dir` as the memory item of dream d it names (trailing slashes aside),
/// or why it isn't one.
fn memory_item(ctx: &Ctx, d: &Dream, dir: &str) -> anyhow::Result<PathBuf> {
    let want = dir.trim_end_matches('/');
    let items = ctx.store()?.dream_items(d.id)?;
    let mems: Vec<&str> = items
        .iter()
        .filter(|i| i.kind == gather::KIND_MEMORY)
        .map(|i| i.key.trim_end_matches('/'))
        .collect();
    match mems.iter().find(|k| **k == want) {
        Some(k) => Ok(PathBuf::from(k)),
        None => anyhow::bail!(
            "{dir} is no memory directory of dream #{}: it may change {}",
            d.id,
            if mems.is_empty() {
                "none".to_string()
            } else {
                mems.join(", ")
            }
        ),
    }
}

// `agent dream idea`

pub const IDEA: Doc = Doc {
    use_line: "idea --from FILE",
    short: "Propose one change for your dream's critic to judge",
    long: r#"Idea records one proposed change of the global dream, read as JSON from
--from: a memory change

  {"op": "put", "dir": DIR, "file": FILE, "expect_version": VERSION,
   "draft": PATH, "why": "...", "citations": [...]}
  {"op": "remove", "dir": DIR, "file": FILE, "expect_version": VERSION,
   "why": "...", "citations": [...]}

or a friction triage

  {"op": "dup", "id": N, "other": ORIGINAL, "note": "...", "citations": [...]}
  {"op": "link", "id": N, "other": "ID@HOST", "note": "...", "citations": [...]}
  {"op": "accept", "id": N, "note": "...", "citations": [...]}

It refuses an unknown field, a missing one, a dir that is none of the
dream's memory directories, a draft outside the dream's directory, a second
idea on a file, a friction entry that isn't here, and any citation that
doesn't resolve: sessions (claude:, agy:, kimi:), friction:, commit: and
dream: citations are checked as commit checks them, and any other kind is
refused. A put's draft is read now: what the critic judges is what is
applied. Nothing changes until the critic approves the idea and the dream
applies it. Only a dream's generator runs it."#,
    example: r#"  agent dream idea --from idea-start-here.json"#,
    args: ArgRule::None,
    required: &["from"],
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct IdeaArgs {
    #[arg(
        long = "from",
        value_name = "string",
        default_value = "",
        help = "The idea, as JSON, in this file"
    )]
    pub from: String,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn idea_def() -> Def {
    leaf!(IDEA, IdeaArgs, run_idea)
}

/// Whether `path` is a file inside `dir` (both resolved).
fn file_inside(path: &Path, dir: &Path) -> bool {
    match (path.canonicalize(), dir.canonicalize()) {
        (Ok(p), Ok(d)) => p.starts_with(&d) && p.is_file(),
        _ => false,
    }
}

pub fn run_idea(ctx: &Ctx, args: &IdeaArgs) -> anyhow::Result<()> {
    let (d, w) = own_worker(ctx, "idea", &[ROLE_GENERATOR])?;
    let text =
        std::fs::read_to_string(&args.from).with_context(|| format!("reading {}", args.from))?;
    let raw: SynthesisIn = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "{} isn't an idea: {e} (agent dream idea --help shows the shapes)",
            args.from
        )
    })?;
    let dream_dir = Path::new(&ctx.data_dir())
        .join("dreams")
        .join(d.id.to_string());
    let content = match raw.draft.as_deref().map(str::trim) {
        Some(draft) if !draft.is_empty() && raw.op.trim() == OP_PUT => {
            if !file_inside(Path::new(draft), &dream_dir) {
                anyhow::bail!(
                    "the draft {draft} isn't a file in your dream's directory ({})",
                    dream_dir.display()
                );
            }
            Some(std::fs::read_to_string(draft).with_context(|| format!("reading {draft}"))?)
        }
        _ => None,
    };
    let s = ideas::check_synthesis(raw, content)?;
    let store = ctx.store()?;
    let known = Known::of(ctx, &d)?;
    let knows = |kind: &str, key: &str| known.knows(kind, key);
    let mut bad = ideas::unresolved_citations(s.citations(), &knows);
    match &s {
        Synthesis::Memory {
            dir, file, content, ..
        } => {
            memory_item(ctx, &d, dir)?;
            let same = store.dream_ideas(d.id)?.into_iter().find(|i| {
                i.status != IDEA_REJECTED
                    && matches!(
                        serde_json::from_str::<Synthesis>(&i.idea),
                        Ok(Synthesis::Memory { dir: d2, file: f2, .. })
                            if d2.trim_end_matches('/') == dir.trim_end_matches('/') && f2 == *file
                    )
            });
            if let Some(i) = same {
                anyhow::bail!(
                    "idea #{} already changes {file} in {dir}: one idea per file",
                    i.id
                );
            }
            // The draft's own citations, by commit's rules (other kinds of
            // source are left alone, as commit leaves them).
            if !content.is_empty() {
                for c in memory::parse_file(file, content).citations() {
                    if let Some((kind, key)) = agent_dream::review::citation_item(&c)
                        && !known.knows(kind, &key)
                        && !bad.contains(&c)
                    {
                        bad.push(c);
                    }
                }
            }
        }
        Synthesis::Friction { op, id, other, .. } => {
            if store.get_friction(*id)?.is_none() {
                anyhow::bail!("there is no friction #{id} on this host");
            }
            if op == OP_DUP
                && other
                    .parse::<i64>()
                    .ok()
                    .and_then(|o| store.get_friction(o).ok().flatten())
                    .is_none()
            {
                anyhow::bail!("there is no friction #{other} on this host to be the original");
            }
        }
    }
    if !bad.is_empty() {
        anyhow::bail!(
            "not recorded: {} cite{} nothing this dream was given or a committed dream reviewed (cite sessions, friction:, commit: and dream: the digest lists)",
            bad.join(", "),
            if bad.len() == 1 { "s" } else { "" }
        );
    }
    let json = serde_json::to_string(&s)?;
    let earlier = store.ideas_of_caller(CALLER_SYNTHESIS)?;
    if let Some(e) = ideas::converges_with(CALLER_SYNTHESIS, &json, &earlier) {
        store.add_idea_vote(&IdeaVote {
            idea_id: e.id,
            dream_id: d.id,
            worker: w.agent.clone(),
            model: w.model.clone(),
            lens: w.lens.clone(),
            evidence: s.citations().to_vec(),
        })?;
        return ctx.write_out(&format!(
            "Voted for idea #{}: it is the same as this one.\n",
            e.id
        ));
    }
    let id = store.add_dream_idea(d.id, CALLER_SYNTHESIS, &w, &json, s.citations())?;
    ctx.write_out(&format!("Recorded idea #{id}: {}.\n", s.describe()))
}

// `agent dream ideas`

pub const IDEAS: Doc = Doc {
    use_line: "ideas [<dream-id>]",
    short: "List a dream's ideas, their verdicts and what came of them",
    long: r#"Ideas lists the ideas of a dream (your own open dream if you are one of its
agents, else the newest dream; or the one named): each with what it would
change, its citations and why, the critic's verdict, what applying it did,
and for a memory put the whole content that would be written."#,
    example: "  agent dream ideas\n  agent dream ideas 12 --json",
    args: ArgRule::Range(0, 1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct IdeasArgs {
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn ideas_def() -> Def {
    leaf!(IDEAS, IdeasArgs, run_ideas)
}

#[derive(Serialize)]
struct IdeaView<'a> {
    #[serde(flatten)]
    idea: &'a DreamIdea,
    proposal: Option<Synthesis>,
}

pub fn run_ideas(ctx: &Ctx, args: &IdeasArgs) -> anyhow::Result<()> {
    let store = ctx.store()?;
    let d = match args.args.first() {
        Some(a) => {
            let id = a
                .strip_prefix('#')
                .unwrap_or(a)
                .parse::<i64>()
                .map_err(|_| {
                    anyhow::anyhow!("{a:?} isn't a dream id (agent dream list shows them)")
                })?;
            store.get_dream(id)?.ok_or_else(|| {
                anyhow::anyhow!("there is no dream #{id} (agent dream list shows them)")
            })?
        }
        None => match own_worker(ctx, "ideas", ANY_ROLE) {
            Ok((d, _)) => d,
            Err(_) => store
                .list_dreams(1)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("no dreams on this host yet"))?,
        },
    };
    let list = store.dream_ideas(d.id)?;
    if args.json {
        let views: Vec<IdeaView> = list
            .iter()
            .map(|i| IdeaView {
                idea: i,
                proposal: serde_json::from_str(&i.idea).ok(),
            })
            .collect();
        return ctx.write_out(&format!("{}\n", gojson::marshal(&views)?));
    }
    if list.is_empty() {
        return ctx.write_out(&format!("Dream #{} has no ideas.\n", d.id));
    }
    let mut b = format!("Dream #{}'s ideas ({}):\n", d.id, list.len());
    for i in &list {
        let s: Option<Synthesis> = serde_json::from_str(&i.idea).ok();
        let what = s
            .as_ref()
            .map(Synthesis::describe)
            .unwrap_or_else(|| i.idea.clone());
        let _ = writeln!(b, "\n#{}  {}  {what}  (by {})", i.id, i.status, i.worker);
        match &s {
            Some(Synthesis::Memory {
                why,
                expect_version,
                ..
            }) => {
                let _ = writeln!(b, "  why: {why}");
                let _ = writeln!(b, "  expects version: {expect_version}");
            }
            Some(Synthesis::Friction { note, .. }) => {
                let _ = writeln!(b, "  note: {note}");
            }
            None => {}
        }
        let _ = writeln!(b, "  citations: {}", i.citations.join(", "));
        if !i.judged_by.is_empty() {
            let _ = writeln!(
                b,
                "  verdict: {} by {}: {}",
                verdict_word(i),
                i.judged_by,
                i.why
            );
            if !i.checked.is_empty() {
                let _ = writeln!(b, "  checked: {}", i.checked.join(", "));
            }
            let scores = describe_scores(&i.scores);
            if !scores.is_empty() {
                let _ = writeln!(b, "  scores: {scores}");
            }
        }
        if !i.outcome.is_empty() {
            let _ = writeln!(b, "  outcome: {}", i.outcome);
        }
        if let Some(Synthesis::Memory { content, .. }) = &s
            && !content.is_empty()
        {
            let _ = writeln!(b, "  content:");
            for l in content.lines() {
                let _ = writeln!(b, "    | {l}");
            }
        }
    }
    ctx.write_out(&b)
}

/// The verdict an idea was given, whatever came of it since.
fn verdict_word(i: &DreamIdea) -> &'static str {
    match i.status.as_str() {
        IDEA_REJECTED => "rejected",
        IDEA_PROPOSED => "none",
        _ => "approved",
    }
}

fn describe_scores(s: &Scores) -> String {
    [
        ("leverage", s.leverage),
        ("evidence", s.evidence),
        ("feasibility", s.feasibility),
        ("novelty", s.novelty),
    ]
    .iter()
    .filter_map(|(n, v)| v.map(|v| format!("{n} {v}")))
    .collect::<Vec<_>>()
    .join(", ")
}

// `agent dream judge`

pub const JUDGE: Doc = Doc {
    use_line: "judge <idea-id> approve|reject --why \"...\"",
    short: "Approve or reject one of your dream's ideas",
    long: r#"Judge records the critic's verdict on an idea of its dream, with why. To
approve, name with --checked (repeatable) each primary source you read
yourself, at least one of them among the idea's citations: the critic
checks the sources, not the generator's summary of them. Scores, 1 to 5,
are optional. An idea may be judged again until its dream applies it. Only
a dream's critic runs it."#,
    example: r#"  agent dream judge 3 approve --why "both sessions show the trip" --checked claude:3f3a9c --checked dream:12 --evidence 4
  agent dream judge 4 reject --why "one session, and the rule needs three""#,
    args: ArgRule::Exact(2),
    required: &["why"],
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct JudgeArgs {
    #[arg(
        long = "why",
        value_name = "string",
        default_value = "",
        help = "Why, from the sources"
    )]
    pub why: String,
    #[arg(
        long = "checked",
        value_name = "stringArray",
        action = clap::ArgAction::Append,
        help = "A source you read (a citation); repeatable"
    )]
    pub checked: Vec<String>,
    #[arg(long = "leverage", value_name = "int", default_value = "0", value_parser = flags::parse_int, help = "How much it would help, 1 to 5")]
    pub leverage: i64,
    #[arg(long = "evidence", value_name = "int", default_value = "0", value_parser = flags::parse_int, help = "How well the sources bear it out, 1 to 5")]
    pub evidence: i64,
    #[arg(long = "feasibility", value_name = "int", default_value = "0", value_parser = flags::parse_int, help = "How sure it applies cleanly, 1 to 5")]
    pub feasibility: i64,
    #[arg(long = "novelty", value_name = "int", default_value = "0", value_parser = flags::parse_int, help = "How new it is against what exists, 1 to 5")]
    pub novelty: i64,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn judge_def() -> Def {
    leaf!(JUDGE, JudgeArgs, run_judge)
}

pub fn run_judge(ctx: &Ctx, args: &JudgeArgs) -> anyhow::Result<()> {
    let (d, w) = own_worker(ctx, "judge", &[ROLE_CRITIC])?;
    let a = &args.args[0];
    let id = a
        .strip_prefix('#')
        .unwrap_or(a)
        .parse::<i64>()
        .map_err(|_| {
            anyhow::anyhow!("{a:?} isn't an idea's number (agent dream ideas lists them)")
        })?;
    let store = ctx.store()?;
    let idea = store
        .get_dream_idea(id)?
        .filter(|i| i.dream_id == d.id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "dream #{} has no idea #{id} (agent dream ideas lists them)",
                d.id
            )
        })?;
    let status = match args.args[1].as_str() {
        "approve" => IDEA_APPROVED,
        "reject" => IDEA_REJECTED,
        v => anyhow::bail!("{v:?} is no verdict: approve or reject"),
    };
    let why = args.why.trim();
    if why.is_empty() {
        anyhow::bail!("--why is empty: say what the sources show");
    }
    let checked: Vec<String> = args
        .checked
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    if status == IDEA_APPROVED && !checked.iter().any(|c| idea.citations.contains(c)) {
        anyhow::bail!(
            "approving needs --checked naming a source you read among the idea's citations ({}): the critic checks the primary sources, not the summary",
            idea.citations.join(", ")
        );
    }
    let score = |v: i64| (v != 0).then_some(v);
    store.judge_dream_idea(
        id,
        &Verdict {
            status: status.to_string(),
            judged_by: w.agent.clone(),
            why: why.to_string(),
            checked,
            scores: Scores {
                leverage: score(args.leverage),
                evidence: score(args.evidence),
                feasibility: score(args.feasibility),
                novelty: score(args.novelty),
            },
        },
    )?;
    let word = if status == IDEA_APPROVED {
        "Approved"
    } else {
        "Rejected"
    };
    let what = serde_json::from_str::<Synthesis>(&idea.idea)
        .map(|s| s.describe())
        .unwrap_or_default();
    ctx.write_out(&format!("{word} idea #{id}: {what}.\n"))
}

// `agent dream done`

pub const DONE: Doc = Doc {
    use_line: "done",
    short: "Finish your part of the dream: the next step starts",
    long: r#"Done says a worker of the global dream has finished its part, and runs
the dream's next step in code: after the generator, the critic starts, on
the other model (or, with no ideas, the dream commits); after the critic,
the approved ideas are applied in order and the dream commits, or, if the
result breaks commit's guards, memory is put back and it is abandoned. The
critic's done refuses while an idea is unjudged. A dreamer commits
instead. Only a dream's generator or critic runs it."#,
    example: "  agent dream done",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct DoneArgs {
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn done_def() -> Def {
    leaf!(DONE, DoneArgs, run_done)
}

pub fn run_done(ctx: &Ctx, _args: &DoneArgs) -> anyhow::Result<()> {
    let (d, w) = own_worker(ctx, "done", &[ROLE_GENERATOR, ROLE_CRITIC])?;
    if w.status != WORKER_RUNNING {
        anyhow::bail!("{} is already {} in dream #{}", w.agent, w.status, d.id);
    }
    let store = ctx.store()?;
    let ideas = store.dream_ideas(d.id)?;
    if w.role == ROLE_CRITIC {
        let open: Vec<String> = ideas
            .iter()
            .filter(|i| i.status == IDEA_PROPOSED)
            .map(|i| format!("#{}", i.id))
            .collect();
        if !open.is_empty() {
            anyhow::bail!(
                "judge every idea first: {} {} no verdict (agent dream judge <id> approve|reject)",
                open.join(", "),
                if open.len() == 1 { "has" } else { "have" }
            );
        }
    }
    store.set_dream_worker_status(d.id, &w.agent, WORKER_DONE)?;
    let did = if w.role == ROLE_CRITIC {
        let approved = ideas.iter().filter(|i| i.status == IDEA_APPROVED).count();
        format!("{approved} of {} ideas approved", ideas.len())
    } else {
        format!(
            "{} idea{} proposed",
            ideas.len(),
            if ideas.len() == 1 { "" } else { "s" }
        )
    };
    store.append_dream_diary(
        d.id,
        &format!("{} {} is done: {did}", stamp_now(), describe_worker(&w)),
    )?;
    ctx.write_out(&format!("{} is done ({did}).\n", w.agent))?;
    let a = advance(ctx, &d, false)?;
    ctx.write_out(&a.said)
}

#[cfg(test)]
#[path = "dream_steps_test.rs"]
mod dream_steps_test;

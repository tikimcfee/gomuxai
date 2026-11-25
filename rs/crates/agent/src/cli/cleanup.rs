//! `agent cleanup` (Go: internal/cli/cleanup.go).
use std::collections::{HashMap, HashSet};

use agent_base::gostd::strconv::quote;
use agent_base::gostd::time::format_duration_nanos;
use agent_service::{AGENT_STATUS_ENDED, CleanupError, StatusInfo};
use agent_service::{AGENT_STATUS_IDLE, AgentStatus};
use agent_store::{Agent, STATE_REMOVED_WHY, StateEntry, Time};
use clap::{ArgMatches, FromArgMatches as _};

use super::command::{ArgRule, Cmd, Def, Doc};
use super::output::plural;
use super::root::Ctx;
use super::status::status_words;

// `agent cleanup`

pub const DOC: Doc = Doc {
    use_line: "cleanup <name> [<name>...] | --ended [--dry-run]",
    short: "Stop agents and delete their records",
    long: r#"Cleanup removes the named agents: for a spawned agent it asks an idle CLI to
exit, snapshots its scrollback, and kills its session; for an adopted session
(agent adopt) it leaves the pre-existing session running. In both cases it
deletes the agent's record, queued messages, state, and files while keeping its
log file and list of transcripts, for agent logs. It prints one line per name ("cleaned up",
"refused", or "failed"), and exits 1 if any of them wasn't cleaned up.

Inside an agent (AGENT_NAME set), you can clean up only your own helpers
and their helpers, as "agent list" shows under PARENT, and not yourself:
your session would be killed partway through the cleanup. Name a helper's
own helpers along with it, or it is refused, since you couldn't reach them
once it's gone. A person can clean up any agent. Given names, cleanup never
removes an agent you didn't name: it says which helpers remain instead.

--approved-by-person "who said yes, when, why" lets an agent clean up one
that isn't its helper, once a person has said to: an agent a person started,
or one whose spawner (and that agent's, up the chain) is gone or has ended.
One whose spawner is still here is refused: ask that agent. It must be idle
or ended (--force takes it mid-turn or at a prompt), and it is never you or
an agent above you in your own chain. Its helpers go with it, named in the
same command. The words are the caller's claim, and they are kept: in the
output, in agent history (with the caller's name), and in the agent's
removed event. With --ended it adds the ended agents it covers.

--ended picks the agents instead of names: every agent on this host whose
status is ended, as agent status resolves it (not just the last report), that
you may clean up (inside an agent: your helpers and theirs). An ended agent's
helpers that haven't ended stay; for a person cleanup says so, and inside an
agent the ended one is skipped with them, since only a person could reach
them once it's gone ("skipped"). It prints one line per agent, as for names,
then a total, and exits 1 if any it picked wasn't cleaned up. --dry-run lists what it
would clean up, why each is ended, what happens to its session, and how
many held messages would be deleted with it, and changes nothing."#,
    example: r#"  agent cleanup researcher
  agent cleanup lead-build lead-build-test
  agent cleanup --ended --dry-run     # what --ended would clean up, and why
  agent cleanup --ended
  agent cleanup worker --approved-by-person "Sam, in chat at 14:10: its branch is merged""#,
    args: ArgRule::Custom(cleanup_args),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent cleanup`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "dry-run",
        help = "With --ended: list what it would clean up and why, changing nothing",
    )]
    pub dry_run: bool,
    #[arg(
        long = "ended",
        help = "Clean up every agent here whose status is ended (yours only, inside an agent)",
    )]
    pub ended: bool,
    #[arg(
        long = "approved-by-person",
        value_name = "string",
        help = "Inside an agent: a person said to clean these up, though they aren't your helpers (kept in the log)",
    )]
    pub approved_by_person: Option<String>,
    #[arg(
        long = "force",
        help = "With --approved-by-person: also take an agent that is mid-turn or at a prompt",
    )]
    pub force: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

/// cleanup's argument rule: agents by name, or --ended and none; --dry-run only
/// with --ended.
fn cleanup_args(_cmd: &Cmd, m: &ArgMatches, args: &[String]) -> anyhow::Result<()> {
    let flags = Args::from_arg_matches(m)?;
    match &flags.approved_by_person {
        Some(why) if why.trim().is_empty() => anyhow::bail!(
            "--approved-by-person needs who said yes, when and why: --approved-by-person \"Sam, in chat at 14:10: its branch is merged\""
        ),
        None if flags.force => anyhow::bail!(
            "--force goes with --approved-by-person: it takes an agent that isn't idle or ended"
        ),
        _ => {}
    }
    if flags.ended {
        if !args.is_empty() {
            anyhow::bail!("--ended picks the agents itself: name agents, or pass --ended, not both");
        }
        return Ok(());
    }
    if flags.dry_run {
        anyhow::bail!("--dry-run goes with --ended: agent cleanup --ended --dry-run");
    }
    if args.is_empty() {
        anyhow::bail!("requires at least 1 arg(s), only received 0");
    }
    Ok(())
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    if args.ended {
        return run_cleanup_ended(ctx, args);
    }
    let svc = ctx.service()?;
    let list = svc.list()?;
    let mut t = Teams::new(list);
    let caller = ctx.non_interactive_caller_agent();
    let why = approval_words(&caller, args);

    let mut names: Vec<String> = Vec::new();
    for name in &args.args {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    // Helpers before their parents: a parent's check needs its helpers'
    // outcomes, and its note then names only the helpers that remain.
    names.sort_by_key(|n| std::cmp::Reverse(t.depth(n)));

    // Decide every name before removing any, so each is judged by the
    // recorded parents as they were.
    let mut refusals: HashMap<String, String> = HashMap::new();
    let mut cleared: HashSet<String> = HashSet::new();
    // What a person's approval covers, parents before their helpers: a
    // helper goes with its approved parent.
    let mut approved: HashSet<String> = HashSet::new();
    if why.is_some() {
        let mut statuses = Statuses::new(ctx);
        for name in names.iter().rev() {
            if !t.agents.contains_key(name) || t.owns(&caller, name) {
                continue;
            }
            let reason = t.approval_refusal(&caller, name, &approved, args.force, &mut statuses);
            if reason.is_empty() {
                approved.insert(name.clone());
            } else {
                refusals.insert(name.clone(), reason);
            }
        }
    }
    for name in &names {
        if !t.agents.contains_key(name) || refusals.contains_key(name) {
            continue;
        }
        let reason = t.refusal(&caller, name, &cleared, &approved);
        if reason.is_empty() {
            cleared.insert(name.clone());
        } else {
            refusals.insert(name.clone(), reason);
        }
    }

    let mut failed = 0;
    for name in &names {
        if !t.agents.contains_key(name) {
            let err = match svc.agent(name) {
                Err(e) => format!("{e:#}"),
                Ok(_) => "<nil>".to_string(),
            };
            ctx.write_out(&format!("failed {}: {err}\n", quote(name)))?;
            failed += 1;
            continue;
        }
        if !cleared.contains(name) {
            ctx.write_out(&format!("refused {}: {}\n", quote(name), refusals[name]))?;
            failed += 1;
            continue;
        }
        let said = why.as_deref().filter(|_| approved.contains(name));
        if !cleanup_one(ctx, &mut t, name, said)? {
            failed += 1;
        }
        let left = t.helpers(name);
        if !left.is_empty() {
            ctx.write_out(&format!(
                "  note: its helpers {} remain (agent cleanup {})\n",
                left.join(", "),
                left.join(" ")
            ))?;
        }
    }

    if failed > 0 {
        anyhow::bail!("{failed} of {} cleanups failed", names.len());
    }
    Ok(())
}

/// Clears this host's focus if name has it, publishing the change for
/// followers when a bus runs; reports whether it did. A failure leaves focus
/// on a name that is gone, which focus shows as it is, so it isn't fatal.
fn clear_focus(ctx: &Ctx, name: &str) -> bool {
    let Ok(store) = ctx.store() else {
        return false;
    };
    let had = matches!(agent_fleet::focus::get(store), Ok(Some(ev)) if ev.agent == name);
    if !had {
        return false;
    }
    let me = ctx.non_interactive_caller_agent();
    let by = if me.is_empty() { "user".to_string() } else { me };
    agent_fleet::focus::clear_if_with_publisher(
        store,
        Some(ctx.bus_publisher()),
        &ctx.cfg().host,
        name,
        &by,
    )
    .unwrap_or(false)
}

/// What the record keeps of a person's approval: the caller's words, and that
/// they are the caller's. None for a person, or without the flag.
fn approval_words(caller: &str, args: &Args) -> Option<String> {
    let why = args.approved_by_person.as_deref()?.trim();
    if caller.is_empty() {
        return None;
    }
    let why = why.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(format!(
        "approved by a person, as {} says: {why}",
        quote(caller)
    ))
}

/// Agents' resolved statuses, each read once.
struct Statuses<'a> {
    ctx: &'a Ctx,
    seen: HashMap<String, Option<StatusInfo>>,
}

impl<'a> Statuses<'a> {
    fn new(ctx: &'a Ctx) -> Statuses<'a> {
        Statuses {
            ctx,
            seen: HashMap::new(),
        }
    }

    /// name's status, or None if it couldn't be read.
    fn of(&mut self, name: &str) -> Option<&StatusInfo> {
        if !self.seen.contains_key(name) {
            let info = self.ctx.service().ok().and_then(|s| s.status_info(name).ok());
            self.seen.insert(name.to_string(), info);
        }
        self.seen[name].as_ref()
    }
}

/// Cleans up name, a listed agent, prints its line, and drops it from t. It
/// reports whether the cleanup went through in full (Go: cleanupOne). said
/// is a person's approval as the caller relayed it: it is printed, and kept
/// as the removed event's detail.
fn cleanup_one(ctx: &Ctx, t: &mut Teams, name: &str, said: Option<&str>) -> anyhow::Result<bool> {
    let agent = t.agents[name].clone();
    if let Some(said) = said {
        // Best effort: the command log has the words too.
        let _ = ctx.store().and_then(|s| {
            s.set_state(&StateEntry {
                agent_name: name.to_string(),
                key: STATE_REMOVED_WHY.to_string(),
                value: said.to_string(),
                updated_at: Time::now(),
            })
        });
    }
    let killed = ctx
        .termd()?
        .session_exists(&agent.session_name)
        .unwrap_or(false);
    let res = ctx.service()?.cleanup(name);
    let partial = match &res {
        Ok(()) => None,
        Err(e) => match e.downcast_ref::<CleanupError>() {
            Some(_) => Some(format!("{e:#}")),
            None => {
                ctx.write_out(&format!("failed {}: {e:#}\n", quote(name)))?;
                return Ok(false);
            }
        },
    };
    t.agents.remove(name);
    // The focused agent is gone, so nothing has focus (docs/FLEET.md).
    let unfocused = clear_focus(ctx, name);

    let mut did = format!(
        "removed its record, state, queued messages, and files; its log stays (agent logs {name})"
    );
    if killed {
        if agent.adopted {
            did = format!("left session {} running; {did}", agent.session_name);
        } else {
            did = format!("killed session {}; {did}", agent.session_name);
        }
    }
    if unfocused {
        did += "; it had focus, so nothing has now";
    }
    ctx.write_out(&format!("cleaned up {}: {did}\n", quote(name)))?;
    if let Some(said) = said {
        ctx.write_out(&format!("  not your helper: {said}\n"))?;
    }
    if let Some(err) = partial {
        ctx.write_out(&format!("  but: {err}\n"))?;
        return Ok(false);
    }
    Ok(true)
}

/// cleanup --ended: cleans up (or, dry, lists) every agent here whose
/// resolved status is ended and that the caller may clean up (Go:
/// runCleanupEnded).
fn run_cleanup_ended(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let dry_run = args.dry_run;
    let svc = ctx.service()?;
    let list = svc.list()?;
    let mut t = Teams::new(list.clone());
    let caller = ctx.non_interactive_caller_agent();
    let why = approval_words(&caller, args);
    // Ended agents that aren't the caller's.
    let mut outside: Vec<String> = Vec::new();

    // Every status first, so each is judged as things stood before any
    // cleanup, as named cleanup decides every name before removing any.
    let mut ended: HashMap<String, StatusInfo> = HashMap::new();
    let mut picked: Vec<String> = Vec::new();
    let (mut others, mut skipped) = (0, 0);
    for a in &list {
        let info = match svc.status_info(&a.name) {
            Ok(i) => i,
            Err(e) => {
                ctx.write_out(&format!(
                    "skipped {}: its status couldn't be read: {e:#}\n",
                    quote(&a.name)
                ))?;
                skipped += 1;
                continue;
            }
        };
        if info.status != AGENT_STATUS_ENDED {
            continue;
        }
        ended.insert(a.name.clone(), info);
        if !caller.is_empty() && !t.owns(&caller, &a.name) {
            outside.push(a.name.clone());
            continue;
        }
        picked.push(a.name.clone());
    }
    // A person's approval adds the ones it covers, parents before their
    // helpers: a helper goes with its approved parent.
    let mut approved: HashSet<String> = HashSet::new();
    if why.is_some() {
        outside.sort_by(|a, b| t.depth(a).cmp(&t.depth(b)).then_with(|| a.cmp(b)));
        let mut statuses = Statuses::new(ctx);
        statuses.seen = ended.iter().map(|(n, i)| (n.clone(), Some(i.clone()))).collect();
        for name in &outside {
            if t.approval_refusal(&caller, name, &approved, args.force, &mut statuses).is_empty() {
                approved.insert(name.clone());
                picked.push(name.clone());
            } else {
                others += 1;
            }
        }
    } else {
        others = outside.len();
    }
    // Helpers before their parents, as for names: a parent's check needs
    // its helpers' outcomes. Then by name.
    picked.sort_by(|a, b| t.depth(b).cmp(&t.depth(a)).then_with(|| a.cmp(b)));

    let mut names: Vec<String> = Vec::new();
    let mut cleared: HashSet<String> = HashSet::new();
    for name in &picked {
        if *name == caller {
            ctx.write_out(&format!(
                "skipped {} (ended): that's you, and an agent can't clean itself up\n",
                quote(name)
            ))?;
            skipped += 1;
            continue;
        }
        // Inside an agent, nothing would record that the helpers left
        // behind are yours.
        let left = helpers_left(&t, name, &cleared);
        if !caller.is_empty() && !left.is_empty() {
            ctx.write_out(&format!(
                "skipped {} (ended): its helpers {} haven't ended, and only a person could clean them up once it's gone\n",
                quote(name),
                left.join(", ")
            ))?;
            skipped += 1;
            continue;
        }
        cleared.insert(name.clone());
        names.push(name.clone());
    }

    let mut failed = 0;
    for name in &names {
        let said = why.as_deref().filter(|_| approved.contains(name));
        if dry_run {
            let info = &ended[name];
            ctx.write_out(&format!("would clean up {}: {}\n", quote(name), ended_why(info)))?;
            ctx.write_out(&format!(
                "  {}; {}\n",
                session_fate(ctx, &t.agents[name])?,
                held_words(info.held)
            ))?;
            if let Some(said) = said {
                ctx.write_out(&format!("  not your helper: {said}\n"))?;
            }
        } else if !cleanup_one(ctx, &mut t, name, said)? {
            failed += 1;
        }
        let left = helpers_left(&t, name, &cleared);
        if !left.is_empty() {
            ctx.write_out(&format!(
                "  note: its helpers {} haven't ended, so they stay (agent cleanup {})\n",
                left.join(", "),
                left.join(" ")
            ))?;
        }
    }

    let mut total = if names.is_empty() && caller.is_empty() {
        "No ended agents to clean up here.".to_string()
    } else if names.is_empty() && why.is_some() {
        "No ended agents here that you may clean up.".to_string()
    } else if names.is_empty() {
        "None of your helpers has ended to clean up.".to_string()
    } else if dry_run {
        format!(
            "Would clean up {}; nothing changed (dry run).",
            plural(names.len(), "ended agent")
        )
    } else if failed > 0 {
        format!(
            "Cleaned up {} of {}.",
            names.len() - failed,
            plural(names.len(), "ended agent")
        )
    } else {
        format!("Cleaned up {}.", plural(names.len(), "ended agent"))
    };
    if skipped > 0 {
        total += &format!(" Skipped {skipped}.");
    }
    if others > 0 && why.is_some() {
        total += &format!(
            " Left {} here that a person's approval doesn't cover: an agent above it is still here, or it is above you ({}).",
            plural(others, "more ended agent"),
            quote(&caller)
        );
    } else if others > 0 {
        total += &format!(
            " Left {} here that others spawned, not you ({}) or your helpers; once a person says to, --approved-by-person \"who, when, why\" adds them.",
            plural(others, "more ended agent"),
            quote(&caller)
        );
    }
    ctx.write_out(&format!("{total}\n"))?;

    if failed > 0 {
        anyhow::bail!("{failed} of {} cleanups failed", names.len());
    }
    Ok(())
}

/// name's listed helpers that aren't in cleared (Go: helpersLeft).
fn helpers_left(t: &Teams, name: &str, cleared: &HashSet<String>) -> Vec<String> {
    t.helpers(name)
        .into_iter()
        .filter(|h| !cleared.contains(h))
        .collect()
}

/// Says how an ended agent ended: "ended (quit) 8h34m ago:
/// prompt_input_exit" (Go: endedWhy).
fn ended_why(info: &StatusInfo) -> String {
    let mut why = status_words(info);
    if !info.updated_at.is_zero() {
        let ago = chrono::Utc::now().signed_duration_since(info.updated_at.0);
        // Go: time.Since(..).Round(time.Second).
        let ms = ago.num_milliseconds();
        let secs = (ms + if ms >= 0 { 500 } else { -500 }) / 1000;
        why += &format!(
            " {} ago",
            format_duration_nanos(secs.saturating_mul(1_000_000_000))
        );
    }
    if !info.detail.is_empty() {
        why += ": ";
        why += &info.detail.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    why
}

/// Says what cleanup would do to agent's termd session (Go: sessionFate).
fn session_fate(ctx: &Ctx, agent: &Agent) -> anyhow::Result<String> {
    Ok(match ctx.termd()?.session_exists(&agent.session_name) {
        Err(e) => format!(
            "its session {} couldn't be checked: {e:#}",
            agent.session_name
        ),
        Ok(false) => format!("its session {} is already gone", agent.session_name),
        Ok(true) if agent.adopted => format!(
            "adopted: its session {} stays running, and only the record goes",
            agent.session_name
        ),
        Ok(true) => format!("kills its session {}", agent.session_name),
    })
}

/// Says how many held messages cleanup would delete (Go: heldWords).
fn held_words(n: usize) -> String {
    if n == 0 {
        return "no held messages".to_string();
    }
    format!("deletes {} held for it", plural(n, "message"))
}

/// Agents by name, with each agent's recorded parent (Go: teams).
pub struct Teams {
    pub agents: HashMap<String, Agent>,
    /// Names as listed.
    order: Vec<String>,
}

impl Teams {
    /// Go: newTeams.
    pub fn new(list: Vec<Agent>) -> Teams {
        let mut t = Teams {
            agents: HashMap::with_capacity(list.len()),
            order: Vec::with_capacity(list.len()),
        };
        for a in list {
            t.order.push(a.name.clone());
            t.agents.insert(a.name.clone(), a);
        }
        t
    }

    /// The agents that name spawned and are still present.
    pub fn helpers(&self, name: &str) -> Vec<String> {
        self.order
            .iter()
            .filter(|n| self.agents.get(*n).is_some_and(|a| a.spawned_by == name))
            .cloned()
            .collect()
    }

    /// name's recorded parent, that agent's parent, and so on, up to a person
    /// or an agent that isn't listed (a coordinator that wasn't spawned
    /// through agent, or one that's been cleaned up). A parent's name can be
    /// reused after cleanup, so recorded parents can loop: each name appears
    /// once.
    pub fn ancestors(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen: HashSet<&str> = HashSet::from([name]);
        let mut a = self.agents.get(name);
        while let Some(agent) = a {
            if agent.spawned_by.is_empty() || seen.contains(agent.spawned_by.as_str()) {
                break;
            }
            seen.insert(agent.spawned_by.as_str());
            out.push(agent.spawned_by.clone());
            a = self.agents.get(&agent.spawned_by);
        }
        out
    }

    pub fn depth(&self, name: &str) -> usize {
        self.ancestors(name).len()
    }

    /// Whether name is caller, or one of its helpers or theirs.
    pub fn owns(&self, caller: &str, name: &str) -> bool {
        name == caller || self.ancestors(name).iter().any(|a| a == caller)
    }

    /// Says why a person's approval (--approved-by-person) doesn't let
    /// caller clean up name, a listed agent that caller doesn't own, or
    /// returns "" if it does. approved holds the agents above name that the
    /// approval already covers in this command. The approval covers an agent
    /// no agent here could clean up: one a person started, or whose chain
    /// of spawners is gone, ended, or going with it.
    fn approval_refusal(
        &self,
        caller: &str,
        name: &str,
        approved: &HashSet<String>,
        force: bool,
        statuses: &mut Statuses,
    ) -> String {
        if self.ancestors(caller).iter().any(|a| a == name) {
            return format!(
                "it is above you ({}) in your own chain, so only a person can clean it up",
                quote(caller)
            );
        }
        let status = statuses.of(name).map(|i| i.status.clone());
        let settled = |s: &AgentStatus| *s == AGENT_STATUS_IDLE || *s == AGENT_STATUS_ENDED;
        if !force && !status.as_ref().is_some_and(settled) {
            let is = match &status {
                Some(s) => format!("it is {s}"),
                None => "its status couldn't be read".to_string(),
            };
            return format!(
                "{is}, and a person's approval covers an idle or ended agent: wait for it, or add --force"
            );
        }
        for above in self.ancestors(name) {
            if !self.agents.contains_key(&above) || approved.contains(&above) {
                break;
            }
            let ended = statuses
                .of(&above)
                .is_some_and(|i| i.status == AGENT_STATUS_ENDED);
            if !ended {
                return format!(
                    "{} is above it and still here, and can clean it up: ask it to (agent send {above} \"...\")",
                    quote(&above)
                );
            }
        }
        String::new()
    }

    /// Says why caller ("" for a person) may not clean up name, a listed
    /// agent, or returns "" if it may. approved holds the names a person's
    /// approval covers, though they aren't caller's. cleared holds the names already
    /// cleared to be cleaned up alongside it.
    pub fn refusal(
        &self,
        caller: &str,
        name: &str,
        cleared: &HashSet<String>,
        approved: &HashSet<String>,
    ) -> String {
        if caller.is_empty() {
            return String::new();
        }
        let parent = &self.agents[name].spawned_by;
        let by = if parent.is_empty() {
            "a person".to_string()
        } else {
            quote(parent)
        };
        if name == caller {
            return format!(
                "an agent can't clean itself up, since its session would be killed partway through; ask {by} to"
            );
        }
        if !self.owns(caller, name) && !approved.contains(name) {
            return format!(
                "it was spawned by {by}, not by you ({}) or your helpers; once a person says to: agent cleanup {name} --approved-by-person \"who, when, why\"",
                quote(caller)
            );
        }
        // Once it's gone, nothing records that its helpers are yours.
        let left = helpers_left(self, name, cleared);
        if !left.is_empty() {
            return format!(
                "its helpers {} would be left behind, and only a person could clean them up then; clean them up with it (agent cleanup {name} {})",
                left.join(", "),
                left.join(" ")
            );
        }
        String::new()
    }
}

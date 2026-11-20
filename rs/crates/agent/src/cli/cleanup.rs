//! `agent cleanup` (Go: internal/cli/cleanup.go).
use std::collections::{HashMap, HashSet};

use agent_base::gostd::strconv::quote;
use agent_base::gostd::time::format_duration_nanos;
use agent_service::{AGENT_STATUS_ENDED, CleanupError, StatusInfo};
use agent_store::Agent;
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
  agent cleanup --ended"#,
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
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

/// cleanup's argument rule: agents by name, or --ended and none; --dry-run only
/// with --ended.
fn cleanup_args(_cmd: &Cmd, m: &ArgMatches, args: &[String]) -> anyhow::Result<()> {
    let flags = Args::from_arg_matches(m)?;
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
        return run_cleanup_ended(ctx, args.dry_run);
    }
    let svc = ctx.service()?;
    let list = svc.list()?;
    let mut t = Teams::new(list);
    let caller = ctx.non_interactive_caller_agent();

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
    for name in &names {
        if !t.agents.contains_key(name) {
            continue;
        }
        let reason = t.refusal(&caller, name, &cleared);
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
        if !cleanup_one(ctx, &mut t, name)? {
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

/// Cleans up name, a listed agent, prints its line, and drops it from t. It
/// reports whether the cleanup went through in full (Go: cleanupOne).
fn cleanup_one(ctx: &Ctx, t: &mut Teams, name: &str) -> anyhow::Result<bool> {
    let agent = t.agents[name].clone();
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
    if let Some(err) = partial {
        ctx.write_out(&format!("  but: {err}\n"))?;
        return Ok(false);
    }
    Ok(true)
}

/// cleanup --ended: cleans up (or, dry, lists) every agent here whose
/// resolved status is ended and that the caller may clean up (Go:
/// runCleanupEnded).
fn run_cleanup_ended(ctx: &Ctx, dry_run: bool) -> anyhow::Result<()> {
    let svc = ctx.service()?;
    let list = svc.list()?;
    let mut t = Teams::new(list.clone());
    let caller = ctx.non_interactive_caller_agent();

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
        if !caller.is_empty() && a.name != caller && !t.ancestors(&a.name).contains(&caller) {
            others += 1;
            continue;
        }
        picked.push(a.name.clone());
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
        if dry_run {
            let info = &ended[name];
            ctx.write_out(&format!("would clean up {}: {}\n", quote(name), ended_why(info)))?;
            ctx.write_out(&format!(
                "  {}; {}\n",
                session_fate(ctx, &t.agents[name])?,
                held_words(info.held)
            ))?;
        } else if !cleanup_one(ctx, &mut t, name)? {
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
    if others > 0 {
        total += &format!(
            " Left {} here that others spawned, not you ({}) or your helpers.",
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

    /// Says why caller ("" for a person) may not clean up name, a listed
    /// agent, or returns "" if it may. cleared holds the names already
    /// cleared to be cleaned up alongside it.
    pub fn refusal(&self, caller: &str, name: &str, cleared: &HashSet<String>) -> String {
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
        if !self.ancestors(name).iter().any(|a| a == caller) {
            return format!(
                "it was spawned by {by}, not by you ({}) or your helpers",
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

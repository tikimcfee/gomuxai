//! `agent spawn --pick KIND` and `--saved NAME[@VERSION]`: the roster
//! chooses the backend, model, effort and host (docs/ROSTER.md, "spawn
//! --pick").
use std::fmt::Write as _;

use agent_roster::{HostRoster, PickOptions};

use super::root::Ctx;
use super::roster::{availability, check_saved_ref, gather_rosters};
use super::spawn::{Args, spawn_target};

/// What --pick or --saved chose for spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chosen {
    pub backend: String,
    pub model: String,
    pub effort: String,
    /// The host the agent starts on, as the roster names it.
    pub host: String,
    /// Whether that is another host than this one.
    pub remote: bool,
    /// What spawn prints before it spawns: what was chosen, and why.
    pub said: String,
}

/// Refuses what doesn't go with --pick or --saved: each other, and the
/// flags that would choose what they choose.
pub fn check_flags(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let pick = !args.pick.trim().is_empty();
    let saved = !args.saved.trim().is_empty();
    if pick && saved {
        anyhow::bail!(
            "--pick and --saved don't go together: --pick asks the roster now, --saved replays a pick kept earlier"
        );
    }
    let flag = if pick { "--pick" } else { "--saved" };
    for (set, other) in [
        (!ctx.globals.backend.is_empty(), "--backend"),
        (!ctx.globals.backend_config.is_empty(), "--backend-config"),
        (!args.model.is_empty(), "--model"),
        (!args.effort.is_empty(), "--effort"),
    ] {
        if set {
            anyhow::bail!(
                "{flag} chooses the backend, model and effort, so it doesn't go with {other}: drop {other}, or drop {flag} and name them yourself"
            );
        }
    }
    Ok(())
}

/// Resolves --pick or --saved, or None when neither is given. The agent's
/// name argument (NAME@HOST) or --host, if given, limits a pick to that
/// host's roster, and must agree with a saved pick's host.
pub fn choose(ctx: &Ctx, args: &Args) -> anyhow::Result<Option<Chosen>> {
    let pick = args.pick.trim();
    let saved = args.saved.trim();
    if pick.is_empty() && saved.is_empty() {
        return Ok(None);
    }
    check_flags(ctx, args)?;
    let (r, remote) = spawn_target(ctx, &args.args[0], &args.host)?;
    // A host named on the command line, even this one; "" for none.
    let named = if remote {
        r.host.clone()
    } else if args.args[0].contains('@') || !args.host.trim().is_empty() {
        ctx.cfg().host.clone()
    } else {
        String::new()
    };
    let local_host = ctx.cfg().host.clone();
    let chosen = if !pick.is_empty() {
        choose_pick(ctx, pick, &named, &local_host)?
    } else {
        choose_saved(ctx, saved, &named, &local_host)?
    };
    Ok(Some(chosen))
}

fn is_local(host: &str, local_host: &str) -> bool {
    host.is_empty() || host.eq_ignore_ascii_case(local_host)
}

/// --pick KIND: the top of agent roster pick --task KIND, across this host
/// and every linked one (or the named host only).
fn choose_pick(ctx: &Ctx, task: &str, named: &str, local_host: &str) -> anyhow::Result<Chosen> {
    // The kind is checked before any host is asked.
    let task =
        agent_roster::check_task(task).map_err(|e| anyhow::anyhow!("--pick {task}: {e:#}"))?;
    let (read, unread): (Vec<HostRoster>, Vec<HostRoster>) = gather_rosters(ctx, named)?
        .into_iter()
        .partition(HostRoster::read);
    let opts = PickOptions {
        task: task.clone(),
        prefer: String::new(),
    };
    let picks = match agent_roster::pick(&read, local_host, &opts) {
        Ok(p) => p,
        Err(e) => anyhow::bail!(
            "{}",
            nothing_to_pick(&task, &format!("{e:#}"), &read, &unread)
        ),
    };
    let p = &picks[0];
    let remote = !is_local(&p.host, local_host);
    let mut said = format!("Picked {}", p.key);
    if remote {
        let _ = write!(said, " on {}", p.host);
    }
    let _ = writeln!(said, " for {task}: {}", p.why);
    if p.score <= 0 {
        let _ = writeln!(
            said,
            "Nothing on record suits {task} (no entry lists it, no good notes on it): this is a guess; agent roster pick --task {task} shows the runners-up."
        );
    }
    Ok(Chosen {
        backend: p.harness.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        host: p.host.clone(),
        remote,
        said,
    })
}

/// The refusal when pick has nothing to offer: why, each entry's
/// availability as agent roster list shows it, and how to add one.
fn nothing_to_pick(task: &str, why: &str, read: &[HostRoster], unread: &[HostRoster]) -> String {
    let mut b = format!("--pick {task}: nothing to spawn: {why}");
    let entries: usize = read.iter().map(|h| h.entries.len()).sum();
    if entries > 0 || !unread.is_empty() {
        b.push_str("\nagent roster list shows:");
        for h in read {
            for e in &h.entries {
                let _ = write!(b, "\n  {} on {}: {}", e.key, h.host, availability(e));
            }
        }
        for h in unread {
            let _ = write!(b, "\n  {}: not read: {}", h.host, h.error);
        }
    }
    let _ = write!(
        b,
        "\nAdd one with agent roster set HARNESS/MODEL[@EFFORT] --tasks {task} (or enable one with agent roster enable HARNESS/MODEL), or spawn with --backend instead of --pick."
    );
    b
}

/// --saved NAME[@VERSION]: that saved pick's harness, model, effort and
/// host, if its entry is still available there.
fn choose_saved(ctx: &Ctx, saved: &str, named: &str, local_host: &str) -> anyhow::Result<Chosen> {
    let c = check_saved_ref(ctx, saved)?;
    let p = &c.pick;
    let key = agent_roster::Key::new(&p.harness, &p.model, &p.effort).to_string();
    let remote = !is_local(&p.host, local_host);
    if !named.is_empty() && !named.eq_ignore_ascii_case(&p.host) {
        anyhow::bail!(
            "saved pick {}@{} runs {key} on {}, but the agent is named for {named}: drop the host, or use --pick {} to pick for {named}",
            p.name,
            p.version,
            p.host,
            p.task
        );
    }
    if c.available == Some(false) {
        anyhow::bail!(
            "saved pick {}@{} runs {key} on {}, which is {} now: agent roster pick --task {} picks again (or --pick {} here)",
            p.name,
            p.version,
            p.host,
            c.status,
            p.task,
            p.task
        );
    }
    let mut said = format!("Spawning saved pick {}@{}: {key}", p.name, p.version);
    if remote {
        let _ = write!(said, " on {}", p.host);
    }
    let _ = write!(said, ", picked for {}", p.task);
    if !p.why.is_empty() {
        let _ = write!(said, " ({})", p.why);
    }
    let _ = writeln!(said, "; its entry is {}.", c.status);
    Ok(Chosen {
        backend: p.harness.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        host: p.host.clone(),
        remote,
        said,
    })
}

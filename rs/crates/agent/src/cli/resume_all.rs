//! `agent resume --all` (Go: internal/cli/resume_all.go).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_base::gostd::strconv::quote;
use agent_base::types::END_GONE;
use agent_bus::{Context, ResumeResponse};
use agent_service::AGENT_STATUS_ENDED;

use super::cleanup::Teams;
use super::output::plural;
use super::resume::{resume_agent, resume_session};
use super::root::Ctx;
use super::status::status_words;

/// How resume --all runs resumes side by side: at most parallel at once,
/// each launch at least spacing after the one before. Each resume spends
/// most of its time waiting for its CLI to start (startup_delay), so a few at
/// once save most of that wait, while spacing the launches keeps CLIs from
/// starting in the same instant (a CLI writes its own shared config, such as
/// Claude Code's ~/.claude.json, as it starts) (Go: resumeAllParallel,
/// resumeAllSpacing).
pub const RESUME_ALL_PARALLEL: usize = 4;
pub const RESUME_ALL_SPACING: Duration = Duration::from_millis(500);

/// Resumes one agent, as agent resume does (Go: resumeOne's type):
/// (ctx, context, name, caller, skip_permissions, trust_workspace).
pub type ResumeOne<'a> =
    &'a (dyn Fn(&Ctx, &Context, &str, &str, bool, bool) -> anyhow::Result<ResumeResponse> + Sync);

/// What tests change about resume --all (Go: the package variables).
pub struct ResumeAllOpts<'a> {
    pub parallel: usize,
    pub spacing: Duration,
    pub resume_one: ResumeOne<'a>,
}

/// One ended agent resume --all looked at (Go: resumeEntry).
struct ResumeEntry {
    name: String,
    depth: usize,
    /// Why it is left as it is; "" to resume it.
    skip: String,
}

/// resume --all: resumes every agent here (inside an agent, every helper of
/// the caller's, and theirs) that a reboot or a stopped g3d-termd ended,
/// parents before their helpers, and lists the ended agents it leaves and
/// why. dry_run changes nothing (Go: runResumeAll).
pub fn run_resume_all(
    ctx: &Ctx,
    c: &Context,
    dry_run: bool,
    caller: &str,
    skip_permissions: bool,
    trust_workspace: bool,
) -> anyhow::Result<()> {
    run_resume_all_with(
        ctx,
        c,
        dry_run,
        caller,
        skip_permissions,
        trust_workspace,
        &ResumeAllOpts {
            parallel: RESUME_ALL_PARALLEL,
            spacing: RESUME_ALL_SPACING,
            resume_one: &resume_agent,
        },
    )
}

/// run_resume_all with the test's settings.
pub fn run_resume_all_with(
    ctx: &Ctx,
    c: &Context,
    dry_run: bool,
    caller: &str,
    skip_permissions: bool,
    trust_workspace: bool,
    opts: &ResumeAllOpts,
) -> anyhow::Result<()> {
    let svc = ctx.service()?;
    let list = svc.list()?;
    let t = Teams::new(list.clone());

    // Every status first, so each is judged as things stood before any
    // resume.
    let mut entries: Vec<ResumeEntry> = Vec::new();
    let (mut others, mut running) = (0, 0);
    for a in &list {
        if a.name == caller {
            continue;
        }
        if !caller.is_empty() && !t.ancestors(&a.name).iter().any(|x| x == caller) {
            others += 1;
            continue;
        }
        let mut e = ResumeEntry {
            name: a.name.clone(),
            depth: t.depth(&a.name),
            skip: String::new(),
        };
        match svc.status_info(&a.name) {
            Err(err) => e.skip = format!("its status couldn't be read: {err:#}"),
            Ok(info) if info.status != AGENT_STATUS_ENDED => {
                running += 1;
                continue;
            }
            Ok(info) if info.end_reason != END_GONE => {
                e.skip = format!(
                    "{}, on purpose, not by a reboot: agent resume {} resumes it",
                    status_words(&info),
                    a.name
                );
            }
            Ok(info) => {
                if let Err(refusal) =
                    svc.resume_refusal(a, &info.status, &svc.last_session_id(&a.name))
                {
                    e.skip = format!("{refusal:#}");
                }
            }
        }
        entries.push(e);
    }
    // Parents before their helpers, so a helper comes back to a running
    // parent. Then by name: the order lines print in.
    entries.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.name.cmp(&b.name)));

    // Each entry's line arrives on its channel once done; the lines print in
    // the entries' order, each as soon as it and those before it are done.
    let mut senders: Vec<Option<mpsc::Sender<(String, bool)>>> = Vec::new();
    let mut receivers = Vec::new();
    for _ in &entries {
        let (tx, rx) = mpsc::channel();
        senders.push(Some(tx));
        receivers.push(rx);
    }
    let mut picked: Vec<(&ResumeEntry, mpsc::Sender<(String, bool)>)> = Vec::new();
    for (e, tx) in entries.iter().zip(senders.iter_mut()) {
        let tx = tx.take().expect("one sender each");
        if !e.skip.is_empty() {
            let _ = tx.send((format!("skipped {}: {}", e.name, e.skip), false));
            continue;
        }
        picked.push((e, tx));
    }
    let n_picked = picked.len();

    let mut failed = 0;
    std::thread::scope(|scope| -> anyhow::Result<()> {
        scope.spawn(|| {
            run_resume_waves(
                ctx,
                c,
                picked,
                dry_run,
                caller,
                skip_permissions,
                trust_workspace,
                opts,
            )
        });
        for rx in &receivers {
            let (line, fail) = rx.recv().unwrap_or_else(|_| ("".to_string(), true));
            ctx.write_out(&format!("{line}\n"))?;
            if fail {
                failed += 1;
            }
        }
        Ok(())
    })?;
    ctx.write_out(&format!(
        "{}\n",
        resume_all_total(
            n_picked,
            entries.len() - n_picked,
            failed,
            others,
            running,
            dry_run,
            caller
        )
    ))?;
    if failed > 0 && !dry_run {
        anyhow::bail!("{failed} of {n_picked} resumes failed");
    }
    Ok(())
}

/// Resumes picked (sorted by depth), one depth at a time, a few at once
/// within it, sending each entry's line once it has it (Go: runResumeWaves).
#[allow(clippy::too_many_arguments)]
fn run_resume_waves(
    ctx: &Ctx,
    c: &Context,
    picked: Vec<(&ResumeEntry, mpsc::Sender<(String, bool)>)>,
    dry_run: bool,
    caller: &str,
    skip_permissions: bool,
    trust_workspace: bool,
    opts: &ResumeAllOpts,
) {
    let parallel = opts.parallel.max(1);
    let mut start = 0;
    while start < picked.len() {
        let mut end = start;
        while end < picked.len() && picked[end].0.depth == picked[start].0.depth {
            end += 1;
        }
        // A depth's resumes finish before their helpers start; within it,
        // at most `parallel` run at once (Go: a semaphore channel).
        let wave = &picked[start..end];
        let mut next: Option<Instant> = None;
        let slots = std::sync::Mutex::new(0usize);
        let freed = std::sync::Condvar::new();
        std::thread::scope(|scope| {
            for (e, tx) in wave {
                {
                    let mut busy = slots.lock().unwrap_or_else(|p| p.into_inner());
                    while *busy >= parallel {
                        busy = freed.wait(busy).unwrap_or_else(|p| p.into_inner());
                    }
                    *busy += 1;
                }
                if let Some(at) = next
                    && !dry_run
                {
                    let now = Instant::now();
                    if at > now {
                        std::thread::sleep(at - now);
                    }
                }
                next = Some(Instant::now() + opts.spacing);
                let (slots, freed) = (&slots, &freed);
                scope.spawn(move || {
                    let res = if dry_run {
                        dry_resume_line(ctx, &e.name)
                    } else {
                        match (opts.resume_one)(
                            ctx,
                            c,
                            &e.name,
                            caller,
                            skip_permissions,
                            trust_workspace,
                        ) {
                            Err(err) => (format!("failed {}: {err:#}", e.name), true),
                            Ok(resp) => (resumed_line(&e.name, &resp), false),
                        }
                    };
                    let _ = tx.send(res);
                    *slots.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
                    freed.notify_one();
                });
            }
        });
        start = end;
    }
}

/// Says what resume --all would do with name, checking what resume checks
/// before it starts anything; true if it would fail (Go: dryResumeLine).
fn dry_resume_line(ctx: &Ctx, name: &str) -> (String, bool) {
    let agent = match ctx.service().and_then(|s| s.agent(name)) {
        Ok(a) => a,
        Err(e) => return (format!("would fail {name}: {e:#}"), true),
    };
    match resume_session(ctx, &agent) {
        Err(e) => (format!("would fail {name}: {e:#}"), true),
        Ok(s) => (
            format!(
                "would resume {name} ({}) on session {}, in {}",
                agent.backend, s.session_id, s.cwd
            ),
            false,
        ),
    }
}

/// resume --all's line for an agent it resumed (Go: resumedLine).
fn resumed_line(name: &str, resp: &ResumeResponse) -> String {
    let mut line = format!(
        "resumed {name} ({}) on session {}, in {}",
        resp.backend, resp.session_id, resp.cwd
    );
    if !resp.status.is_empty() {
        let mut status = resp.status.clone();
        if !resp.end_reason.is_empty() {
            status += &format!(" ({})", resp.end_reason);
        }
        line += &format!(": {status}");
    }
    if resp.held > 0 {
        line += &format!(
            "; {} held for it go in at its prompt",
            plural(resp.held as usize, "message")
        );
    }
    line
}

/// resume --all's last line (Go: resumeAllTotal).
fn resume_all_total(
    picked: usize,
    skipped: usize,
    failed: usize,
    others: usize,
    running: usize,
    dry_run: bool,
    caller: &str,
) -> String {
    let mut parts = Vec::new();
    parts.push(if picked == 0 && caller.is_empty() {
        "No agents here were ended by a reboot or a stopped g3d-termd.".to_string()
    } else if picked == 0 {
        "None of your helpers was ended by a reboot or a stopped g3d-termd.".to_string()
    } else if dry_run && failed > 0 {
        format!(
            "Would resume {} of {}; {failed} would fail; nothing changed (dry run).",
            picked - failed,
            plural(picked, "agent")
        )
    } else if dry_run {
        format!(
            "Would resume {}; nothing changed (dry run).",
            plural(picked, "agent")
        )
    } else if failed > 0 {
        format!(
            "Resumed {} of {}; {failed} failed.",
            picked - failed,
            plural(picked, "agent")
        )
    } else {
        format!("Resumed {}.", plural(picked, "agent"))
    });
    if skipped > 0 {
        parts.push(format!("Skipped {skipped}."));
    }
    if running > 0 {
        parts.push(format!("{} still running.", plural(running, "agent")));
    }
    if others > 0 {
        parts.push(format!(
            "Left {} here that others spawned, not you ({}) or your helpers.",
            plural(others, "more agent"),
            quote(caller)
        ));
    }
    parts.join(" ")
}

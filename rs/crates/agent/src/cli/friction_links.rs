//! `agent friction link`, `agent friction unlink`, `agent friction links` (Go: internal/cli/friction_links.go).
//!
//! Links between friction entries on different hosts: the same problem, filed
//! on each. Each host keeps its own half (agent_store::FrictionLink); the
//! bus's friction op reads, links and resolves the other host's entry. See
//! "The friction box" in docs/ARCHITECTURE.md.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::time::Duration;

use agent_base::gostd::strconv;
use agent_base::util::shell_quote;
use agent_bus::{
    Context, FRICTION_GET, FRICTION_RESOLVE, FrictionEntry, FrictionRequest, FrictionResponse,
    RemoteError, RpcClient,
};
use agent_service::{
    close_friction, friction_entry_of, friction_is_open, original_friction, see_friction_peer,
};
use agent_store::{FRICTION_DUPLICATE, FRICTION_WONTFIX, Friction, FrictionLink, Time, gojson};
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::friction::{friction_caller, get_friction, note_part, stamp};
use super::history::clip;
use super::remote::canonical_host;
use super::root::Ctx;

/// Bounds each host's answer when show, list and links check linked entries;
/// one that doesn't answer in time is unreachable.
const FRICTION_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

// `agent friction link`

pub const FRICTION_LINK: Doc = Doc {
    use_line: "link <id> <id@host>",
    short: "Link a friction entry to the same problem filed on another host",
    long: r#"Link records that an entry here and one on another host are the same
problem. Both stay open and count their own votes; show and list name the
other, and resolving either resolves both.

The other host records its half over the bus (its bus must serve the
friction op, which it does by default). If it can't be reached, the link
is recorded here only, and link says what to run there.

Same-host duplicates are closed with agent friction resolve ID --dup OTHER."#,
    example: r#"  agent friction link 4 34@monolith1
  agent friction link 4 34@monolith1 --note "same trust prompt, seen with agy""#,
    args: ArgRule::Exact(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction link`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionLinkArgs {
    #[arg(
        long = "note",
        value_name = "string",
        default_value = "",
        help = "Why they're the same problem"
    )]
    pub note: String,
    /// The positional arguments, checked against FRICTION_LINK.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_link_def() -> Def {
    leaf!(FRICTION_LINK, FrictionLinkArgs, run_friction_link)
}

// `agent friction unlink`

pub const FRICTION_UNLINK: Doc = Doc {
    use_line: "unlink <id> <id@host>",
    short: "Remove a link between friction entries on two hosts",
    long: r#"Unlink removes a link that link made, on both hosts: the entries are no
longer the same problem, and resolving one leaves the other open. If the
other host can't be reached, unlink removes this host's half and says
what to run there."#,
    example: "  agent friction unlink 4 34@monolith1",
    args: ArgRule::Exact(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction unlink`.
#[derive(clap::Args, Debug, Clone)]
pub struct FrictionUnlinkArgs {
    /// The positional arguments, checked against FRICTION_UNLINK.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn friction_unlink_def() -> Def {
    leaf!(FRICTION_UNLINK, FrictionUnlinkArgs, run_friction_unlink)
}

// `agent friction links`

pub const DOC: Doc = Doc {
    use_line: "links",
    short: "Check every link to other hosts' friction entries",
    long: r#"Links lists this host's links to entries on other hosts, each checked
with its host over the bus, and says which have gone stale (--dangling
lists only those), with what to run:

  out of step   one entry is closed and the other still open
  one-sided     the other host has no link back
  not found     the other host has no such entry, or a different one
                under that id now
  unreachable   the other host didn't answer; shows what was last seen"#,
    example: r#"  agent friction links
  agent friction links --dangling"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent friction links`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "dangling",
        help = "Only links that are out of step, one-sided, not found, or unreachable"
    )]
    pub dangling: bool,
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// Formats an entry on host, e.g. "#34@monolith1".
pub fn friction_ref(host: &str, id: i64) -> String {
    format!("#{id}@{host}")
}

/// Identifies an entry on host, ignoring the host's case.
fn friction_key(host: &str, id: i64) -> String {
    format!("{}#{id}", host.to_lowercase())
}

/// Reads an entry argument that may name another host: "34", "#34",
/// "34@monolith1" or "#34@monolith1". remote is false for a bare id and for
/// this host's own name (in any case).
pub fn parse_friction_ref(ctx: &Ctx, arg: &str) -> anyhow::Result<(String, i64, bool)> {
    let (id_part, host) = match arg.split_once('@') {
        Some((i, h)) => (i, Some(h)),
        None => (arg, None),
    };
    let id = id_part
        .strip_prefix('#')
        .unwrap_or(id_part)
        .parse::<i64>()
        .ok()
        .filter(|id| *id >= 1);
    let Some(id) = id else {
        anyhow::bail!(
            "{} isn't a friction entry: want an id like 34, or 34@host for another host's",
            strconv::quote(arg)
        );
    };
    let Some(host) = host else {
        return Ok((ctx.cfg().host.clone(), id, false));
    };
    agent_bus::validate_host(host)?;
    let host = canonical_host(ctx, host);
    let remote = host != ctx.cfg().host;
    Ok((host, id, remote))
}

/// Who is running the command, for another host's records: "name@host", or
/// "user@host" for a person.
fn friction_who(ctx: &Ctx) -> String {
    format!("{}@{}", friction_caller(ctx).0, ctx.cfg().host)
}

/// Why a FrictionBus has no connection.
enum NoConn {
    NoBus,
    Failed(String),
}

/// Sends friction requests to other hosts over one connection to this host's
/// bus. With no bus, every request fails with why.
struct FrictionBus {
    client: Option<RpcClient>,
    err: Option<NoConn>,
}

fn open_friction_bus(ctx: &Ctx, timeout: Duration) -> FrictionBus {
    match ctx.bus_connect() {
        Ok(nc) => FrictionBus {
            client: Some(RpcClient::new(nc, timeout)),
            err: None,
        },
        Err(e) if agent_bus::is_no_bus(&e) => FrictionBus {
            client: None,
            err: Some(NoConn::NoBus),
        },
        Err(e) => FrictionBus {
            client: None,
            err: Some(NoConn::Failed(format!(
                "connecting to the agent bus: {e:#}"
            ))),
        },
    }
}

impl FrictionBus {
    fn call(&self, host: &str, req: FrictionRequest) -> anyhow::Result<FrictionResponse> {
        match (&self.err, &self.client) {
            (Some(NoConn::NoBus), _) => Err(agent_bus::ErrNoBus.into()),
            (Some(NoConn::Failed(msg)), _) => Err(anyhow::anyhow!("{msg}")),
            (None, Some(c)) => c.friction(&Context::background(), host, req),
            (None, None) => Err(agent_bus::ErrNoBus.into()),
        }
    }
}

/// Whether err is the other host's answer (it refused), rather than no answer
/// at all.
fn is_refusal(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<RemoteError>().is_some())
}

// Link states, as check_friction_links finds them. Every state but LINK_OK is
// a dangling link.
pub const LINK_OK: &str = "linked";
pub const LINK_OUT_OF_STEP: &str = "out of step";
pub const LINK_ONE_SIDED: &str = "one-sided";
pub const LINK_NOT_FOUND: &str = "not found";
pub const LINK_UNREACHABLE: &str = "unreachable";

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// A link of a local entry, checked against its other end.
#[derive(Serialize, Clone, Debug)]
pub struct LinkView {
    #[serde(flatten)]
    pub link: FrictionLink,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub state: String,
    /// What's wrong, for a dangling link.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub problem: String,
    /// What to run about a dangling link, one way per line.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fix: Vec<String>,
    /// The id the link named there, when that entry was closed as a duplicate
    /// of Ref's.
    #[serde(skip_serializing_if = "is_zero")]
    pub redirected: i64,
}

impl LinkView {
    fn new(l: &FrictionLink) -> LinkView {
        LinkView {
            link: l.clone(),
            ref_: friction_ref(&l.peer_host, l.peer_id),
            state: String::new(),
            problem: String::new(),
            fix: Vec::new(),
            redirected: 0,
        }
    }

    pub fn dangling(&self) -> bool {
        self.state != LINK_OK
    }
}

/// Checks links (of the entries in local, by id) with their hosts, one request
/// per host, and updates what each link remembers of its other end. Links
/// whose entry isn't in local are skipped.
fn check_friction_links(
    ctx: &Ctx,
    local: &HashMap<i64, Friction>,
    links: Vec<FrictionLink>,
) -> Vec<LinkView> {
    let mut by_host: HashMap<String, Vec<i64>> = HashMap::new();
    let mut hosts: Vec<String> = Vec::new();
    for l in &links {
        if !local.contains_key(&l.friction_id) {
            continue;
        }
        let h = l.peer_host.to_lowercase();
        let ids = by_host.entry(h).or_insert_with(|| {
            hosts.push(l.peer_host.clone());
            Vec::new()
        });
        if !ids.contains(&l.peer_id) {
            ids.push(l.peer_id);
        }
    }
    if hosts.is_empty() {
        return Vec::new();
    }

    let bus = open_friction_bus(ctx, FRICTION_CHECK_TIMEOUT);
    let answers: HashMap<String, anyhow::Result<FrictionResponse>> = std::thread::scope(|s| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|host| {
                let bus = &bus;
                let ids = by_host[&host.to_lowercase()].clone();
                s.spawn(move || {
                    let resp = bus.call(
                        host,
                        FrictionRequest {
                            action: FRICTION_GET.to_string(),
                            ids,
                            ..Default::default()
                        },
                    );
                    (host.to_lowercase(), resp)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let now = Time::now();
    let store = ctx.store().ok();
    let mut views = Vec::new();
    for mut l in links {
        let Some(f) = local.get(&l.friction_id) else {
            continue;
        };
        let answer = answers.get(&l.peer_host.to_lowercase());
        let (resp, err) = match answer {
            Some(Ok(r)) => (Some(r), None),
            Some(Err(e)) => (None, Some(e)),
            None => (None, None),
        };
        let v = judge_link(ctx, f, &mut l, resp, err, now);
        if err.is_none()
            && v.state != LINK_NOT_FOUND
            && let Some(store) = store
        {
            let _ = store.update_friction_link(&l);
        }
        views.push(v);
    }
    views
}

/// Works out the state of f's link l from its host's answer to a get (or the
/// error it got instead), and records what was seen in l.
fn judge_link(
    ctx: &Ctx,
    f: &Friction,
    l: &mut FrictionLink,
    resp: Option<&FrictionResponse>,
    err: Option<&anyhow::Error>,
    now: Time,
) -> LinkView {
    let mut v = LinkView::new(l);
    let resp = match (resp, err) {
        (Some(r), None) => r,
        (_, err) => {
            v.state = LINK_UNREACHABLE.to_string();
            v.problem = match err {
                Some(e) => format!("unreachable: {e:#}"),
                None => "unreachable".to_string(),
            };
            if !l.peer_checked_at.is_zero() {
                let _ = write!(
                    v.problem,
                    "; last seen {}, {}",
                    l.peer_status,
                    stamp(&l.peer_checked_at)
                );
            }
            if l.confirmed_at.is_zero() {
                v.fix = vec![
                    format!(
                        "once {}'s bus answers: agent friction link {} {}@{}",
                        l.peer_host, f.id, l.peer_id, l.peer_host
                    ),
                    format!(
                        "or on {}: agent friction link {} {}@{}",
                        l.peer_host,
                        l.peer_id,
                        f.id,
                        ctx.cfg().host
                    ),
                ];
            }
            return v;
        }
    };
    let mut unlink = format!(
        "agent friction unlink {} {}@{}",
        f.id, l.peer_id, l.peer_host
    );
    let e = resp
        .entries
        .iter()
        .find(|c| c.id == l.peer_id || c.requested.as_ref().is_some_and(|r| r.id == l.peer_id));
    let Some(e) = e else {
        v.state = LINK_NOT_FOUND.to_string();
        v.problem = format!("not found: {} has no #{}", l.peer_host, l.peer_id);
        v.fix = vec![unlink];
        return v;
    };
    let asked_uid = e.requested.as_ref().map_or(&e.uid, |r| &r.uid);
    if !l.peer_uid.is_empty() && *asked_uid != l.peer_uid {
        v.state = LINK_NOT_FOUND.to_string();
        v.problem = format!(
            "not found: #{} on {} is a different entry now ({})",
            l.peer_id,
            l.peer_host,
            clip(&e.summary, 60)
        );
        v.fix = vec![unlink];
        return v;
    }

    if e.id != l.peer_id {
        v.redirected = l.peer_id;
        unlink = format!("agent friction unlink {} {}@{}", f.id, e.id, l.peer_host);
    }
    let host = &ctx.cfg().host;
    let back_linked = e
        .links
        .iter()
        .any(|r| r.host.eq_ignore_ascii_case(host) && r.id == f.id);
    see_friction_peer(l, e, now);
    if !back_linked {
        l.confirmed_at = Time::zero();
    } else if l.confirmed_at.is_zero() {
        l.confirmed_at = now;
    }
    v.link = l.clone();
    v.ref_ = friction_ref(&l.peer_host, l.peer_id);

    let (here_open, there_open) = (friction_is_open(&f.status), friction_is_open(&e.status));
    if here_open && !there_open {
        v.state = LINK_OUT_OF_STEP.to_string();
        v.problem = format!(
            "out of step: {} there{}, {} here",
            e.status,
            by_note(&e.resolved_by, &e.resolution),
            f.status
        );
        v.fix = vec![
            format!(
                "if the same fix covers it here: agent friction resolve {} {}",
                f.id,
                resolve_flags(&e.status, &e.resolution)
            ),
            format!("if it isn't the same problem: {unlink}"),
        ];
    } else if !here_open && there_open {
        v.state = LINK_OUT_OF_STEP.to_string();
        v.problem = format!("out of step: {} there, {} here", e.status, f.status);
        v.fix = vec![format!("agent friction resolve {} --linked", f.id)];
        if f.status == FRICTION_DUPLICATE {
            v.fix = vec![format!(
                "agent friction resolve {} --linked, once #{} is resolved",
                f.duplicate_of, f.duplicate_of
            )];
        }
    } else if !back_linked {
        v.state = LINK_ONE_SIDED.to_string();
        v.problem = format!("one-sided: {} has no link back to #{}", l.peer_host, f.id);
        v.fix = vec![
            format!("agent friction link {} {}@{}", f.id, e.id, l.peer_host),
            format!("or, if they aren't the same problem: {unlink}"),
        ];
    } else {
        v.state = LINK_OK.to_string();
    }
    v
}

/// Says who closed an entry and how, e.g. " (by fixer@h1: fixed in abc123)".
fn by_note(by: &str, note: &str) -> String {
    if by.is_empty() && note.is_empty() {
        return String::new();
    }
    format!(" (by {by}{})", note_part(&clip(note, 80)))
}

/// resolve's flags that close an entry as status with note, shell-quoted.
fn resolve_flags(status: &str, note: &str) -> String {
    let note = if note.is_empty() { "..." } else { note };
    if status == FRICTION_WONTFIX {
        return format!("--wontfix --note {}", shell_quote(note));
    }
    format!("--note {}", shell_quote(note))
}

/// Describes a link's other end as last seen, e.g. "(bug) open, +2 there:
/// summary", or that it hasn't been seen. The kind goes in parentheses, so it
/// doesn't read as the link's state.
fn peer_summary(l: &FrictionLink) -> String {
    if l.peer_checked_at.is_zero() {
        return format!(
            "not seen yet: it was linked while {} was unreachable",
            l.peer_host
        );
    }
    let kind = if l.peer_kind.is_empty() {
        String::new()
    } else {
        format!("({}) ", l.peer_kind)
    };
    format!(
        "{kind}{}, +{} there: {}",
        l.peer_status, l.peer_votes, l.peer_summary
    )
}

/// The links of the entries in entries, by entry id.
pub fn links_of(ctx: &Ctx, entries: &[Friction]) -> anyhow::Result<HashMap<i64, Vec<LinkView>>> {
    let all = ctx.store()?.list_friction_links(0)?;
    if all.is_empty() {
        return Ok(HashMap::new());
    }
    let local: HashMap<i64, Friction> = entries.iter().map(|f| (f.id, f.clone())).collect();
    let mut by_entry: HashMap<i64, Vec<LinkView>> = HashMap::new();
    for v in check_friction_links(ctx, &local, all) {
        by_entry.entry(v.link.friction_id).or_default().push(v);
    }
    Ok(by_entry)
}

/// list's note on an entry's links, e.g. " [= #34@monolith1, #12@hostc
/// !unreachable]".
pub fn format_link_marker(links: Option<&Vec<LinkView>>) -> String {
    let Some(links) = links.filter(|l| !l.is_empty()) else {
        return String::new();
    };
    let refs: Vec<String> = links
        .iter()
        .map(|v| {
            if v.dangling() {
                format!("{} !{}", v.ref_, v.state)
            } else {
                v.ref_.clone()
            }
        })
        .collect();
    format!(" [= {}]", refs.join(", "))
}

/// show's section on an entry's links.
pub fn format_friction_links(f: &Friction, links: &[LinkView]) -> String {
    if links.is_empty() {
        return String::new();
    }
    let mut b = String::new();
    if friction_is_open(&f.status) {
        let _ = writeln!(
            b,
            "Linked (the same problem on other hosts; resolving #{} resolves these too):",
            f.id
        );
    } else {
        b.push_str("Linked (the same problem on other hosts):\n");
    }
    for v in links {
        let _ = writeln!(b, "  {} {}", v.ref_, peer_summary(&v.link));
        if v.redirected != 0 {
            let _ = writeln!(
                b,
                "    (#{} there, which it links to, is closed as its duplicate)",
                v.redirected
            );
        }
        let _ = writeln!(
            b,
            "    linked by {}, {}{}",
            v.link.linked_by,
            stamp(&v.link.linked_at),
            note_part(&v.link.note)
        );
        if v.dangling() {
            let _ = writeln!(b, "    ! {}", v.problem);
            for fix in &v.fix {
                let _ = writeln!(b, "      {fix}");
            }
        }
    }
    b
}

pub fn run_friction_link(ctx: &Ctx, args: &FrictionLinkArgs) -> anyhow::Result<()> {
    let mut f = get_friction(ctx, &args.args[0])?;
    if f.status == FRICTION_DUPLICATE {
        let dup = f.id;
        f = get_friction(ctx, &f.duplicate_of.to_string())?;
        ctx.write_out(&format!(
            "#{dup} is a duplicate of #{}: linking #{}.\n",
            f.id, f.id
        ))?;
    }
    let (host, id, remote) = parse_friction_ref(ctx, &args.args[1])?;
    if !remote {
        anyhow::bail!(
            "#{id} is on this host: link joins entries on two hosts; close a same-host duplicate with agent friction resolve {id} --dup {} (or the other way round)",
            f.id
        );
    }

    let store = ctx.store()?;
    let from = friction_entry_of(store, &ctx.cfg().host, &f)?;
    let note = args.note.trim().to_string();
    let who = friction_caller(ctx).0;
    let now = Time::now();
    let mut l = FrictionLink {
        friction_id: f.id,
        peer_host: host.clone(),
        peer_id: id,
        linked_by: who,
        linked_at: now,
        note: note.clone(),
        ..Default::default()
    };

    let bus = open_friction_bus(ctx, Duration::ZERO);
    let resp = bus.call(
        &host,
        FrictionRequest {
            action: agent_bus::FRICTION_LINK.to_string(),
            id,
            from: Some(from),
            by: friction_who(ctx),
            note,
            ..Default::default()
        },
    );
    let resp = match resp {
        Err(e) if is_refusal(&e) => {
            return Err(anyhow::anyhow!("{host} didn't link it: {e:#}"));
        }
        Err(e) => {
            store.add_friction_link(&mut l)?;
            let mut out = format!(
                "Linked #{} to {} on this host only: couldn't reach {host} ({e:#}).\n",
                f.id,
                friction_ref(&host, id)
            );
            let _ = writeln!(
                out,
                "To link it there too, once its bus answers: agent friction link {} {id}@{host}",
                f.id
            );
            let _ = writeln!(
                out,
                "or on {host}: agent friction link {id} {}@{}",
                f.id,
                ctx.cfg().host
            );
            return ctx.write_out(&out);
        }
        Ok(r) => r,
    };
    let Some(e) = resp.entries.first() else {
        anyhow::bail!("{host} has no #{id}");
    };
    see_friction_peer(&mut l, e, now);
    l.confirmed_at = now;
    let mut out = String::new();
    // Already linked to what the other host's duplicate stands for: keep one.
    if e.requested.is_some() {
        let _ = writeln!(
            out,
            "{} is a duplicate of #{} there: linking #{}.",
            friction_ref(&host, id),
            e.id,
            e.id
        );
    }
    ctx.write_out(&out)?;
    store.add_friction_link(&mut l)?;
    let mut out = format!(
        "Linked #{} and {} {}\nThey're the same problem: resolving either resolves both.\n",
        f.id,
        friction_ref(&host, e.id),
        clip(&peer_summary(&l), 100)
    );
    if friction_is_open(&f.status) != friction_is_open(&e.status) {
        let _ = writeln!(
            out,
            "They're out of step: #{} is {} here and {} there (agent friction show {} says what to run).",
            f.id, f.status, e.status, f.id
        );
    }
    ctx.write_out(&out)
}

pub fn run_friction_unlink(ctx: &Ctx, args: &FrictionUnlinkArgs) -> anyhow::Result<()> {
    let mut f = get_friction(ctx, &args.args[0])?;
    if f.status == FRICTION_DUPLICATE {
        f = get_friction(ctx, &f.duplicate_of.to_string())?;
    }
    let (host, id, remote) = parse_friction_ref(ctx, &args.args[1])?;
    if !remote {
        anyhow::bail!(
            "#{id} is on this host: links join entries on two hosts (agent friction show {} lists #{}'s)",
            f.id,
            f.id
        );
    }
    let had = ctx.store()?.delete_friction_link(f.id, &host, id)?;
    let r = friction_ref(&host, id);
    let bus = open_friction_bus(ctx, Duration::ZERO);
    let res = bus.call(
        &host,
        FrictionRequest {
            action: agent_bus::FRICTION_UNLINK.to_string(),
            id,
            from: Some(FrictionEntry {
                host: ctx.cfg().host.clone(),
                id: f.id,
                uid: f.uid.clone(),
                ..Default::default()
            }),
            by: friction_who(ctx),
            ..Default::default()
        },
    );
    match res {
        Err(e) if !had => anyhow::bail!(
            "#{} has no link to {r} here, and {host} didn't unlink its half: {e:#}",
            f.id
        ),
        Err(e) => {
            let mut out = format!(
                "Unlinked #{} from {r} here; {host} still has its half: couldn't reach it ({e:#}).\n",
                f.id
            );
            let _ = writeln!(
                out,
                "Once its bus answers: agent friction unlink {} {id}@{host}",
                f.id
            );
            let _ = writeln!(
                out,
                "or on {host}: agent friction unlink {id} {}@{}",
                f.id,
                ctx.cfg().host
            );
            ctx.write_out(&out)
        }
        Ok(_) if !had => ctx.write_out(&format!(
            "#{} had no link to {r} here; removed {host}'s half, if it had one.\n",
            f.id
        )),
        Ok(_) => ctx.write_out(&format!("Unlinked #{} and {r}.\n", f.id)),
    }
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let store = ctx.store()?;
    let all = store.list_friction_links(0)?;
    let n_all = all.len();
    let mut local: HashMap<i64, Friction> = HashMap::new();
    for l in &all {
        if !local.contains_key(&l.friction_id)
            && let Some(f) = store.get_friction(l.friction_id)?
        {
            local.insert(l.friction_id, f);
        }
    }
    let views: Vec<LinkView> = check_friction_links(ctx, &local, all)
        .into_iter()
        .filter(|v| v.dangling() || !args.dangling)
        .collect();
    if args.json {
        return ctx.write_out(&format!("{}\n", gojson::marshal(&views)?));
    }
    if views.is_empty() {
        if n_all == 0 {
            return ctx.write_out(&format!(
                "No links to other hosts' friction entries on host {}.\n",
                ctx.cfg().host
            ));
        }
        return ctx.write_out(&format!(
            "No dangling links ({n_all} in all: agent friction links).\n"
        ));
    }
    ctx.write_out(&format_links_report(&local, &views))
}

/// The links command's output: a line per link, and what to run under each
/// dangling one.
fn format_links_report(local: &HashMap<i64, Friction>, views: &[LinkView]) -> String {
    let mut b = String::new();
    for v in views {
        let f = &local[&v.link.friction_id];
        let _ = write!(b, "#{} {} = {}: ", f.id, f.status, v.ref_);
        if v.dangling() {
            let _ = writeln!(b, "{}", v.problem);
            for fix in &v.fix {
                let _ = writeln!(b, "    {fix}");
            }
            continue;
        }
        let _ = writeln!(b, "{}", clip(&peer_summary(&v.link), 100));
    }
    b
}

/// A target of resolve_linked: an entry to resolve, with the entry whose link
/// led to it: another host resolves only an entry linked to the one a request
/// comes from.
struct Target {
    host: String,
    id: i64,
    uid: String,
    /// f's own link to it, to update.
    link: Option<FrictionLink>,
    via: FrictionEntry,
}

/// Passes f's resolution on to the entries linked to it, following their
/// links in turn (A linked to B linked to C), and prints a line for each.
/// Entries on hosts it can't reach are listed with what to run.
pub fn resolve_linked(ctx: &Ctx, f: &Friction) -> anyhow::Result<()> {
    let store = ctx.store()?;
    let links = store.list_friction_links(f.id)?;
    if links.is_empty() {
        return Ok(());
    }
    let this_host = ctx.cfg().host.clone();
    let from = friction_entry_of(store, &this_host, f)?;
    let by = friction_who(ctx);
    let who = friction_caller(ctx).0;

    let mut queue: VecDeque<Target> = links
        .into_iter()
        .map(|l| Target {
            host: l.peer_host.clone(),
            id: l.peer_id,
            uid: l.peer_uid.clone(),
            link: Some(l),
            via: from.clone(),
        })
        .collect();
    let mut left: Vec<Target> = Vec::new();
    let mut visited: HashSet<String> = HashSet::from([friction_key(&this_host, f.id)]);
    let bus = open_friction_bus(ctx, Duration::ZERO);

    while let Some(mut t) = queue.pop_front() {
        if !visited.insert(friction_key(&t.host, t.id)) {
            continue;
        }

        if t.host.eq_ignore_ascii_case(&this_host) {
            // Another entry here, linked through another host's.
            let mut g = match original_friction(store, t.id) {
                Ok(Some((_, g))) if !(!t.uid.is_empty() && g.uid != t.uid && g.id == t.id) => g,
                _ => continue,
            };
            visited.insert(friction_key(&this_host, g.id));
            if friction_is_open(&g.status) {
                close_friction(store, &mut g, &f.status, &f.resolution, &who)?;
                ctx.write_out(&format!(
                    "Resolved #{} too (linked through another host's entry).\n",
                    g.id
                ))?;
            }
            let via = friction_entry_of(store, &this_host, &g)?;
            for r in &via.links {
                queue.push_back(Target {
                    host: r.host.clone(),
                    id: r.id,
                    uid: r.uid.clone(),
                    link: None,
                    via: via.clone(),
                });
            }
            continue;
        }

        let r = friction_ref(&t.host, t.id);
        let resp = bus.call(
            &t.host,
            FrictionRequest {
                action: FRICTION_RESOLVE.to_string(),
                id: t.id,
                uid: t.uid.clone(),
                from: Some(t.via.clone()),
                by: by.clone(),
                status: f.status.clone(),
                note: f.resolution.clone(),
                ..Default::default()
            },
        );
        let resp = match resp {
            Err(e) if is_refusal(&e) => {
                // Its error says what to run.
                ctx.write_out(&format!("{} didn't resolve {r}: {e:#}.\n", t.host))?;
                continue;
            }
            Err(e) => {
                ctx.write_out(&format!(
                    "Couldn't reach {} to resolve {r}: {e:#}.\n",
                    t.host
                ))?;
                left.push(t);
                continue;
            }
            Ok(resp) => resp,
        };
        let Some(e) = resp.entries.first() else {
            ctx.write_out(&format!(
                "{r} wasn't found: {} has no #{}, so it wasn't resolved (agent friction links shows it).\n",
                t.host, t.id
            ))?;
            continue;
        };
        let asked_uid = e.requested.as_ref().map_or(&e.uid, |q| &q.uid);
        if !t.uid.is_empty() && *asked_uid != t.uid {
            ctx.write_out(&format!(
                "{r} wasn't resolved: it's a different entry there now ({}); agent friction links shows it.\n",
                clip(&e.summary, 60)
            ))?;
            continue;
        }
        visited.insert(friction_key(&t.host, e.id));
        let eref = friction_ref(&t.host, e.id);
        if resp.resolved {
            ctx.write_out(&format!("Resolved {eref} too.\n"))?;
        } else {
            ctx.write_out(&format!(
                "{eref} was already {}{}.\n",
                e.status,
                by_whom_name(&e.resolved_by)
            ))?;
        }
        if let Some(l) = &mut t.link {
            see_friction_peer(l, e, Time::now());
            let _ = store.update_friction_link(l);
        }
        for r in &e.links {
            queue.push_back(Target {
                host: canonical_host(ctx, &r.host),
                id: r.id,
                uid: r.uid.clone(),
                link: None,
                via: e.clone(),
            });
        }
    }

    if !left.is_empty() {
        let mut out = format!(
            "Left to do, once those hosts' buses answer: agent friction resolve {} --linked\n",
            f.id
        );
        for t in &left {
            let _ = writeln!(
                out,
                "  or on {}: agent friction resolve {} {}",
                t.host,
                t.id,
                resolve_flags(&f.status, &f.resolution)
            );
        }
        ctx.write_out(&out)?;
    }
    Ok(())
}

/// " (by NAME)", or nothing for no name.
fn by_whom_name(by: &str) -> String {
    if by.is_empty() {
        return String::new();
    }
    format!(" (by {by})")
}

/// export's line on an entry's links, from what was last seen of them (export
/// doesn't use the bus).
pub fn format_export_links(links: Option<&Vec<FrictionLink>>) -> String {
    let Some(links) = links.filter(|l| !l.is_empty()) else {
        return String::new();
    };
    let parts: Vec<String> = links
        .iter()
        .map(|l| {
            let mut p = friction_ref(&l.peer_host, l.peer_id);
            if !l.peer_checked_at.is_zero() {
                let _ = write!(
                    p,
                    " ({} there, {})",
                    l.peer_status,
                    stamp(&l.peer_checked_at)
                );
            }
            p
        })
        .collect();
    format!("Linked to {}.\n", parts.join(", "))
}

#[cfg(test)]
#[path = "friction_links_test.rs"]
pub(crate) mod friction_links_test;

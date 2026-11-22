//! `agent template` and its subcommands (no Go file: new in the Rust build;
//! docs/FLEET.md, "Templates: starting agents across hosts"; the work is
//! agent_fleet::templates and agent_fleet::template_op). The bus's template
//! op is served here too (serve_template).
use std::sync::Arc;

use agent_bus::{
    AgentInfo, AgentRef, Context, RemoteError, ResumeRequest, ResumeResponse, TEMPLATE_LIST,
    TEMPLATE_RESUME, TEMPLATE_START, Template as WireTemplate, TemplateRequest,
    TemplateResponse,
};
use agent_fleet::{template_op, templates};
use agent_service::{AGENT_STATUS_ENDED, AgentService, AgentStatus};

use super::command::{ArgRule, Def, Doc};
use super::logs::abs_path;
use super::remote::{call_remote, canonical_host, is_remote_host};
use super::root::{Ctx, CtxShare};
use super::spawn::{Readiness, spawn_readiness};

// `agent template`

pub const DOC: Doc = Doc {
    use_line: "template",
    short: "Start agents from the templates a host agrees to run, from any host",
    long: r#"Template starts agents on a host from the templates that host lists in
its own <data dir>/templates.json: each names an agent and what it runs
(backend, model, effort, directory, and a brief: the file it is asked to read
first). A host starts only what its file lists, so other hosts can start
its agents without being able to choose what runs there.

A request names a template and who asks, nothing else: no command,
directory, brief, model or effort. The agent is named after its template, and is a
helper of whoever started it (name@host for an agent on another host, so
its replies go back there; none for a person). A template whose agent is
already there isn't started again: template resume brings back one that
has ended.

list, start and resume take --host to ask another host through the bus
(its bus serves template unless --remote-ops leaves it out). add and remove
edit this host's file only: no bus op writes it."#,
    example: r#"  agent template list --host hub
  agent template start builder --host hub
  agent template resume builder --host hub
  agent template add builder --backend claude --cwd /srv/project --brief docs/lanes/builder.md
  agent template remove builder"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![
            template_list_def(),
            template_start_def(),
            template_resume_def(),
            template_add_def(),
            template_remove_def(),
        ],
    )
}

/// Answers a template request, from the bus or from this host's own
/// commands: agent_fleet::template_op on this host's file and service, with
/// agent resume's own resume for "resume". The host agreed to run its
/// templates, so the resume brings a template's agent back whoever asks,
/// rather than only for the agent that started it; template_op lets it
/// resume only an agent its start recorded as the template's
/// (STATE_KEY_TEMPLATE), so the op never resumes another agent by name.
pub(crate) fn serve_template(
    share: &CtxShare,
    svc: &Arc<AgentService>,
    c: &Context,
    req: TemplateRequest,
) -> anyhow::Result<TemplateResponse> {
    let resume_share = share.clone();
    let resume = move |c: &Context, r: ResumeRequest| -> anyhow::Result<ResumeResponse> {
        super::resume::resume_agent(&resume_share.quiet_ctx(), c, &r.agent, "", false, false)
    };
    template_op::handle(c, svc, svc.config(), req, Some(&resume))
}

/// The host a --host flag names ("" for this one), and whether it is another.
fn target(ctx: &Ctx, host: &str) -> anyhow::Result<(String, bool)> {
    if host.is_empty() {
        return Ok((ctx.cfg().host.clone(), false));
    }
    agent_bus::validate_host(host)?;
    let host = canonical_host(ctx, host);
    let remote = is_remote_host(ctx, &host);
    Ok((host, remote))
}

/// Sends req to host (another host's bus, or this host's own handler), with
/// from set to who asks: the caller agent (name@this host when it crosses the
/// bus), or nobody for a person.
fn request(
    ctx: &Ctx,
    host: &str,
    remote: bool,
    mut req: TemplateRequest,
) -> anyhow::Result<TemplateResponse> {
    let me = ctx.non_interactive_caller_agent();
    if !me.is_empty() && req.action != TEMPLATE_LIST {
        req.from = if remote {
            AgentRef {
                name: me,
                host: ctx.cfg().host.clone(),
            }
            .to_string()
        } else {
            me
        };
    }
    if !remote {
        return serve_template(&ctx.share()?, ctx.service()?, &Context::background(), req);
    }
    let action = req.action.clone();
    let name = req.template.clone();
    // A host that doesn't answer template but answers list runs an agent
    // without templates, or a bus that doesn't allow the op (whose leaf
    // permissions drop the request: a timeout, not no responders); one that
    // answers neither is down or unknown, which the error itself says.
    let answers_list = std::cell::Cell::new(false);
    let res = call_remote(ctx, host, |c| {
        let bg = Context::background();
        c.template(&bg, host, req).inspect_err(|e| {
            if agent_bus::is_unanswered(e) {
                answers_list.set(c.list_host(&bg, host).is_ok());
            }
        })
    });
    match res {
        Ok(resp) => Ok(resp),
        Err(e) if e.chain().any(|c| c.downcast_ref::<RemoteError>().is_some()) => {
            if action == TEMPLATE_LIST {
                Err(e.context(format!("{host} couldn't list its templates")))
            } else {
                Err(e.context(format!("{host} couldn't {action} template {name}")))
            }
        }
        Err(e) => {
            if answers_list.get() {
                ctx.hint(format!(
                    "{host} serves templates from an agent built with them, and only if its bus allows template (agent bus status there shows what it allows)"
                ));
            }
            Err(e)
        }
    }
}

// `agent template list`

pub const TEMPLATE_LIST_DOC: Doc = Doc {
    use_line: "list",
    short: "List the templates a host agrees to start",
    long: r#"List prints the templates in a host's templates.json: each one's name,
backend, model, effort, directory and brief. --host asks another host."#,
    example: r#"  agent template list
  agent template list --host hub"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent template list`.
#[derive(clap::Args, Debug, Clone)]
pub struct TemplateListArgs {
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Host whose templates to list (default: this host)",
    )]
    pub host: String,
    #[arg(long = "json", help = "Print the templates as JSON")]
    pub json: bool,
    /// The positional arguments, checked against TEMPLATE_LIST_DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn template_list_def() -> Def {
    leaf!(TEMPLATE_LIST_DOC, TemplateListArgs, run_template_list)
}

pub fn run_template_list(ctx: &Ctx, args: &TemplateListArgs) -> anyhow::Result<()> {
    let (host, remote) = target(ctx, &args.host)?;
    let req = TemplateRequest {
        action: TEMPLATE_LIST.to_string(),
        ..Default::default()
    };
    let list: Vec<WireTemplate> = if remote {
        request(ctx, &host, true, req)?.templates
    } else {
        // What the template op lists (a broken entry with its problem),
        // read here without a service or termd.
        templates::list_wire(template_op::data_dir(ctx.cfg()))?
    };
    if args.json {
        return ctx.print_json(&list);
    }
    if list.is_empty() {
        if remote {
            return ctx.write_out(&format!("No templates on {host}.\n"));
        }
        let path = templates::templates_path(template_op::data_dir(ctx.cfg()));
        return ctx.write_out(&format!(
            "No templates here: {} lists none (agent template add makes one).\n",
            path.display()
        ));
    }
    ctx.write_out(&list.iter().map(template_text).collect::<String>())
}

/// A template as list prints it.
fn template_text(t: &WireTemplate) -> String {
    let mut b = format!("{}: {}", t.name, t.backend);
    if !t.model.is_empty() {
        b += &format!(", model {}", t.model);
    }
    if !t.effort.is_empty() {
        b += &format!(", effort {}", t.effort);
    }
    b += &format!("\n  cwd: {}\n", t.cwd);
    if !t.brief.is_empty() {
        b += &format!("  brief: {}\n", t.brief);
    }
    // What is wrong with it on its host (its directory or brief gone): it
    // can't start until that is fixed.
    if !t.problem.is_empty() {
        b += &format!("  problem: {}\n", printable(&t.problem));
    }
    b
}

/// Text from another host, quoted if it isn't plain printable text.
fn printable(s: &str) -> String {
    if s.chars().all(|c| !c.is_control()) {
        s.to_string()
    } else {
        agent_base::gostd::strconv::quote(s)
    }
}

// `agent template start`

pub const TEMPLATE_START_DOC: Doc = Doc {
    use_line: "start <template>",
    short: "Start a template's agent on a host",
    long: r#"Start spawns the agent a host's template describes, on that host, with
what the template says and nothing else: the agent is named after the
template, and is a helper of whoever started it. A template whose agent is
already there (running, or ended and not cleaned up) isn't started again:
template resume brings back an ended one. --host starts it on another
host."#,
    example: r#"  agent template start builder
  agent template start builder --host hub"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent template start` and `resume`.
#[derive(clap::Args, Debug, Clone)]
pub struct TemplateStartArgs {
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Host whose template it is (default: this host)",
    )]
    pub host: String,
    /// The positional arguments, checked against the Doc's args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn template_start_def() -> Def {
    leaf!(TEMPLATE_START_DOC, TemplateStartArgs, run_template_start)
}

pub fn run_template_start(ctx: &Ctx, args: &TemplateStartArgs) -> anyhow::Result<()> {
    let (host, remote) = target(ctx, &args.host)?;
    let req = TemplateRequest {
        action: TEMPLATE_START.to_string(),
        template: args.args[0].clone(),
        ..Default::default()
    };
    let resp = request(ctx, &host, remote, req)?;
    let a = resp
        .agent
        .ok_or_else(|| anyhow::anyhow!("{host} started template {} but didn't say which agent", args.args[0]))?;
    print_agent(ctx, "Started", &host, remote, &a)
}

/// Says what start or resume did, and what the agent's status means for
/// sending to it.
fn print_agent(ctx: &Ctx, did: &str, host: &str, remote: bool, a: &AgentInfo) -> anyhow::Result<()> {
    let name = if remote {
        format!("{}@{host}", a.name)
    } else {
        a.name.clone()
    };
    let mut b = format!(
        "{did} agent {name} (session {}, backend {}) from its template",
        a.session, a.backend
    );
    if remote {
        b += &format!(" on {host}");
    }
    if !a.cwd.is_empty() {
        b += &format!(", in {}", a.cwd);
    }
    if !a.spawned_by.is_empty() {
        b += &format!(", a helper of {}", a.spawned_by);
    }
    b += ".\n";
    let status = AgentStatus::from(a.status.as_str());
    if status == AGENT_STATUS_ENDED {
        b += &format!("Its session has already ended: agent capture {name} shows its screen.\n");
    } else if !a.status.is_empty() {
        b += &spawn_readiness(&name, &a.backend, &status, &a.detail, Readiness { task: false, remote });
        b.push('\n');
    }
    ctx.write_out(&b)
}

// `agent template resume`

pub const TEMPLATE_RESUME_DOC: Doc = Doc {
    use_line: "resume <template>",
    short: "Bring back a template's agent that has ended",
    long: r#"Resume continues the conversation of a template's agent whose CLI has
ended, as agent resume does on that host (claude and agy, with resume's
refusals). Only an agent that host started from the template (template
start) is resumed: one of the same name started any other way is refused.
One that is still running is left as it is. --host resumes it on another
host."#,
    example: r#"  agent template resume builder
  agent template resume builder --host hub"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

pub fn template_resume_def() -> Def {
    leaf!(TEMPLATE_RESUME_DOC, TemplateStartArgs, run_template_resume)
}

pub fn run_template_resume(ctx: &Ctx, args: &TemplateStartArgs) -> anyhow::Result<()> {
    let (host, remote) = target(ctx, &args.host)?;
    let req = TemplateRequest {
        action: TEMPLATE_RESUME.to_string(),
        template: args.args[0].clone(),
        ..Default::default()
    };
    let resp = request(ctx, &host, remote, req)?;
    let a = resp
        .agent
        .ok_or_else(|| anyhow::anyhow!("{host} resumed template {} but didn't say which agent", args.args[0]))?;
    print_agent(ctx, "Resumed", &host, remote, &a)
}

// `agent template add`

pub const TEMPLATE_ADD_DOC: Doc = Doc {
    use_line: "add <template>",
    short: "Add a template to this host's templates.json",
    long: r#"Add lists a template in this host's templates.json, so any host can start
its agent here (agent template start <template> --host <this host>). The
agent is named after the template. --backend is what it runs (claude, agy,
aider or shell), --cwd the directory it runs in, and --brief the file it is
asked to read first (relative to --cwd, or to this directory); both must
exist. A template already listed under the name is refused: remove it
first. It edits this host's file only."#,
    example: r#"  agent template add builder --backend claude --cwd /srv/project --brief docs/lanes/builder.md
  agent template add reviewer --backend claude --model claude-opus-5-5 --cwd /srv/project
  agent template add deep --backend agy --effort xhigh --cwd /srv/project"#,
    args: ArgRule::Exact(1),
    required: &["cwd"],
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent template add` (--backend is the global
/// flag, as for spawn).
#[derive(clap::Args, Debug, Clone)]
pub struct TemplateAddArgs {
    #[arg(
        long = "brief",
        value_name = "string",
        default_value = "",
        help = "File the agent is asked to read first (relative to --cwd)",
    )]
    pub brief: String,
    #[arg(
        long = "cwd",
        value_name = "string",
        default_value = "",
        help = "Directory the agent runs in",
    )]
    pub cwd: String,
    #[arg(
        long = "effort",
        value_name = "string",
        default_value = "",
        help = "Reasoning effort the CLI runs at (claude, agy): low, medium, high, xhigh or max",
    )]
    pub effort: String,
    /// Refused: templates are edited on their own host.
    #[arg(long = "host", value_name = "string", default_value = "", hide = true)]
    pub host: String,
    #[arg(
        long = "model",
        value_name = "string",
        default_value = "",
        help = "Model the CLI runs on (default: its own default)",
    )]
    pub model: String,
    /// The positional arguments, checked against TEMPLATE_ADD_DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn template_add_def() -> Def {
    leaf!(TEMPLATE_ADD_DOC, TemplateAddArgs, run_template_add)
}

/// Refuses --host for add and remove.
fn local_only(ctx: &Ctx, cmd: &str, host: &str) -> anyhow::Result<()> {
    if host.is_empty() {
        return Ok(());
    }
    let (host, remote) = target(ctx, host)?;
    if remote {
        anyhow::bail!(
            "templates are edited only on their own host: run agent template {cmd} on {host} (no bus op writes a host's templates.json)"
        );
    }
    Ok(())
}

pub fn run_template_add(ctx: &Ctx, args: &TemplateAddArgs) -> anyhow::Result<()> {
    local_only(ctx, "add", &args.host)?;
    if !args.model.is_empty() {
        agent_service::validate_model(&args.model)?;
    }
    let name = &args.args[0];
    let backend = ctx.globals.backend.clone();
    if backend.is_empty() {
        anyhow::bail!(
            "template {name} needs --backend: {}",
            templates::BACKENDS.join(", ")
        );
    }
    let cwd = abs_path(&args.cwd);
    // A relative brief is --cwd's: kept relative, as the template keeps it.
    // Only one found under this directory and not under --cwd is taken from
    // here (absolute, so the output names it); one found under neither stays
    // --cwd's, so the error names the file under --cwd.
    let brief = if args.brief.is_empty()
        || std::path::Path::new(&args.brief).is_absolute()
        || std::path::Path::new(&cwd).join(&args.brief).is_file()
    {
        args.brief.clone()
    } else {
        let here = abs_path(&args.brief);
        if std::path::Path::new(&here).is_file() {
            here
        } else {
            args.brief.clone()
        }
    };
    let t = templates::Template {
        effort: args.effort.clone(),
        ..templates::Template::new(name, &backend, &args.model, &cwd, &brief)
    };
    let dir = template_op::data_dir(ctx.cfg());
    templates::add(&dir, t.clone())?;
    let mut b = format!("Added template {name}: {}", t.backend);
    if !t.model.is_empty() {
        b += &format!(", model {}", t.model);
    }
    if !t.effort.is_empty() {
        b += &format!(", effort {}", t.effort);
    }
    b += &format!(", in {cwd}");
    if let Some(p) = t.brief_path() {
        b += &format!(", brief {}", p.display());
    }
    b += &format!(" ({}).\n", templates::templates_path(&dir).display());
    b += &who_can_start(ctx, name)?;
    ctx.write_out(&b)
}

/// The line template add ends with: which hosts can start the template. Only
/// a host this host's bus trusts and links to can ask, and only while the
/// bus serves the template op, so without a bus identity, or with saved
/// settings (what agent bus start runs with) that leave the op out, it says
/// so and how to turn that on.
fn who_can_start(ctx: &Ctx, name: &str) -> anyhow::Result<String> {
    let cfg = ctx.cfg();
    let host = &cfg.host;
    if let Err(e) = agent_bus::load_identity(cfg.bus_dir(), host) {
        if !agent_bus::is_no_identity(&e) {
            return Err(e);
        }
        return Ok(format!(
            "Start it here with agent template start {name}; other hosts can't ask for it, since this host has no bus identity (agent bus init, then agent bus start, lets the hosts it trusts and links to ask).\n"
        ));
    }
    if let Some(s) = super::bus_identity::load_bus_settings(ctx)?
        && !s.remote_ops.iter().any(|op| op == agent_bus::OP_TEMPLATE)
    {
        let mut ops = s.remote_ops.clone();
        ops.push(agent_bus::OP_TEMPLATE.to_string());
        return Ok(format!(
            "Start it here with agent template start {name}; other hosts can't ask for it, since this host's bus doesn't serve template to them (agent bus restart --remote-ops {} turns it on).\n",
            ops.join(",")
        ));
    }
    Ok(format!(
        "A host this host's bus trusts and links to can start it: agent template start {name} --host {host}\n"
    ))
}

// `agent template remove`

pub const TEMPLATE_REMOVE_DOC: Doc = Doc {
    use_line: "remove <template>",
    short: "Remove a template from this host's templates.json",
    long: r#"Remove takes a template out of this host's templates.json, so no host can
start it here any more. Its agent, if there is one, is left as it is
(agent cleanup removes it). It edits this host's file only."#,
    example: r#"  agent template remove builder"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent template remove`.
#[derive(clap::Args, Debug, Clone)]
pub struct TemplateRemoveArgs {
    /// Refused: templates are edited on their own host.
    #[arg(long = "host", value_name = "string", default_value = "", hide = true)]
    pub host: String,
    /// The positional arguments, checked against TEMPLATE_REMOVE_DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn template_remove_def() -> Def {
    leaf!(TEMPLATE_REMOVE_DOC, TemplateRemoveArgs, run_template_remove)
}

pub fn run_template_remove(ctx: &Ctx, args: &TemplateRemoveArgs) -> anyhow::Result<()> {
    local_only(ctx, "remove", &args.host)?;
    let name = &args.args[0];
    let dir = template_op::data_dir(ctx.cfg());
    templates::remove(&dir, name)?;
    let mut b = format!("Removed template {name}.\n");
    if ctx.store()?.get_agent(name)?.is_some() {
        b += &format!("Its agent {name} is left as it is (agent cleanup {name} removes it).\n");
    }
    ctx.write_out(&b)
}

#[cfg(test)]
#[path = "template_test.rs"]
mod template_test;

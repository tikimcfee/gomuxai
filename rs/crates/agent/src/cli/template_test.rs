//! template's cases: add, list and remove here; start and resume from the
//! leaf on the hub, with the hub's template deciding everything the request
//! could have tried to.
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use agent_backend::EndReason;
use agent_bus::{RpcClient, TemplateRequest};
use agent_fleet::{template_op, templates};
use agent_service::{AGENT_STATUS_ENDED, StatusReport};
use agent_store::{Agent, StateEntry, Time};

use crate::cli::fleet::fleet_test::TwoHosts;
use crate::cli::resume_all_test::launched_resume;
use crate::cli::spawn_continue_test::{claude_transcript, setup_continue};
use crate::cli::testenv::{TestEnv, temp_dir};

#[test]
fn test_template_add_list_remove() {
    let mut env = TestEnv::new();
    let work = temp_dir("agent-template-");
    let dir = work.path().to_string_lossy().into_owned();
    std::fs::write(work.path().join("brief.md"), "build it\n").unwrap();
    let file = templates::templates_path(template_op::data_dir(&env.cfg));
    let file = file.to_string_lossy().into_owned();

    let (out, res) = env.run(&["template", "list"]);
    res.unwrap();
    assert_eq!(out, format!("No templates here: {file} lists none (agent template add makes one).\n"));

    let (_, res) = env.run(&["template", "add", "builder", "--cwd", &dir]);
    assert_eq!(
        format!("{:#}", res.unwrap_err()),
        "template builder needs --backend: claude, agy, aider, shell"
    );
    let (out, res) = env.run(&[
        "template", "add", "builder", "--backend", "shell", "--cwd", &dir, "--brief", "brief.md",
    ]);
    res.unwrap();
    assert_eq!(
        out,
        format!(
            "Added template builder: shell, in {dir}, brief {dir}/brief.md ({file}).\nStart it here with agent template start builder; other hosts can't ask for it, since this host has no bus identity (agent bus init, then agent bus start, lets the hosts it trusts and links to ask).\n"
        )
    );
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "templates.json's mode");

    let (out, res) = env.run(&["template", "list"]);
    res.unwrap();
    assert_eq!(out, format!("builder: shell\n  cwd: {dir}\n  brief: brief.md\n"));
    let (out, res) = env.run(&["template", "list", "--json"]);
    res.unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["name"], "builder");

    for (words, want) in [
        (
            vec!["template", "add", "builder", "--backend", "shell", "--cwd", dir.as_str()],
            "already exists",
        ),
        (
            vec!["template", "add", "x", "--backend", "shell", "--cwd", "/nonexistent-dir"],
            "/nonexistent-dir",
        ),
        (
            vec!["template", "add", "x", "--backend", "zsh", "--cwd", dir.as_str()],
            "unknown backend",
        ),
        (
            vec!["template", "add", "x", "--backend", "shell", "--cwd", dir.as_str(), "--host", "hub"],
            "templates are edited only on their own host: run agent template add on hub",
        ),
        (
            vec!["template", "remove", "builder", "--host", "hub"],
            "templates are edited only on their own host: run agent template remove on hub",
        ),
        (vec!["template", "add", "x"], "required flag(s) \"cwd\" not set"),
    ] {
        let (_, res) = env.run(&words);
        let err = format!("{:#}", res.unwrap_err());
        assert!(err.contains(want), "{words:?}: {err}");
    }

    let (out, res) = env.run(&["template", "remove", "builder"]);
    res.unwrap();
    assert_eq!(out, "Removed template builder.\n");
    let (_, res) = env.run(&["template", "remove", "builder"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("no template named \"builder\""));

    // Other hosts can ask only through the bus: with an identity, the hosts
    // it trusts and links to (the bus's default ops serve template); with
    // saved ops that leave template out, none, and how to turn it on.
    agent_bus::init_identity(env.cfg.bus_dir(), "local").unwrap();
    let (out, res) = env.run(&["template", "add", "b2", "--backend", "shell", "--cwd", &dir]);
    res.unwrap();
    assert!(
        out.ends_with(").\nA host this host's bus trusts and links to can start it: agent template start b2 --host local\n"),
        "{out}"
    );
    crate::cli::bus_identity::save_bus_settings(
        &env.ctx,
        &crate::cli::bus_identity::BusSettings {
            listen: String::new(),
            client_port: 7420,
            remote_ops: vec!["list".into(), "status".into()],
        },
    )
    .unwrap();
    let (out, res) = env.run(&["template", "add", "b3", "--backend", "shell", "--cwd", &dir]);
    res.unwrap();
    assert!(
        out.ends_with(").\nStart it here with agent template start b3; other hosts can't ask for it, since this host's bus doesn't serve template to them (agent bus restart --remote-ops list,status,template turns it on).\n"),
        "{out}"
    );
}

// review-4 W1: list shows what is wrong with a template whose brief or
// directory has gone, as --json does. W2: a relative --brief that exists
// under neither --cwd nor this directory is reported under --cwd, and one
// under --cwd is kept relative to it.
#[test]
fn test_template_list_problem_and_brief_under_cwd() {
    let mut env = TestEnv::new();
    let work = temp_dir("agent-template-");
    let dir = work.path().to_string_lossy().into_owned();
    std::fs::write(work.path().join("brief.md"), "build it\n").unwrap();

    let (_, res) = env.run(&[
        "template", "add", "builder", "--backend", "shell", "--cwd", &dir, "--brief", "docs/typo.md",
    ]);
    assert_eq!(
        format!("{:#}", res.unwrap_err()),
        format!("template \"builder\": brief {dir}/docs/typo.md: no such file or directory"),
        "the error names the brief under --cwd, not under this process's directory"
    );

    env.run(&["template", "add", "builder", "--backend", "shell", "--cwd", &dir, "--brief", "brief.md"])
        .1
        .unwrap();
    std::fs::remove_file(work.path().join("brief.md")).unwrap();
    let problem = format!("template \"builder\": brief {dir}/brief.md: no such file or directory");
    let (out, res) = env.run(&["template", "list"]);
    res.unwrap();
    assert_eq!(out, format!("builder: shell\n  cwd: {dir}\n  brief: brief.md\n  problem: {problem}\n"));
    let (out, res) = env.run(&["template", "list", "--json"]);
    res.unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["problem"], problem.as_str());
}

/// Adds a shell template to h's hub, in dir, with brief brief.md.
fn hub_template(h: &TwoHosts, name: &str, dir: &str) {
    templates::add(
        template_op::data_dir(&h.hub.cfg),
        templates::Template::new(name, "shell", "", dir, "brief.md"),
    )
    .unwrap();
}

// The leaf starts an agent on the hub from the hub's template: its name,
// backend, directory and brief are the hub's, whatever the leaf runs with or
// sends; its parent is the agent that asked, as name@leaf.
#[test]
fn test_template_start_from_another_host() {
    crate::require_termd!();
    let Some(mut h) = TwoHosts::new(true) else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    let work = temp_dir("agent-template-hub-");
    let dir = work.path().to_string_lossy().into_owned();
    std::fs::write(work.path().join("brief.md"), "build it\n").unwrap();
    for name in ["builder", "other", "solo"] {
        hub_template(&h, name, &dir);
    }

    let (out, _, res) = h.leaf.run(&["template", "list", "--host", "hub"]);
    res.unwrap();
    assert!(out.starts_with(&format!("builder: shell\n  cwd: {dir}\n  brief: brief.md\n")), "{out}");

    // An agent of the leaf asks, with a --backend of its own: ignored.
    h.hub.set_env("AGENT_NAME", "lead");
    let (out, _, res) = h.leaf.run(&["template", "start", "builder", "--host", "hub", "--backend", "agy"]);
    res.unwrap_or_else(|e| panic!("template start: {e:#}"));
    eprintln!("agent template start builder --host hub (on leaf, as lead):\n{out}");
    assert!(
        out.starts_with(&format!(
            "Started agent builder@hub (session rpc-cli-builder, backend shell) from its template on hub, in {dir}, a helper of lead@leaf.\n"
        )),
        "{out}"
    );
    let a = h.hub.store().get_agent("builder").unwrap().expect("builder's record on the hub");
    assert_eq!(
        (a.backend.as_str(), a.cwd.as_str(), a.spawned_by.as_str(), a.task_file.as_str()),
        ("shell", dir.as_str(), "lead@leaf", format!("{dir}/brief.md").as_str())
    );
    // What template resume checks later: the hub started it from its template.
    let key = h.hub.store().get_state("builder", template_op::STATE_KEY_TEMPLATE).unwrap();
    assert_eq!(key.map(|e| e.value).as_deref(), Some("builder"));
    assert!(h.leaf.ctx.store().unwrap().get_agent("builder").unwrap().is_none());

    // Started again: refused, saying so.
    let (_, _, res) = h.leaf.run(&["template", "start", "builder", "--host", "hub"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(
        err.starts_with("hub couldn't start template builder: template \"builder\" is already running"),
        "{err}"
    );
    let (_, _, res) = h.leaf.run(&["template", "resume", "builder", "--host", "hub"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("nothing to resume"), "{err}");
    let (_, _, res) = h.leaf.run(&["template", "start", "nope", "--host", "hub"]);
    assert!(
        format!("{:#}", res.unwrap_err()).contains("unknown template \"nope\" on hub"),
        "unknown template"
    );

    // A request carrying a command, directory, brief and backend of its own:
    // the hub reads none of them.
    let bus = h.hub.bus().unwrap();
    let raw = serde_json::json!({
        "action": "start", "template": "other", "from": "lead@leaf",
        "cwd": "/", "backend": "agy", "brief": "/etc/hostname", "model": "x",
        "command": "rm -rf /", "name": "evil", "state": {"template": "evil"},
    });
    let reply = bus
        .conn
        .request(&agent_bus::subject_rpc("hub", "template"), raw.to_string(), Duration::from_secs(20))
        .unwrap();
    let reply = String::from_utf8_lossy(&reply.data).into_owned();
    let a = h.hub.store().get_agent("other").unwrap().unwrap_or_else(|| panic!("no agent other: {reply}"));
    assert_eq!(
        (a.backend.as_str(), a.cwd.as_str(), a.task_file.as_str(), a.spawned_by.as_str()),
        ("shell", dir.as_str(), format!("{dir}/brief.md").as_str(), "lead@leaf")
    );
    assert!(h.hub.store().get_agent("evil").unwrap().is_none());
    let key = h.hub.store().get_state("other", template_op::STATE_KEY_TEMPLATE).unwrap();
    assert_eq!(key.map(|e| e.value).as_deref(), Some("other"), "the key is the hub's, not the request's");

    // A from naming the hub itself, or naming no host, crossed the bus:
    // refused. review-4 S2: so is the hub under a name it had before agent
    // bus rename, or in another case, which name@host resolves to the hub's
    // own agents.
    agent_bus::init_identity(h.hub.cfg.bus_dir(), "alpha").unwrap();
    agent_bus::rename_identity(h.hub.cfg.bus_dir(), "alpha", "hub").unwrap();
    let rpc = RpcClient::new(bus.connect(), Duration::from_secs(20));
    for (from, want) in [
        ("lead@hub", "names an agent on this host (hub)"),
        ("lead@HUB", "names an agent on this host (hub)"),
        ("lead@alpha", "names an agent on this host (hub)"),
        ("lead@Alpha", "names an agent on this host (hub)"),
        ("lead", "names no host"),
    ] {
        let err = rpc
            .template(
                &agent_bus::Context::background(),
                "hub",
                TemplateRequest {
                    action: "start".into(),
                    template: "solo".into(),
                    from: from.into(),
                },
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains(want), "from {from}: {err:#}");
    }

    // A person asks: no parent.
    h.hub.set_env("AGENT_NAME", "");
    let (out, _, res) = h.leaf.run(&["template", "start", "solo", "--host", "hub"]);
    res.unwrap();
    assert!(!out.contains("a helper of"), "{out}");
    assert_eq!(h.hub.store().get_agent("solo").unwrap().unwrap().spawned_by, "");

    // Ended (its session gone): resume is the hub's own, with its refusals.
    h.hub.termd_ops().kill_session("rpc-cli-builder").unwrap();
    let (_, _, res) = h.leaf.run(&["template", "resume", "builder", "--host", "hub"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.starts_with("hub couldn't resume template builder: "), "{err}");
    let (_, _, res) = h.leaf.run(&["template", "start", "builder", "--host", "hub"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("agent template resume builder"));
}

/// Records agent name on env's host as a person's claude agent that ran
/// session id in dir and has ended (quit), as an agent spawn by hand leaves
/// it.
fn ended_claude_agent(env: &TestEnv, home: &str, name: &str, id: &str, dir: &str) {
    let path = claude_transcript(home, id, dir, "", Duration::ZERO);
    env.store()
        .create_agent(&Agent {
            name: name.into(),
            session_name: format!("rpc-cli-{name}"),
            backend: "claude".into(),
            cwd: dir.into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.service()
        .report(
            name,
            StatusReport {
                status: AGENT_STATUS_ENDED,
                end_reason: EndReason::from("quit"),
                transcript: path,
                ..Default::default()
            },
        )
        .unwrap();
}

fn set_template_key(env: &TestEnv, name: &str, value: &str) {
    env.store()
        .set_state(&StateEntry {
            agent_name: name.into(),
            key: template_op::STATE_KEY_TEMPLATE.into(),
            value: value.into(),
            updated_at: Time::now(),
        })
        .unwrap();
}

// review-4 S1: template resume brings back only an agent this host started
// from that template. An agent a person spawned by hand under the template's
// name (here in another directory) is refused, from another host and here,
// even though resume itself isn't served to other hosts.
#[test]
fn test_template_resume_only_its_own_agent() {
    crate::require_termd!();
    let mut c = setup_continue(None);
    let (home, dir, secret) = (c.home.clone(), c.a.clone(), c.b.clone());
    let env = &mut c.env;
    // The default ops: template, and not resume.
    crate::require_bus!(env.serve_as_h1_with(agent_bus::DEFAULT_REMOTE_OPS));
    templates::add(
        template_op::data_dir(&env.cfg),
        templates::Template::new("done", "claude", "", &dir, ""),
    )
    .unwrap();
    let id = "11111111-2222-3333-4444-555555555555";
    ended_claude_agent(env, &home, "done", id, &secret);

    let refused = "agent \"done\" wasn't started from template \"done\" here, so template resume leaves it (agent resume done on local resumes it)";
    let (_, res) = env.run(&["template", "resume", "done", "--host", "h1"]);
    assert_eq!(
        format!("{:#}", res.unwrap_err()),
        format!("h1 couldn't resume template done: {refused}")
    );
    let (_, res) = env.run(&["template", "resume", "done"]);
    assert_eq!(format!("{:#}", res.unwrap_err()), refused);
    // A key naming another template is no better.
    set_template_key(env, "done", "other");
    let (_, res) = env.run(&["template", "resume", "done", "--host", "h1"]);
    assert!(format!("{:#}", res.unwrap_err()).ends_with(refused));
    assert_eq!(launched_resume(env, "done"), "", "nothing was launched");

    // Started from the template (the key template start records): resumed,
    // as agent resume resumes it, for whoever asks.
    set_template_key(env, "done", "done");
    let (out, res) = env.run(&["template", "resume", "done", "--host", "h1"]);
    res.unwrap_or_else(|e| panic!("template resume: {e:#}"));
    assert!(
        out.starts_with("Resumed agent done@h1 (session rpc-cli-done, backend claude) from its template on h1"),
        "{out}"
    );
    assert_eq!(launched_resume(env, "done"), id, "the session resumed");
    assert_eq!(
        env.store().get_state("done", template_op::STATE_KEY_TEMPLATE).unwrap().unwrap().value,
        "done",
        "resume keeps the key"
    );
}

// Without --host, start runs the same template op on this host, the caller
// as a bare name.
#[test]
fn test_template_start_here() {
    crate::require_termd!();
    let Some(mut h) = TwoHosts::new(true) else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    let work = temp_dir("agent-template-here-");
    let dir = work.path().to_string_lossy().into_owned();
    std::fs::write(work.path().join("brief.md"), "build it\n").unwrap();
    hub_template(&h, "builder", &dir);
    h.hub.set_env("AGENT_NAME", "lead");
    let (out, res) = h.hub.run(&["template", "start", "builder"]);
    res.unwrap_or_else(|e| panic!("template start: {e:#}"));
    assert!(
        out.starts_with(&format!(
            "Started agent builder (session rpc-cli-builder, backend shell) from its template, in {dir}, a helper of lead.\n"
        )),
        "{out}"
    );
    assert_eq!(h.hub.store().get_agent("builder").unwrap().unwrap().spawned_by, "lead");
    let key = h.hub.store().get_state("builder", template_op::STATE_KEY_TEMPLATE).unwrap();
    assert_eq!(key.map(|e| e.value).as_deref(), Some("builder"));
}

// review-4 W4: a host that answers nothing isn't taken for an older agent: the
// hint about agents built without templates, or a bus that doesn't allow the
// op, comes only when the host answers list.
#[test]
fn test_template_hint_only_for_a_host_that_answers() {
    crate::require_termd!();
    let Some(mut h) = TwoHosts::new(false) else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    h.hub.take_err();
    let (_, res) = h.hub.run(&["template", "list", "--host", "gone"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(
        err.contains("no bus answering for host \"gone\"") && err.ends_with("; this host is hub, and its bus trusts gone, leaf, old"),
        "{err}"
    );
    assert_eq!(h.hub.take_err(), "", "no hint for a host that answers nothing");

    let (_, res) = h.hub.run(&["template", "start", "builder", "--host", "old"]);
    res.unwrap_err();
    assert_eq!(
        h.hub.take_err(),
        "hint: old serves templates from an agent built with them, and only if its bus allows template (agent bus status there shows what it allows)\n"
    );
}

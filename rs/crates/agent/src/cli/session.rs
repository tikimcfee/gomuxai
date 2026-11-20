//! `agent session`, `agent session new`, `agent session kill`, `agent session list` (Go: internal/cli/session.go).
use std::collections::HashMap;
use std::fmt::Write as _;

use agent_base::gostd::strconv::quote;
use agent_base::util;
use agent_store::Agent;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;

// `agent session`

pub const DOC: Doc = Doc {
    use_line: "session",
    short: "Create, kill, and list raw g3d-termd sessions",
    long: r#"Session manages g3d-termd sessions in the configured namespace directly, by
session name.

An agent's session is named after it with a prefix ("agent-helper" for agent
"helper"); session list shows which agent each session belongs to. To remove
an agent, use "agent cleanup", which also deletes its record."#,
    example: r#"  agent session new scratch
  agent session list"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![
            session_new_def(),
            session_kill_def(),
            session_list_def(),
        ],
    )
}

// `agent session new`

pub const SESSION_NEW: Doc = Doc {
    use_line: "new <session>",
    short: "Create a session running a shell",
    long: r#"New creates a detached g3d-termd session running a shell, with no agent record:
drive it with "agent keys" and "agent key"."#,
    example: r#"  agent session new scratch
  agent session new scratch && agent keys scratch "make test""#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent session new`.
#[derive(clap::Args, Debug, Clone)]
pub struct SessionNewArgs {
    /// The positional arguments, checked against SESSION_NEW.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn session_new_def() -> Def {
    leaf!(SESSION_NEW, SessionNewArgs, run_session_new)
}

pub fn run_session_new(ctx: &Ctx, args: &SessionNewArgs) -> anyhow::Result<()> {
    let name = &args.args[0];
    let termd = ctx.termd()?;
    if termd.session_exists(name).unwrap_or(false) {
        anyhow::bail!("session {name} already exists (agent session list shows them)");
    }
    termd.create_session(name, &HashMap::new())?;
    ctx.write_out(&format!("Created session {name}.\n"))
}

// `agent session kill`

pub const SESSION_KILL: Doc = Doc {
    use_line: "kill <session>",
    short: "Kill a session",
    long: r#"Kill kills a g3d-termd session and its process tree. Killing an agent's session
leaves its record; "agent cleanup <name>" removes both."#,
    example: "  agent session kill scratch",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent session kill`.
#[derive(clap::Args, Debug, Clone)]
pub struct SessionKillArgs {
    /// The positional arguments, checked against SESSION_KILL.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn session_kill_def() -> Def {
    leaf!(SESSION_KILL, SessionKillArgs, run_session_kill)
}

pub fn run_session_kill(ctx: &Ctx, args: &SessionKillArgs) -> anyhow::Result<()> {
    let name = &args.args[0];
    check_session(ctx, name)?;
    ctx.termd()?.kill_session(name)?;
    ctx.write_out(&format!("Killed session {name}.\n"))?;
    if let Some(a) = session_agent(ctx, name) {
        ctx.hint(format_args!(
            "it was agent {}'s session; its record remains until agent cleanup {}",
            a.name, a.name
        ));
    }
    Ok(())
}

// `agent session list`

pub const SESSION_LIST: Doc = Doc {
    use_line: "list",
    short: "List all sessions in the namespace, with the agent each belongs to",
    example: "  agent session list",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent session list`.
#[derive(clap::Args, Debug, Clone)]
pub struct SessionListArgs {
    #[arg(long = "json", help = "Print the sessions as JSON")]
    pub json: bool,
    /// The positional arguments, checked against SESSION_LIST.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn session_list_def() -> Def {
    leaf!(SESSION_LIST, SessionListArgs, run_session_list)
}

pub fn run_session_list(ctx: &Ctx, args: &SessionListArgs) -> anyhow::Result<()> {
    let sessions = ctx.termd()?.list_sessions()?;
    let owners = session_owners(ctx);

    if args.json {
        #[derive(Serialize)]
        struct SessionJson<'a> {
            name: &'a str,
            #[serde(skip_serializing_if = "str::is_empty")]
            agent: &'a str,
            attached: bool,
        }
        let out: Vec<SessionJson> = sessions
            .iter()
            .map(|s| SessionJson {
                name: &s.name,
                agent: owners.get(&s.name).map_or("", String::as_str),
                attached: s.attached,
            })
            .collect();
        return ctx.print_json(&out);
    }

    if sessions.is_empty() {
        return ctx.write_out("No sessions (agent spawn or agent session new creates one).\n");
    }

    let mut out = format!("{:<30} {:<20} {}\n", "NAME", "AGENT", "ATTACHED");
    out.push_str(&"-".repeat(60));
    out.push('\n');
    for s in &sessions {
        let attached = if s.attached { "yes" } else { "no" };
        let owner = owners
            .get(&s.name)
            .map_or("-", |o| if o.is_empty() { "-" } else { o.as_str() });
        let _ = writeln!(out, "{:<30} {:<20} {attached}", s.name, owner);
    }
    ctx.write_out(&out)
}

/// The agents on this host (Go: agentService.List, which is the store's list).
fn list_agents(ctx: &Ctx) -> Vec<Agent> {
    // TODO(agent-service): ctx.service()?.list(), once there is one.
    ctx.store()
        .and_then(|s| s.list_agents())
        .unwrap_or_default()
}

/// Maps each agent's session name to the agent's name.
fn session_owners(ctx: &Ctx) -> HashMap<String, String> {
    list_agents(ctx)
        .into_iter()
        .map(|a| (a.session_name, a.name))
        .collect()
}

/// The agent whose session is named session, if any.
fn session_agent(ctx: &Ctx, session: &str) -> Option<Agent> {
    list_agents(ctx)
        .into_iter()
        .find(|a| a.session_name == session)
}

/// Fails, saying what to use instead, if there is no session named name: the
/// session of the agent called name, or close session names.
pub fn check_session(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let termd = ctx.termd()?;
    match termd.session_exists(name) {
        Err(_) | Ok(true) => return Ok(()),
        Ok(false) => {}
    }
    if let Ok(Some(a)) = ctx.store().and_then(|s| s.get_agent(name)) {
        anyhow::bail!(
            "no session {}, but there is an agent {}: this command takes session names, and its session is {}",
            quote(name),
            a.name,
            a.session_name
        );
    }
    let names: Vec<String> = termd
        .list_sessions()
        .map(|ss| ss.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    let similar = util::similar(name, &names);
    if !similar.is_empty() {
        anyhow::bail!(
            "no session {}; did you mean {}?",
            quote(name),
            util::or_list(&similar, None)
        );
    }
    anyhow::bail!("no session {} (agent session list shows them)", quote(name))
}

// session, key and keys against a daemon in the test socket dir.
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::cli::testenv::TestEnv;
    use crate::cli::{key, keys};

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Waits for the session's screen to show want.
    fn screen_shows(env: &TestEnv, session: &str, want: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = env.ctx.termd().unwrap().capture(session, 50).unwrap_or_default();
            if screen.contains(want) || Instant::now() > deadline {
                return screen;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn new_list_kill() {
        let env = TestEnv::with_termd();
        let ctx = &env.ctx;
        let list = |json: bool| {
            run_session_list(ctx, &SessionListArgs { json, args: vec![] }).unwrap();
            env.take_out()
        };
        assert_eq!(
            list(false),
            "No sessions (agent spawn or agent session new creates one).\n"
        );

        run_session_new(ctx, &SessionNewArgs { args: args(&["scratch"]) }).unwrap();
        assert_eq!(env.take_out(), "Created session scratch.\n");
        let err = run_session_new(ctx, &SessionNewArgs { args: args(&["scratch"]) }).unwrap_err();
        assert_eq!(
            err.to_string(),
            "session scratch already exists (agent session list shows them)"
        );

        // An agent's session shows whose it is.
        run_session_new(ctx, &SessionNewArgs { args: args(&["rpc-cli-worker"]) }).unwrap();
        env.store()
            .create_agent(&Agent {
                name: "worker".to_string(),
                session_name: "rpc-cli-worker".to_string(),
                backend: "claude".to_string(),
                created_at: agent_store::Time::now(),
                ..Default::default()
            })
            .unwrap();
        env.take_out();
        let out = list(false);
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(format!("{:<30} {:<20} ATTACHED", "NAME", "AGENT").as_str()));
        assert_eq!(lines.next(), Some("-".repeat(60).as_str()));
        let mut rows: Vec<&str> = lines.collect();
        rows.sort();
        assert_eq!(
            rows,
            [
                format!("{:<30} {:<20} no", "rpc-cli-worker", "worker"),
                format!("{:<30} {:<20} no", "scratch", "-"),
            ]
        );
        let json: serde_json::Value = serde_json::from_str(&list(true)).unwrap();
        let mut entries = json.as_array().unwrap().clone();
        entries.sort_by_key(|e| e["name"].as_str().unwrap_or_default().to_string());
        assert_eq!(
            serde_json::Value::Array(entries),
            serde_json::json!([
                {"name": "rpc-cli-worker", "agent": "worker", "attached": false},
                {"name": "scratch", "attached": false},
            ])
        );

        run_session_kill(ctx, &SessionKillArgs { args: args(&["rpc-cli-worker"]) }).unwrap();
        assert_eq!(env.take_out(), "Killed session rpc-cli-worker.\n");
        assert_eq!(
            env.err.contents(),
            "hint: it was agent worker's session; its record remains until agent cleanup worker\n"
        );
        let err = run_session_kill(ctx, &SessionKillArgs { args: args(&["scrach"]) }).unwrap_err();
        assert_eq!(err.to_string(), r#"no session "scrach"; did you mean scratch?"#);
        let err = run_session_kill(ctx, &SessionKillArgs { args: args(&["zzz"]) }).unwrap_err();
        assert_eq!(err.to_string(), r#"no session "zzz" (agent session list shows them)"#);
        run_session_kill(ctx, &SessionKillArgs { args: args(&["scratch"]) }).unwrap();
        assert_eq!(env.take_out(), "Killed session scratch.\n");
        assert_eq!(
            list(false),
            "No sessions (agent spawn or agent session new creates one).\n"
        );
    }

    #[test]
    fn keys_and_key_type_into_the_session() {
        let env = TestEnv::with_termd();
        let ctx = &env.ctx;
        run_session_new(ctx, &SessionNewArgs { args: args(&["typing"]) }).unwrap();
        env.take_out();

        // Typed, and run with Enter: the shell prints the joined words.
        let a = keys::Args { no_enter: false, args: args(&["typing", "echo", "gomux-$((6*7))"]) };
        keys::run(ctx, &a).unwrap();
        assert_eq!(env.take_out(), "Typed into session typing and pressed Enter.\n");
        let screen = screen_shows(&env, "typing", "gomux-42");
        assert!(screen.contains("gomux-42"), "screen:\n{screen}");

        // --no-enter types and leaves it; key presses Enter.
        let a = keys::Args { no_enter: true, args: args(&["typing", "echo kept-$((2+3))"]) };
        keys::run(ctx, &a).unwrap();
        assert_eq!(env.take_out(), "Typed into session typing (no Enter).\n");
        let screen = screen_shows(&env, "typing", "echo kept-$((2+3))");
        assert!(!screen.contains("kept-5"), "ran without Enter:\n{screen}");
        key::run(ctx, &key::Args { args: args(&["typing", "Enter"]) }).unwrap();
        assert_eq!(env.take_out(), "Pressed Enter in session typing.\n");
        let screen = screen_shows(&env, "typing", "kept-5");
        assert!(screen.contains("kept-5"), "screen:\n{screen}");

        let err = key::run(ctx, &key::Args { args: args(&["typng", "Enter"]) }).unwrap_err();
        assert_eq!(err.to_string(), r#"no session "typng"; did you mean typing?"#);
    }
}

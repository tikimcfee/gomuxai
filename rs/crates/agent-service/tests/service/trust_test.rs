//! Go tests/service/trust_test.go.

use std::sync::Arc;
use std::time::Duration;

use agent_backend::{END_QUIT, PaneSession, PermissionResponse, Status};
use agent_base::await_;
use agent_base::util::shell_quote;
use agent_service::*;

use crate::common::*;
use crate::require_termd;

/// fakeTrustCLI stands in for Claude Code at its folder trust dialog: it
/// draws the dialog as Claude Code v2.1.310 does (tests/backend's
/// claudeTrustScreen has a real capture), "No, exit" highlighted, moves the
/// highlight with Up and Down, and takes Enter. "Yes" writes yes to $1 and
/// leaves it sitting there as a started CLI; "No, exit" writes no and exits,
/// leaving its last frame on screen under the shell's prompt, as Claude does.
const FAKE_TRUST_CLI: &str = r#"
sel=no
draw() {
  printf '\033[2J\033[H Accessing workspace:\n\n %s\n\n' "$2"
  printf ' Quick safety check: Is this a project you created or one you trust? (Like your\n own code, a well-known open source project, or work from your team).\n\n'
  printf " Claude Code'll be able to read, edit, and execute files here.\n\n"
  if [ $sel = no ]; then printf ' ❯ No, exit\n   Yes, I trust this folder\n'
  else printf '   No, exit\n ❯ Yes, I trust this folder\n'; fi
  printf '\n Enter to confirm · Esc to cancel\n'
}
draw "$@"
while IFS= read -rsn1 k; do
  case $k in
    $'\033') IFS= read -rsn2 -t 1 rest
      case $rest in '[A') sel=no ;; '[B') sel=yes ;; esac
      draw "$@" ;;
    '') printf '%s\n' $sel > "$1"
      if [ $sel = yes ]; then printf '\033[2J\033[H● trusted\n'; exec sleep 600; fi
      exit 0 ;;
  esac
done
"#;

/// trustFolder is the folder the fake asks about: long enough to wrap in an
/// 80-column pane, as real ones do.
const TRUST_FOLDER: &str = "/srv/projects/a-folder-claude-has-never-seen-before/with-a-path-long-enough-to-wrap-at-80-columns";

/// fakeTrustBackend is the claude backend launching fakeTrustCLI. Like
/// Claude's registry, its native status doesn't list the session until the
/// folder is trusted, and then says idle.
#[derive(Clone)]
pub struct FakeTrust {
    pub script: String,
    pub answer: Arc<std::sync::Mutex<String>>,
    _dir: Arc<tempfile::TempDir>,
}

impl FakeTrust {
    /// answered is what the fake was answered: "yes", "no", or "".
    pub fn answered(&self) -> String {
        let path = self.answer.lock().unwrap().clone();
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

/// spawnAtTrustPrompt spawns agent name on the fake, and waits until its
/// dialog is drawn.
fn spawn_at_trust_prompt(env: &mut TestEnv, mut opts: SpawnOptions) -> FakeTrust {
    let dir = Arc::new(tempdir());
    let fake = FakeTrust {
        script: dir.path().join("claude.sh").display().to_string(),
        answer: Arc::new(std::sync::Mutex::new(
            dir.path().join("answer").display().to_string(),
        )),
        _dir: dir,
    };
    std::fs::write(&fake.script, FAKE_TRUST_CLI).unwrap();
    let launch = {
        let f = fake.clone();
        move || {
            format!(
                "bash {} {} {}",
                shell_quote(&f.script),
                shell_quote(&f.answer.lock().unwrap()),
                shell_quote(TRUST_FOLDER)
            )
        }
    };
    let launch2 = launch.clone();
    let f = fake.clone();
    register_claude_fake(
        &env.backends,
        ClaudeOverrides {
            name: "faketrust".into(),
            hooks_installed: Some(Box::new(|_| Ok(()))),
            launch_command: Some(Box::new(launch)),
            launch_command_with_session: Some(Box::new(move |_| launch2())),
            native_status: Some(Box::new(move |_| {
                if f.answered() == "yes" {
                    return (Status::Idle, true, None);
                }
                (Status::Unknown, false, None)
            })),
            pane_session: Some(Box::new(|_| Ok(None::<PaneSession>))),
            ..Default::default()
        },
    );
    // Spawn waits for the CLI's input box, or a dialog in its place, before
    // typing its first message, as send does.
    env.cfg_mut().submit_wait = Duration::from_millis(300);
    env.cfg_mut().agent_binary = "/opt/gomuxai/agent".into(); // spawn pins a session id, as for real
    opts.backend = "faketrust".into();
    let name = opts.name.clone();
    env.service
        .spawn(&bg(), opts)
        .unwrap_or_else(|e| panic!("spawn: {e:#}"));
    wait_pane(
        env,
        &env.session_name(&name),
        "the trust dialog drawn",
        |screen| has_line(screen, "Enter to confirm · Esc to cancel"),
    );
    fake
}

/// observe is what `agent wait` checks for a local agent.
pub fn observe(env: &TestEnv, name: &str) -> anyhow::Result<agent_bus::Observation> {
    let info = env.service.status_info(name)?;
    Ok(agent_bus::Observation {
        status: info.status.to_string(),
        reported_status: info.reported_status.to_string(),
        reported_at: agent_bus::Time(info.reported_at.0),
        sent_at: agent_bus::Time(info.sent_at.0),
        turn_at: agent_bus::Time(info.turn_at.0),
        ..Default::default()
    })
}

// A Claude agent at its folder trust dialog read unknown: wait --for
// waiting never returned, approve refused it, and the spawner had to send
// raw keys (friction #4). It is waiting now, on tool "trust", and approve
// answers it.
#[test]
fn test_trust_prompt_waiting_then_approved() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_at_trust_prompt(
        &mut env,
        SpawnOptions {
            name: "worker".into(),
            ..Default::default()
        },
    );

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_WAITING
            && info.tool == TRUST_TOOL
            && info.detail == format!("trust: {TRUST_FOLDER}")
            && info.is_trust_prompt(),
        "status = {}, tool {:?}, detail {:?}; want waiting on trust: {TRUST_FOLDER}",
        info.status,
        info.tool,
        info.detail
    );

    // wait --for waiting (and --any, which checks the same status) returns.
    let (wait_ctx, _cancel) = Context::with_timeout(&bg(), Duration::from_secs(5));
    let got = agent_bus::wait(
        &wait_ctx,
        None,
        agent_bus::WaitOptions {
            host: "h".into(),
            agent: "worker".into(),
            for_statuses: vec!["idle".into(), "waiting".into()],
            interval: Duration::from_millis(20),
            current: Some(Box::new(|_| observe(&env, "worker"))),
            fail_statuses: Vec::new(),
            next: false,
            after: agent_bus::Time::default(),
        },
    );
    assert_eq!(
        got.as_ref().map_err(|(s, e)| format!("{s}: {e:#}")),
        Ok(&"waiting".to_string())
    );

    // A message's Enter would answer it.
    let err = env
        .service
        .send(&bg(), "worker", "hello", SendOptions::default())
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("agent approve worker trusts the folder"),
        "send at the trust prompt: err = {err:#}"
    );

    // --expect guards it like any prompt, and it has no "always".
    assert!(
        env.service
            .respond_permission(
                "worker",
                PermissionResponse::Approve,
                PermissionOptions {
                    expect_tool: "Bash".into(),
                    ..Default::default()
                }
            )
            .is_err(),
        "approved the trust prompt while expecting Bash"
    );
    let err = env
        .service
        .respond_permission(
            "worker",
            PermissionResponse::ApproveAlways,
            PermissionOptions::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("no \"always\" answer"),
        "approve --always: err = {err:#}"
    );
    assert_eq!(fake.answered(), "", "a refused approve answered the prompt");

    let answered = env
        .service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions {
                expect_tool: TRUST_TOOL.into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("approve: {e:#}"));
    assert!(
        answered.is_trust_prompt(),
        "approve answered {:?} ({}), want the trust prompt",
        answered.tool,
        answered.detail
    );
    // "No, exit" was highlighted: approve moved to "Yes" before Enter.
    await_::until("the fake answered", || {
        let a = fake.answered();
        let ok = !a.is_empty();
        (a, ok)
    });
    assert_eq!(fake.answered(), "yes");
    // Starting, then listed idle by its CLI.
    status_is(&env, "worker", "idle");

    // Nothing to answer now.
    assert!(
        env.service
            .respond_permission(
                "worker",
                PermissionResponse::Approve,
                PermissionOptions::default()
            )
            .is_err(),
        "approve with no prompt went through"
    );
}

// Until the CLI lists the session it was just trusted to start, status says
// it is starting, not that it died.
#[test]
fn test_trust_prompt_approved_cli_is_starting() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_at_trust_prompt(
        &mut env,
        SpawnOptions {
            name: "worker".into(),
            ..Default::default()
        },
    );
    // As if the fake's registry entry hadn't appeared yet.
    let never = tempdir();
    *fake.answer.lock().unwrap() = never.path().join("never-written").display().to_string();

    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions::default(),
        )
        .unwrap_or_else(|e| panic!("approve: {e:#}"));
    wait_pane(
        &env,
        &env.session_name("worker"),
        "the fake started",
        |screen| has_line(screen, "● trusted"),
    );
    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_PROCESSING
            && info.event == EVENT_TRUSTED
            && info.detail.contains(TRUST_FOLDER),
        "status = {} ({}: {}), want processing (trusted)",
        info.status,
        info.event,
        info.detail
    );
}

// deny picks "No, exit": the CLI exits, and status says it ended because
// of that.
#[test]
fn test_trust_prompt_denied() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_at_trust_prompt(
        &mut env,
        SpawnOptions {
            name: "worker".into(),
            ..Default::default()
        },
    );

    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Deny,
            PermissionOptions {
                expect_tool: TRUST_TOOL.into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("deny: {e:#}"));
    await_::until("the fake answered", || {
        let a = fake.answered();
        let ok = !a.is_empty();
        (a, ok)
    });
    assert_eq!(fake.answered(), "no");
    let info = status_is(&env, "worker", "ended");
    assert!(
        info.end_reason == END_QUIT && info.event == EVENT_TRUST_DECLINED,
        "ended ({}), event {:?}; want quit, {EVENT_TRUST_DECLINED}",
        info.end_reason.as_str(),
        info.event
    );
}

// A first message typed at the trust prompt was lost (Claude's dialog
// ignores a paste), or its Enter would pick "No, exit". Spawn holds it
// instead, and approving the prompt has it delivered.
#[test]
fn test_trust_prompt_spawn_holds_the_task() {
    require_termd!();
    let mut env = setup_test_env();
    let dir = tempdir();
    let task = dir.path().join("task.md").display().to_string();
    std::fs::write(&task, "say ok").unwrap();
    let fake = spawn_at_trust_prompt(
        &mut env,
        SpawnOptions {
            name: "worker".into(),
            task_file: task.clone(),
            ..Default::default()
        },
    );

    let held = env.store.get_pending_messages("worker", 10).unwrap();
    assert!(
        held.len() == 1 && held[0].content.contains(&task),
        "held = {held:?}, want the task's message"
    );
    assert_eq!(fake.answered(), "", "the task's Enter answered the prompt");
    let info = status_is(&env, "worker", "waiting");
    assert!(
        info.is_trust_prompt() && info.held == 1,
        "status {} ({}), {} held; want the trust prompt with the task held",
        info.status,
        info.detail,
        info.held
    );

    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions::default(),
        )
        .unwrap_or_else(|e| panic!("approve: {e:#}"));
    assert_eq!(
        env.delivering(),
        vec!["worker".to_string()],
        "deliverers started"
    );
}

/// A CLI that asks about each folder it is given (up to the first flag) in
/// turn, drawn as
/// FAKE_TRUST_CLI draws its one: as a CLI may ask about the folder it was
/// started in under each of its names. Each answer goes on a line of $1
/// ("yes <folder>" or "no <folder>": No exits); once all are trusted it
/// writes "trusted" and sits there as a started CLI.
const FAKE_TRUST_EACH: &str = r#"
ans=$1; shift
draw() {
  printf '\033[2J\033[H Accessing workspace:\n\n %s\n\n' "$1"
  printf ' Quick safety check: Is this a project you created or one you trust? (Like your\n own code, a well-known open source project, or work from your team).\n\n'
  printf " Claude Code'll be able to read, edit, and execute files here.\n\n"
  if [ $sel = no ]; then printf ' ❯ No, exit\n   Yes, I trust this folder\n'
  else printf '   No, exit\n ❯ Yes, I trust this folder\n'; fi
  printf '\n Enter to confirm · Esc to cancel\n'
}
for folder in "$@"; do
  case $folder in -*) break ;; esac # the flags spawn adds
  sel=no
  draw "$folder"
  while IFS= read -rsn1 k; do
    case $k in
      $'\033') IFS= read -rsn2 -t 1 rest
        case $rest in '[A') sel=no ;; '[B') sel=yes ;; esac
        draw "$folder" ;;
      '') printf '%s %s\n' $sel "$folder" >> "$ans"
        [ $sel = yes ] || exit 0
        break ;;
    esac
  done
done
printf 'trusted\n' >> "$ans"
printf '\033[2J\033[H● trusted\n'
exec sleep 600
"#;

/// Spawns agent name on FAKE_TRUST_EACH asking about folders, and returns
/// the file its answers go to.
fn spawn_trust_each(
    env: &mut TestEnv,
    mut opts: SpawnOptions,
    folders: &[&str],
) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir();
    let script = dir.path().join("claude.sh");
    let answers = dir.path().join("answers");
    std::fs::write(&script, FAKE_TRUST_EACH).unwrap();
    let mut launch = format!(
        "bash {} {}",
        shell_quote(&script.display().to_string()),
        shell_quote(&answers.display().to_string())
    );
    for f in folders {
        launch += &format!(" {}", shell_quote(f));
    }
    let launch2 = launch.clone();
    let done = answers.clone();
    register_claude_fake(
        &env.backends,
        ClaudeOverrides {
            name: "faketrust".into(),
            hooks_installed: Some(Box::new(|_| Ok(()))),
            launch_command: Some(Box::new(move || launch.clone())),
            launch_command_with_session: Some(Box::new(move |_| launch2.clone())),
            native_status: Some(Box::new(move |_| {
                if std::fs::read_to_string(&done)
                    .unwrap_or_default()
                    .ends_with("trusted\n")
                {
                    return (Status::Idle, true, None);
                }
                (Status::Unknown, false, None)
            })),
            pane_session: Some(Box::new(|_| Ok(None::<PaneSession>))),
            ..Default::default()
        },
    );
    env.cfg_mut().submit_wait = Duration::from_millis(300);
    env.cfg_mut().agent_binary = "/opt/gomuxai/agent".into();
    opts.backend = "faketrust".into();
    env.service
        .spawn(&bg(), opts)
        .unwrap_or_else(|e| panic!("spawn: {e:#}"));
    (dir, answers)
}

/// A folder and a symlink to it: (link, real), real with every link in its
/// own path resolved too (as a CLI's getcwd sees it).
fn symlinked_folder(root: &std::path::Path) -> (String, String) {
    let real = root.join("real-project");
    std::fs::create_dir_all(&real).unwrap();
    let link = root.join("linked-project");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let real = std::fs::canonicalize(&real).unwrap().display().to_string();
    (link.display().to_string(), real)
}

// A folder's names: as given, and with symlinks resolved (macOS's /var/tmp
// is /private/var/tmp), or under "~".
#[test]
fn test_same_folder_resolves_symlinks() {
    let root = tempdir();
    let (link, real) = symlinked_folder(root.path());
    assert!(same_folder(&real, &link));
    assert!(same_folder(&link, &real));
    assert!(same_folder(&format!("{link}/"), &real));
    assert!(!same_folder("/srv/elsewhere", &link));
    assert!(!same_folder("", &link));
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        assert!(same_folder("~", &home));
    }
}

// spawn --trust-workspace in a folder reached through a symlink: the CLI
// asks about it under the name it was given, then under its real path (or
// the other way round); spawn answers both, and the agent starts rather than
// waiting at the second prompt.
#[test]
fn test_trust_workspace_answers_each_name_of_a_symlinked_folder() {
    require_termd!();
    let mut env = setup_test_env();
    let root = tempdir();
    let (link, real) = symlinked_folder(root.path());
    let (_dir, answers) = spawn_trust_each(
        &mut env,
        SpawnOptions {
            name: "worker".into(),
            cwd: link.clone(),
            trust_workspace: true,
            ..Default::default()
        },
        &[&link, &real],
    );
    await_::until("both prompts answered", || {
        let a = std::fs::read_to_string(&answers).unwrap_or_default();
        let ok = a.ends_with("trusted\n");
        (
            format!(
                "{a}\nscreen:\n{}",
                env.tmux
                    .capture(&env.session_name("worker"), 0)
                    .unwrap_or_default()
            ),
            ok,
        )
    });
    assert_eq!(
        std::fs::read_to_string(&answers).unwrap(),
        format!("yes {link}\nyes {real}\ntrusted\n")
    );
    status_is(&env, "worker", "idle");
}

// --trust-workspace trusts the agent's folder, under any name, and no other:
// a prompt about another folder after it is left for a person, and status
// describes that one (trust: <folder>), which approve --expect trust answers.
#[test]
fn test_trust_workspace_leaves_another_folder_to_a_person() {
    require_termd!();
    let mut env = setup_test_env();
    let root = tempdir();
    let (link, real) = symlinked_folder(root.path());
    let other = root.path().join("another-project");
    std::fs::create_dir_all(&other).unwrap();
    let other = other.display().to_string();
    let (_dir, answers) = spawn_trust_each(
        &mut env,
        SpawnOptions {
            name: "worker".into(),
            cwd: link.clone(),
            trust_workspace: true,
            ..Default::default()
        },
        &[&real, &other],
    );
    let info = status_is(&env, "worker", "waiting");
    assert!(
        info.is_trust_prompt() && info.detail == format!("trust: {other}"),
        "status {} ({}: {}), want the trust prompt for {other}",
        info.status,
        info.tool,
        info.detail
    );
    assert_eq!(
        std::fs::read_to_string(&answers).unwrap(),
        format!("yes {real}\n")
    );

    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions {
                expect_tool: TRUST_TOOL.into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("approve: {e:#}"));
    await_::until("the other folder answered", || {
        let a = std::fs::read_to_string(&answers).unwrap_or_default();
        let ok = a.ends_with("trusted\n");
        (a, ok)
    });
    assert_eq!(
        std::fs::read_to_string(&answers).unwrap(),
        format!("yes {real}\nyes {other}\ntrusted\n")
    );
}

// approve answers what status described: a trust dialog on screen where
// status said permission prompt is refused, not sent "1".
#[test]
fn test_respond_permission_refuses_a_trust_prompt_for_a_permission_prompt() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");
    let payload = r#"{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    env.service
        .send_unchecked(
            &bg(),
            "worker",
            r"printf ' Quick safety check: Is this a project you created or one you trust?\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm\n'; read -r -n 1 _",
        )
        .unwrap();
    wait_pane(
        &env,
        &env.session_name("worker"),
        "the trust dialog printed",
        |screen| has_line(screen, "Enter to confirm"),
    );
    for resp in [PermissionResponse::Approve, PermissionResponse::Deny] {
        let err = env
            .service
            .respond_permission("worker", resp, PermissionOptions::default())
            .unwrap_err();
        assert!(
            err.to_string().contains("workspace trust prompt"),
            "{resp:?}: err = {err:#}, want a refusal naming the trust prompt"
        );
    }
    // The report still describes the Bash prompt: nothing was answered.
    assert_eq!(env.state_value("worker", STATE_KEY_TOOL), "Bash");
}

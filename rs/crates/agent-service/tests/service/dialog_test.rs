//! Go tests/service/dialog_test.go.
//!
//! Claude's auto-mode offer ("Teach auto mode about your environment?")
//! comes up when a turn ends, fires no hook, and leaves its session registry
//! saying idle. A message sent then was reported "Sent (it was idle)", but
//! the offer took its keys and its Enter accepted the offer (friction #29).

use std::sync::Arc;
use std::time::Duration;

use agent_backend::{PaneSession, PermissionResponse, Status};
use agent_base::util::shell_quote;
use agent_service::*;

use crate::common::*;
use crate::require_termd;

/// atOffer returns a Claude agent "worker" whose last turn ended (reported
/// idle) with the offer on screen; with holds, its messages are held (as
/// for a spawned agent).
fn at_offer(holds: bool) -> (ClaudeEnv, Arc<ClaudeTerminal>) {
    let term = ClaudeTerminal::new(|s| s.offer = true);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    if holds {
        ce.set_state(STATE_KEY_SPAWN_ID, "s1");
    }
    turn_started(&ce.store, "worker");
    ce.svc
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                ..Default::default()
            },
        )
        .unwrap();
    (ce, term)
}

// The offer reads as waiting, on tool "dialog", naming it and what Enter
// would pick.
#[test]
fn test_dialog_status_is_waiting() {
    let (ce, term) = at_offer(true);
    let info = ce.svc.status_info("worker").unwrap();
    assert!(
        info.is_dialog() && info.tool == DIALOG_TOOL && info.source == STATUS_SOURCE_SCRAPE,
        "status {}, tool {:?}, source {} ({}); want waiting at a dialog",
        info.status,
        info.tool,
        info.source,
        info.detail
    );
    assert_eq!(
        info.detail,
        "dialog: Teach auto mode about your environment?; Enter picks: 1. Yes"
    );
    assert_eq!(
        dialog_title(&info.status, &info.detail).as_deref(),
        Some("Teach auto mode about your environment?")
    );

    // Gone, the agent is idle again.
    term.set(|s| s.offer = false);
    let info = ce.svc.status_info("worker").unwrap();
    assert_eq!(
        info.status, AGENT_STATUS_IDLE,
        "status with the dialog gone ({})",
        info.detail
    );
}

// A message to an agent that holds messages is held, untyped, until the
// dialog is gone; deny closes it with Escape, and the held message is then
// typed in.
#[test]
fn test_dialog_send_holds_until_deny_closes_it() {
    let (ce, term) = at_offer(true);

    let res = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions {
                from: "lead".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(res.held && res.typed == 0, "Send = {res:?}; want held");
    assert!(
        res.held_until.contains("dialog is gone") && res.detail.starts_with("dialog: "),
        "held until {:?} ({}); want until the dialog is gone",
        res.held_until,
        res.detail
    );
    let (enters, submitted, _) = term.state();
    assert!(
        enters == 0 && submitted.is_empty() && term.offer_answers().is_empty(),
        "typed into the dialog: {enters} Enters, submitted {submitted:?}, offer answered {:?}",
        term.offer_answers()
    );
    assert!(
        !ce.delivering.lock().unwrap().is_empty(),
        "no deliverer started: no hook will deliver it"
    );

    assert!(
        ce.svc
            .respond_permission(
                "worker",
                PermissionResponse::Deny,
                PermissionOptions {
                    expect_tool: "Bash".into(),
                    ..Default::default()
                }
            )
            .is_err(),
        "deny --expect Bash answered a dialog"
    );
    assert!(
        ce.svc
            .respond_permission(
                "worker",
                PermissionResponse::ApproveAlways,
                PermissionOptions::default()
            )
            .is_err(),
        "approve --always answered a dialog, which has no always"
    );
    ce.svc
        .respond_permission(
            "worker",
            PermissionResponse::Deny,
            PermissionOptions {
                expect_tool: DIALOG_TOOL.into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("deny: {e:#}"));
    assert_eq!(
        term.offer_answers().join(","),
        "Esc",
        "offer answered; want closed with Escape"
    );

    let (n, err) = ce
        .svc
        .deliver_held(&bg(), "worker", &DeliverOptions::default());
    assert!(
        err.is_ok() && n == 1,
        "DeliverHeld = {n}, {err:?}; want the held message typed"
    );
    let (_, submitted, _) = term.state();
    assert_eq!(submitted, vec!["[from lead@hub] run the tests".to_string()]);
}

// approve presses Enter, picking the highlighted option.
#[test]
fn test_dialog_approve_picks_the_highlighted_option() {
    let (ce, term) = at_offer(true);
    let info = ce
        .svc
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions {
                expect_tool: DIALOG_TOOL.into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(info.is_dialog(), "approve = {info:?}");
    assert_eq!(term.offer_answers().join(","), "Yes");
    assert!(
        ce.svc
            .respond_permission(
                "worker",
                PermissionResponse::Approve,
                PermissionOptions::default()
            )
            .is_err(),
        "approve with the dialog gone pressed a key"
    );
}

// An agent that doesn't hold messages has the send refused, saying how to
// get past the dialog, with nothing typed.
#[test]
fn test_dialog_send_refused_where_not_held() {
    let (ce, term) = at_offer(false);
    let err = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions::default(),
        )
        .unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains("shows a dialog (Teach auto mode about your environment?)")
            && s.contains("agent deny worker")
            && s.contains(r#""1. Yes""#),
        "err {s}; want the dialog named, and how to close it"
    );
    let (enters, _, _) = term.state();
    assert!(
        enters == 0 && term.offer_answers().is_empty(),
        "typed into the dialog: {enters} Enters, answers {:?}",
        term.offer_answers()
    );
}

// A dialog that comes up between send's look at the screen and the typing
// takes the text: no Enter goes after it, which would accept it.
#[test]
fn test_submit_no_enter_into_a_dialog_that_took_the_text() {
    let term = ClaudeTerminal::new(|s| s.offer_on_read = true);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    turn_started(&ce.store, "worker");
    let err = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions::default(),
        )
        .unwrap_err();
    let took = err
        .downcast_ref::<DialogTookError>()
        .unwrap_or_else(|| panic!("err {err:#}; want DialogTookError"));
    assert_eq!(took.dialog.title, "Teach auto mode about your environment?");
    let (enters, _, _) = term.state();
    assert!(
        enters == 0 && term.offer_answers().is_empty(),
        "{enters} Enters, offer answered {:?}; want no Enter",
        term.offer_answers()
    );
}

// A box that never shows the message, with no dialog to blame, still gets
// its Enter, but send doesn't call it sent: it says the box never showed
// it, and status keeps saying so until a turn starts.
#[test]
fn test_submit_says_when_the_box_never_showed_the_message() {
    let term = ClaudeTerminal::new(|s| s.deaf_type = true);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    turn_started(&ce.store, "worker");
    let res = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions {
                now: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(res.typed, 1);
    assert!(
        res.unchecked.contains("never showed the message"),
        "Unchecked = {:?}; want that the box never showed it",
        res.unchecked
    );
    let (why, _) = ce.svc.unchecked("worker").expect("Unchecked recorded");
    assert_eq!(why, res.unchecked);
}

/// fakeDialogCLI stands in for Claude Code starting with a dialog up, drawn
/// above its input box as the fullscreen renderer draws the auto-mode offer
/// (tests/backend/testdata/claude-offer-above-box.txt is a real one). Typed
/// text goes to the dialog ($1.typed); Escape closes it ($1: esc) and leaves
/// the bare input box; Enter accepts it ($1: yes).
const FAKE_DIALOG_CLI: &str = r#"
rule=$(printf '─%.0s' $(seq 60))
printf '\033[2J\033[H● ok\n\n%s\n' "$rule"
printf '  Teach auto mode about your environment?\n\n'
printf '  ❯ 1. Yes\n    2. Not now\n    3. Don'"'"'t show again\n\n'
printf '  Enter to confirm · Esc to cancel\n%s\n❯ \n%s\n  ⏵⏵ auto mode on\n' "$rule" "$rule"
while IFS= read -rsn1 k; do
  case $k in
    $'\033') IFS= read -rsn2 -t 1 rest
      if [ -z "$rest" ]; then
        printf 'esc\n' > "$1"
        printf '\033[2J\033[H● ok\n\n%s\n❯ \n%s\n  ⏵⏵ auto mode on\n' "$rule" "$rule"
        exec sleep 600
      fi ;;
    '') printf 'yes\n' > "$1"; exit 0 ;;
    *) printf '%s' "$k" >> "$1.typed" ;;
  esac
done
"#;

// A dialog up when spawn types the first message would take it: spawn
// holds the task instead, starts the deliverer, and status says why it
// waits.
#[test]
fn test_dialog_spawn_holds_the_task() {
    require_termd!();
    let mut env = setup_test_env();
    let dir = tempdir();
    let script = dir.path().join("claude.sh").display().to_string();
    let answer = dir.path().join("answer").display().to_string();
    std::fs::write(&script, FAKE_DIALOG_CLI).unwrap();
    let launch = format!("bash {} {}", shell_quote(&script), shell_quote(&answer));
    let launch2 = launch.clone();
    // fakeDialogBackend is the claude backend launching fakeDialogCLI; like
    // Claude's registry, its native status says idle with the dialog up.
    register_claude_fake(
        &env.backends,
        ClaudeOverrides {
            name: "fakedialog".into(),
            hooks_installed: Some(Box::new(|_| Ok(()))),
            launch_command: Some(Box::new(move || launch.clone())),
            launch_command_with_session: Some(Box::new(move |_| launch2.clone())),
            native_status: Some(Box::new(|_| (Status::Idle, true, None))),
            pane_session: Some(Box::new(|_| Ok(None::<PaneSession>))),
            ..Default::default()
        },
    );
    // How long spawn waits for the CLI to draw its input box or a dialog:
    // the wait ends as soon as the dialog shows, so a generous one costs
    // nothing, while a short one under load (a full parallel test run)
    // looks before the fake has drawn anything.
    env.cfg_mut().submit_wait = Duration::from_secs(3);
    env.cfg_mut().agent_binary = "/opt/gomuxai/agent".into();
    let task = dir.path().join("task.md").display().to_string();
    std::fs::write(&task, "say ok").unwrap();
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "worker".into(),
                backend: "fakedialog".into(),
                task_file: task.clone(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn: {e:#}"));

    let held = env.store.get_pending_messages("worker", 10).unwrap();
    assert!(
        held.len() == 1 && held[0].content.contains(&task),
        "held = {held:?}, want the task's message"
    );
    let typed = std::fs::read_to_string(format!("{answer}.typed")).unwrap_or_default();
    assert!(
        typed.is_empty(),
        "the task was typed into the dialog: {typed:?}"
    );
    assert_eq!(
        env.delivering(),
        vec!["worker".to_string()],
        "deliverers started"
    );
    let info = status_is(&env, "worker", "waiting");
    assert!(
        info.is_dialog() && info.held == 1,
        "status {} ({}), {} held; want the dialog with the task held",
        info.status,
        info.detail,
        info.held
    );

    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Deny,
            PermissionOptions {
                expect_tool: DIALOG_TOOL.into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("deny: {e:#}"));
    wait_pane(
        &env,
        &env.session_name("worker"),
        "the dialog closed",
        |screen| !screen.contains("Teach auto mode"),
    );
    assert_eq!(
        std::fs::read_to_string(&answer).unwrap_or_default().trim(),
        "esc",
        "dialog answered; want esc"
    );
    status_is(&env, "worker", "idle");
}

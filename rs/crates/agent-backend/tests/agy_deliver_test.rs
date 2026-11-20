//! Go tests/backend/agy_deliver_test.go.

use agent_backend::namedagents;
use agent_backend::*;
use regex::Regex;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    #[serde(rename = "injectSteps", default)]
    inject_steps: Vec<Step>,
    #[serde(rename = "terminationBehavior", default)]
    termination_behavior: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    #[serde(rename = "userMessage")]
    user_message: Option<String>,
}

/// Decodes PostInvocation's delivery output strictly, as agy's protojson
/// does: any field but these two is an error.
fn agy_output(out: &[u8]) -> Vec<String> {
    let v: Output = serde_json::from_slice(out)
        .unwrap_or_else(|e| panic!("delivery output {}: {e}", String::from_utf8_lossy(out)));
    let steps: Vec<String> = v
        .inject_steps
        .into_iter()
        .map(|s| s.user_message.expect("step without a userMessage"))
        .collect();
    assert!(
        !steps.is_empty() && v.termination_behavior == "force_continue",
        "delivery output {}: want userMessage steps and force_continue",
        String::from_utf8_lossy(out)
    );
    steps
}

#[test]
fn test_agy_post_invocation_delivery_only() {
    let b = namedagents::new_agy_backend();
    let r = b
        .parse_hook_event(
            "PostInvocation",
            br#"{"conversationId":"c1","invocationNum":0}"#,
        )
        .unwrap()
        .unwrap();
    assert!(
        r.delivery_only && b.can_deliver(&r),
        "PostInvocation: want delivery_only and can_deliver"
    );
    for event in ["PreInvocation", "PostToolUse", "Stop"] {
        let r = b
            .parse_hook_event(event, br#"{"conversationId":"c1","fullyIdle":true}"#)
            .unwrap()
            .unwrap();
        assert!(
            !b.can_deliver(&r) && !r.delivery_only,
            "{event}: want neither (only PostInvocation delivers)"
        );
    }
    assert_eq!(b.delivery_point(), "its current step ends");
}

/// One userMessage per message, tag kept, text intact; nothing to deliver
/// prints nothing (force_continue alone makes the model invent a reply).
#[test]
fn test_agy_delivery_output() {
    let b = namedagents::new_agy_backend();
    assert!(
        b.delivery_output(&[]).is_empty(),
        "delivery_output(nil) should be empty"
    );
    let msgs = vec![
        "[from lead@hub] first".to_string(),
        "[from a person on hub] two lines\n\"quoted\" `code` tab\tand é ✓".to_string(),
        // Go's test passes invalid UTF-8 here; a Rust String can't hold it,
        // so it arrives already replaced.
        String::from_utf8_lossy(b"[from lead@hub] bad utf-8: \xff\xfe").into_owned(),
    ];
    let steps = agy_output(&b.delivery_output(&msgs));
    assert!(
        steps.len() == 3 && steps[0] == msgs[0] && steps[1] == msgs[1],
        "steps = {steps:?}"
    );
    assert!(
        steps[2].starts_with("[from lead@hub] bad utf-8: "),
        "step 3 = {:?}",
        steps[2]
    );
    // The bytes as Go writes them.
    assert_eq!(
        String::from_utf8(b.delivery_output(&["<x>".to_string()])).unwrap(),
        "{\"injectSteps\":[{\"userMessage\":\"\\u003cx\\u003e\"}],\"terminationBehavior\":\"force_continue\"}"
    );
}

/// agy cuts a userMessage at about 192 KB, silently, so a big message goes
/// in labeled parts that each fit, and together hold all of it.
#[test]
fn test_agy_delivery_output_splits_big_messages() {
    let b = namedagents::new_agy_backend();
    let line = format!("{} line of a big message\n", "é".repeat(50));
    let big = format!("[from lead@hub] {}END-CODEWORD", line.repeat(3000));
    let steps = agy_output(&b.delivery_output(&["[from lead@hub] small".to_string(), big.clone()]));
    assert_eq!(steps[0], "[from lead@hub] small");
    let parts = &steps[1..];
    assert!(
        parts.len() >= 4,
        "{} bytes went in {} parts, want at least 4",
        big.len(),
        parts.len()
    );
    let label = Regex::new(
        r"(?m)^\[part \d+/\d+ of the message above\]\n|\n\[part \d+/\d+: the message goes on in the next prompt\]$",
    )
    .unwrap();
    let mut joined = String::new();
    for (i, p) in parts.iter().enumerate() {
        assert!(
            p.len() <= 64 << 10,
            "part {} is {} bytes, over 64 KB",
            i + 1,
            p.len()
        );
        if i == 0 {
            assert!(
                p.starts_with("[from lead@hub] "),
                "part 1 starts {:?}, want the sender's tag",
                &p[..30]
            );
        }
        joined.push_str(&label.replace_all(p, ""));
    }
    assert!(
        joined == big,
        "the parts, labels removed, don't add up to the message"
    );
    assert!(
        parts[1].starts_with("[part 2/"),
        "part 2 starts {:?}",
        &parts[1][..20]
    );
}

#[test]
fn test_agy_ask_question_summary() {
    let b = namedagents::new_agy_backend();
    let payload = br#"{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[{"options":["red","green","blue"],"question":"Which color do you prefer?"},{"options":["a","b"],"question":"And?"}]}}}"#;
    let r = b.parse_hook_event("PreToolUse", payload).unwrap().unwrap();
    assert_eq!(
        r.detail,
        "ask_question: Which color do you prefer? (red, green, blue); 1 more question"
    );
    let none =
        br#"{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[]}}}"#;
    assert_eq!(
        b.parse_hook_event("PreToolUse", none)
            .unwrap()
            .unwrap()
            .detail,
        "ask_question"
    );
    let three = br#"{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[{"question":" Q "},{"question":"2"},{"question":"3"}]}}}"#;
    assert_eq!(
        b.parse_hook_event("PreToolUse", three)
            .unwrap()
            .unwrap()
            .detail,
        "ask_question: Q; 2 more questions"
    );
    let nameless = br#"{"conversationId":"c1","toolCall":{"args":{"x":1}}}"#;
    assert_eq!(
        b.parse_hook_event("PreToolUse", nameless)
            .unwrap()
            .unwrap()
            .detail,
        "tool"
    );
}

const AGY_RULE: &str =
    "────────────────────────────────────────────────────────────────────────────────";

/// The bottom of an agy pane (ANSI-stripped): body lines, then the input
/// box holding input, then footer.
fn agy_screen(body: &[&str], input: &str, footer: &str) -> String {
    let boxed = format!(">{input}");
    let mut lines: Vec<&str> = body.to_vec();
    lines.extend_from_slice(&[AGY_RULE, &boxed, AGY_RULE, footer, "", ""]);
    lines.join("\n")
}

/// agy_screen with agy's panel of running background tasks between the
/// input box and the footer, as agy 1.5.3 draws it.
fn agy_tasks_screen(body: &[&str], input: &str, footer: &str) -> String {
    let boxed = format!(">{input}");
    let mut lines: Vec<&str> = body.to_vec();
    lines.extend_from_slice(&[
        AGY_RULE,
        &boxed,
        AGY_RULE,
        "  ● [15:22:44] sleep 300 running",
        AGY_RULE,
        footer,
        "",
        "",
    ]);
    lines.join("\n")
}

fn agy_question_screen() -> String {
    [
        "? Which color do you prefer?",
        "",
        "Question",
        AGY_RULE,
        "",
        "Question 1/1: Which color do you prefer?",
        "",
        "> 1. red",
        "  2. green",
        "  3. blue",
        "  4. Write-in...",
        "",
        "  ↑/↓ Navigate · enter Select · esc Skip",
        "esc to cancel                                              Gemini 4 Argon · high",
    ]
    .join("\n")
}

/// Screens from agy 1.4.7 (and 1.5.3's tasks panel), trimmed.
#[test]
fn test_agy_at_prompt() {
    let b = namedagents::new_agy_backend();
    let reply: &[&str] = &["> [from lead@hub] Reply READY.", "", "  READY", ""];
    let interrupted: &[&str] = &[
        "● Ran (sleep 40; echo slept) (ctrl+o to expand)",
        "",
        "  ⎿  Interrupted · What should Antigravity CLI do instead?",
    ];
    let running: &[&str] = &[
        "● Running (sleep 40; echo slept) (ctrl+o to expand)",
        "⣟  Running command...",
        "└ Tip: Press ctrl+o to expand or collapse tool details.",
    ];
    let mut running_queued = running.to_vec();
    running_queued.push("▸ typed while busy (alt+m to send immediately)");
    let idle_footer =
        "? for shortcuts                                            Gemini 4 Argon · high";
    let typed_footer =
        "                                                           Gemini 4 Argon · high";
    let busy_footer =
        "esc to cancel                                              Gemini 4 Argon · high";
    let tasks_idle =
        "? for shortcuts                       Gemini 4 Argon · high · 1 task(s) · /tasks";
    let tasks_typed =
        "                                      Gemini 4 Argon · high · 1 task(s) · /tasks";
    let tasks_busy =
        "esc to cancel                         Gemini 4 Argon · high · 1 task(s) · /tasks";
    let tests: Vec<(&str, String, Option<&str>)> = vec![
        ("idle", agy_screen(reply, "", idle_footer), Some("")),
        (
            "idle, text typed",
            agy_screen(reply, " half typed text", typed_footer),
            Some(""),
        ),
        (
            "interrupted",
            agy_screen(interrupted, "", idle_footer),
            Some("interrupted"),
        ),
        (
            "interrupted, queued text put back",
            agy_screen(interrupted, " typed while busy", typed_footer),
            Some("interrupted"),
        ),
        ("running", agy_screen(running, "", busy_footer), None),
        (
            "running, text typed",
            agy_screen(running, " typed while busy", busy_footer),
            None,
        ),
        (
            "running, text queued",
            agy_screen(
                &running_queued,
                "",
                "  Press up to edit queued messages                         Gemini 4 Argon · high",
            ),
            None,
        ),
        (
            "idle, background task running",
            agy_tasks_screen(reply, "", tasks_idle),
            Some(""),
        ),
        (
            "idle, background task running, text typed",
            agy_tasks_screen(reply, " half typed", tasks_typed),
            Some(""),
        ),
        (
            "interrupted, background task running",
            agy_tasks_screen(interrupted, "", tasks_idle),
            Some("interrupted"),
        ),
        (
            "running, background task running",
            agy_tasks_screen(running, "", tasks_busy),
            None,
        ),
        ("question dialog", agy_question_screen(), None),
        ("no box", "user@laptop $ agy\n".to_string(), None),
    ];
    for (name, screen, want) in tests {
        assert_eq!(b.at_prompt(&screen).as_deref(), want, "{name}");
    }
}

/// agy's input line, styled as tmux capture-pane -e shows it.
#[test]
fn test_agy_input_text() {
    let b = namedagents::new_agy_backend();
    let rule = format!("\x1b[90m{AGY_RULE}");
    let screen = |input: &str| {
        [
            "  READY",
            "",
            rule.as_str(),
            &format!("\x1b[94m>\x1b[39m{input}"),
            rule.as_str(),
            "\x1b[39m                                                           \x1b[2mGemini 4 Argon · high",
        ]
        .join("\n")
    };
    assert_eq!(b.input_text(&screen("")).as_deref(), Some(""), "empty");
    assert_eq!(
        b.input_text(&screen(" half typed text")).as_deref(),
        Some("half typed text"),
        "typed"
    );
    assert_eq!(
        b.input_text(&agy_question_screen()),
        None,
        "question dialog: found an input line"
    );
    // agy 1.5.8 and on draw a mode hint in the empty input line after every
    // turn, in grey (SGR 90; captured from 1.5.9). Read as typed text, it
    // held every send to an idle agy agent forever (friction #45, #47).
    let hint =
        " \x1b[90mAuto mode: edits & safe commands auto-approved (shift+tab to cycle)\x1b[39m";
    assert_eq!(
        b.input_text(&screen(hint)).as_deref(),
        Some(""),
        "the mode hint"
    );
    // Whatever the hint says: it is the style that marks it.
    assert_eq!(
        b.input_text(&screen(
            " \x1b[38;5;244mPlan mode: some future wording\x1b[39m"
        ))
        .as_deref(),
        Some(""),
        "a 256-color grey hint"
    );
    assert_eq!(
        b.input_text(&screen(" \x1b[38;2;128;128;128mtruecolor grey\x1b[0m"))
            .as_deref(),
        Some(""),
        "an RGB grey hint"
    );
    // Text a person typed is drawn in the default (or another) color.
    for typed in [
        " hello typed",
        " \x1b[37mwhite\x1b[39m",
        " \x1b[38;2;250;250;250malmost white\x1b[0m",
    ] {
        let got = b.input_text(&screen(typed));
        assert!(
            got.as_deref().is_some_and(|t| !t.is_empty()),
            "typed {typed:?}: {got:?}; want its text"
        );
    }

    // A background task's panel sits between the box and the footer (agy
    // 1.5.3): a send to the idle agent was held as "its input line isn't on
    // screen" (friction #39).
    let tasks = |input: &str| {
        [
            "  started.",
            "",
            rule.as_str(),
            &format!("\x1b[94m>\x1b[39m{input}"),
            rule.as_str(),
            "\x1b[39m  \x1b[33m●\x1b[39m [15:22:44] \x1b[94msleep 300\x1b[39m \x1b[33mrunning\x1b[39m",
            rule.as_str(),
            "? for shortcuts\x1b[39m                       \x1b[2mGemini 4 Argon · high · 1 task(s) · /tasks",
        ]
        .join("\n")
    };
    assert_eq!(
        b.input_text(&tasks("")).as_deref(),
        Some(""),
        "empty, tasks panel"
    );
    assert_eq!(
        b.input_text(&tasks(" half typed")).as_deref(),
        Some("half typed"),
        "typed, tasks panel"
    );
    // The panel is never taken for the box, nor is a box above two panels.
    let two_panels = [
        rule.as_str(),
        "\x1b[94m>\x1b[39m old",
        rule.as_str(),
        "  ● one",
        rule.as_str(),
        "  ● two",
        rule.as_str(),
        "footer",
    ]
    .join("\n");
    assert_eq!(b.input_text(&two_panels), None, "a box above two panels");
}

fn read_screen(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name);
    std::fs::read_to_string(path).expect("testdata")
}

/// agy 1.5.x's screens, synthesized from what review-3 (2.1, 2.2) captured,
/// as agent capture shows them (unstyled): its idle prompt draws a mode
/// hint in the input box, which the scrape regexes never matched (idle read
/// unknown), while "Thinking" and a bare ">" quote in a reply did match
/// (processing, idle) whatever the prompt showed. The prompt reader reads
/// them right, and status goes by it for agy.
#[test]
fn test_agy_1_5_screens() {
    let b = namedagents::new_agy_backend();
    let idle = read_screen("agy-1.5-idle.txt");
    let busy = read_screen("agy-1.5-busy.txt");
    let survey = read_screen("agy-feedback-survey.txt");
    assert_eq!(
        b.at_prompt(&idle).as_deref(),
        Some(""),
        "the idle prompt with the mode hint"
    );
    assert_eq!(b.at_prompt(&busy), None, "a turn running");
    assert_eq!(b.at_prompt(&survey), None, "the feedback survey");
    // The box is there while a turn runs, and the survey covers it.
    assert!(b.input_text(&busy).is_some());
    assert_eq!(b.input_text(&survey), None);
    assert!(!b.permission_prompt_visible(&survey) && !b.question_prompt_visible(&survey));
    // What the regexes said, and why status no longer goes by them for agy.
    let idle_re = Regex::new(Backend::idle_pattern(&b)).unwrap();
    let busy_re = Regex::new(Backend::processing_pattern(&b)).unwrap();
    assert!(
        busy_re.is_match(&idle) && busy_re.is_match(&survey),
        "\"Thinking\" in scrollback"
    );
    let tail = |s: &str| s.lines().rev().take(6).collect::<Vec<_>>().join("\n");
    assert!(!idle_re.is_match(&tail(&idle)), "the hint in the box");
}

#[test]
fn test_agy_question_prompt_visible() {
    let b = namedagents::new_agy_backend();
    let question = agy_question_screen();
    assert!(
        b.question_prompt_visible(&question),
        "question dialog not seen"
    );
    assert!(
        !b.permission_prompt_visible(&question),
        "question dialog taken for a permission prompt"
    );
    let yes_no = question
        .replace("1. red", "1. Yes")
        .replace("2. green", "2. No");
    assert!(
        b.question_prompt_visible(&yes_no) && !b.permission_prompt_visible(&yes_no),
        "a Yes/No question must read as a question, not a permission prompt"
    );
    let permission = [
        "Run this command?",
        "> 1. Yes, run command",
        "  2. No, cancel",
        "  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command",
        "esc to cancel",
    ]
    .join("\n");
    assert!(
        !b.question_prompt_visible(&permission) && b.permission_prompt_visible(&permission),
        "permission dialog taken for a question"
    );
    let idle = agy_screen(&[], "", "? for shortcuts");
    assert!(
        !b.question_prompt_visible(&idle),
        "idle prompt taken for a question"
    );
}

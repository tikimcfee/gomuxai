//! `agent status` against what the Go prints (review 5, finding 2).

use agent_service::{AGENT_STATUS_WAITING, StatusReport};
use agent_store::{StateEntry, Time};

use crate::cli::testenv::TestEnv;
use super::{format_tool_input, round_duration, since_rounded};

/// worker waiting on a prompt whose call's input is input.
fn waiting_on(env: &TestEnv, input: &str) {
    env.service()
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_WAITING,
                detail: "permission: Write: /var/tmp/target.sh".into(),
                tool: "Write".into(),
                tool_input: input.into(),
                ..Default::default()
            },
        )
        .expect("reporting worker's prompt");
}

// Go keeps a valid tool_input as json.RawMessage: --json prints its text as
// it came (a number past float64, half a surrogate pair's escape), indented
// with the rest; -v decodes a string member as Go does (U+FFFD for the half
// pair) and shows any other member's text as it came.
#[test]
fn status_tool_input_reads_as_go_prints_it() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    waiting_on(
        &env,
        r#"{"id": 999999999999999999999, "big":1e400, "nested":{ "a" : [1,{"b":null}],"e":{},"l":[]}, "content":"curl x # \ud800 <&>", "file_path":"/var/tmp/target.sh"}"#,
    );
    let (out, res) = env.run(&["status", "worker", "--json"]);
    res.unwrap();
    let want = r#"  "tool_input": {
    "id": 999999999999999999999,
    "big": 1e400,
    "nested": {
      "a": [
        1,
        {
          "b": null
        }
      ],
      "e": {},
      "l": []
    },
    "content": "curl x # \ud800 <&>",
    "file_path": "/var/tmp/target.sh"
  },
"#;
    assert!(out.contains(want), "status --json:\n{out}\nwant it to hold:\n{want}");

    let (out, res) = env.run(&["status", "worker", "-v"]);
    res.unwrap();
    let want = "  input:  big: 1e400\n          content: curl x # \u{fffd} <&>\n          file_path: /var/tmp/target.sh\n          id: 999999999999999999999\n          nested: { \"a\" : [1,{\"b\":null}],\"e\":{},\"l\":[]}\n";
    assert!(out.ends_with(want), "status -v:\n{out}\nwant it to end:\n{want}");
}

#[test]
fn format_tool_input_as_go() {
    assert_eq!(format_tool_input(r#"{"k": "a\ud800b", "n": null}"#), "k: a\u{fffd}b\n          n: ");
    assert_eq!(format_tool_input("null"), "");
    assert_eq!(format_tool_input("{}"), "");
    assert_eq!(format_tool_input("[1]"), "");
}

// A report with no time (a peer that sent none, or one that didn't parse):
// Go's time.Since saturates, and Round keeps the largest Duration; status
// -v panicked on the overflow here.
#[test]
fn status_event_with_no_time() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.service()
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_WAITING,
                event: "PermissionRequest".into(),
                detail: "permission: Bash: ls".into(),
                ..Default::default()
            },
        )
        .unwrap();
    for key in ["_status", "_event"] {
        let e = env.store().get_state("worker", key).unwrap().expect(key);
        env.store()
            .set_state(&StateEntry { updated_at: Time::zero(), ..e })
            .unwrap();
    }
    let (out, res) = env.run(&["status", "worker", "-v"]);
    res.unwrap();
    assert!(
        out.contains(" (2562047h47m16.854775807s ago)\n"),
        "status -v:\n{out}"
    );
    assert_eq!(since_rounded(&Time::zero()), "2562047h47m16.854775807s");
}

#[test]
fn round_duration_as_go() {
    const S: i64 = 1_000_000_000;
    for (d, want) in [
        (1_499_999_999, S),
        (1_500_000_000, 2 * S),
        (-1_500_000_000, -2 * S),
        (-1_499_999_999, -S),
        (i64::MAX, i64::MAX),
        (i64::MIN, i64::MIN),
        (i64::MAX - S, i64::MAX - S - (i64::MAX % S) + S),
    ] {
        assert_eq!(round_duration(d, S), want, "round({d})");
    }
}

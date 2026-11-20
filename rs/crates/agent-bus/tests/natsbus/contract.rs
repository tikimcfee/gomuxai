//! Port of `tests/natsbus/contract_test.go`.

use std::time::Duration;

use agent_bus::{
    AgentRef, FrictionEntry, OP_STATUS, Time, inbox_prefix, parse_agent_ref, subject_all_status,
    subject_local, subject_rpc, subject_status, validate_agent_binary,
};

use crate::common::TestBus;

#[test]
fn test_parse_agent_ref() {
    let cases = [
        ("helper", Some(AgentRef::local("helper"))),
        ("helper@air-1", Some(AgentRef::new("helper", "air-1"))),
        ("helper@", None),
        ("@air", None),
        ("a.b@air", None),
        ("helper@air.local", None),
        ("helper@*", None),
    ];
    for (input, want) in cases {
        match (parse_agent_ref(input), want) {
            (Ok(got), Some(want)) => {
                assert_eq!(got, want, "parse_agent_ref({input:?})");
                assert_eq!(got.to_string(), input, "to_string({input:?})");
            }
            (Err(_), None) => {}
            (res, want) => panic!("parse_agent_ref({input:?}) = {res:?}, want {want:?}"),
        }
    }
}

#[test]
fn test_subjects() {
    assert_eq!(subject_rpc("air", OP_STATUS), "gomux.air.rpc.status");
    assert_eq!(
        subject_status("air", "helper"),
        "gomux.air.agent.helper.status"
    );
    assert_eq!(subject_all_status("*"), "gomux.*.agent.*.status");
    assert_eq!(subject_local("air", "bus"), "gomux.air.local.bus");
    assert_eq!(inbox_prefix("air"), "_INBOX.air");

    assert!(validate_agent_binary("/usr/local/bin/agent").is_ok());
    assert!(validate_agent_binary("relative/agent").is_err());
    assert!(validate_agent_binary("/tmp/bad space/agent").is_err());
    assert!(validate_agent_binary("/tmp/'quote'/agent").is_err());
}

#[test]
fn test_friction_entry_omitzero_resolved_at() {
    let open = FrictionEntry {
        host: "hub".to_string(),
        id: 1,
        uid: "u1".to_string(),
        status: "open".to_string(),
        votes: 0,
        resolved_at: Time::zero(),
        ..Default::default()
    };
    let json = serde_json::to_string(&open).unwrap();
    assert!(
        !json.contains("resolved_at"),
        "zero resolved_at should be omitted: {json}"
    );

    let resolved = FrictionEntry {
        resolved_at: Time::date_utc(2026, 11, 20, 12, 0, 0, 0),
        ..open
    };
    let json = serde_json::to_string(&resolved).unwrap();
    assert!(
        json.contains("\"resolved_at\":\"2026-11-20T12:00:00Z\""),
        "{json}"
    );
}

#[test]
fn test_natstest_round_trip() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let other = bus.connect_another();

    let mut sub = other.subscribe(&subject_all_status("*")).unwrap();
    other.flush().unwrap();
    bus.conn
        .publish(&subject_status("air", "helper"), b"{}".to_vec())
        .unwrap();
    bus.conn.flush().unwrap();
    sub.next_msg(Duration::from_secs(2))
        .expect("event not received");
}

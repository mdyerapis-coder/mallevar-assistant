use ironbark_core::contracts::{
    ActionId, ActionSpec, ApprovalGrant, ArtifactRead, CommandResult, CoreCommand, CoreReply, CoreEvent,
    DoctorReport, Empty, EventEnvelope, GrantId, ProjectId, ProjectPolicy, RunId, SpendPolicy, StatusReport,
};
use ironbark_core::policy::TaskAuthority;
use ironbark_core::store::objects::CapacityReport;
use ironbark_core::automation::ApprovedTarget;
use serde_json::json;

fn policy() -> ProjectPolicy {
    ProjectPolicy {
        version: 9_007_199_254_740_993,
        project_id: ProjectId::new(), allowed_connection_ids: vec![], allowed_tools: vec![],
        read_roots: vec![], write_roots: vec![], network_destinations: vec![], local_only: true,
        max_workers: 1, max_requests: 1, max_cost_microusd: Some(u64::MAX),
        spend_policy: SpendPolicy::FreeOnly, allow_unknown_cost: false,
    }
}

#[test]
fn policy_wide_values_cross_the_wire_as_exact_canonical_decimal_strings() {
    let original = policy();
    let wire = serde_json::to_value(&original).unwrap();
    assert_eq!(wire["version"], json!("9007199254740993"));
    assert_eq!(wire["max_cost_microusd"], json!("18446744073709551615"));
    let decoded: ProjectPolicy = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.version, 9_007_199_254_740_993);
    assert_eq!(decoded.max_cost_microusd, Some(u64::MAX));
}

#[test]
fn wire_policy_decoder_rejects_json_numbers_even_when_they_are_small() {
    for value in [json!(0), json!(1), json!(9_007_199_254_740_992u64)] {
        let mut wire = serde_json::to_value(policy()).unwrap();
        wire["version"] = value;
        assert!(serde_json::from_value::<ProjectPolicy>(wire).is_err(), "numeric wire authority is not exact decimal-string authority");
    }
}

#[test]
fn canonical_unsigned_wire_lexemes_and_full_range_are_enforced() {
    for value in ["", "00", "01", "+1", "-0", "-1", " 1", "1\n", "1.0", "1e3", "18446744073709551616"] {
        let mut wire = serde_json::to_value(policy()).unwrap();
        wire["version"] = json!(value);
        assert!(serde_json::from_value::<ProjectPolicy>(wire).is_err(), "accepted noncanonical version: {value:?}");
    }
    let mut wire = serde_json::to_value(policy()).unwrap();
    wire["version"] = json!("18446744073709551615");
    let decoded: ProjectPolicy = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.version, u64::MAX);
}

#[test]
fn event_sequence_and_signed_timestamps_keep_the_full_rust_range() {
    for timestamp in [i64::MIN, i64::MAX] {
        let event = EventEnvelope {
            protocol: 2, seq: u64::MAX, request_id: None, project_id: None, run_id: None,
            timestamp, payload: CoreEvent::StateChanged { entity_id: "historical-subject".into(), state: "observed".into() },
        };
        let wire = serde_json::to_value(event).unwrap();
        assert_eq!(wire["seq"], json!("18446744073709551615"));
        assert_eq!(wire["timestamp"], json!(if timestamp == i64::MIN { "-9223372036854775808" } else { "9223372036854775807" }));
        let decoded: EventEnvelope = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.timestamp, timestamp);
        assert_eq!(decoded.seq, u64::MAX);
    }
}

#[test]
fn signed_wire_decoder_refuses_noncanonical_and_overflow_values() {
    let event = EventEnvelope {
        protocol: 2, seq: 1, request_id: None, project_id: None, run_id: None, timestamp: 0,
        payload: CoreEvent::StateChanged { entity_id: "subject".into(), state: "observed".into() },
    };
    for value in [json!(0), json!("-0"), json!("+1"), json!("01"), json!("9223372036854775808"), json!("-9223372036854775809")] {
        let mut wire = serde_json::to_value(&event).unwrap();
        wire["timestamp"] = value;
        assert!(serde_json::from_value::<EventEnvelope>(wire).is_err());
    }
}

#[test]
fn every_remaining_64bit_dto_field_crosses_as_a_canonical_decimal_string() {
    // The A1 wire foundation converted ProjectPolicy/EventEnvelope; these were
    // the remaining u64/i64 fields of the exported DTO surface.
    let action = serde_json::json!({
        "id": ActionId::new(), "run_id": RunId::new(), "project_id": ProjectId::new(),
        "kind": "publish", "canonical_args": {}, "scope": {},
        "policy_version": "9007199254740993", "expected_state_digest": "b".repeat(64),
    });
    let parsed: ActionSpec = serde_json::from_value(action.clone()).unwrap();
    assert_eq!(parsed.policy_version, 9_007_199_254_740_993);
    assert_eq!(action["policy_version"], json!("9007199254740993"));

    let grant: ApprovalGrant = serde_json::from_value(json!({
        "id": GrantId::new(), "action_id": ActionId::new(), "action_digest": "c".repeat(64),
        "policy_version": "1", "expected_state_digest": "d".repeat(64),
        "expires_at": "-9223372036854775808", "revoked_at": "9223372036854775807",
    }))
    .unwrap();
    assert_eq!(grant.expires_at, i64::MIN);
    assert_eq!(grant.revoked_at, Some(i64::MAX));

    let read: ArtifactRead = serde_json::from_value(json!({
        "digest": "a".repeat(64), "bytes": "18446744073709551615", "data": "AQI=",
    }))
    .unwrap();
    assert_eq!(read.bytes, u64::MAX);

    let authority: TaskAuthority = serde_json::from_value(json!({
        "run_id": RunId::new(), "project_id": ProjectId::new(), "owner_approval_id": GrantId::new(),
        "scope": {"read_roots": [], "write_roots": [], "tools": [], "connection_ids": [], "network_destinations": []},
        "policy_version": "2", "expires_at": "1700000000000",
    }))
    .unwrap();
    assert_eq!(authority.policy_version, 2);

    let target: ApprovedTarget = serde_json::from_value(json!({
        "scope_id": "scope", "role": "owner_operator", "kind": "document_file",
        "target_id": "target", "expires_at": "1700000000000",
    }))
    .unwrap();
    assert_eq!(target.expires_at, 1_700_000_000_000);

    let capacity: CapacityReport = serde_json::from_value(json!({
        "bytes": "0", "soft_bytes": "2147483648", "hard_bytes": "10737418240", "soft_exceeded": false,
    }))
    .unwrap();
    assert_eq!(capacity.hard_bytes, 10 * 1024 * 1024 * 1024);
    // Numeric wire form for these fields is rejected everywhere, not just on
    // the foundational types.
    let mut numeric = action.clone();
    numeric["policy_version"] = json!(1);
    assert!(serde_json::from_value::<ActionSpec>(numeric).is_err(), "ActionSpec policy_version must be a decimal string");
}

#[test]
fn command_result_envelope_is_command_tagged_and_rejects_mismatched_or_echoed_values() {
    let policy = policy();
    let wire = serde_json::to_value(CoreReply::Ok { value: CommandResult::ProjectPolicyShow { value: policy.clone() } }).unwrap();
    assert_eq!(wire["type"], json!("ok"));
    assert_eq!(wire["value"]["command"], json!("project_policy_show"));
    assert_eq!(wire["value"]["value"]["version"], json!("9007199254740993"));
    let back: CoreReply = serde_json::from_value(wire).unwrap();
    match back {
        CoreReply::Ok { value: CommandResult::ProjectPolicyShow { value } } => assert_eq!(value, policy),
        other => panic!("round trip changed the command result envelope: {other:?}"),
    }
}

#[test]
fn command_result_deserialization_rejects_an_unknown_or_wrong_leaf_command() {
    let policy = policy();
    let mut wrong = serde_json::to_value(CommandResult::ProjectPolicyShow { value: policy.clone() }).unwrap();
    wrong["command"] = json!("forged_command");
    assert!(serde_json::from_value::<CommandResult>(wrong).is_err(), "unknown command tag must not deserialize");
    // An echoed envelope whose leaf is a different result (events array under
    // project_policy_show) cannot deserialize: it is neither shape nor tag
    // compatible with the requested command.
    let event = EventEnvelope {
        protocol: 2, seq: 1, request_id: None, project_id: None, run_id: None, timestamp: 0,
        payload: CoreEvent::StateChanged { entity_id: "subject".into(), state: "observed".into() },
    };
    let echoed = json!({ "command": "project_policy_show", "value": [event] });
    assert!(serde_json::from_value::<CommandResult>(echoed).is_err(), "mismatched leaf shape must not deserialize");
    // Unknown fields inside a leaf are rejected by deny_unknown_fields.
    let mut extra = serde_json::to_value(CommandResult::Status { value: StatusReport { protocol: 1, projects: 2, last_seq: 3, abandoned_requests: 0 } }).unwrap();
    extra["value"]["forged"] = json!(true);
    assert!(serde_json::from_value::<CommandResult>(extra).is_err(), "unknown leaf field must not deserialize");
}

#[test]
fn status_doctor_and_unit_results_keep_wide_counters_on_the_exact_wire() {
    let status = json!({"protocol": 1, "projects": "2", "last_seq": "18446744073709551615", "abandoned_requests": "0"});
    let report: StatusReport = serde_json::from_value(status).unwrap();
    assert_eq!(report.last_seq, u64::MAX);
    let doctor: DoctorReport = serde_json::from_value(json!({
        "protocol": 1, "sqlite_integrity": "ok", "foreign_keys": true, "schema_version": "9007199254740993", "journal_mode": "wal",
    }))
    .unwrap();
    assert_eq!(doctor.schema_version, 9_007_199_254_740_993);
    let unit = serde_json::from_value::<Empty>(json!({})).unwrap();
    assert_eq!(unit, Empty {});
    // CommandResult carries the unit leaf as `{}`, preserving the old shape.
    let release = serde_json::to_value(CommandResult::AutomationRelease { value: Empty {} }).unwrap();
    assert_eq!(release, json!({"command": "automation_release", "value": {}}));
    // CoreCommand::Events takes its cursor as a decimal string now.
    let events = serde_json::to_value(CoreCommand::Events { after: u64::MAX }).unwrap();
    assert_eq!(events["after"], json!("18446744073709551615"));
    assert_eq!(serde_json::from_value::<CoreCommand>(events).unwrap(), CoreCommand::Events { after: u64::MAX });
}

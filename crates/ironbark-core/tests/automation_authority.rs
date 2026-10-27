//! H8 invariants for action-scoped browser/desktop automation authority.
//!
//! Offline-qualifiable surface only: the `document_file` backend (real bytes on
//! a real temporary file), scope lifecycle, exact-grant enforcement and the
//! consent rules. Browser (`firefox_bridge`) and desktop (`atspi`) backends
//! need the pinned AlphaCode executor and a live session, so they stay
//! unqualified here rather than being faked.
use std::path::PathBuf;

use ironbark_core::automation::{
    check_target_in_scope, validate_consent, ApprovedTarget, AutomationBroker, ConsentKind,
    ConsentRecord, ConsentSource, Effect, ScopePhase, ScopeRole, TargetKind,
    HOST_SESSION_TARGET,
};
use ironbark_core::contracts::{
    ActionId, ActionSpec, ApprovalGrant, Digest, DisclosurePolicy, GrantId, JobId, NodeId,
    ProjectId, ProjectPolicy, RunId, SpendPolicy,
};
use ironbark_core::policy::service::{ApprovalDecision, ApprovalService, ActionOutcome};
use ironbark_core::store::{events::now_ms, Store};
use tempfile::tempdir;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    project: ProjectId,
    run: RunId,
    broker: AutomationBroker,
}

async fn setup() -> Fixture {
    let dir = tempdir().unwrap();
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let project = ProjectId::new();
    let run = RunId::new();
    let job = JobId::new();
    let node = NodeId::new();
    let root = dir.path().to_string_lossy().into_owned();
    let policy = serde_json::to_string(&ProjectPolicy {
        version: 1,
        project_id: project,
        allowed_connection_ids: vec!["local".into()],
        allowed_tools: vec!["desktop".into(), "browser".into()],
        read_roots: vec![],
        write_roots: vec![],
        network_destinations: vec!["*".into()],
        local_only: false,
        max_workers: 2,
        max_requests: 10,
        max_cost_microusd: None,
        spend_policy: SpendPolicy::FreeOnly,
        allow_unknown_cost: false,
    })
    .unwrap();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects (id,name,root,created_at) VALUES (?1,'p',?2,0)",
                rusqlite::params![project.to_string(), root],
            )?;
            tx.execute(
                "INSERT INTO policies (project_id,version,body) VALUES (?1,1,?2)",
                rusqlite::params![project.to_string(), policy],
            )?;
            tx.execute(
                "INSERT INTO jobs (id,project_id,body,state) VALUES (?1,?2,'{}','queued')",
                rusqlite::params![job.to_string(), project.to_string()],
            )?;
            tx.execute(
                "INSERT INTO nodes (id,job_id,body,state) VALUES (?1,?2,'{}','queued')",
                rusqlite::params![node.to_string(), job.to_string()],
            )?;
            tx.execute(
                "INSERT INTO runs (id,node_id,state,body) VALUES (?1,?2,'ready','{}')",
                rusqlite::params![run.to_string(), node.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    Fixture { _dir: dir, store: store.clone(), project, run, broker: AutomationBroker::new(store) }
}

fn document_target(path: &PathBuf, scope_id: &str) -> ApprovedTarget {
    ApprovedTarget {
        scope_id: scope_id.to_string(),
        role: ScopeRole::OwnerOperator,
        kind: TargetKind::DocumentFile,
        target_id: path.to_string_lossy().into_owned(),
        expires_at: now_ms() + 3_600_000,
    }
}

/// A `desktop` action naming the observed revision. `digest` is what the action
/// claims the target looked like, which the broker re-observes.
fn document_action(
    f: &Fixture,
    path: &PathBuf,
    digest: Digest,
    operation: &str,
    parameters: serde_json::Value,
) -> ActionSpec {
    serde_json::from_value(serde_json::json!({
        "id": ActionId::new(),
        "run_id": f.run,
        "project_id": f.project,
        "kind": "desktop",
        "canonical_args": {
            "target_id": path.to_string_lossy(),
            "observation_digest": digest,
            "operation": operation,
            "parameters": parameters,
        },
        "scope": { "tools": ["desktop"], "paths": [] },
        "policy_version": "1",
        "expected_state_digest": digest,
    }))
    .unwrap()
}

async fn approved_grant(f: &Fixture, action: &ActionSpec) -> ApprovalGrant {
    let approvals = ApprovalService::new(f.store.clone());
    let now = now_ms();
    approvals.record(action.clone(), now).await.unwrap();
    approvals
        .decide(action.id, ApprovalDecision::Approve { expires_at: now + 3_600_000 }, now)
        .await
        .unwrap()
        .expect("owner approval issues a grant")
}

#[tokio::test]
async fn document_operation_appends_real_bytes_and_reports_the_effect() {
    let f = setup().await;
    let path = f._dir.path().join("worksheet.txt");
    std::fs::write(&path, b"line one\n").unwrap();
    let target = document_target(&path, "scope-doc");
    let observed = f.broker.observe(target.clone()).await.unwrap();

    let action = document_action(
        &f,
        &path,
        observed.revision_digest.clone(),
        "type",
        serde_json::json!({ "text": "inserted" }),
    );
    let grant = approved_grant(&f, &action).await;
    let receipt = f.broker.execute(action, grant, None).await.unwrap();

    assert_eq!(receipt.outcome, ActionOutcome::Succeeded);
    assert_eq!(receipt.effect, Effect::BytesAppended { count: 8 });
    assert_eq!(receipt.backend, "document_file");
    assert_ne!(receipt.revision_before, receipt.revision_after);
    assert_eq!(std::fs::read(&path).unwrap(), b"line one\ninserted");
}

#[tokio::test]
async fn stale_snapshot_cannot_receive_a_click() {
    let f = setup().await;
    let path = f._dir.path().join("worksheet.txt");
    std::fs::write(&path, b"stable\n").unwrap();
    let target = document_target(&path, "scope-doc");
    f.broker.observe(target).await.unwrap();

    // The owner observed `stable`; the action claims a different revision.
    let stale = Digest::parse(&"a".repeat(64)).unwrap();
    let action = document_action(&f, &path, stale.clone(), "click", serde_json::json!({}));
    let grant = approved_grant(&f, &action).await;
    let err = f.broker.execute(action, grant, None).await.unwrap_err();
    assert_eq!(err.code, "stale_target");
    assert_eq!(std::fs::read(&path).unwrap(), b"stable\n", "no effect on rejection");
}

#[tokio::test]
async fn released_scope_revokes_authority() {
    let f = setup().await;
    let path = f._dir.path().join("worksheet.txt");
    std::fs::write(&path, b"stable\n").unwrap();
    let target = document_target(&path, "scope-doc");
    let observed = f.broker.observe(target).await.unwrap();
    f.broker.release("scope-doc".to_string()).await.unwrap();

    let action = document_action(
        &f,
        &path,
        observed.revision_digest.clone(),
        "type",
        serde_json::json!({ "text": "must not land" }),
    );
    let grant = approved_grant(&f, &action).await;
    let err = f.broker.execute(action, grant, None).await.unwrap_err();
    assert_eq!(err.code, "scope_missing", "a released scope grants no authority");
    assert_eq!(std::fs::read(&path).unwrap(), b"stable\n", "the released scope produced no effect");
}

#[tokio::test]
async fn an_unapproved_grant_cannot_execute() {
    let f = setup().await;
    let path = f._dir.path().join("worksheet.txt");
    std::fs::write(&path, b"stable\n").unwrap();
    let target = document_target(&path, "scope-doc");
    let observed = f.broker.observe(target).await.unwrap();

    let action = document_action(
        &f,
        &path,
        observed.revision_digest.clone(),
        "type",
        serde_json::json!({ "text": "must not land" }),
    );
    // A grant object that was never issued for this action.
    let forged: ApprovalGrant = serde_json::from_value(serde_json::json!({
        "id": GrantId::new(),
        "action_id": action.id,
        "action_digest": action.digest().unwrap(),
        "policy_version": "1",
        "expected_state_digest": observed.revision_digest,
        "expires_at": (now_ms() + 3_600_000).to_string(),
        "revoked_at": null,
    }))
    .unwrap();
    let err = f.broker.execute(action, forged, None).await.unwrap_err();
    assert_eq!(err.code, "grant_denied", "only a durable owner approval may execute");
    assert_eq!(std::fs::read(&path).unwrap(), b"stable\n");
}

#[tokio::test]
async fn unsupported_operation_reports_a_failed_receipt_without_effect() {
    let f = setup().await;
    let path = f._dir.path().join("worksheet.txt");
    std::fs::write(&path, b"stable\n").unwrap();
    let target = document_target(&path, "scope-doc");
    let observed = f.broker.observe(target).await.unwrap();

    let action = document_action(
        &f,
        &path,
        observed.revision_digest.clone(),
        "midi_note",
        serde_json::json!({ "note": 60 }),
    );
    let grant = approved_grant(&f, &action).await;
    let receipt = f.broker.execute(action, grant, None).await.unwrap();
    assert_eq!(receipt.outcome, ActionOutcome::Failed, "an unsupported operation is not success");
    assert!(receipt.error.as_deref().unwrap_or_default().contains("unsupported"));
    assert_eq!(std::fs::read(&path).unwrap(), b"stable\n", "no bytes changed");
}

#[tokio::test]
async fn a_coding_worker_cannot_scope_the_host_session_or_personal_browser() {
    let now = now_ms();
    for kind in [TargetKind::BrowserProfile, TargetKind::NativeWindow] {
        let worker = ApprovedTarget {
            scope_id: "worker-scope".into(),
            role: ScopeRole::CodingWorker,
            kind,
            target_id: HOST_SESSION_TARGET.to_string(),
            expires_at: now + 60_000,
        };
        let err = check_target_in_scope(&worker, now).unwrap_err();
        assert_eq!(err.code, "scope_violation", "a coding worker never gets host-session authority");
    }
    // A coding worker may however be scoped to a disposable document target.
    let disposable = ApprovedTarget {
        scope_id: "worker-doc".into(),
        role: ScopeRole::CodingWorker,
        kind: TargetKind::DocumentFile,
        target_id: "/tmp/scratch.txt".into(),
        expires_at: now + 60_000,
    };
    check_target_in_scope(&disposable, now).unwrap();
    // An expired scope is refused outright, and an empty target is invalid.
    let expired = ApprovedTarget { expires_at: now - 1, ..disposable };
    assert_eq!(check_target_in_scope(&expired, now).unwrap_err().code, "scope_expired");
    let empty = ApprovedTarget { target_id: String::new(), expires_at: now + 60_000, ..expired };
    assert_eq!(check_target_in_scope(&empty, now).unwrap_err().code, "invalid_target");
}

#[tokio::test]
async fn page_world_content_cannot_approve_a_disclosure() {
    let consent = ConsentRecord {
        source: ConsentSource::PageWorld,
        kind: ConsentKind::FileUpload,
        scope_id: "scope-browser".into(),
        digest: Digest::parse(&"b".repeat(64)).unwrap(),
    };
    let err = validate_consent(&consent, TargetKind::BrowserProfile, "upload").unwrap_err();
    assert_eq!(err.code, "consent_owner_required", "page-world content is not owner consent");

    let owner = ConsentRecord { source: ConsentSource::Owner, ..consent };
    validate_consent(&owner, TargetKind::BrowserProfile, "upload").unwrap();
    // A screenshot consent does not authorise an upload.
    let wrong_kind = ConsentRecord { kind: ConsentKind::Screenshot, ..owner };
    assert_eq!(
        validate_consent(&wrong_kind, TargetKind::BrowserProfile, "upload").unwrap_err().code,
        "consent_mismatch"
    );
}

#[tokio::test]
async fn scope_records_are_durable_state_events() {
    let f = setup().await;
    let path = f._dir.path().join("worksheet.txt");
    std::fs::write(&path, b"stable\n").unwrap();
    let target = document_target(&path, "scope-doc");
    f.broker.observe(target).await.unwrap();
    f.broker.release("scope-doc".to_string()).await.unwrap();
    // Release is idempotent and never errors on a missing scope.
    f.broker.release("never-scoped".to_string()).await.unwrap();

    let phases: Vec<String> = f
        .store
        .with_connection(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT json_extract(payload,'$.state') FROM events \
                 WHERE json_extract(payload,'$.entity_id')='automation-scope/scope-doc' ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                let state: String = row?;
                let record: serde_json::Value = serde_json::from_str(&state)?;
                out.push(record["phase"].as_str().unwrap_or_default().to_string());
            }
            Ok(out)
        })
        .await
        .unwrap();
    assert_eq!(phases, vec!["active", "released"]);
}

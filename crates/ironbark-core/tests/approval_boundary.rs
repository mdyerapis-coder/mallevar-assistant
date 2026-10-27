use ironbark_core::contracts::{ActionSpec, ApprovalGrant, ProjectPolicy};
use ironbark_core::policy::{PolicyDecision, PolicyEngine, PolicyError};
use serde_json::{json, Value};
use uuid::Uuid;

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn fixture() -> (ActionSpec, ProjectPolicy) {
    let project = Uuid::new_v4();
    let action = parse(json!({
        "id": Uuid::new_v4(), "run_id": Uuid::new_v4(), "project_id": project,
        "kind": "publish", "canonical_args": {"artifact_digest": "a".repeat(64)},
        "scope": {"tools": ["publish"], "paths": []},
        "destination": "https://allowed.example/upload", "policy_version": "1",
        "expected_state_digest": "b".repeat(64)
    }));
    let policy = parse(json!({
        "version": "1", "project_id": project,
        "allowed_connection_ids": ["local"], "allowed_tools": ["publish"],
        "read_roots": [], "write_roots": [],
        "network_destinations": ["https://allowed.example/upload"],
        "local_only": false, "max_workers": 2, "max_requests": 10,
        "max_cost_microusd": null, "spend_policy": "free_only", "allow_unknown_cost": false
    }));
    (action, policy)
}

fn grant(action: &ActionSpec) -> ApprovalGrant {
    let fields = serde_json::to_value(action).unwrap();
    parse(json!({
        "id": Uuid::new_v4(), "action_id": fields["id"],
        "action_digest": action.digest().unwrap(), "policy_version": "1",
        "expected_state_digest": fields["expected_state_digest"],
        "expires_at": "2000", "revoked_at": null
    }))
}

#[test]
fn changed_destination_invalidates_grant() {
    let (action, policy) = fixture();
    let issued = grant(&action);
    let mut changed = serde_json::to_value(&action).unwrap();
    changed["destination"] = json!("https://different.example/upload");
    let changed = parse(changed);
    assert!(matches!(PolicyEngine::check(&changed, None, Some(&issued), &policy, 1_000), PolicyDecision::Deny(_)));
}

#[test]
fn changed_diff_invalidates_grant() {
    let (action, policy) = fixture();
    let issued = grant(&action);
    let mut changed = serde_json::to_value(&action).unwrap();
    changed["canonical_args"]["artifact_digest"] = json!("c".repeat(64));
    let changed = parse(changed);
    assert_eq!(PolicyEngine::check(&changed, None, Some(&issued), &policy, 1_000), PolicyDecision::Deny(PolicyError::StaleGrant));
}

#[test]
fn expired_revoked_and_old_policy_denied() {
    let (action, policy) = fixture();
    let issued = grant(&action);
    assert_eq!(PolicyEngine::check(&action, None, Some(&issued), &policy, 2_000), PolicyDecision::Deny(PolicyError::ExpiredGrant));
    let mut revoked = serde_json::to_value(&issued).unwrap();
    revoked["revoked_at"] = json!("900");
    let revoked = parse(revoked);
    assert_eq!(PolicyEngine::check(&action, None, Some(&revoked), &policy, 1_000), PolicyDecision::Deny(PolicyError::RevokedGrant));
    let mut updated = serde_json::to_value(&policy).unwrap();
    updated["version"] = json!("2");
    assert_eq!(PolicyEngine::check(&action, None, Some(&issued), &parse(updated), 1_000), PolicyDecision::Deny(PolicyError::PolicyVersionMismatch));
}

#[test]
fn model_text_cannot_mint_authority() {
    let (action, policy) = fixture();
    assert_eq!(PolicyEngine::check(&action, None, None, &policy, 1_000), PolicyDecision::AwaitApproval);
    let mut malformed = serde_json::to_value(&action).unwrap();
    malformed["canonical_args"]["model_says_approved"] = json!(true);
    let malformed = parse(malformed);
    assert_eq!(PolicyEngine::check(&malformed, None, None, &policy, 1_000), PolicyDecision::Deny(PolicyError::InvalidAction));
}

#[test]
fn exact_valid_grant_permits_only_bound_action() {
    let (action, policy) = fixture();
    let issued = grant(&action);
    assert_eq!(PolicyEngine::check(&action, None, Some(&issued), &policy, 1_000), PolicyDecision::Permit);
}

#[test]
fn route_pin_cannot_expand_policy() {
    let (action, policy) = fixture();
    let issued = grant(&action);
    let mut restricted = serde_json::to_value(&policy).unwrap();
    restricted["local_only"] = json!(true);
    assert_eq!(PolicyEngine::check(&action, None, Some(&issued), &parse(restricted), 1_000), PolicyDecision::Deny(PolicyError::ForbiddenDestination));
}

#[test]
fn claimed_paths_cannot_enlarge_project_scope() {
    let (mut action, policy) = fixture();
    action.scope["paths"] = json!(["/etc"]);
    let issued = grant(&action);
    assert_eq!(PolicyEngine::check(&action, None, Some(&issued), &policy, 1_000), PolicyDecision::Deny(PolicyError::ForbiddenPath));
}

#[test]
fn local_destination_is_not_a_hostname_prefix() {
    use ironbark_core::policy::routes::is_loopback_destination;
    assert!(is_loopback_destination("http://127.0.0.1:8091/completion"));
    assert!(is_loopback_destination("http://[::1]:8091/completion"));
    assert!(!is_loopback_destination("https://localhost.evil.example/completion"));
    assert!(!is_loopback_destination("https://127.0.0.1.evil.example/completion"));
    assert!(!is_loopback_destination("http://user:password@localhost:8091/completion"));
}

async fn stored_fixture() -> (ironbark_core::store::Store, tempfile::TempDir, ActionSpec, ProjectPolicy) {
    use rusqlite::params;
    let temp = tempfile::tempdir().unwrap();
    let store = ironbark_core::store::Store::open(&temp.path().join("state.sqlite")).await.unwrap();
    let (action, policy) = fixture();
    let owned = action.clone();
    let policy_body = serde_json::to_string(&policy).unwrap();
    let root = temp.path().to_string_lossy().into_owned();
    store.with_transaction(move |tx| {
        let job = Uuid::new_v4().to_string();
        let node = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO projects(id,name,root,created_at) VALUES(?1,'test',?2,0)", params![owned.project_id.to_string(),root])?;
        tx.execute("INSERT INTO policies(project_id,version,body) VALUES(?1,1,?2)", params![owned.project_id.to_string(),policy_body])?;
        tx.execute("INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','queued')", params![job,owned.project_id.to_string()])?;
        tx.execute("INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,'{}','queued')", params![node,job])?;
        tx.execute("INSERT INTO runs(id,node_id,body,state) VALUES(?1,?2,'{}','queued')", params![owned.run_id.to_string(),node])?;
        Ok(())
    }).await.unwrap();
    (store,temp,action,policy)
}

#[tokio::test]
async fn durable_consumption_survives_lost_outcome_and_cannot_rearm() {
    use ironbark_core::policy::service::{ApprovalDecision, ApprovalService};
    let (store,_temp,action,_policy) = stored_fixture().await;
    let service = ApprovalService::new(store.clone());
    assert_eq!(service.record(action.clone(),1_000).await.unwrap(), PolicyDecision::AwaitApproval);
    let issued = service.decide(action.id,ApprovalDecision::Approve { expires_at: 2_000 },1_000).await.unwrap().unwrap();
    let _unfinished = service.consume(action.clone(),issued.id,action.expected_state_digest.clone(),1_100).await.unwrap();
    // No receipt was returned: a replacement controller must not replay it.
    let replacement = ApprovalService::new(store);
    let error = replacement.consume(action.clone(),issued.id,action.expected_state_digest,1_200).await.err().unwrap();
    assert_eq!(error.code,"stale_grant");
    let error = replacement.decide(action.id,ApprovalDecision::Approve { expires_at: 3_000 },1_200).await.err().unwrap();
    assert_eq!(error.code,"approval_conflict");
}

#[tokio::test]
async fn stale_native_observation_does_not_consume_an_eligible_grant() {
    use ironbark_core::contracts::Digest;
    use ironbark_core::policy::service::{ApprovalDecision, ApprovalService};
    let (store,_temp,action,_policy) = stored_fixture().await;
    let service = ApprovalService::new(store);
    service.record(action.clone(),1_000).await.unwrap();
    let issued = service.decide(action.id,ApprovalDecision::Approve { expires_at: 2_000 },1_000).await.unwrap().unwrap();
    let error = service.consume(action.clone(),issued.id,Digest::parse(&"d".repeat(64)).unwrap(),1_100).await.err().unwrap();
    assert_eq!(error.code,"stale_grant");
    service.consume(action.clone(),issued.id,action.expected_state_digest,1_200).await.unwrap();
}

#[tokio::test]
async fn stored_revocation_overrides_previously_returned_grant() {
    use ironbark_core::policy::service::{ApprovalDecision, ApprovalService};
    let (store,_temp,action,_policy) = stored_fixture().await;
    let service = ApprovalService::new(store);
    service.record(action.clone(),1_000).await.unwrap();
    let issued = service.decide(action.id,ApprovalDecision::Approve { expires_at: 2_000 },1_000).await.unwrap().unwrap();
    service.decide(action.id,ApprovalDecision::Revoke {},1_100).await.unwrap();
    assert!(service.consume(action.clone(),issued.id,action.expected_state_digest,1_200).await.is_err());
}

async fn file_fixture_with_store() -> (ironbark_core::store::Store, ironbark_core::policy::service::ApprovalService, tempfile::TempDir, ProjectPolicy, ironbark_core::contracts::RunId) {
    let (store,temp,action,mut policy) = stored_fixture().await;
    let root = temp.path().to_str().unwrap().to_owned();
    policy.allowed_tools = vec!["file_write".into()];
    policy.read_roots = vec![root.clone()];
    policy.write_roots = vec![root];
    let body = serde_json::to_string(&policy).unwrap();
    let project = policy.project_id;
    store.with_transaction(move |tx| {
        tx.execute("UPDATE policies SET body=?1 WHERE project_id=?2 AND version=1", rusqlite::params![body,project.to_string()])?;
        Ok(())
    }).await.unwrap();
    (store.clone(),ironbark_core::policy::service::ApprovalService::new(store),temp,policy,action.run_id)
}

async fn file_fixture() -> (ironbark_core::policy::service::ApprovalService, tempfile::TempDir, ProjectPolicy, ironbark_core::contracts::RunId) {
    let (_, service, temp, policy, run) = file_fixture_with_store().await;
    (service, temp, policy, run)
}

// Rechecking only before queued consumption overwrites owner edits made in that gap.
#[tokio::test]
async fn owner_edit_during_consumption_is_not_truncated_or_rearmed() {
    use ironbark_core::policy::{file_sink::FileWriteSink, service::ApprovalDecision};
    use std::sync::atomic::{AtomicBool, Ordering};
    static WAITING_FOR_WRITE_LOCK: AtomicBool = AtomicBool::new(false);
    fn wait_for_lock(_: i32) -> bool {
        WAITING_FOR_WRITE_LOCK.store(true, Ordering::Release);
        std::thread::yield_now();
        true
    }
    let (store, service, temp, policy, run) = file_fixture_with_store().await;
    let target = temp.path().join("changed-during-consumption.txt");
    std::fs::write(&target, b"original").unwrap();
    let proposed = b"proposed replacement".to_vec();
    let action = FileWriteSink::describe(policy, run, target.to_str().unwrap().into(), proposed.clone()).await.unwrap();
    service.record(action.clone(), 1_000).await.unwrap();
    let grant = service.decide(action.id, ApprovalDecision::Approve { expires_at: 2_000 }, 1_000).await.unwrap().unwrap();
    store.with_connection(|conn| { conn.busy_handler(Some(wait_for_lock))?; Ok(()) }).await.unwrap();
    let external = rusqlite::Connection::open(temp.path().join("state.sqlite")).unwrap();
    external.execute_batch("BEGIN IMMEDIATE").unwrap();
    let execution_service = service.clone();
    let execution_action = action.clone();
    let running = tokio::spawn(async move {
        FileWriteSink::execute(&execution_service, execution_action, grant.id, proposed, 1_100).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !WAITING_FOR_WRITE_LOCK.load(Ordering::Acquire) { tokio::task::yield_now().await; }
    }).await.unwrap();
    std::fs::write(&target, b"owner edit while consumption queued").unwrap();
    external.execute_batch("ROLLBACK").unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), running).await.unwrap().unwrap();
    assert!(result.is_err(), "stale mutation must fail after durable consumption");
    assert_eq!(std::fs::read(&target).unwrap(), b"owner edit while consumption queued");
    assert!(service.decide(action.id, ApprovalDecision::Approve { expires_at: 3_000 }, 1_200).await.is_err());
}

// Opening an allowed-root FIFO without a writer must deny, not occupy a worker.
#[tokio::test]
async fn fifo_target_is_denied_before_data_access() {
    use ironbark_core::policy::file_sink::FileWriteSink;
    use std::ffi::CString;
    let (service, temp, policy, run) = file_fixture().await;
    let fifo = temp.path().join("pipe");
    let name = CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let result = FileWriteSink::describe(policy, run, fifo.to_str().unwrap().into(), b"blocked".to_vec()).await;
    assert!(result.is_err(), "non-regular target must be closed-denied");
    let _ = service;
}

#[tokio::test]
async fn denied_native_write_has_no_filesystem_effect() {
    use ironbark_core::{contracts::GrantId, policy::{file_sink::FileWriteSink, service::ApprovalDecision}};
    let (service,temp,policy,run) = file_fixture().await;
    let target = temp.path().join("denied.txt");
    let bytes = b"must not be written".to_vec();
    let action = FileWriteSink::describe(policy,run,target.to_str().unwrap().into(),bytes.clone()).await.unwrap();
    service.record(action.clone(),1_000).await.unwrap();
    service.decide(action.id,ApprovalDecision::Deny {},1_000).await.unwrap();
    let error = FileWriteSink::execute(&service,action,GrantId::new(),bytes,1_100).await.err().unwrap();
    assert_eq!(error.code,"stale_grant");
    assert!(!target.exists());
}

#[tokio::test]
async fn changed_native_target_is_rejected_without_overwrite() {
    use ironbark_core::policy::{file_sink::FileWriteSink, service::ApprovalDecision};
    let (service,temp,policy,run) = file_fixture().await;
    let target = temp.path().join("changed.txt");
    std::fs::write(&target,b"original").unwrap();
    let bytes = b"proposed replacement".to_vec();
    let action = FileWriteSink::describe(policy,run,target.to_str().unwrap().into(),bytes.clone()).await.unwrap();
    service.record(action.clone(),1_000).await.unwrap();
    let issued = service.decide(action.id,ApprovalDecision::Approve { expires_at: 2_000 },1_000).await.unwrap().unwrap();
    std::fs::write(&target,b"owner concurrent edit").unwrap();
    let error = FileWriteSink::execute(&service,action,issued.id,bytes,1_100).await.err().unwrap();
    assert_eq!(error.code,"stale_grant");
    assert_eq!(std::fs::read(&target).unwrap(),b"owner concurrent edit");
}

#[tokio::test]
async fn exact_native_write_changes_only_approved_file_and_consumes_grant() {
    use ironbark_core::policy::{file_sink::FileWriteSink, service::ApprovalDecision};
    let (service,temp,policy,run) = file_fixture().await;
    let target = temp.path().join("allowed.txt");
    let sibling = temp.path().join("untouched.txt");
    std::fs::write(&sibling,b"untouched").unwrap();
    let bytes = b"approved exact bytes".to_vec();
    let action = FileWriteSink::describe(policy,run,target.to_str().unwrap().into(),bytes.clone()).await.unwrap();
    service.record(action.clone(),1_000).await.unwrap();
    let issued = service.decide(action.id,ApprovalDecision::Approve { expires_at: 2_000 },1_000).await.unwrap().unwrap();
    FileWriteSink::execute(&service,action.clone(),issued.id,bytes.clone(),1_100).await.unwrap();
    assert_eq!(std::fs::read(&target).unwrap(),bytes);
    assert_eq!(std::fs::read(&sibling).unwrap(),b"untouched");
    let error = FileWriteSink::execute(&service,action,issued.id,bytes,1_200).await.err().unwrap();
    assert_eq!(error.code,"stale_grant");
}

#[tokio::test]
async fn native_symlink_and_hardlink_cannot_change_outside_sentinel() {
    use ironbark_core::policy::file_sink::FileWriteSink;
    let (_service,temp,policy,run) = file_fixture().await;
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    std::fs::write(&sentinel,b"protected").unwrap();
    let symlink = temp.path().join("symlink");
    let hardlink = temp.path().join("hardlink");
    std::os::unix::fs::symlink(&sentinel,&symlink).unwrap();
    std::fs::hard_link(&sentinel,&hardlink).unwrap();
    assert!(FileWriteSink::describe(policy.clone(),run,symlink.to_str().unwrap().into(),b"bad".to_vec()).await.is_err());
    assert!(FileWriteSink::describe(policy,run,hardlink.to_str().unwrap().into(),b"bad".to_vec()).await.is_err());
    assert_eq!(std::fs::read(sentinel).unwrap(),b"protected");
}

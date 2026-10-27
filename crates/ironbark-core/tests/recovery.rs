//! C4 recovery invariants tested against real SQLite and the real services.
//!
//! - `native_done_without_acceptance_is_not_succeeded`: a native completion
//!   label parks the run in Reviewing; only an explicit acceptance receipt
//!   backed by a passing post-verification checkpoint reaches Succeeded.
//! - `mutation_without_receipt_is_blocked`: a consequential action with no
//!   durable consumption receipt blocks the run instead of mutating.
//! - `stale_resume_does_not_reuse_grant`: a bound grant that is no longer
//!   eligible under the current policy is never reused; the run parks in
//!   AwaitingApproval for fresh owner approval.
//! - `cancel_request_is_not_cancelled_receipt`: a cancel request only reaches
//!   Cancelling; Cancelled requires an observed stop.
//! - `pause_preserves_native_session`: pause keeps the native session ref and
//!   partial artifacts in the durable run body.
//! - `inspect_reports_pending_mutation`: crash inspection observes durable
//!   state and recommends explicit reconciliation.
use std::path::PathBuf;

use ironbark_core::contracts::{
    ActionId, ActionSpec, ApprovalGrant, ArtifactId, CheckpointPhase, Digest, HarnessKind, JobId,
    NodeId, ProjectId, ProjectPolicy, RecoveryDecision, ResultManifest, RoutePin, RunId, RunState,
    SessionRef,
};
use ironbark_core::policy::service::{ApprovalDecision, ApprovalService};
use ironbark_core::scheduler::{state, Recovery, RunControl};
use ironbark_core::store::{events::now_ms, Store};
use serde_json::json;

const POLICY_VERSION_1: u64 = 1;

fn digest(seed: &str) -> Digest {
    Digest::parse(&seed.repeat(64)).unwrap()
}

fn policy_value(project: ProjectId, version: u64) -> ProjectPolicy {
    serde_json::from_value(json!({
        // Wire contract: policy version is a canonical uint64 decimal STRING.
        "version": version.to_string(),
        "project_id": project,
        "allowed_connection_ids": ["local"],
        "allowed_tools": ["publish"],
        "read_roots": [],
        "write_roots": [],
        "network_destinations": ["https://allowed.example/upload"],
        "local_only": false,
        "max_workers": 2,
        "max_requests": 10,
        "max_cost_microusd": null,
        "spend_policy": "free_only",
        "allow_unknown_cost": false,
    }))
    .unwrap()
}

fn route() -> RoutePin {
    RoutePin { connection_id: "local".into(), model_id: "fixture-model".into(), harness: Some(HarnessKind::Omp) }
}

fn session(project: ProjectId) -> SessionRef {
    SessionRef {
        harness: HarnessKind::Omp,
        native_id: "native-session-1".into(),
        native_version: "18.8.9".into(),
        project_id: project,
        cwd: "/tmp".into(),
        resolved_route: route(),
    }
}

/// A project, its version-1 policy, one job/node chain and one `ready` run.
async fn setup() -> (tempfile::TempDir, Store, ProjectId, RunId, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let cas = tmp.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let store = Store::open(&tmp.path().join("core.db")).await.unwrap();
    let project = ProjectId::new();
    let run = RunId::new();
    let job = JobId::new();
    let node = NodeId::new();
    let root = tmp.path().to_string_lossy().into_owned();
    let policy = serde_json::to_string(&policy_value(project, POLICY_VERSION_1)).unwrap();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects(id,name,root,created_at) VALUES(?1,'fixture',?2,1)",
                rusqlite::params![project.to_string(), root],
            )?;
            tx.execute(
                "INSERT INTO policies(project_id,version,body) VALUES(?1,?2,?3)",
                rusqlite::params![project.to_string(), POLICY_VERSION_1 as i64, policy],
            )?;
            tx.execute(
                "INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![job.to_string(), project.to_string()],
            )?;
            tx.execute(
                "INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![node.to_string(), job.to_string()],
            )?;
            tx.execute(
                "INSERT INTO runs(id,node_id,state,body) VALUES(?1,?2,'ready','{}')",
                rusqlite::params![run.to_string(), node.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (tmp, store, project, run, cas)
}

async fn state_of(store: &Store, run: RunId) -> RunState {
    let raw: String = store
        .with_connection(move |conn| {
            conn.query_row("SELECT state FROM runs WHERE id=?1", [run.to_string()], |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    serde_json::from_value(json!(raw)).unwrap()
}

async fn set_state(store: &Store, run: RunId, next: RunState) {
    let raw = serde_json::to_value(next).unwrap().as_str().unwrap().to_string();
    store
        .with_transaction(move |tx| {
            tx.execute("UPDATE runs SET state=?2 WHERE id=?1", rusqlite::params![run.to_string(), raw])?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn run_body(store: &Store, run: RunId) -> serde_json::Value {
    let body: String = store
        .with_connection(move |conn| {
            conn.query_row("SELECT body FROM runs WHERE id=?1", [run.to_string()], |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    serde_json::from_str(&body).unwrap()
}

fn passing_manifest(run: RunId) -> ResultManifest {
    ResultManifest {
        run_id: run,
        native_session: None,
        artifacts: vec![],
        checks: vec![serde_json::from_value(json!({
            "name": "cargo test",
            "passed": true,
            "evidence_digest": digest("c"),
            "details": "fixture verification receipt"
        }))
        .unwrap()],
        acceptance_satisfied: true,
        unresolved: vec![],
    }
}

fn action(run: RunId, project: ProjectId) -> ActionSpec {
    serde_json::from_value(json!({
        "id": ActionId::new(),
        "run_id": run,
        "project_id": project,
        "kind": "publish",
        "canonical_args": { "artifact_digest": digest("a") },
        "scope": { "tools": ["publish"], "paths": [] },
        "destination": "https://allowed.example/upload",
        // Wire contract: policy version is a canonical uint64 decimal STRING.
        "policy_version": POLICY_VERSION_1.to_string(),
        "expected_state_digest": digest("b"),
    }))
    .unwrap()
}

#[test]
fn transition_rejects_native_completion_as_success() {
    use state::{transition, RunEvent};
    assert_eq!(transition(RunState::Running, RunEvent::NativeDone).unwrap(), RunState::Reviewing);
    assert_eq!(transition(RunState::Reviewing, RunEvent::Accepted).unwrap(), RunState::Succeeded);
    // A cancel request alone never produces Cancelled, and a native label never
    // produces Succeeded.
    assert_eq!(transition(RunState::Running, RunEvent::CancelRequested).unwrap(), RunState::Cancelling);
    assert!(transition(RunState::Running, RunEvent::Accepted).is_err());
    assert!(transition(RunState::Cancelling, RunEvent::Accepted).is_err());
}

#[tokio::test]
async fn native_done_without_acceptance_is_not_succeeded() {
    let (_tmp, store, project, run, cas) = setup().await;
    let control = RunControl::new(store.clone(), project, cas.clone()).unwrap();
    assert_eq!(control.start(run, Some(session(project)), vec![]).await.unwrap(), RunState::Running);
    assert_eq!(control.observe_native_done(run).await.unwrap(), RunState::Reviewing);
    assert_ne!(state_of(&store, run).await, RunState::Succeeded);

    // The native completion label alone is not acceptance evidence.
    let err = control.accept(run).await.unwrap_err();
    assert_eq!(err.code, "acceptance_evidence_missing");
    assert_ne!(state_of(&store, run).await, RunState::Succeeded);

    // A real passing post-verification checkpoint is the only path to success.
    let recovery = Recovery::new(store.clone(), project, cas).unwrap();
    recovery.checkpoints().capture(run, CheckpointPhase::PostVerification, passing_manifest(run)).await.unwrap();
    assert_eq!(control.accept(run).await.unwrap(), RunState::Succeeded);
}

#[tokio::test]
async fn mutation_without_receipt_is_blocked() {
    let (_tmp, store, project, run, cas) = setup().await;
    let control = RunControl::new(store.clone(), project, cas).unwrap();
    control.start(run, None, vec![]).await.unwrap();
    let err = control.commit_mutation(run, None).await.unwrap_err();
    assert_eq!(err.code, "mutation_without_receipt");
    assert_eq!(state_of(&store, run).await, RunState::Blocked);
}

#[tokio::test]
async fn stale_resume_does_not_reuse_grant() {
    let (_tmp, store, project, run, cas) = setup().await;
    let approvals = ApprovalService::new(store.clone());
    let now = now_ms();
    let spec = action(run, project);
    approvals.record(spec.clone(), now).await.unwrap();
    approvals
        .decide(spec.id, ApprovalDecision::Approve { expires_at: now + 3_600_000 }, now)
        .await
        .unwrap()
        .expect("grant issued");

    // A checkpoint exists, so the crash boundary is resumable…
    let recovery = Recovery::new(store.clone(), project.clone(), cas.clone()).unwrap();
    recovery.checkpoints().capture(run, CheckpointPhase::Planned, ResultManifest {
        run_id: run,
        native_session: None,
        artifacts: vec![],
        checks: vec![],
        acceptance_satisfied: false,
        unresolved: vec![],
    }).await.unwrap();
    set_state(&store, run, RunState::Blocked).await;

    // …but the owner republishes policy, so the bound grant is stale.
    let policy2 = serde_json::to_string(&policy_value(project, 2)).unwrap();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO policies(project_id,version,body) VALUES(?1,2,?2)",
                rusqlite::params![project.to_string(), policy2],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let err = recovery.reconcile(run, RecoveryDecision::ContinueFromCheckpoint).await.unwrap_err();
    assert_eq!(err.code, "stale_grant");
    // The stale grant was not reused: the run parks for fresh approval.
    assert_eq!(state_of(&store, run).await, RunState::AwaitingApproval);
}

#[tokio::test]
async fn cancel_request_is_not_cancelled_receipt() {
    let (_tmp, store, project, run, cas) = setup().await;
    let control = RunControl::new(store.clone(), project, cas).unwrap();
    control.start(run, None, vec![]).await.unwrap();
    assert_eq!(control.request_cancel(run).await.unwrap(), RunState::Cancelling);
    assert_ne!(state_of(&store, run).await, RunState::Cancelled);
    assert_eq!(control.observe_stop(run).await.unwrap(), RunState::Cancelled);
}

#[tokio::test]
async fn pause_preserves_native_session() {
    let (_tmp, store, project, run, cas) = setup().await;
    let control = RunControl::new(store.clone(), project, cas).unwrap();
    control.start(run, Some(session(project)), vec![ArtifactId::new()]).await.unwrap();
    assert_eq!(control.pause(run).await.unwrap(), RunState::Paused);
    let body = run_body(&store, run).await;
    assert_eq!(body["native_session"]["native_id"], json!("native-session-1"));
    assert_eq!(body["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(control.resume(run).await.unwrap(), RunState::Ready);
}

#[tokio::test]
async fn inspect_reports_pending_mutation_and_uncertain_outcome() {
    let (_tmp, store, project, run, cas) = setup().await;
    let recovery = Recovery::new(store.clone(), project, cas).unwrap();
    // A crash leaves the run blocked over an unresolved external action.
    set_state(&store, run, RunState::Blocked).await;
    let now = now_ms();
    let spec = action(run, project);
    let approvals = ApprovalService::new(store.clone());
    approvals.record(spec.clone(), now).await.unwrap();
    let grant: ApprovalGrant = approvals
        .decide(spec.id, ApprovalDecision::Approve { expires_at: now + 3_600_000 }, now)
        .await
        .unwrap()
        .unwrap();
    let run_id = run;
    let action_id = spec.id;
    let grant_id = grant.id;
    store
        .with_transaction(move |tx| {
            tx.execute("UPDATE actions SET state='executing' WHERE id=?1", [action_id.to_string()])?;
            // Consumed, no durable outcome: an uncertain external action.
            tx.execute(
                "UPDATE grants SET consumed_at=?2 WHERE id=?1",
                rusqlite::params![grant_id.to_string(), now],
            )?;
            let _ = run_id;
            Ok(())
        })
        .await
        .unwrap();

    let report = recovery.inspect(run).await.unwrap();
    assert!(report.pending_mutation);
    assert_eq!(report.uncertain_actions.len(), 1);
    assert!(report.recommended.iter().any(|r| r == "resolve_mutation_outcome"));
    // Accepting the observed state is refused while the outcome is unresolved.
    let err = recovery.reconcile(run, RecoveryDecision::AcceptObserved).await.unwrap_err();
    assert_eq!(err.code, "outcome_unresolved");
}

#[tokio::test]
async fn created_run_is_durable_and_admitted_through_the_transition_table() {
    let (_tmp, store, project, _unused_run, cas) = setup().await;
    let scheduler = ironbark_core::scheduler::SchedulerService::new(store.clone(), project).unwrap();
    let receipt = scheduler.create_run("  fixture task  ").await.unwrap();
    assert_eq!(receipt.state, RunState::Queued, "a run is born queued");
    assert_eq!(receipt.project_id, project);

    // The run exists with the durable job/node chain behind it.
    let receipt_ids = (receipt.job_id, receipt.node_id, receipt.run_id);
    let (job_project, task): (String, String) = store
        .with_connection(move |conn| {
            conn.query_row(
                "SELECT j.project_id, r.body FROM runs r
                 JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id
                 WHERE r.id=?1 AND n.id=?2 AND j.id=?3",
                rusqlite::params![
                    receipt_ids.2.to_string(),
                    receipt_ids.1.to_string(),
                    receipt_ids.0.to_string()
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(job_project, project.to_string());
    assert!(task.contains("fixture task"), "the task text is recorded: {task}");

    // An empty task is refused rather than creating an unusable run.
    let err = scheduler.create_run("   ").await.unwrap_err();
    assert_eq!(err.code, "invalid_task");

    // `start` admits the queued run through the table and reaches Running.
    let control = RunControl::new(store.clone(), project, cas).unwrap();
    assert_eq!(control.start(receipt.run_id, Some(session(project)), vec![]).await.unwrap(), RunState::Running);
    let events: i64 = store
        .with_connection(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM events WHERE run_id=?1 AND payload LIKE '%admitted%'",
                [receipt.run_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(events, 1, "the admission is a durable event");
}

#[tokio::test]
async fn abandon_resolves_a_blocked_run() {
    let (_tmp, store, project, run, cas) = setup().await;
    set_state(&store, run, RunState::Blocked).await;
    let recovery = Recovery::new(store.clone(), project, cas).unwrap();
    recovery.reconcile(run, RecoveryDecision::Abandon).await.unwrap();
    assert_eq!(state_of(&store, run).await, RunState::Cancelled);
}

//! The product now has a way to commit a project policy, which every retrieval,
//! context and embedding-ranking path reads through `require_policy`. These
//! tests drive the real `PolicyService` against real SQLite and prove:
//!
//! - committing a policy is what makes the retrieval path work end to end
//!   (without it, retrieval is refused with `policy_missing` — the exact gap);
//! - a stale or equal version is refused (`stale_policy_version`);
//! - a foreign `project_id` is refused (`project_mismatch`);
//! - an out-of-range bound is refused (`invalid_policy_bounds`);
//! - credential-bearing values are refused (`sensitive_policy`);
//! - a policy for an unregistered project is refused (`project_missing`).
use ironbark_core::contracts::{DisclosurePolicy, ProjectId, ProjectPolicy, SourceInput, SpendPolicy};
use ironbark_core::knowledge::KnowledgeService;
use ironbark_core::policy::service::PolicyService;
use ironbark_core::store::Store;
use tempfile::tempdir;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    project: ProjectId,
    service: PolicyService,
}

async fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let store = Store::open(&dir.path().join("policy.sqlite3")).await.unwrap();
    let project = ProjectId::new();
    let root = dir.path().to_string_lossy().into_owned();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects (id,name,root,created_at) VALUES (?1,'policy-fixture',?2,0)",
                rusqlite::params![project.to_string(), root],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let service = PolicyService::new(store.clone());
    Fixture { _dir: dir, store, project, service }
}

fn policy(project: ProjectId, version: u64) -> ProjectPolicy {
    ProjectPolicy {
        version,
        project_id: project,
        allowed_connection_ids: vec!["local".into()],
        allowed_tools: vec![],
        read_roots: vec![],
        write_roots: vec![],
        network_destinations: vec![],
        local_only: false,
        max_workers: 2,
        max_requests: 10,
        max_cost_microusd: None,
        spend_policy: SpendPolicy::FreeOnly,
        allow_unknown_cost: false,
    }
}

/// The gap being fixed: retrieval refuses a project with no committed policy,
/// and the new command is what flips it to a working end-to-end retrieval.
#[tokio::test]
async fn committed_policy_enables_retrieval_end_to_end() {
    let f = fixture().await;
    let file = f._dir.path().join("doc.txt");
    tokio::fs::write(&file, b"an ironbark retrieval sentinel lives here").await.unwrap();
    let knowledge = KnowledgeService::new(f.store.clone(), f._dir.path().join("objects"));
    knowledge
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .unwrap();

    // Ingested content is present, but retrieval cannot run without a policy.
    let refused = knowledge.retrieve(f.project, "sentinel", 5).await.unwrap_err();
    assert_eq!(refused.code, "policy_missing", "retrieval must be refused before any policy is committed");

    let committed = f.service.set(f.project, policy(f.project, 1)).await.unwrap();
    assert_eq!(committed.version, 1);
    assert_eq!(f.service.latest(f.project).await.unwrap(), committed);

    let hits = knowledge.retrieve(f.project, "sentinel", 5).await.unwrap();
    assert_eq!(hits.len(), 1, "the committed policy enables retrieval of the ingested source");
    assert!(hits[0].text.contains("sentinel"));
}

#[tokio::test]
async fn equal_or_lower_version_is_refused() {
    let f = fixture().await;
    f.service.set(f.project, policy(f.project, 2)).await.unwrap();

    for candidate in [1, 2] {
        let refused = f.service.set(f.project, policy(f.project, candidate)).await.unwrap_err();
        assert_eq!(refused.code, "stale_policy_version", "version {candidate} must not overwrite version 2");
    }
    // A refused write never advanced the committed version.
    assert_eq!(f.service.latest(f.project).await.unwrap().version, 2);
}

#[tokio::test]
async fn foreign_project_id_is_refused() {
    let f = fixture().await;
    let mut foreign = policy(ProjectId::new(), 1);
    foreign.project_id = ProjectId::new();
    let refused = f.service.set(f.project, foreign).await.unwrap_err();
    assert_eq!(refused.code, "project_mismatch");
    assert_eq!(f.service.latest(f.project).await.unwrap_err().code, "policy_missing");
}

#[tokio::test]
async fn out_of_range_bounds_are_refused() {
    let f = fixture().await;
    for (workers, requests) in [(0, 10), (33, 10), (2, 0), (2, 100_001)] {
        let mut candidate = policy(f.project, 1);
        candidate.max_workers = workers;
        candidate.max_requests = requests;
        let refused = f.service.set(f.project, candidate).await.unwrap_err();
        assert_eq!(refused.code, "invalid_policy_bounds", "max_workers={workers} max_requests={requests} must be refused");
    }
    assert_eq!(f.service.latest(f.project).await.unwrap_err().code, "policy_missing");
}

#[tokio::test]
async fn credential_bearing_values_are_refused() {
    let f = fixture().await;

    let mut url = policy(f.project, 1);
    url.network_destinations = vec!["https://user:pass@example.test/upload".into()];
    let refused = f.service.set(f.project, url).await.unwrap_err();
    assert_eq!(refused.code, "sensitive_policy");

    let mut key = policy(f.project, 1);
    key.allowed_connection_ids = vec!["api_key=sk-live-abcdef123456".into()];
    let refused = f.service.set(f.project, key).await.unwrap_err();
    assert_eq!(refused.code, "sensitive_policy");

    // Nothing was persisted.
    assert_eq!(f.service.latest(f.project).await.unwrap_err().code, "policy_missing");
}

#[tokio::test]
async fn unknown_project_is_refused() {
    let f = fixture().await;
    let ghost = ProjectId::new();
    let refused = f.service.set(ghost, policy(ghost, 1)).await.unwrap_err();
    assert_eq!(refused.code, "project_missing");
}

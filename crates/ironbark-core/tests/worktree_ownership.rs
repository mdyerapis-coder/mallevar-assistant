//! H1 invariants for the shared native boundary's workspace half.
//!
//! A native worker runs in a core-owned git worktree derived from the project's
//! base commit. The original checkout is never mutated except by an approved
//! integration, and exactly one writer may hold a worktree at a time.
//!
//! Covered here (the Rust-side rules; the adapter-side permission/cancel rules
//! live in `packages/adapters/test/native-boundaries.test.ts`):
//! - one writer per worktree, readers may coexist
//! - a run from another project cannot borrow this project's worktree
//! - integration requires passing review AND verification receipts
//! - a failed receipt is never overwritten by a later pass
//! - a diff that changed after the action was described cannot integrate
//! - releasing the last lease removes the worktree, never the original
use std::path::{Path, PathBuf};
use std::process::Command;

use ironbark_core::contracts::{ApprovalGrant, CheckReceipt, JobId, NodeId, ProjectId, ProjectPolicy, RunId, SpendPolicy};
use ironbark_core::store::{events::now_ms, Store};
use ironbark_core::workspaces::{CheckKind, DiffSummary, WorkspaceMode, WorkspaceService, WorktreeLease};
use tempfile::tempdir;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    project: ProjectId,
    runs: Vec<RunId>,
    repo: PathBuf,
    worktrees: PathBuf,
    service: WorkspaceService,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git must be installed for the workspace tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

async fn insert_project(store: &Store, id: ProjectId, name: &str, root: &str) {
    let (id_str, name, root) = (id.to_string(), name.to_string(), root.to_string());
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects(id,name,root,created_at) VALUES(?1,?2,?3,?4)",
                rusqlite::params![id_str, name, root, now_ms()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Insert a committed policy for a project. `acquire` resolves the base commit
/// through the policy, so every project under test needs one.
async fn insert_policy(store: &Store, project: ProjectId, root: &str) {
    let policy = serde_json::to_string(&ProjectPolicy {
        version: 1,
        project_id: project,
        allowed_connection_ids: vec!["local".into()],
        allowed_tools: vec!["integration".into(), "file_write".into()],
        read_roots: vec![root.to_string()],
        write_roots: vec![root.to_string()],
        network_destinations: vec![],
        local_only: true,
        max_workers: 2,
        max_requests: 16,
        max_cost_microusd: None,
        spend_policy: SpendPolicy::FreeOnly,
        allow_unknown_cost: false,
    })
    .unwrap();
    let (project_str, policy) = (project.to_string(), policy);
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO policies(project_id,version,body) VALUES(?1,1,?2)",
                rusqlite::params![project_str, policy],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn insert_run(store: &Store, project: ProjectId, run: RunId) {
    let (project_str, run_str) = (project.to_string(), run.to_string());
    let job = JobId::new().to_string();
    let node = NodeId::new().to_string();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![job, project_str],
            )?;
            tx.execute(
                "INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![node, job],
            )?;
            tx.execute(
                "INSERT INTO runs(id,node_id,state,body) VALUES(?1,?2,'ready','{}')",
                rusqlite::params![run_str, node],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// A real git repository with two commits, plus one project and three runs.
async fn setup() -> Fixture {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.txt"), "one\n").unwrap();
    git(&repo, &["add", "src/lib.txt"]);
    git(&repo, &["commit", "-q", "-m", "add lib"]);

    let worktrees = dir.path().join("worktrees");
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let project = ProjectId::new();
    let root = repo.to_string_lossy().into_owned();
    insert_project(&store, project, "worktree-fixture", &root).await;
    insert_policy(&store, project, &root).await;

    let mut runs = Vec::new();
    for _ in 0..3 {
        let run = RunId::new();
        insert_run(&store, project, run).await;
        runs.push(run);
    }
    let service = WorkspaceService::with_worktree_root(store.clone(), worktrees.clone());
    Fixture { _dir: dir, store, project, runs, repo, worktrees, service }
}

fn passing_check(name: &str) -> CheckReceipt {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "passed": true,
        "evidence_digest": "c".repeat(64),
        "details": "fixture receipt",
    }))
    .unwrap()
}

fn failing_check(name: &str) -> CheckReceipt {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "passed": false,
        "details": "fixture receipt",
    }))
    .unwrap()
}

/// Write a change inside the leased worktree so the diff is non-empty.
fn change_worktree(lease_path: &str, contents: &str) {
    std::fs::write(Path::new(lease_path).join("src/lib.txt"), contents).unwrap();
}

async fn approve(service: &WorkspaceService, lease: &WorktreeLease) -> ApprovalGrant {
    let action = service.describe_integration(lease).await.unwrap();
    service.approve_integration(action, now_ms() + 3_600_000).await.unwrap()
}

#[tokio::test]
async fn one_writer_per_worktree_and_readers_may_coexist() {
    let f = setup().await;
    let writer = f.service.acquire(f.project, f.runs[0], WorkspaceMode::Writer).await.unwrap();
    assert_eq!(writer.mode, WorkspaceMode::Writer);

    // A second writer for the same project must be refused.
    let busy = f.service.acquire(f.project, f.runs[1], WorkspaceMode::Writer).await.unwrap_err();
    assert_eq!(busy.code, "workspace_busy", "one writer owns a worktree at a time");

    // Readers are not blocked by the writer.
    let reader = f.service.acquire(f.project, f.runs[1], WorkspaceMode::ReadOnly).await.unwrap();
    let reader2 = f.service.acquire(f.project, f.runs[2], WorkspaceMode::ReadOnly).await.unwrap();
    assert_eq!(reader.path, writer.path, "leases for a project share one worktree");
    assert_eq!(reader2.path, writer.path);

    // A reader may not integrate: only the writer lease can.
    let grant = approve(&f.service, &reader).await;
    let err = f.service.integrate(reader.clone(), grant).await.unwrap_err();
    assert_eq!(err.code, "lease_not_writer");

    // Releasing the writer (with readers still active) keeps the worktree.
    f.service.release(writer).await.unwrap();
    assert!(Path::new(&reader.path).exists(), "an active reader keeps the worktree on disk");
    f.service.release(reader).await.unwrap();
    f.service.release(reader2).await.unwrap();
    assert!(
        !f.worktrees.join(f.project.to_string()).exists(),
        "the last release removes the worktree"
    );
    assert!(f.repo.join("README.md").exists(), "the original checkout is untouched");
}

#[tokio::test]
async fn a_run_from_another_project_cannot_borrow_the_worktree() {
    let f = setup().await;
    let other_project = ProjectId::new();
    let other_run = RunId::new();
    let root = f.repo.to_string_lossy().into_owned();
    insert_project(&f.store, other_project, "other", &root).await;
    insert_policy(&f.store, other_project, &root).await;
    insert_run(&f.store, other_project, other_run).await;

    // The lease is requested for `other_project` while the run belongs to the
    // other project: it must be refused, not silently borrowed.
    let err = f.service.acquire(other_project, f.runs[0], WorkspaceMode::Writer).await.unwrap_err();
    assert_eq!(err.code, "run_mismatch");
}

#[tokio::test]
async fn integration_requires_passing_review_and_verification_receipts() {
    let f = setup().await;
    let lease = f.service.acquire(f.project, f.runs[0], WorkspaceMode::Writer).await.unwrap();
    change_worktree(&lease.path, "approved change\n");

    // No receipts at all: refused, and nothing reaches the original checkout.
    let grant = approve(&f.service, &lease).await;
    let err = f.service.integrate(lease.clone(), grant).await.unwrap_err();
    assert_eq!(err.code, "integration_receipts_missing");
    assert_eq!(std::fs::read_to_string(f.repo.join("src/lib.txt")).unwrap(), "one\n", "nothing applied");

    // Both receipts present and passing: the observed diff integrates.
    f.service.record_check(lease.run_id, CheckKind::Review, passing_check("review")).await.unwrap();
    f.service.record_check(lease.run_id, CheckKind::Verification, passing_check("verify")).await.unwrap();
    let grant = approve(&f.service, &lease).await;
    let receipt = f.service.integrate(lease.clone(), grant).await.unwrap();
    assert!(!receipt.integration_commit.is_empty());
    assert_eq!(
        git(&f.repo, &["rev-parse", "HEAD"]),
        receipt.integration_commit,
        "the checkout advanced to the integration commit"
    );
    assert_eq!(std::fs::read_to_string(f.repo.join("src/lib.txt")).unwrap(), "approved change\n");
    assert_eq!(
        f.service.release(lease).await.unwrap_err().code,
        "lease_released",
        "a successful integration releases its own lease"
    );
}

#[tokio::test]
async fn a_failing_receipt_blocks_integration_and_is_not_overwritten_by_a_later_pass() {
    let f = setup().await;
    let lease = f.service.acquire(f.project, f.runs[1], WorkspaceMode::Writer).await.unwrap();
    change_worktree(&lease.path, "unverified change\n");
    f.service.record_check(lease.run_id, CheckKind::Review, failing_check("review")).await.unwrap();
    f.service.record_check(lease.run_id, CheckKind::Verification, passing_check("verify")).await.unwrap();

    let grant = approve(&f.service, &lease).await;
    let err = f.service.integrate(lease.clone(), grant).await.unwrap_err();
    assert_eq!(err.code, "integration_not_passed");

    // A later passing review for the same run does not erase the recorded
    // failure: every recorded receipt for the run must pass, so the owner
    // re-runs the work rather than silently overwriting a failed verification.
    f.service.record_check(lease.run_id, CheckKind::Review, passing_check("review-2")).await.unwrap();
    let grant = approve(&f.service, &lease).await;
    let err = f.service.integrate(lease.clone(), grant).await.unwrap_err();
    assert_eq!(err.code, "integration_not_passed", "a failed receipt is never overwritten");
    assert_eq!(std::fs::read_to_string(f.repo.join("src/lib.txt")).unwrap(), "one\n", "the original is untouched");
}

#[tokio::test]
async fn a_diff_that_changed_after_describe_cannot_integrate() {
    let f = setup().await;
    let lease = f.service.acquire(f.project, f.runs[0], WorkspaceMode::Writer).await.unwrap();
    change_worktree(&lease.path, "reviewed content\n");
    f.service.record_check(lease.run_id, CheckKind::Review, passing_check("review")).await.unwrap();
    f.service.record_check(lease.run_id, CheckKind::Verification, passing_check("verify")).await.unwrap();
    let head_before = git(&f.repo, &["rev-parse", "HEAD"]);
    let grant = approve(&f.service, &lease).await;

    // The worker keeps editing after the owner approved a specific diff.
    change_worktree(&lease.path, "sneaked extra content\n");

    let err = f.service.integrate(lease.clone(), grant).await.unwrap_err();
    assert_eq!(err.code, "stale_integration", "a grant binds one observed diff");
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), head_before, "the original checkout did not move");
    assert_eq!(
        std::fs::read_to_string(f.repo.join("src/lib.txt")).unwrap(),
        "one\n",
        "no unapproved content reached the original checkout"
    );
}

#[tokio::test]
async fn worktree_diff_reports_the_observed_change_without_touching_the_original() {
    let f = setup().await;
    let lease = f.service.acquire(f.project, f.runs[0], WorkspaceMode::Writer).await.unwrap();
    change_worktree(&lease.path, "observed\n");
    let summary: DiffSummary = f.service.worktree_diff(&lease).await.unwrap();
    assert_eq!(summary.base_commit, lease.base_commit);
    assert_eq!(
        summary.changed_paths,
        vec!["src/lib.txt".to_string()],
        "only the touched path appears in the diff"
    );
    assert_eq!(std::fs::read_to_string(f.repo.join("src/lib.txt")).unwrap(), "one\n");
}

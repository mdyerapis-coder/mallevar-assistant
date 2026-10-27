// H1: core-owned Git worktrees. The original project checkout is never
// modified: every writer lease is a fresh `git worktree add --detach <base>`
// from the project-selected base commit. One writer per worktree (the second
// writer gets the typed `workspace_busy` error); read-only reviewer leases may
// share the worktree. Dirty or non-Git checkouts require an explicit workspace
// choice — the core never invents an implicit baseline. Integration is the
// single cutover: it re-checks the base/diff digest, the core-issued approval
// grant and the review/verification receipts, then applies ONLY the approved
// diff into the original checkout as one reviewable commit. No automatic
// checkout/reset/merge of unrelated changes.
use std::{fs, path::{Path, PathBuf}};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use sha2::Digest as _;

use crate::{
    contracts::{ActionId, ActionSpec, ApprovalGrant, CheckReceipt, CoreEvent, GrantId, ProjectId, RunId, Digest, WorkspaceMode},
    error::{CoreError, Result},
    policy::service::{ActionOutcome, ApprovalDecision, ApprovalService, OutcomeEvidence},
    store::{events::now_ms, Store},
    workspaces::{diffs::{DiffCapture, DiffSummary}, git_failed, workspace_busy},
};

/// Leaf-owned tables, created on first use. The shared schema in migrations.rs
/// is not modified.
const WORKSPACE_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS workspace_choices (
  project_id TEXT PRIMARY KEY,
  base_ref TEXT NOT NULL,
  base_commit TEXT NOT NULL,
  chosen_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS worktree_leases (
  lease_id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  path TEXT NOT NULL,
  base_commit TEXT NOT NULL,
  mode TEXT NOT NULL CHECK(mode IN ('reader','writer')),
  state TEXT NOT NULL CHECK(state IN ('active','released','integrated')),
  created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS worktree_leases_one_active_writer
  ON worktree_leases(path) WHERE mode='writer' AND state='active';
CREATE TABLE IF NOT EXISTS integration_receipts (
  receipt_digest TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  lease_id TEXT NOT NULL,
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS workspace_checks (
  id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('review','verification')),
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS workspace_checks_run_kind
  ON workspace_checks(run_id, kind);
"#;

/// The two owner-recorded receipt kinds an integration must pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind { Review, Verification }

impl CheckKind {
    fn as_str(&self) -> &'static str {
        match self { CheckKind::Review => "review", CheckKind::Verification => "verification" }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeLease {
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub path: String,
    pub base_commit: String,
    pub mode: WorkspaceMode,
    pub lease_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationReceipt {
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub lease_id: String,
    pub grant_id: GrantId,
    pub base_commit: String,
    pub head_commit: String,
    pub worktree_digest: Digest,
    pub review_receipts: Vec<CheckReceipt>,
    pub verification_receipts: Vec<CheckReceipt>,
    pub applied_paths: Vec<String>,
    pub original_head: String,
    pub integration_commit: String,
    pub receipt_digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceChoice {
    pub base_ref: String,
    pub base_commit: String,
}

/// Run `git apply --index` (plus `extra` flags) in `root`, feeding `diff` on
/// stdin. The child must receive the patch before its stdin closes, so the
/// helper writes the diff and drops the pipe before waiting.
fn git_apply(root: &std::path::Path, diff: &str, extra: &[&str]) -> std::result::Result<(), String> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["apply", "--index"])
        .args(extra)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("git could not be spawned: {error}"))?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| "git apply stdin is unavailable".to_string())?;
        stdin
            .write_all(diff.as_bytes())
            .map_err(|error| format!("diff pipe failed: {error}"))?;
    }
    // Dropping stdin closes the pipe so git sees EOF and can apply the patch.
    drop(child.stdin.take());
    let output = child.wait_with_output().map_err(|error| format!("git apply stopped: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[derive(Clone)]
pub struct WorkspaceService {
    store: Store,
    worktree_root: PathBuf,
    approvals: ApprovalService,
}

impl WorkspaceService {
    pub fn new(store: Store) -> Self {
        let worktree_root = crate::paths::RuntimePaths::from_env()
            .map(|paths| paths.state_dir.join("worktrees"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/ironbark-worktrees"));
        Self::with_worktree_root(store, worktree_root)
    }

    /// Owner-selected worktree root; production keeps the state-dir root,
    /// tests and the live smoke pass an isolated directory.
    pub fn with_worktree_root(store: Store, worktree_root: PathBuf) -> Self {
        let approvals = ApprovalService::new(store.clone());
        Self { store, worktree_root, approvals }
    }

    async fn ensure_schema(&self) -> Result<()> {
        self.store.with_connection(|conn| { conn.execute_batch(WORKSPACE_SCHEMA_SQL)?; Ok(()) }).await
    }

    fn project_row(conn: &rusqlite::Connection, project: ProjectId) -> Result<(PathBuf, crate::contracts::ProjectPolicy)> {
        let root: Option<String> =
            conn.query_row("SELECT root FROM projects WHERE id=?1", [project.to_string()], |r| r.get(0)).optional()?;
        let root = PathBuf::from(root.ok_or_else(|| CoreError::new("project_missing", "Project is not registered"))?);
        let body: Option<String> = conn
            .query_row("SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1", [project.to_string()], |r| r.get(0))
            .optional()?;
        let policy: crate::contracts::ProjectPolicy =
            serde_json::from_str(body.as_ref().ok_or_else(|| CoreError::new("policy_missing", "Project has no policy"))?)?;
        if policy.project_id != project {
            return Err(CoreError::new("policy_mismatch", "Stored policy belongs to another project"));
        }
        Ok((root, policy))
    }

    /// Record the explicit workspace choice for a project. Required before any
    /// acquire when the checkout is dirty or is not a Git repository.
    pub async fn choose_workspace(&self, project: ProjectId, base_ref: &str) -> Result<WorkspaceChoice> {
        validate_ref(base_ref)?;
        self.ensure_schema().await?;
        let (root, _policy) = self
            .store
            .with_connection({
                let project = project;
                move |conn| Self::project_row(conn, project)
            })
            .await?;
        let root2 = root.clone();
        let base_ref_owned = base_ref.to_owned();
        let base_commit = tokio::task::spawn_blocking(move || resolve_base_ref(&root2, &base_ref_owned))
            .await
            .map_err(|e| CoreError::new("workspace_setup_failed", format!("base resolution stopped: {e}")))?
            .map_err(|_| {
                CoreError::new(
                    "workspace_choice_required",
                    "Base ref does not resolve to a commit; the explicit workspace choice is refused",
                )
            })?;
        let now = now_ms();
        self.store
            .with_transaction({
                let id = project.to_string();
                let base_commit = base_commit.clone();
                let base_ref = base_ref.to_owned();
                move |tx| {
                    tx.execute(
                        "INSERT INTO workspace_choices(project_id, base_ref, base_commit, chosen_at) VALUES(?1,?2,?3,?4)
                         ON CONFLICT(project_id) DO UPDATE SET base_ref=excluded.base_ref, base_commit=excluded.base_commit, chosen_at=excluded.chosen_at",
                        params![id, base_ref, base_commit, now],
                    )?;
                    Ok(())
                }
            })
            .await?;
        Ok(WorkspaceChoice { base_ref: base_ref.to_owned(), base_commit })
    }

    /// (base_ref, base_commit): the recorded explicit choice, else a CLEAN
    /// checkout's HEAD. Anything else is an explicit-choice requirement.
    async fn resolve_base(&self, project: ProjectId, root: &Path) -> Result<(String, String)> {
        let explicit = self
            .store
            .with_connection({
                let id = project.to_string();
                move |conn| {
                    let value: Option<(String, String)> = conn
                        .query_row("SELECT base_ref, base_commit FROM workspace_choices WHERE project_id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    Ok(value)
                }
            })
            .await?;
        let root2 = root.to_path_buf();
        tokio::task::spawn_blocking(move || match explicit {
            Some((ref_, commit)) => Ok((ref_, commit)),
            None => {
                if !is_git_repository(&root2) {
                    return Err(CoreError::new(
                        "workspace_choice_required",
                        "Project root is not a Git repository; an explicit workspace choice is required before acquiring a worktree",
                    ));
                }
                if dirty(&root2) {
                    return Err(CoreError::new(
                        "workspace_choice_required",
                        "Project checkout is dirty; choose an explicit base commit before acquiring a worktree",
                    ));
                }
                head_commit(&root2).map(|commit| ("HEAD".to_owned(), commit)).ok_or_else(|| {
                    CoreError::new("workspace_choice_required", "Checkout has no HEAD commit to branch from")
                })
            }
        })
        .await
        .map_err(|e| CoreError::new("workspace_setup_failed", format!("base resolution stopped: {e}")))?
    }

    /// Acquire a lease on the project's worktree. The worktree path is stable
    /// per project; a writer lease is exclusive (second writer →
    /// `workspace_busy`), reader leases may share it.
    pub async fn acquire(&self, project: ProjectId, run: RunId, mode: WorkspaceMode) -> Result<WorktreeLease> {
        self.ensure_schema().await?;
        let (root, _policy) = self
            .store
            .with_connection({
                let project = project;
                move |conn| Self::project_row(conn, project)
            })
            .await?;
        let (_base_ref, base_commit) = self.resolve_base(project, &root).await?;
        require_commit(&base_commit)?;
        // A run from another project cannot borrow this project's worktree.
        let belongs: bool = self
            .store
            .with_connection({
                let run = run.to_string();
                let id = project.to_string();
                move |conn| {
                    Ok(conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE r.id=?1 AND j.project_id=?2)",
                        [run, id],
                        |r| r.get(0),
                    )?)
                }
            })
            .await?;
        if !belongs {
            return Err(CoreError::new("run_mismatch", "Run does not belong to this project"));
        }
        fs::create_dir_all(&self.worktree_root).map_err(|error| {
            CoreError::new("workspace_setup_failed", format!("Worktree root is not usable: {error}"))
        })?;
        let target = self.worktree_root.join(project.to_string());
        let target_str = target.to_string_lossy().into_owned();
        // Disk step: create the worktree only when it does not exist yet.
        // `git worktree add --detach <base>` never touches the original tree.
        let root2 = root.clone();
        let base_commit2 = base_commit.clone();
        let target_str2 = target_str.clone();
        let target2 = target.clone();
        let created = tokio::task::spawn_blocking(move || {
            if worktree_occupied(&root2, &target_str2) {
                return Ok(false);
            }
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root2)
                .args(["worktree", "add", &target_str2, "--detach", &base_commit2])
                .output()
                .map_err(|error| CoreError::new("git_spawn_failed", format!("git could not be spawned: {error}")));
            let output = output?;
            if !output.status.success() {
                let _ = fs::remove_dir_all(&target2);
                return Err(git_failed("worktree add", output.status, &String::from_utf8_lossy(&output.stderr)));
            }
            Ok(true)
        })
        .await
        .map_err(|e| CoreError::new("workspace_setup_failed", format!("worktree setup stopped: {e}")))??;
        let lease_id = Uuid::new_v4().to_string();
        let mode_str = match mode { WorkspaceMode::Writer => "writer", WorkspaceMode::ReadOnly => "reader" };
        let (lease_id2, project_id, run_id, tx_target, tx_base) = (lease_id.clone(), project.to_string(), run.to_string(), target_str.clone(), base_commit.clone());
        let result = self
            .store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO worktree_leases(lease_id, project_id, run_id, path, base_commit, mode, state, created_at)
                     VALUES(?1,?2,?3,?4,?5,?6,'active',?7)",
                    params![lease_id2, project_id, run_id, tx_target.clone(), tx_base.clone(), mode_str, now_ms()],
                )
                .map_err(|error| match error {
                    rusqlite::Error::SqliteFailure(f, _)
                        if f.code == rusqlite::ErrorCode::ConstraintViolation && mode == WorkspaceMode::Writer =>
                    {
                        workspace_busy(&tx_target)
                    }
                    other => CoreError::new("storage_error", format!("lease record failed: {other}")),
                })?;
                crate::store::events::append_tx(
                    tx,
                    None,
                    Some(ProjectId(Uuid::parse_str(&project_id).expect("fresh uuid"))),
                    Some(RunId(Uuid::parse_str(&run_id).expect("fresh uuid"))),
                    &CoreEvent::StateChanged { entity_id: tx_target.clone(), state: format!("worktree_acquired:{mode_str}") },
                )?;
                Ok(())
            })
            .await;
        if result.is_err() && created {
            // The durable lease row was not recorded; never leave an orphan
            // worktree the registry does not know about.
            let _ = crate::workspaces::diffs::git(Some(&root), &["worktree", "remove", "--force", &target_str]);
            let _ = fs::remove_dir_all(&target);
        }
        result?;
        Ok(WorktreeLease { project_id: project, run_id: run, path: target_str, base_commit, mode, lease_id })
    }

    /// Release a lease. When the last active lease on the worktree goes away
    /// the worktree is removed from disk and the git registry; the original
    /// checkout is untouched either way.
    pub async fn release(&self, lease: WorktreeLease) -> Result<()> {
        self.ensure_schema().await?;
        let (lease_id, path, project, run) = (lease.lease_id.clone(), lease.path.clone(), lease.project_id, lease.run_id);
        let path_for_cleanup = path.clone();
        let remaining: bool = self
            .store
            .with_transaction(move |tx| {
                let state: String = tx.query_row("SELECT state FROM worktree_leases WHERE lease_id=?1", [lease_id.clone()], |r| r.get(0))?;
                if state != "active" {
                    return Err(CoreError::new("lease_released", "Lease is already released"));
                }
                if tx.execute("UPDATE worktree_leases SET state='released' WHERE lease_id=?1 AND state='active'", [lease_id.clone()])? != 1 {
                    return Err(CoreError::new("lease_released", "Lease is already released"));
                }
                let other: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM worktree_leases WHERE path=?1 AND state='active' AND lease_id<>?2)",
                    params![path.clone(), lease_id.clone()],
                    |r| r.get(0),
                )?;
                crate::store::events::append_tx(
                    tx,
                    None,
                    Some(project),
                    Some(run),
                    &CoreEvent::StateChanged { entity_id: lease_id.clone(), state: "worktree_released".into() },
                )?;
                Ok(other)
            })
            .await?;
        if !remaining {
            let root: String = self
                .store
                .with_connection({
                    let project = project;
                    move |conn| Ok(conn.query_row("SELECT root FROM projects WHERE id=?1", [project.to_string()], |r| r.get::<_, String>(0))?)
                })
                .await?;
            let path2 = path_for_cleanup;
            let _ = tokio::task::spawn_blocking(move || {
                let _ = crate::workspaces::diffs::git(Some(&PathBuf::from(root)), &["worktree", "remove", "--force", &path2]);
                fs::remove_dir_all(Path::new(&path2)).ok();
            })
            .await;
        }
        Ok(())
    }

    /// The canonical observation bound into integration actions: base + head +
    /// full binary diff digest, computed from the live worktree.
    fn integration_state_digest(summary: &DiffSummary) -> Result<Digest> {
        let canonical = serde_json::to_vec_pretty(&serde_json::json!({
            "base": summary.base_commit,
            "head": summary.head_commit,
            "diff": summary.worktree_digest.as_str(),
        }))
        .map_err(|_| CoreError::new("invalid_dto", "integration state serialization failed"))?;
        let digest = format!("{:x}", sha2::Sha256::digest(&canonical));
        Digest::parse(&digest)
    }

    async fn finish_outcome(
        &self,
        consumption: crate::policy::service::ConsumptionReceipt,
        action: &ActionSpec,
        state: &Digest,
        outcome: ActionOutcome,
        now: i64,
    ) -> Result<()> {
        self.approvals
            .finish(
                consumption,
                OutcomeEvidence { outcome, receipt_digest: state.clone(), observed_state_digest: state.clone() },
                now,
            )
            .await
    }

    /// Describe the `integration` action for a lease's current diff. The
    /// action binds the observed worktree digest and base commit; a grant
    /// issued for one diff cannot integrate another.
    pub async fn describe_integration(&self, lease: &WorktreeLease) -> Result<ActionSpec> {
        let (root, policy) = self
            .store
            .with_connection({
                let project = lease.project_id;
                move |conn| Self::project_row(conn, project)
            })
            .await?;
        let canonical_root = Path::new(&root)
            .canonicalize()
            .map_err(|e| CoreError::new("invalid_path", format!("Project root cannot be canonicalized: {e}")))?;
        let worktree = PathBuf::from(&lease.path);
        let base = lease.base_commit.clone();
        let summary = tokio::task::spawn_blocking(move || DiffCapture::capture(&worktree, &base))
            .await
            .map_err(|e| CoreError::new("workspace_setup_failed", format!("diff capture stopped: {e}")))?;
        let summary = summary?;
        let state = Self::integration_state_digest(&summary)?;
        Ok(ActionSpec {
            id: ActionId::new(),
            run_id: lease.run_id,
            project_id: lease.project_id,
            kind: "integration".into(),
            canonical_args: serde_json::json!({
                "worktree_digest": summary.worktree_digest.as_str(),
                "base_commit": lease.base_commit,
            }),
            scope: serde_json::json!({ "tools": ["integration"], "paths": [canonical_root.to_string_lossy()] }),
            destination: None,
            policy_version: policy.version,
            expected_state_digest: state,
        })
    }

    /// Issue the owner's approval grant for an integration action. Core-issued
    /// only: a role definition or a worker-supplied grant never reaches this
    /// path.
    pub async fn approve_integration(&self, action: ActionSpec, expires_at: i64) -> Result<ApprovalGrant> {
        let now = now_ms();
        if expires_at <= now {
            return Err(CoreError::new("integration_grant_expired", "Grant expiry must be in the future"));
        }
        let decision = self.approvals.record(action.clone(), now).await?;
        if !matches!(decision, crate::policy::PolicyDecision::AwaitApproval) {
            return Err(CoreError::new("integration_action_denied", "Current policy does not permit this integration action"));
        }
        self.approvals
            .decide(action.id, ApprovalDecision::Approve { expires_at }, now)
            .await?
            .ok_or_else(|| CoreError::new("integration_grant_denied", "No grant was issued for this action"))
    }

    /// The current worktree diff (for check receipts before integration).
    pub async fn worktree_diff(&self, lease: &WorktreeLease) -> Result<DiffSummary> {
        let worktree = PathBuf::from(&lease.path);
        let base = lease.base_commit.clone();
        tokio::task::spawn_blocking(move || DiffCapture::capture(&worktree, &base))
            .await
            .map_err(|e| CoreError::new("workspace_setup_failed", format!("diff capture stopped: {e}")))?
    }

    /// Persist a review or verification check receipt for a run. `integrate`
    /// only reads the owner-recorded receipts; it never trusts a worker.
    pub async fn record_check(&self, run: RunId, kind: CheckKind, receipt: CheckReceipt) -> Result<()> {
        self.ensure_schema().await?;
        let id = Uuid::new_v4().to_string();
        let run_id = run.to_string();
        let kind_str = kind.as_str().to_owned();
        let body = serde_json::to_string(&receipt)?;
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO workspace_checks(id,run_id,kind,body,created_at) VALUES(?1,?2,?3,?4,?5)",
                    params![id, run_id, kind_str, body, now_ms()],
                )?;
                Ok(())
            })
            .await
    }


    /// The single cutover. Checks, in order:
    ///   1. review and verification receipts exist and all pass;
    ///   2. the lease is an active writer lease and the action/grant bind it;
    ///   3. a FRESH diff of the worktree still matches the granted action's
    ///      state digest (any post-grant write makes the grant stale);
    ///   4. the original checkout's HEAD is still at the lease's base commit
    ///      (no unrelated checkout/reset/merge is replayed);
    ///   5. the owner's grant is consumed BEFORE the disk apply.
    /// Then applies ONLY the approved diff into the original checkout as one
    /// reviewable commit and records the durable receipt. Any failure leaves
    /// the original checkout at its previous HEAD.
    pub async fn integrate(&self, lease: WorktreeLease, grant: ApprovalGrant) -> Result<IntegrationReceipt> {
        self.ensure_schema().await?;
        // 1. the action is recovered from the core-issued grant; the owner
        //    signed this exact state digest, never an arbitrary diff.
        let action: ActionSpec = self
            .store
            .with_connection({
                let action_id = grant.action_id;
                move |conn| {
                    let body: String = conn.query_row("SELECT body FROM actions WHERE id=?1", [action_id.to_string()], |r| r.get(0))?;
                    Ok(serde_json::from_str(&body)?)
                }
            })
            .await?;
        if action.kind != "integration" || action.project_id != lease.project_id || action.run_id != lease.run_id {
            return Err(CoreError::new("integration_action_mismatch", "Action does not bind this lease"));
        }
        if grant.action_id != action.id {
            return Err(CoreError::new("grant_action_mismatch", "Grant does not cover this integration action"));
        }
        if lease.mode != WorkspaceMode::Writer {
            return Err(CoreError::new("lease_not_writer", "Only a writer lease can integrate"));
        }
        // The owner's recorded review and verification receipts, all passing.
        let (review_receipts, verification_receipts) = self
            .store
            .with_connection({
                let run = action.run_id;
                move |conn| {
                    let mut stmt = conn.prepare("SELECT body FROM workspace_checks WHERE run_id=?1 AND kind=?2")?;
                    let mut load = |kind: &str| -> std::result::Result<Vec<CheckReceipt>, CoreError> {
                        let bodies: Vec<String> = stmt
                            .query_map(params![run.to_string(), kind], |r| r.get(0))
                            .map_err(|e| CoreError::new("storage_error", format!("check load failed: {e}")))?
                            .collect::<std::result::Result<_, _>>()
                            .map_err(|e| CoreError::new("storage_error", format!("check row failed: {e}")))?;
                        bodies
                            .into_iter()
                            .map(|body| serde_json::from_str(&body))
                            .collect::<std::result::Result<_, _>>()
                            .map_err(|e| CoreError::new("invalid_dto", format!("check receipt is not a receipt: {e}")))
                    };
                    let review = load("review")?;
                    let verification = load("verification")?;
                    Ok((review, verification))
                }
            })
            .await?;
        if review_receipts.is_empty() || verification_receipts.is_empty() {
            return Err(CoreError::new("integration_receipts_missing", "Integration requires review and verification receipts"));
        }
        if review_receipts.iter().any(|r| !r.passed) || verification_receipts.iter().any(|r| !r.passed) {
            return Err(CoreError::new("integration_not_passed", "All review and verification receipts must pass"));
        }
        if lease.mode != WorkspaceMode::Writer {
            return Err(CoreError::new("lease_not_writer", "Only a writer lease can integrate"));
        }
        if action.kind != "integration" || action.project_id != lease.project_id || action.run_id != lease.run_id {
            return Err(CoreError::new("integration_action_mismatch", "Action does not bind this lease"));
        }
        if grant.action_id != action.id {
            return Err(CoreError::new("grant_action_mismatch", "Grant does not cover this integration action"));
        }
        let lease_id = lease.lease_id.clone();
        let lease_state: String = self
            .store
            .with_connection(move |conn| Ok(conn.query_row("SELECT state FROM worktree_leases WHERE lease_id=?1", [lease_id], |r| r.get(0))?))
            .await?;
        if lease_state != "active" {
            return Err(CoreError::new("lease_released", "Lease is not active"));
        }
        let (root, _policy) = self
            .store
            .with_connection({
                let project = lease.project_id;
                move |conn| Self::project_row(conn, project)
            })
            .await?;
        let worktree = PathBuf::from(&lease.path);
        let base = lease.base_commit.clone();
        let capture_wt = worktree.clone();
        let capture_base = base.clone();
        // 3. fresh observation; the grant bound a digest, not a promise.
        let summary = tokio::task::spawn_blocking(move || DiffCapture::capture(&capture_wt, &capture_base))
            .await
            .map_err(|e| CoreError::new("workspace_setup_failed", format!("diff capture stopped: {e}")))?;
        let summary = summary?;
        let state = Self::integration_state_digest(&summary)?;
        if state != action.expected_state_digest {
            return Err(CoreError::new(
                "stale_integration",
                "Worktree changed after the integration action was described; the grant is stale",
            ));
        }
        // 4. the original HEAD must still be at the lease's base commit.
        let root2 = root.clone();
        let original_head =
            tokio::task::spawn_blocking(move || head_commit(&root2)).await.map_err(|e| {
                CoreError::new("workspace_setup_failed", format!("head resolution stopped: {e}"))
            })?;
        let original_head =
            original_head.ok_or_else(|| CoreError::new("not_a_git_repository", "Original checkout no longer has a HEAD"))?;
        if original_head != lease.base_commit {
            return Err(CoreError::new(
                "original_changed",
                "The original checkout moved since the lease was acquired; integration refuses to merge unrelated changes",
            ));
        }
        let diff_wt = worktree.clone();
        let diff_base = base.clone();
        let diff_text = tokio::task::spawn_blocking(move || DiffCapture::diff_text(&diff_wt, &diff_base))
            .await
            .map_err(|e| CoreError::new("workspace_setup_failed", format!("diff capture stopped: {e}")))?;
        let diff_text = diff_text?;
        // 5. consume the grant BEFORE any disk side effect; a crash never
        // re-arms it.
        let now = now_ms();
        let consumption = self
            .approvals
            .consume(action.clone(), grant.id, state.clone(), now)
            .await
            .map_err(|error| CoreError::new("integration_grant_denied", format!("{}: {}", error.code, error.message)))?;
        let run_id = lease.run_id;
        let root2 = root.clone();
        let run_for_commit = run_id.to_string();
        let applied = tokio::task::spawn_blocking(move || {
            // Both the check and the apply must receive the diff on stdin.
            // `Command::output()` with a piped stdin hands the child an empty
            // patch (the write end is dropped), so the check would reject every
            // diff as "No valid patches in input".
            let check = git_apply(&root2, &diff_text, &["--check"])
                .map_err(|error| format!("approved diff does not apply cleanly to the current checkout: {error}"))?;
            let () = check;
            git_apply(&root2, &diff_text, &[])
                .map_err(|error| format!("approved diff failed to apply to the current checkout: {error}"))?;
            let commit = std::process::Command::new("git")
                .arg("-C")
                .arg(&root2)
                .args([
                    "-c", "user.name=ironbark-core",
                    "-c", "user.email=core@ironbark.local",
                    "-c", "commit.gpgsign=false",
                    "commit", "--no-verify", "-m",
                ])
                .arg(format!("ironbark integration: approved diff for run {run_for_commit}"))
                // No GIT_AUTHOR_DATE/GIT_COMMITTER_DATE override: "now" is not a
                // date git accepts, and the default (the current time) is what
                // the integration commit should carry.
                .output()
                .map_err(|error| format!("git could not be spawned: {error}"))?;
            if !commit.status.success() {
                return Err(format!(
                    "approved diff applied but could not be committed: {}",
                    String::from_utf8_lossy(&commit.stderr).trim()
                ));
            }
            let sha = std::process::Command::new("git")
                .arg("-C")
                .arg(&root2)
                .args(["rev-parse", "HEAD"])
                .output()
                .map_err(|error| format!("git could not be spawned: {error}"))?;
            if !sha.status.success() {
                return Err("integration commit is not resolvable".to_owned());
            }
            let sha = String::from_utf8_lossy(&sha.stdout).trim().to_owned();
            Ok::<_, String>(sha)
        })
        .await
        .map_err(|e| CoreError::new("integration_apply_failed", format!("apply stopped: {e}")))?;
        let integration_commit = match applied {
            Ok(commit) => commit,
            Err(message) => {
                let _ = self.finish_outcome(consumption, &action, &state, ActionOutcome::Failed, now).await;
                return Err(CoreError::new("integration_apply_failed", message));
            }
        };
        let mut receipt = IntegrationReceipt {
            project_id: lease.project_id,
            run_id,
            lease_id: lease.lease_id.clone(),
            grant_id: grant.id,
            base_commit: lease.base_commit.clone(),
            head_commit: summary.head_commit.clone(),
            worktree_digest: summary.worktree_digest.clone(),
            review_receipts,
            verification_receipts,
            applied_paths: summary.changed_paths.clone(),
            original_head: original_head.clone(),
            integration_commit: integration_commit.clone(),
            receipt_digest: Digest::parse(&"0".repeat(64))?,
        };
        let canonical = serde_json::to_vec_pretty(&serde_json::json!({
            "project_id": receipt.project_id.to_string(),
            "run_id": receipt.run_id.to_string(),
            "lease_id": receipt.lease_id,
            "grant_id": receipt.grant_id.to_string(),
            "base_commit": receipt.base_commit,
            "head_commit": receipt.head_commit,
            "worktree_digest": receipt.worktree_digest.as_str(),
            "review_receipts": receipt.review_receipts,
            "verification_receipts": receipt.verification_receipts,
            "applied_paths": receipt.applied_paths,
            "original_head": receipt.original_head,
            "integration_commit": receipt.integration_commit,
        }))
        .map_err(|_| CoreError::new("invalid_dto", "receipt serialization failed"))?;
        let digest = format!("{:x}", sha2::Sha256::digest(&canonical));
        receipt.receipt_digest = Digest::parse(&digest)?;
        self.finish_outcome(consumption, &action, &state, ActionOutcome::Succeeded, now).await?;
        let (receipt2, project, run) = (receipt.clone(), receipt.project_id, receipt.run_id);
        let result = self
            .store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO integration_receipts(receipt_digest, project_id, run_id, lease_id, body, created_at)
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        receipt2.receipt_digest.as_str(),
                        project.to_string(),
                        run.to_string(),
                        receipt2.lease_id,
                        crate::store::events::redacted_json(&receipt2)?,
                        now_ms()
                    ],
                )?;
                tx.execute("UPDATE worktree_leases SET state='integrated' WHERE lease_id=?1 AND state='active'", [receipt2.lease_id])?;
                crate::store::events::append_tx(
                    tx,
                    None,
                    Some(project),
                    Some(run),
                    &CoreEvent::StateChanged {
                        entity_id: receipt2.receipt_digest.as_str().to_owned(),
                        state: "worktree_integrated".into(),
                    },
                )?;
                Ok(())
            })
            .await;
        if let Err(error) = result {
            // The disk commit exists but the receipt row does not: surface it
            // as an event for reconciliation instead of silently pretending
            // the cutover never happened.
            let _ = self
                .store
                .append(
                    Some(run),
                    CoreEvent::Error {
                        code: "integration_receipt_unrecorded".into(),
                        message: format!(
                            "{}; integration commit {} is recorded in the original repository and requires reconciliation",
                            error.message, integration_commit
                        ),
                        details: serde_json::Value::Null,
                    },
                )
                .await;
            return Err(CoreError::new(
                "integration_receipt_unrecorded",
                format!("{}; integration commit {} requires reconciliation", error.message, integration_commit),
            ));
        }
        Ok(receipt)
    }
}

fn validate_ref(ref_: &str) -> Result<()> {
    if ref_.is_empty() || ref_.len() > 256 || ref_.bytes().any(|b| b == b'\0') || ref_.starts_with('-') {
        return Err(CoreError::new("invalid_base_ref", "Base ref must be nonempty, bounded and not start with '-'"));
    }
    Ok(())
}

fn require_commit(commit: &str) -> Result<()> {
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CoreError::new("invalid_base_commit", "Base does not resolve to a 40-hex commit"));
    }
    Ok(())
}

fn is_git_repository(root: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn dirty(root: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .is_ok_and(|output| {
            output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        })
}

fn head_commit(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    require_commit(&commit).ok()?;
    Some(commit)
}

fn worktree_occupied(root: &Path, target: &str) -> bool {
    let occupied = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim_end() == target)
        })
        .unwrap_or(false);
    occupied || Path::new(target).exists()
}

fn resolve_base_ref(root: &Path, ref_: &str) -> Result<String> {
    if ref_ == "HEAD" {
        return head_commit(root).ok_or_else(|| CoreError::new("workspace_choice_required", "Checkout has no HEAD commit"));
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", &format!("{ref_}^{{commit}}")])
        .output()
        .map_err(|error| CoreError::new("git_spawn_failed", format!("git could not be spawned: {error}")))?;
    if !output.status.success() {
        return Err(CoreError::new("workspace_choice_required", format!("Base ref {ref_} does not resolve to a commit")));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    require_commit(&commit)?;
    Ok(commit)
}

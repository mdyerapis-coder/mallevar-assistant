//! Crash inspection, durable reconciliation and the run lifecycle gates.
//! `Recovery::inspect` only observes (recorded PID identity/start time, actual
//! workspace, durable events) and never kills anything: a recycled PID is
//! reported, never signalled, and no uncertain mutation is replayed.
//! `Recovery::reconcile` commits the owner's decision under valid current
//! authority; uncertain external actions (consumed grant, no durable outcome)
//! must be resolved explicitly before the run may resolve. `RunControl` owns
//! the lifecycle commands (pause/cancel/accept/mutation gate) that feed the
//! transition table. Pause/interrupt/cancel are commands to the native
//! adapter: UI disconnect alone never pauses or cancels a run, and a cancel
//! REQUEST is not a stop receipt.
use std::fs;
use std::path::PathBuf;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::contracts::{
    ActionId, ApprovalGrant, ActionSpec, CheckpointPhase, CoreEvent, Digest, GrantId, ProjectId,
    ProjectPolicy, RecoveryDecision, RecoveryReport, ResultManifest, RunId, RunState, SessionRef,
};
use crate::error::{CoreError, Result};
use crate::policy::service::{ActionOutcome};
use crate::policy::{PolicyDecision, PolicyEngine};
use crate::store::{
    Store,
    events::{append_tx, now_ms, redacted_json},
    objects::{ObjectLimits, ObjectStore},
};

use super::checkpoints::CheckpointStore;
use super::state::{RunEvent, transition, map_state, state_str};

#[derive(Clone)]
pub struct Recovery {
    store: Store,
    project: ProjectId,
    checkpoints: CheckpointStore,
}

/// A durable consumption + observed-outcome pair. The consumption was already
/// committed by the sink before the side effect (a crash never re-arms it); the
/// outcome digest is a real observation, not a frontend-supplied value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationReceipt {
    pub action_id: ActionId,
    pub grant_id: GrantId,
    pub outcome: ActionOutcome,
    pub receipt_digest: Digest,
    pub observed_state_digest: Digest,
}

/// The observed-process view of a run body. The body itself carries other
/// keys (task, artifacts, native_session), so this view must tolerate them:
/// it is an internal reading of shared durable state, not a wire DTO.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct RunBodyMeta {
    recorded_pid: Option<u64>,
    recorded_start_time: Option<i64>,
}

impl Recovery {
    pub fn new(store: Store, project: ProjectId, cas_root: PathBuf) -> Result<Self> {
        let objects = ObjectStore::new(store.clone(), project, cas_root, ObjectLimits::default())?;
        Ok(Self {
            store: store.clone(),
            project,
            checkpoints: CheckpointStore::new(store.clone(), project, objects),
        })
    }

    pub fn checkpoints(&self) -> &CheckpointStore {
        &self.checkpoints
    }

    /// On core death: inspect the recorded PID identity/start time, the actual
    /// workspace and the durable state. Read-only by construction: a PID
    /// recycled by an unrelated process is reported (`reused: true`) but never
    /// signalled, and an uncertain mutation is never replayed here.
    pub async fn inspect(&self, run_id: RunId) -> Result<RecoveryReport> {
        let project_str = self.project.to_string();
        let data = self.store.with_connection(move |conn| {
            let (body, state, run_project): (String, String, String) = conn
                .query_row(
                    "SELECT r.body,r.state,j.project_id FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE r.id=?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| CoreError::new("run_missing", "Run does not exist"))?;
            if run_project != project_str {
                return Err(CoreError::new("project_mismatch", "Run does not belong to this project"));
            }
            let pending_mutation: i64 = conn.query_row(
                "SELECT COUNT(*) FROM actions WHERE run_id=?1 AND state='executing'",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let uncertain =
                uncertain_action_ids(conn, &run_id.to_string())?;
            Ok((body, state, pending_mutation != 0, uncertain))
        })
        .await?;
        let (body, state, pending_mutation, uncertain_actions) = data;
        let state = map_state(&state)?;
        let meta: RunBodyMeta =
            serde_json::from_str(&body).unwrap_or_default();
        let checkpoint = self.checkpoints.latest(run_id).await?;
        let observed = json!({
            "run_body": serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!({})),
            "checkpoint": checkpoint,
            "recorded_process_identity": process_identity(meta.recorded_pid, meta.recorded_start_time),
        });
        let verified = checkpoint
            .as_ref()
            .is_some_and(|record| record.phase == CheckpointPhase::PostVerification);
        let mut recommended: Vec<String> = match (state, pending_mutation, uncertain_actions.is_empty(), verified) {
            (RunState::Blocked, true, _, _) => {
                vec!["resolve_mutation_outcome".into(), "continue_from_checkpoint".into()]
            }
            (RunState::Blocked, false, true, true) => vec!["accept_observed".into(), "resume_read_only".into()],
            (RunState::Blocked, false, true, false) => vec!["resume_read_only".into(), "abandon".into()],
            (RunState::Blocked, _, false, _) => vec!["resolve_outcome_then_accept".into(), "abandon".into()],
            (RunState::Cancelling, _, _, _) => vec!["observe_stop".into(), "abandon".into()],
            _ => vec!["none".into()],
        };
        if !recommended.iter().any(|r| r == "abandon" || r == "observe_stop") {
            recommended.push("abandon".into());
        }
        Ok(RecoveryReport {
            run_id,
            run_state: state,
            recorded_pid: meta.recorded_pid,
            recorded_start_time: meta.recorded_start_time,
            checkpoint,
            observed_workspace: observed,
            pending_mutation,
            uncertain_actions,
            recommended,
        })
    }

    /// Apply the owner's reconciliation decision under valid current authority.
    pub async fn reconcile(&self, run_id: RunId, decision: RecoveryDecision) -> Result<()> {
        match decision {
            RecoveryDecision::AcceptObserved => self.accept_observed(run_id).await,
            RecoveryDecision::ResumeReadOnly => self.resume_read_only(run_id).await,
            RecoveryDecision::ContinueFromCheckpoint => self.continue_from_checkpoint(run_id).await,
            RecoveryDecision::Abandon => self.abandon(run_id).await,
        }
    }

    async fn accept_observed(&self, run_id: RunId) -> Result<()> {
        let current = self.require_nonterminal(run_id).await?;
        if current != RunState::Blocked {
            return Err(CoreError::new("invalid_state", "Accepting the observed state requires a crashed (blocked) run"));
        }
        if !self.uncertain_actions(run_id).await?.is_empty() {
            return Err(CoreError::new(
                "outcome_unresolved",
                "A consumed grant has no durable outcome; resolve it explicitly before accepting the observed state",
            ));
        }
        // The observed durable evidence decides the terminal: a passing
        // post-verification checkpoint (acceptance satisfied, all checks
        // passing) is the only observed evidence of success.
        let checkpoint = self
            .checkpoints
            .latest(run_id)
            .await?
            .ok_or_else(|| CoreError::new("checkpoint_missing", "No durable checkpoint exists; cannot accept the observed state"))?;
        if checkpoint.phase != CheckpointPhase::PostVerification {
            return Err(CoreError::new(
                "acceptance_evidence_missing",
                "The latest checkpoint is not a passing post-verification checkpoint; accepting the observed state would be a false success",
            ));
        }
        let manifest: ResultManifest = serde_json::from_value(checkpoint.manifest).map_err(|_| CoreError::new("storage_corrupt", "Checkpoint manifest is corrupt"))?;
        if !manifest.acceptance_satisfied || !manifest.checks.iter().all(|c| c.passed) || !manifest.unresolved.is_empty() {
            return Err(CoreError::new(
                "acceptance_evidence_missing",
                "The post-verification checkpoint does not record a satisfied acceptance receipt",
            ));
        }
        let new_state = transition(current, RunEvent::AcceptObserved)?;
        self.commit(run_id, current, new_state, "reconcile:accept_observed".to_string()).await?;
        self.reconcile_projection(run_id).await?;
        Ok(())
    }

    async fn resume_read_only(&self, run_id: RunId) -> Result<()> {
        let current = self.require_nonterminal(run_id).await?;
        if current != RunState::Blocked {
            return Err(CoreError::new("invalid_state", "Read-only resume requires a crashed (blocked) run"));
        }
        self.require_checkpoint(run_id).await?;
        // Nothing is re-armed: the run simply returns to Ready and any later
        // mutation still needs a fresh consumption receipt.
        let new_state = transition(current, RunEvent::ResumeReadOnly)?;
        self.commit(run_id, current, new_state, "reconcile:resume_read_only".to_string()).await?;
        Ok(())
    }

    async fn continue_from_checkpoint(&self, run_id: RunId) -> Result<()> {
        let current = self.require_nonterminal(run_id).await?;
        if current != RunState::Blocked {
            return Err(CoreError::new("invalid_state", "Continue-from-checkpoint requires a crashed (blocked) run"));
        }
        self.require_checkpoint(run_id).await?;
        if !self.uncertain_actions(run_id).await?.is_empty() {
            return Err(CoreError::new(
                "outcome_unresolved",
                "A consumed grant has no durable outcome; resolve it explicitly before continuing",
            ));
        }
        // Every bound, unconsumed grant must still be eligible under the
        // CURRENT policy. A stale grant (policy version drift, revoked,
        // expired, digest change) is never reused: the run parks awaiting
        // fresh owner approval.
        if self.stale_grant_exists(run_id).await? {
            self.commit(run_id, current, RunState::AwaitingApproval, "reconcile:stale_grant".to_string()).await?;
            return Err(CoreError::new(
                "stale_grant",
                "A bound grant is stale under the current policy; fresh owner approval is required (the old grant was not reused)",
            ));
        }
        let new_state = transition(current, RunEvent::ContinueFromCheckpoint)?;
        self.commit(run_id, current, new_state, "reconcile:continue_from_checkpoint".to_string()).await?;
        Ok(())
    }

    async fn abandon(&self, run_id: RunId) -> Result<()> {
        let current = self.require_nonterminal(run_id).await?;
        let new_state = transition(current, RunEvent::Abandoned)?;
        self.commit(run_id, current, new_state, "reconcile:abandon".to_string()).await?;
        self.reconcile_projection(run_id).await?;
        Ok(())
    }

    async fn require_nonterminal(&self, run_id: RunId) -> Result<RunState> {
        let state = self.state_of(run_id).await?;
        match state {
            RunState::Succeeded | RunState::Failed | RunState::Cancelled => Err(CoreError::new(
                "run_terminal",
                "Run already resolved; reconciliation requires valid current authority over an unresolved run",
            )),
            _ => Ok(state),
        }
    }

    async fn require_checkpoint(&self, run_id: RunId) -> Result<()> {
        self.checkpoints
            .latest(run_id)
            .await?
            .ok_or_else(|| CoreError::new("checkpoint_missing", "No durable checkpoint exists; cannot resume from crash state"))?;
        Ok(())
    }

    async fn stale_grant_exists(&self, run_id: RunId) -> Result<bool> {
        let now = now_ms();
        let project = self.project;
        self.store
            .with_connection(move |conn| {
                let policy = latest_policy(conn, project)?;
                let run_id = run_id.to_string();
                let rows: Vec<(ActionSpec, ApprovalGrant)> = {
                    let mut stmt = conn.prepare(
                        "SELECT a.body,g.body,g.revoked_at FROM actions a JOIN grants g ON g.action_id=a.id WHERE a.run_id=?1 AND a.state='approved' AND g.consumed_at IS NULL AND g.revoked_at IS NULL",
                    )?;
                    let mapped = stmt.query_map([run_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<i64>>(2)?)))?;
                    let mut result = Vec::new();
                    for row in mapped {
                        let (action_body, grant_body, revoked) = row?;
                        let mut grant: ApprovalGrant = serde_json::from_str(&grant_body).map_err(|_| CoreError::new("storage_corrupt", "Grant body is corrupt"))?;
                        grant.revoked_at = revoked;
                        let action: ActionSpec = serde_json::from_str(&action_body).map_err(|_| CoreError::new("storage_corrupt", "Action body is corrupt"))?;
                        result.push((action, grant));
                    }
                    result
                };
                for (action, grant) in rows {
                    if PolicyEngine::check(&action, None, Some(&grant), &policy, now) != PolicyDecision::Permit {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .await
    }

    async fn uncertain_actions(&self, run_id: RunId) -> Result<Vec<String>> {
        let run_id = run_id.to_string();
        self.store
            .with_connection(move |conn| uncertain_action_ids(conn, &run_id))
            .await
    }

    async fn state_of(&self, run_id: RunId) -> Result<RunState> {
        let state = self
            .store
            .with_connection(move |conn| {
                let state: String = conn
                    .query_row("SELECT state FROM runs WHERE id=?1", [run_id.to_string()], |row| row.get(0))
                    .optional()?
                    .ok_or_else(|| CoreError::new("run_missing", "Run does not exist"))?;
                Ok(state)
            })
            .await?;
        map_state(&state)
    }

    /// Commit a validated transition: state + durable event atomically, with a
    /// compare-and-set on the from-state so a concurrent writer is detected.
    async fn commit(&self, run_id: RunId, from: RunState, to: RunState, event: String) -> Result<()> {
        let project = self.project;
        self.store
            .with_transaction(move |tx| {
                let changed = tx.execute(
                    "UPDATE runs SET state=?2 WHERE id=?1 AND state=?3",
                    params![run_id.to_string(), state_str(to), state_str(from)],
                )?;
                if changed != 1 {
                    return Err(CoreError::new("state_conflict", "Run state changed concurrently; reconcile again"));
                }
                append_tx(
                    tx,
                    None,
                    Some(project),
                    Some(run_id),
                    &CoreEvent::StateChanged {
                        entity_id: run_id.to_string(),
                        state: format!("{} -> {} ({})", state_str(from), state_str(to), event),
                    },
                )?;
                Ok(())
            })
            .await
    }

    /// For conversation runs, project the C4 terminal state onto the assistant
    /// message. A native run has no turn row; that is not an error.
    async fn reconcile_projection(&self, run_id: RunId) -> Result<()> {
        self.store
            .with_connection(move |conn| {
                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT r.state,t.assistant_message_id FROM runs r JOIN conversation_turns t ON t.run_id=r.id WHERE r.id=?1",
                        [run_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((state, assistant)) = row else {
                    return Ok(());
                };
                let status = match state.as_str() {
                    "succeeded" => "complete",
                    "failed" => "failed",
                    "cancelled" => "interrupted",
                    _ => return Err(CoreError::new("run_reconciliation_required", "No C4 terminal receipt has been committed")),
                };
                conn.execute("UPDATE messages SET status=?2 WHERE id=?1", params![assistant, status])?;
                Ok(())
            })
            .await
    }
}

/// The only writer of `runs.state` on the lifecycle path, besides the
/// transition-checked reconcile commit. Pause/interrupt are commands to the
/// native adapter; a UI disconnect never pauses or cancels a run.
#[derive(Clone)]
pub struct RunControl {
    store: Store,
    project: ProjectId,
    recovery: Recovery,
}

impl RunControl {
    pub fn new(store: Store, project: ProjectId, cas_root: PathBuf) -> Result<Self> {
        Ok(Self {
            project,
            recovery: Recovery::new(store.clone(), project, cas_root)?,
            store,
        })
    }

    fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// Record the native session reference + partial artifacts into the durable
    /// run body and advance queued/ready -> running. A freshly created run is
    /// `queued`, so admission (queued -> ready) happens here through the same
    /// transition table rather than by writing `runs.state` directly.
    pub async fn start(
        &self,
        run_id: RunId,
        session: Option<SessionRef>,
        artifacts: Vec<crate::contracts::ArtifactId>,
    ) -> Result<RunState> {
        self.start_observed(run_id, session, artifacts, None, None).await
    }

    /// As `start`, additionally recording the observed native process identity
    /// (pid + start time) that crash inspection reads back. The identity is
    /// observed by the adapter that owns the process, never supplied by model
    /// output.
    pub async fn start_observed(
        &self,
        run_id: RunId,
        session: Option<SessionRef>,
        artifacts: Vec<crate::contracts::ArtifactId>,
        recorded_pid: Option<u64>,
        recorded_start_time: Option<i64>,
    ) -> Result<RunState> {
        let mut current = self.state_of(run_id).await?;
        if current == RunState::Queued {
            let ready = transition(current, RunEvent::Ready)?;
            self.commit(run_id, current, ready, "admitted".to_string()).await?;
            current = ready;
        }
        let new_state = transition(current, RunEvent::Started)?;
        self.update_body(run_id, session, artifacts, recorded_pid, recorded_start_time).await?;
        self.commit(run_id, current, new_state, "native_started".to_string()).await
    }

    /// Pause: retain the native session ref and partial artifacts; the run is
    /// resumable. The native process is asked to stop by the adapter, but the
    /// durable record keeps everything.
    pub async fn pause(&self, run_id: RunId) -> Result<RunState> {
        let current = self.state_of(run_id).await?;
        let new_state = transition(current, RunEvent::Paused)?;
        self.commit(run_id, current, new_state, "paused".to_string()).await
    }

    /// Resume a paused run.
    pub async fn resume(&self, run_id: RunId) -> Result<RunState> {
        let current = self.state_of(run_id).await?;
        let new_state = transition(current, RunEvent::Resumed)?;
        self.commit(run_id, current, new_state, "resumed".to_string()).await
    }

    /// Cancel REQUEST: only moves the run to Cancelling. It is not the stop
    /// receipt; Cancelled requires `observe_stop` on the actual native process.
    pub async fn request_cancel(&self, run_id: RunId) -> Result<RunState> {
        let current = self.state_of(run_id).await?;
        let new_state = transition(current, RunEvent::CancelRequested)?;
        self.commit(run_id, current, new_state, "cancel_requested".to_string()).await
    }

    /// Observed stop receipt from the native process: Cancelling -> Cancelled.
    pub async fn observe_stop(&self, run_id: RunId) -> Result<RunState> {
        let current = self.state_of(run_id).await?;
        let new_state = transition(current, RunEvent::StopObserved)?;
        self.commit(run_id, current, new_state, "stop_observed".to_string()).await
    }

    /// Native completion WITHOUT evidence: the run goes to Reviewing, never to
    /// Succeeded. Success requires `accept` with a passing post-verification
    /// checkpoint.
    pub async fn observe_native_done(&self, run_id: RunId) -> Result<RunState> {
        let current = self.state_of(run_id).await?;
        let new_state = transition(current, RunEvent::NativeDone)?;
        self.commit(run_id, current, new_state, "native_done_without_evidence".to_string()).await
    }

    /// Explicit acceptance receipt: the only path into Succeeded. Requires a
    /// passing post-verification checkpoint (verified evidence).
    pub async fn accept(&self, run_id: RunId) -> Result<RunState> {
        let current = self.state_of(run_id).await?;
        if current != RunState::Reviewing {
            return Err(CoreError::new("invalid_state", "Acceptance requires a run under review"));
        }
        let records = self.recovery.checkpoints().records(run_id).await?;
        let verified = records.iter().any(|record| {
            record.phase == CheckpointPhase::PostVerification
                && record
                    .manifest
                    .get("acceptance_satisfied")
                    .and_then(Value::as_bool)
                    == Some(true)
        });
        if !verified {
            return Err(CoreError::new(
                "acceptance_evidence_missing",
                "Acceptance requires a passing post-verification checkpoint; native completion alone is not evidence",
            ));
        }
        let new_state = transition(current, RunEvent::Accepted)?;
        self.commit(run_id, current, new_state, "accepted".to_string()).await
    }

    /// Mutation gate: a consequential action may only be committed with a
    /// durable consumption receipt and a real observed outcome. Without one the
    /// run is Blocked (the unreconciled mutation) and the mutation is NOT
    /// performed. With one, the observed outcome is durably recorded through
    /// the policy sink before the run advances.
    pub async fn commit_mutation(
        &self,
        run_id: RunId,
        receipt: Option<MutationReceipt>,
    ) -> Result<RunState> {
        let current = self.state_of(run_id).await?;
        let now = now_ms();
        match receipt {
            None => {
                let new_state = transition(current, RunEvent::MutationBlocked)?;
                self.commit(run_id, current, new_state, "mutation_blocked_no_receipt".to_string()).await?;
                Err(CoreError::new(
                    "mutation_without_receipt",
                    "A consequential mutation requires a durable consumption receipt and an observed outcome; run is now Blocked",
                ))
            }
            Some(receipt) => {
                let (action, action_state): (ActionSpec, String) = self
                    .store
                    .with_connection(move |conn| {
                        let row: Option<(String, String)> = conn
                            .query_row(
                                "SELECT body,state FROM actions WHERE id=?1",
                                [receipt.action_id.to_string()],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()?;
                        let (body, state) = row.ok_or_else(|| CoreError::new("action_missing", "Action does not exist"))?;
                        Ok((serde_json::from_str(&body)?, state))
                    })
                    .await?;
                if action_state != "executing" {
                    return Err(CoreError::new("action_not_executing", "The bound action has not been durably consumed for execution"));
                }
                self.record_outcome(&action, &receipt, now).await?;
                match receipt.outcome {
                    ActionOutcome::Succeeded => {
                        let new_state = transition(current, RunEvent::Verifying)?;
                        self.commit(run_id, current, new_state, "mutation_committed".to_string()).await
                    }
                    ActionOutcome::Failed | ActionOutcome::Uncertain => {
                        let new_state = transition(current, RunEvent::MutationBlocked)?;
                        self.commit(run_id, current, new_state, "mutation_not_succeeded".to_string()).await?;
                        Err(CoreError::new(
                            "outcome_not_succeeded",
                            "The mutation outcome is not a durable success; reconcile the observed state explicitly",
                        ))
                    }
                }
            }
        }
    }

    async fn record_outcome(&self, action: &ActionSpec, receipt: &MutationReceipt, now: i64) -> Result<()> {
        let action_id = action.id;
        let grant_id = receipt.grant_id;
        let outcome = receipt.outcome;
        let receipt_digest = receipt.receipt_digest.clone();
        let observed_state_digest = receipt.observed_state_digest.clone();
        self.store
            .with_transaction(move |tx| {
                let consumed: bool = tx.query_row(
                    "SELECT consumed_at IS NOT NULL FROM grants WHERE id=?1 AND action_id=?2",
                    params![grant_id.to_string(), action_id.to_string()],
                    |row| row.get(0),
                )?;
                if !consumed {
                    return Err(CoreError::new("receipt_conflict", "Grant has no durable consumption"));
                }
                let executing: bool = tx.query_row(
                    "SELECT state='executing' FROM actions WHERE id=?1",
                    [action_id.to_string()],
                    |row| row.get(0),
                )?;
                if !executing {
                    return Err(CoreError::new("receipt_conflict", "Outcome already recorded or requires reconciliation"));
                }
                let (outcome_str, state) = match outcome {
                    ActionOutcome::Succeeded => ("succeeded", "succeeded"),
                    ActionOutcome::Failed => ("failed", "failed"),
                    ActionOutcome::Uncertain => ("uncertain", "blocked"),
                };
                tx.execute("UPDATE actions SET state=?1 WHERE id=?2 AND state='executing'", params![state, action_id.to_string()])?;
                tx.execute(
                    "INSERT INTO action_outcomes(action_id,grant_id,outcome,receipt_digest,observed_state_digest,recorded_at) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![action_id.to_string(), grant_id.to_string(), outcome_str, receipt_digest.as_str(), observed_state_digest.as_str(), now],
                )?;
                Ok(())
            })
            .await
    }

    async fn state_of(&self, run_id: RunId) -> Result<RunState> {
        let state = self
            .store
            .with_connection(move |conn| {
                let state: String = conn
                    .query_row("SELECT state FROM runs WHERE id=?1", [run_id.to_string()], |row| row.get(0))
                    .optional()?
                    .ok_or_else(|| CoreError::new("run_missing", "Run does not exist"))?;
                Ok(state)
            })
            .await?;
        map_state(&state)
    }

    async fn update_body(
        &self,
        run_id: RunId,
        session: Option<SessionRef>,
        artifacts: Vec<crate::contracts::ArtifactId>,
        recorded_pid: Option<u64>,
        recorded_start_time: Option<i64>,
    ) -> Result<()> {
        self.store
            .with_connection(move |conn| {
                let body: String = conn
                    .query_row("SELECT body FROM runs WHERE id=?1", [run_id.to_string()], |row| row.get(0))?;
                let mut value: Value = serde_json::from_str(&body).map_err(|_| CoreError::new("storage_corrupt", "Run body is corrupt"))?;
                if let Some(session) = &session {
                    value["native_session"] = serde_json::to_value(session)?;
                }
                // The observed native process identity: inspection reads this
                // back to tell a live owned process from a recycled PID.
                if let Some(pid) = recorded_pid {
                    value["recorded_pid"] = serde_json::json!(pid);
                }
                if let Some(start_time) = recorded_start_time {
                    value["recorded_start_time"] = serde_json::json!(start_time);
                }
                let mut known = value.get("artifacts").and_then(Value::as_array).cloned().unwrap_or_default();
                for artifact in artifacts {
                    let artifact = artifact.to_string();
                    if !known.iter().any(|item| item.as_str() == Some(artifact.as_str())) {
                        known.push(Value::String(artifact));
                    }
                }
                value["artifacts"] = Value::Array(known);
                let body = redacted_json(&value)?;
                conn.execute("UPDATE runs SET body=?2 WHERE id=?1", params![run_id.to_string(), body])?;
                Ok(())
            })
            .await
    }

    async fn commit(&self, run_id: RunId, from: RunState, to: RunState, event: String) -> Result<RunState> {
        let project = self.project;
        self.store
            .with_transaction(move |tx| {
                let changed = tx.execute(
                    "UPDATE runs SET state=?2 WHERE id=?1 AND state=?3",
                    params![run_id.to_string(), state_str(to), state_str(from)],
                )?;
                if changed != 1 {
                    return Err(CoreError::new("state_conflict", "Run state changed concurrently"));
                }
                append_tx(
                    tx,
                    None,
                    Some(project),
                    Some(run_id),
                    &CoreEvent::StateChanged {
                        entity_id: run_id.to_string(),
                        state: format!("{} -> {} ({})", state_str(from), state_str(to), event),
                    },
                )?;
                Ok(())
            })
            .await?;
        Ok(to)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn latest_policy(conn: &rusqlite::Connection, project: ProjectId) -> Result<ProjectPolicy> {
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
            [project.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    serde_json::from_str(&body.ok_or_else(|| CoreError::new("policy_missing", "Project has no policy"))?).map_err(Into::into)
}

/// A consumed grant with no durable outcome is an uncertain external action.
fn uncertain_action_ids(conn: &rusqlite::Connection, run_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT a.id FROM actions a JOIN grants g ON g.action_id=a.id WHERE a.run_id=?1 AND g.consumed_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM action_outcomes o WHERE o.action_id=a.id)",
    )?;
    let rows = stmt.query_map([run_id], |row| row.get::<_, String>(0))?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// The recorded process identity as observed NOW: liveness plus start-time
/// match. `reused` means the PID is alive but is a different process; that
/// process is reported, never signalled.
fn process_identity(pid: Option<u64>, start_time: Option<i64>) -> Value {
    let Some(pid) = pid else {
        return json!({ "recorded": false });
    };
    let pid = i64::try_from(pid).unwrap_or(i64::MAX);
    let live = pid_alive(pid);
    let boot_ticks = pid_boot_ticks(pid);
    let start_time_match = start_time.zip(boot_ticks).map(|(recorded, boot)| recorded == boot);
    let boot_start = boot_ticks;
    json!({
        "pid": pid,
        "live": live,
        "boot_start_ticks": boot_start,
        "start_time_match": start_time_match,
        "reused": live && start_time_match == Some(false),
    })
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 1 {
        return false;
    }
    // /proc/<pid> existence is a racy check, but it is the only observation
    // that requires no signal delivery; this module never signals a process.
    fs::metadata(format!("/proc/{pid}")).is_ok()
}

fn pid_boot_ticks(pid: i64) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the (parenthesised) comm are whitespace-split: field 14
    // (utime) onward; the process start time is field 22.
    let after_comm = stat.rfind(')').unwrap_or(0) + 1;
    let fields: Vec<&str> = stat[after_comm..].split_whitespace().collect();
    let field22 = fields.get(19)?; // index 0 == field 14
    let ticks: i64 = field22.parse().ok()?;
    Some(ticks)
}
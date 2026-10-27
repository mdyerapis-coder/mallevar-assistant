use crate::{contracts::*, error::{CoreError, Result}, store::{Store, events::redacted_json}};
use super::{PolicyDecision, PolicyEngine};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use ts_rs::TS;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalDecision {
    Approve { expires_at: i64 },
    Deny {},
    Revoke {},
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome { Succeeded, Failed, Uncertain }

pub struct OutcomeEvidence {
    pub outcome: ActionOutcome,
    pub receipt_digest: Digest,
    pub observed_state_digest: Digest,
}

/// Only the authenticated owner IPC dispatch may call `decide`. Workers use a
/// separate restricted broker and cannot supply ApprovalGrant objects to a sink.
#[derive(Clone)]
pub struct ApprovalService { store: Store }

pub struct ConsumptionReceipt {
    action_id: ActionId,
    grant_id: GrantId,
}

fn latest_policy(tx: &Transaction<'_>, project: ProjectId) -> Result<ProjectPolicy> {
    let body: Option<String> = tx.query_row(
        "SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
        [project.to_string()], |row| row.get(0),
    ).optional()?;
    serde_json::from_str(&body.ok_or_else(|| CoreError::new("policy_missing", "Project has no policy"))?).map_err(Into::into)
}

fn stored_action(tx: &Transaction<'_>, id: ActionId) -> Result<(ActionSpec, String)> {
    let row: Option<(String, String)> = tx.query_row(
        "SELECT body,state FROM actions WHERE id=?1", [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let (body, state) = row.ok_or_else(|| CoreError::new("action_missing", "Action does not exist"))?;
    Ok((serde_json::from_str(&body)?, state))
}

impl ApprovalService {
    pub fn new(store: Store) -> Self { Self { store } }

    pub async fn policy(&self, project: ProjectId) -> Result<ProjectPolicy> {
        self.store.with_connection(move |connection| {
            let body: Option<String> = connection.query_row(
                "SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
                [project.to_string()], |row| row.get(0),
            ).optional()?;
            serde_json::from_str(&body.ok_or_else(|| CoreError::new("policy_missing", "Project has no policy"))?).map_err(Into::into)
        }).await
    }

    pub async fn record(&self, action: ActionSpec, now: i64) -> Result<PolicyDecision> {
        self.store.with_transaction(move |tx| {
            let policy = latest_policy(tx, action.project_id)?;
            let project: Option<String> = tx.query_row(
                "SELECT j.project_id FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE r.id=?1",
                [action.run_id.to_string()], |row| row.get(0),
            ).optional()?;
            if project.as_deref() != Some(action.project_id.to_string().as_str()) {
                return Err(CoreError::new("run_mismatch", "Action run does not belong to this project"));
            }
            let decision = PolicyEngine::check(&action, None, None, &policy, now);
            if matches!(decision, PolicyDecision::Deny(_)) { return Ok(decision); }
            let body = redacted_json(&action)?;
            if serde_json::from_str::<ActionSpec>(&body)? != action {
                return Err(CoreError::new("sensitive_action", "Use a native secret reference, not credential material"));
            }
            let digest = action.digest()?;
            let version = i64::try_from(action.policy_version)
                .map_err(|_| CoreError::new("invalid_policy_version", "Policy version exceeds storage range"))?;
            tx.execute(
                "INSERT INTO actions(id,run_id,project_id,digest,policy_version,state,body) VALUES(?1,?2,?3,?4,?5,'awaiting_approval',?6)",
                params![action.id.to_string(),action.run_id.to_string(),action.project_id.to_string(),digest.as_str(),version,body],
            )?;
            Ok(decision)
        }).await
    }

    pub async fn decide(&self, action_id: ActionId, decision: ApprovalDecision, now: i64) -> Result<Option<ApprovalGrant>> {
        self.store.with_transaction(move |tx| {
            let (action, state) = stored_action(tx, action_id)?;
            let policy = latest_policy(tx, action.project_id)?;
            match decision {
                ApprovalDecision::Approve { expires_at } => {
                    if state != "awaiting_approval" || expires_at <= now {
                        return Err(CoreError::new("approval_conflict", "Action is not awaiting a current approval"));
                    }
                    if matches!(PolicyEngine::check(&action, None, None, &policy, now), PolicyDecision::Deny(_)) {
                        return Err(CoreError::new("policy_denied", "Current policy does not permit this action"));
                    }
                    let grant = ApprovalGrant { id: GrantId::new(), action_id, action_digest: action.digest()?, policy_version: policy.version, expected_state_digest: action.expected_state_digest, expires_at, revoked_at: None };
                    tx.execute(
                        "INSERT INTO grants(id,action_id,action_digest,expires_at,body) VALUES(?1,?2,?3,?4,?5)",
                        params![grant.id.to_string(),action_id.to_string(),grant.action_digest.as_str(),expires_at,redacted_json(&grant)?],
                    )?;
                    tx.execute("UPDATE actions SET state='approved' WHERE id=?1", [action_id.to_string()])?;
                    Ok(Some(grant))
                }
                ApprovalDecision::Deny {} => {
                    if state != "awaiting_approval" { return Err(CoreError::new("approval_conflict", "Action is not awaiting approval")); }
                    tx.execute("UPDATE actions SET state='denied' WHERE id=?1", [action_id.to_string()])?;
                    Ok(None)
                }
                ApprovalDecision::Revoke {} => {
                    tx.execute("UPDATE grants SET revoked_at=?1 WHERE action_id=?2 AND consumed_at IS NULL AND revoked_at IS NULL", params![now,action_id.to_string()])?;
                    if state == "approved" {
                        tx.execute("UPDATE actions SET state='revoked' WHERE id=?1", [action_id.to_string()])?;
                    }
                    Ok(None)
                }
            }
        }).await
    }

    /// The sink computes observed_state from opened native resources immediately
    /// before calling this. A frontend-supplied digest is not an observation.
    /// Commit consumption BEFORE the side effect: crash/disconnect never re-arms it.
    pub async fn consume(&self, action: ActionSpec, grant_id: GrantId, observed_state: Digest, now: i64) -> Result<ConsumptionReceipt> {
        self.consume_inner(action, grant_id, observed_state, now, false).await
    }
    /// Same as [`Self::consume`], but the bound run must still be `running`
    /// for this project in the transaction that sets `consumed_at`.
    pub async fn consume_while_running(&self, action: ActionSpec, grant_id: GrantId, observed_state: Digest, now: i64) -> Result<ConsumptionReceipt> {
        self.consume_inner(action, grant_id, observed_state, now, true).await
    }
    async fn consume_inner(&self, action: ActionSpec, grant_id: GrantId, observed_state: Digest, now: i64, require_running: bool) -> Result<ConsumptionReceipt> {
        self.store.with_transaction(move |tx| {
            if require_running {
                let bound: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE r.id=?1 AND r.state='running' AND j.project_id=?2",
                    params![action.run_id.to_string(), action.project_id.to_string()],
                    |row| row.get(0),
                )?;
                if bound != 1 { return Err(CoreError::new("stale_grant", "Bound run is not running")); }
            }
            let (stored, state) = stored_action(tx, action.id)?;
            if stored != action || state != "approved" || observed_state != action.expected_state_digest {
                return Err(CoreError::new("stale_grant", "Action or observed resource state changed"));
            }
            let row: Option<(String, Option<i64>, Option<i64>)> = tx.query_row(
                "SELECT body,revoked_at,consumed_at FROM grants WHERE id=?1 AND action_id=?2",
                params![grant_id.to_string(),action.id.to_string()],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional()?;
            let (body, revoked, consumed) = row.ok_or_else(|| CoreError::new("grant_missing", "No stored grant for this action"))?;
            if consumed.is_some() { return Err(CoreError::new("grant_consumed", "Grant already consumed; reconcile its outcome")); }
            let mut grant: ApprovalGrant = serde_json::from_str(&body)?;
            grant.revoked_at = revoked;
            let policy = latest_policy(tx, action.project_id)?;
            if PolicyEngine::check(&action, None, Some(&grant), &policy, now) != PolicyDecision::Permit {
                return Err(CoreError::new("grant_denied", "Stored grant is no longer eligible"));
            }
            if tx.execute("UPDATE grants SET consumed_at=?1 WHERE id=?2 AND consumed_at IS NULL AND revoked_at IS NULL", params![now,grant_id.to_string()])? != 1 {
                return Err(CoreError::new("grant_conflict", "Grant was consumed or revoked"));
            }
            tx.execute("UPDATE actions SET state='executing' WHERE id=?1", [action.id.to_string()])?;
            Ok(ConsumptionReceipt { action_id: action.id, grant_id })
        }).await
    }

    pub async fn finish(&self, receipt: ConsumptionReceipt, evidence: OutcomeEvidence, now: i64) -> Result<()> {
        self.store.with_transaction(move |tx| {
            let consumed: bool = tx.query_row("SELECT consumed_at IS NOT NULL FROM grants WHERE id=?1 AND action_id=?2", params![receipt.grant_id.to_string(),receipt.action_id.to_string()], |row| row.get(0))?;
            if !consumed { return Err(CoreError::new("receipt_conflict", "Grant has no durable consumption")); }
            let (outcome, state) = match evidence.outcome {
                ActionOutcome::Succeeded => ("succeeded", "succeeded"),
                ActionOutcome::Failed => ("failed", "failed"),
                ActionOutcome::Uncertain => ("uncertain", "blocked"),
            };
            if tx.execute("UPDATE actions SET state=?1 WHERE id=?2 AND state='executing'", params![state,receipt.action_id.to_string()])? != 1 {
                return Err(CoreError::new("receipt_conflict", "Outcome already recorded or requires reconciliation"));
            }
            tx.execute(
                "INSERT INTO action_outcomes(action_id,grant_id,outcome,receipt_digest,observed_state_digest,recorded_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![receipt.action_id.to_string(),receipt.grant_id.to_string(),outcome,evidence.receipt_digest.as_str(),evidence.observed_state_digest.as_str(),now],
            )?;
            Ok(())
        }).await
    }
}

/// The single product writer for the `policies` table. Every retrieval, context
/// and embedding-ranking path reads the latest committed policy through
/// `require_policy`/`latest_policy`; before this service existed nothing in the
/// product could ever commit one.
#[derive(Clone)]
pub struct PolicyService { store: Store }

impl PolicyService {
    pub fn new(store: Store) -> Self { Self { store } }

    /// Commit `policy` as a NEW version for `project`. All authority and shape
    /// checks run before the write; the row and its event commit in one
    /// transaction, so a refused policy leaves no `policies` row and no event.
    pub async fn set(&self, project: ProjectId, policy: ProjectPolicy) -> Result<ProjectPolicy> {
        if policy.project_id != project {
            return Err(CoreError::new("project_mismatch", "policy.project_id does not match the target project"));
        }
        if policy.version < 1 {
            return Err(CoreError::new("invalid_policy_version", "policy version must be at least 1"));
        }
        if !(1..=32).contains(&policy.max_workers) {
            return Err(CoreError::new("invalid_policy_bounds", "max_workers must be between 1 and 32"));
        }
        if !(1..=100_000).contains(&policy.max_requests) {
            return Err(CoreError::new("invalid_policy_bounds", "max_requests must be between 1 and 100000"));
        }
        // The same round-trip the action path uses: redaction must not change the
        // value, so credential material is refused rather than silently stored.
        let body = redacted_json(&policy)?;
        if serde_json::from_str::<ProjectPolicy>(&body)? != policy {
            return Err(CoreError::new("sensitive_policy", "policy carries credential material; use a native secret reference"));
        }
        let version = i64::try_from(policy.version)
            .map_err(|_| CoreError::new("invalid_policy_version", "policy version exceeds storage range"))?;
        let committed = policy.clone();
        self.store.with_transaction(move |tx| {
            let registered: Option<String> = tx.query_row("SELECT name FROM projects WHERE id=?1", [project.to_string()], |row| row.get(0)).optional()?;
            if registered.is_none() {
                return Err(CoreError::new("project_missing", "policy names a project that is not registered"));
            }
            let latest: Option<i64> = tx.query_row(
                "SELECT version FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
                [project.to_string()], |row| row.get(0),
            ).optional()?;
            if latest.is_some_and(|current| version <= current) {
                return Err(CoreError::new("stale_policy_version", "policy version must be greater than the latest committed version"));
            }
            tx.execute("INSERT INTO policies(project_id,version,body) VALUES(?1,?2,?3)", params![project.to_string(),version,body])?;
            crate::store::events::append_tx(tx, None, Some(project), None, &CoreEvent::StateChanged {
                entity_id: project.to_string(), state: format!("policy_committed:{version}"),
            })?;
            Ok(())
        }).await?;
        Ok(committed)
    }

    /// The latest committed policy for a project, or the typed `policy_missing`.
    pub async fn latest(&self, project: ProjectId) -> Result<ProjectPolicy> {
        self.store.with_connection(move |conn| {
            let body: Option<String> = conn.query_row(
                "SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
                [project.to_string()], |row| row.get(0),
            ).optional()?;
            serde_json::from_str(&body.ok_or_else(|| CoreError::new("policy_missing", "project has no committed policy"))?).map_err(Into::into)
        }).await
    }
}

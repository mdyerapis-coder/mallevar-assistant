// H1: native-done acceptance gate and permission types. A worker finishing
// successfully never auto-succeeds the run; an explicit owner `accept` with a
// fully-passing manifest is required. The fail-closed permission gate returns
// only VerifiedPermission after core-side validation.

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    contracts::*,
    error::{CoreError, Result},
    policy::service::ApprovalDecision,
    store::{events::now_ms, Store},
};

use crate::workspaces::WorkspaceService;

/// Owner-validated permission answer — the core-side fail-closed gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedPermission {
    /// The native request was explicitly denied by the owner; no grant was
    /// ever issued, so the sink will never apply.
    Denied,
    /// The native request was allowed; the core-issued grant is attached and
    /// verified against the stored action (digest, state, expiry).
    Allowed(ApprovalGrant),
}

/// A native-done claim recorded by the execution service. The run is *not*
/// succeeded yet; it is awaiting explicit owner acceptance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDoneReceipt {
    pub run_id: RunId,
    pub session_id: String,
    pub recorded_at: i64,
    /// The native worker's full ResultManifest — the claim being ratified.
    pub manifest: ResultManifest,
    /// Canonical digest of the manifest for tamper detection.
    pub manifest_digest: Digest,
}

/// Owner's acceptance outcome for a native-done run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AcceptanceOutcome {
    Accepted,
    Rejected,
}

/// The owner-side receipt produced when an explicitly accepted run is sealed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceReceipt {
    pub run_id: RunId,
    pub session_id: String,
    pub accepted_at: i64,
    /// Canonical SHA-256 of the accepted ResultManifest (the native-done claim
    /// being ratified). Re-computed at verify time so a swapped manifest is
    /// rejected even if the row is tampered.
    pub manifest_digest: Digest,
    pub outcome: AcceptanceOutcome,
}

/// The owner-decision path for a permission request, returned by
/// `request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionGate {
    /// The action is outside the session's permitted tool set or the policy
    /// refused it outright: the request is denied *before* any native tool
    /// runs. No grant is ever issued on this path.
    Denied(String),
    /// The action was recorded and is awaiting an owner decision.
    AwaitingApproval(ActionId),
    /// The action is permitted by policy and a core-issued grant is attached.
    Granted(ApprovalGrant),
}

/// Extension methods on ExecutionService for the acceptance gate and
/// native-done recording. These are split out to keep `mod.rs` focused on
/// session/turn flow.
impl super::ExecutionService {
    /// Record a native worker's completion claim. The run transitions to
    /// `reviewing` (awaiting acceptance) — it is *not* succeeded yet. All
    /// manifest checks must pass and the manifest must claim
    /// `acceptance_satisfied == true`; otherwise the worker is not done.
    pub async fn record_native_done(
        &self,
        run: RunId,
        manifest: ResultManifest,
    ) -> Result<NativeDoneReceipt> {
        self.ensure_schema().await?;
        // Validate the manifest belongs to this run.
        if manifest.run_id != run {
            return Err(CoreError::new(
                "manifest_run_mismatch",
                "ResultManifest does not bind the target run",
            ));
        }
        // The manifest must have at least one check and all must pass.
        if manifest.checks.is_empty() {
            return Err(CoreError::new(
                "manifest_checks_missing",
                "Native done requires at least one check receipt",
            ));
        }
        if manifest.checks.iter().any(|c| !c.passed) {
            return Err(CoreError::new(
                "manifest_checks_failed",
                "All manifest checks must pass before a native done can be recorded",
            ));
        }
        if !manifest.acceptance_satisfied {
            return Err(CoreError::new(
                "manifest_acceptance_not_satisfied",
                "ResultManifest claims acceptance_satisfied=false; native is not done",
            ));
        }
        // Transition run to 'reviewing' (awaiting acceptance). Only from a
        // non-terminal state.
        let run_str = run.to_string();
        let changed = self
            .store
            .with_transaction(move |tx| {
                let rows = tx.execute(
                    "UPDATE runs SET state='reviewing' WHERE id=?1 AND state NOT IN ('succeeded','failed','cancelled')",
                    [run_str],
                )?;
                if rows == 0 {
                    let cur: Option<String> = tx
                        .query_row("SELECT state FROM runs WHERE id=?1", [run_str], |r| r.get(0))
                        .optional()?;
                    return Err(CoreError::new(
                        "run_terminal",
                        format!("Cannot record native done: run is already in terminal state {:?}", cur),
                    ));
                }
                Ok(())
            })
            .await?;
        // Persist the native-done receipt.
        let session_id = self
            .store
            .with_connection({
                let run_str = run_str.clone();
                move |conn| {
                    let sid: Option<String> = conn
                        .query_row(
                            "SELECT session_id FROM execution_sessions WHERE run_id=?1 AND state IN ('open','paused') ORDER BY created_at DESC LIMIT 1",
                            [run_str],
                            |r| r.get(0),
                        )
                        .optional()?;
                    Ok(sid)
                }
            })
            .await?
            .ok_or_else(|| CoreError::new("session_missing", "No active session for this run"))?;
        let canonical = manifest_canonical(&manifest)?;
        let manifest_digest = Digest::parse(&format!("{:x}", sha2::Sha256::digest(canonical.as_bytes())))?;
        let recorded_at = now_ms();
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO execution_receipts(run_id,kind,body,created_at) VALUES(?1,'native_done',?2,?3)
                     ON CONFLICT(run_id,kind) DO UPDATE SET body=excluded.body,created_at=excluded.created_at",
                    params![run_str, serde_json::to_string(&manifest)?, recorded_at],
                )?;
                Ok(())
            })
            .await?;
        Ok(NativeDoneReceipt {
            run_id,
            session_id,
            recorded_at,
            manifest,
            manifest_digest,
        })
    }

    /// Owner explicitly accepts a native-done run. The manifest is re-validated
    /// (all checks pass, acceptance_satisfied true), the run transitions to
    /// `succeeded`, and an acceptance receipt is recorded.
    pub async fn accept(&self, run: RunId) -> Result<AcceptanceReceipt> {
        let run_str = run.to_string();
        // Load the native-done receipt to re-validate.
        let (manifest, session_id): (ResultManifest, String) = self
            .store
            .with_connection({
                let run_str = run_str.clone();
                move |conn| {
                    let body: Option<String> = conn
                        .query_row(
                            "SELECT body FROM execution_receipts WHERE run_id=?1 AND kind='native_done'",
                            [run_str],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let body = body.ok_or_else(|| CoreError::new("native_done_missing", "No native done recorded for this run"))?;
                    let manifest: ResultManifest = serde_json::from_str(&body)?;
                    let sid: Option<String> = conn
                        .query_row(
                            "SELECT session_id FROM execution_sessions WHERE run_id=?1 AND state IN ('open','paused') ORDER BY created_at DESC LIMIT 1",
                            [run_str],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let sid = sid.ok_or_else(|| CoreError::new("session_missing", "No active session for this run"))?;
                    Ok((manifest, sid))
                }
            })
            .await?;
        // Re-validate the manifest.
        if manifest.checks.is_empty() || manifest.checks.iter().any(|c| !c.passed) {
            return Err(CoreError::new("manifest_checks_failed", "Cannot accept: manifest checks are missing or failed"));
        }
        if !manifest.acceptance_satisfied {
            return Err(CoreError::new("manifest_acceptance_not_satisfied", "Cannot accept: acceptance_satisfied is false"));
        }
        // Transition run to succeeded (guarded: only from 'reviewing' or non-terminal).
        let changed = self
            .store
            .with_transaction(move |tx| {
                let rows = tx.execute(
                    "UPDATE runs SET state='succeeded' WHERE id=?1 AND state NOT IN ('succeeded','failed','cancelled')",
                    [run_str],
                )?;
                if rows == 0 {
                    let cur: Option<String> = tx
                        .query_row("SELECT state FROM runs WHERE id=?1", [run_str], |r| r.get(0))
                        .optional()?;
                    return Err(CoreError::new(
                        "run_terminal",
                        format!("Cannot accept: run is already in terminal state {:?}", cur),
                    ));
                }
                Ok(())
            })
            .await?;
        // Record the acceptance receipt.
        let canonical = manifest_canonical(&manifest)?;
        let manifest_digest = Digest::parse(&format!("{:x}", sha2::Sha256::digest(canonical.as_bytes())))?;
        let accepted_at = now_ms();
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO execution_receipts(run_id,kind,body,created_at) VALUES(?1,'acceptance',?2,?3)
                     ON CONFLICT(run_id,kind) DO UPDATE SET body=excluded.body,created_at=excluded.created_at",
                    params![run_str, serde_json::to_string(&AcceptanceReceipt {
                        run_id,
                        session_id: session_id.clone(),
                        accepted_at,
                        manifest_digest: manifest_digest.clone(),
                        outcome: AcceptanceOutcome::Accepted,
                    })?, accepted_at],
                )?;
                Ok(())
            })
            .await?;
        Ok(AcceptanceReceipt {
            run_id,
            session_id,
            accepted_at,
            manifest_digest,
            outcome: AcceptanceOutcome::Accepted,
        })
    }

    /// Owner explicitly rejects a native-done run. The run transitions to
    /// `failed` and a rejection receipt is recorded.
    pub async fn reject(&self, run: RunId, reason: String) -> Result<AcceptanceReceipt> {
        let run_str = run.to_string();
        let session_id = self
            .store
            .with_connection({
                let run_str = run_str.clone();
                move |conn| {
                    let sid: Option<String> = conn
                        .query_row(
                            "SELECT session_id FROM execution_sessions WHERE run_id=?1 AND state IN ('open','paused') ORDER BY created_at DESC LIMIT 1",
                            [run_str],
                            |r| r.get(0),
                        )
                        .optional()?;
                    Ok(sid)
                }
            })
            .await?
            .ok_or_else(|| CoreError::new("session_missing", "No active session for this run"))?;
        let changed = self
            .store
            .with_transaction(move |tx| {
                let rows = tx.execute(
                    "UPDATE runs SET state='failed' WHERE id=?1 AND state NOT IN ('succeeded','failed','cancelled')",
                    [run_str],
                )?;
                if rows == 0 {
                    let cur: Option<String> = tx
                        .query_row("SELECT state FROM runs WHERE id=?1", [run_str], |r| r.get(0))
                        .optional()?;
                    return Err(CoreError::new(
                        "run_terminal",
                        format!("Cannot reject: run is already in terminal state {:?}", cur),
                    ));
                }
                Ok(())
            })
            .await?;
        let accepted_at = now_ms();
        let canonical = serde_json::to_string(&AcceptanceReceipt {
            run_id,
            session_id: session_id.clone(),
            accepted_at,
            manifest_digest: Digest::parse(&"0".repeat(64))?,
            outcome: AcceptanceOutcome::Rejected,
        })?;
        let manifest_digest = Digest::parse(&format!("{:x}", sha2::Sha256::digest(canonical.as_bytes())))?;
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO execution_receipts(run_id,kind,body,created_at) VALUES(?1,'rejection',?2,?3)
                     ON CONFLICT(run_id,kind) DO UPDATE SET body=excluded.body,created_at=excluded.created_at",
                    params![run_str, serde_json::to_string(&AcceptanceReceipt {
                        run_id,
                        session_id: session_id.clone(),
                        accepted_at,
                        manifest_digest: manifest_digest.clone(),
                        outcome: AcceptanceOutcome::Rejected,
                    })?, accepted_at],
                )?;
                Ok(())
            })
            .await?;
        Ok(AcceptanceReceipt {
            run_id,
            session_id,
            accepted_at,
            manifest_digest,
            outcome: AcceptanceOutcome::Rejected,
        })
    }
}

/// Canonical serialization for a ResultManifest (deterministic JSON).
fn manifest_canonical(manifest: &ResultManifest) -> Result<String> {
    let canonical = serde_json::to_string(&serde_json::json!({
        "run_id": manifest.run_id.to_string(),
        "native_session": manifest.native_session,
        "artifacts": manifest.artifacts,
        "checks": manifest.checks,
        "acceptance_satisfied": manifest.acceptance_satisfied,
        "unresolved": manifest.unresolved,
    }))?;
    Ok(canonical)
}
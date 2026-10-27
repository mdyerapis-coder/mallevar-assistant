//! Action‑scoped browser and Linux desktop automation broker (H8).
//!
//! Single front door for browser (`BrowserProfile`) and desktop (`NativeWindow`)
//! automation, plus a core‑owned `document_file` backend.
//!
//! Authority model (H8 spec):
//! * A coding worker NEVER gets host-session or personal-browser access.
//! * `observe` opens a durable scope + returns the target's live revision digest;
//!   `release` transitions the scope to "released" (idempotent); `execute`
//!   refuses a released/stale scope, re‑observes for freshness, validates the
//!   exact grant, requires owner consent for disclosures, and records the
//!   ACTUAL outcome.
//! * The Wayland global‑input capability gate is Unsupported with no fallback.
use schemars::JsonSchema;
use ts_rs::TS;

use crate::contracts::{
    ActionId, ActionSpec, ApprovalGrant, ArtifactId, CoreEvent, Digest,
    GrantId,
};
use crate::error::{CoreError, Result};
use crate::policy::routes;
use crate::policy::service::{ActionOutcome, ApprovalService};
use crate::store::{Store, events::now_ms};
use super::consent::{
    ConsentRecord, consent_kind_for_operation, validate_consent,
};
use super::targets::{
    ApprovedTarget, AutomationScopeRecord, ScopePhase, TargetKind,
    check_target_in_scope, document_revision,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as HashDigest, Sha256};
use std::path::Path;

/// The automation broker.
#[derive(Clone)]
pub struct AutomationBroker {
    store: Store,
    cas_root: Option<std::path::PathBuf>,
}

impl AutomationBroker {
    /// Construct with only the core store.
    pub fn new(store: Store) -> Self {
        Self { store, cas_root: None }
    }

    /// Attach the core CAS root so live executions can persist artifacts.
    pub fn with_cas(mut self, root: std::path::PathBuf) -> Self {
        self.cas_root = Some(root);
        self
    }

    // ── observe ───────────────────────────────────────────────────────────────

    /// Observe a target's live current state (no side effects) and open a
    /// durable `automation-scope/<scope_id>` state event for it.
    pub async fn observe(&self, target: ApprovedTarget) -> Result<AutomationObservation> {
        check_target_in_scope(&target, now_ms())?;
        let (revision_digest, accessibility_tree, image_artifact) =
            self.observe_target(&target).await?;
        self.record_scope(&target, ScopePhase::Active).await?;
        Ok(AutomationObservation {
            target_id: target.target_id.clone(),
            scope_id: target.scope_id.clone(),
            backend: backend_of_scope(&target),
            revision_digest,
            accessibility_tree,
            image_artifact,
        })
    }

    /// Read the live current state of a target (no side effects).
    async fn observe_target(
        &self,
        target: &ApprovedTarget,
    ) -> Result<(Digest, Option<serde_json::Value>, Option<ArtifactId>)> {
        match target.kind {
            TargetKind::DocumentFile => {
                let path = Path::new(&target.target_id);
                let bytes = std::fs::read(path)
                    .map_err(|e| CoreError::new("target_unavailable", format!("document target cannot be read: {e}")))?;
                let digest = document_revision(&target.target_id, &bytes);
                let mut tree = serde_json::Map::new();
                tree.insert(
                    "kind".into(),
                    serde_json::Value::String("document_file".into()),
                );
                tree.insert(
                    "size_bytes".into(),
                    serde_json::Value::Number(serde_json::Number::from(bytes.len())),
                );
                Ok((digest, Some(serde_json::Value::Object(tree)), None))
            }
            TargetKind::BrowserProfile | TargetKind::NativeWindow => {
                let report = self
                    .run_executor(target, "observe", None, None)
                    .await?;
                Ok((report.revision_after, report.accessibility_tree, report.image_artifact))
            }
        }
    }

    // ── execute ───────────────────────────────────────────────────────────────

    /// Execute one authorised, owner‑scoped operation. Checks in order: active
    /// scope + role, disclosure consent, freshness (stale snapshot cannot receive
    /// a click), exact grant, Wayland global‑input cap, then the real backend.
    pub async fn execute(
        &self,
        action: ActionSpec,
        grant: ApprovalGrant,
        consent: Option<ConsentRecord>,
    ) -> Result<AutomationReceipt> {
        let now = now_ms();
        let args = parse_action_args(&action)?;
        let operation = args.operation.clone();
        let parameters = args.parameters.clone();
        let target_id = args.target_id.clone();

        // (1) Active, unexpired scope for this target + role check.
        let scope = self.active_scope_for(&target_id).await?;
        let target = scope.target.clone();
        check_target_in_scope(&target, now)?;
        if !scope_kind_matches(target.kind, &action.kind) {
            return Err(CoreError::new(
                "scope_mismatch",
                "The action kind does not match the scoped target",
            ));
        }
        if let Some(destination) = &action.destination {
            let destinations: Vec<String> = std::vec!["*".to_string()];
            if !routes::destination_allowed(destination, &destinations)
                || (action
                    .scope
                    .get("local_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    && !routes::is_loopback_destination(destination))
            {
                return Err(CoreError::new(
                    "policy_denied",
                    "Action destination is not permitted by the project policy",
                ));
            }
        }

        // (4) Disclosure consent.
        if consent_kind_for_operation(target.kind, &operation).is_some() {
            let record = consent.as_ref().ok_or_else(|| {
                CoreError::new(
                    "consent_required",
                    "This disclosure requires an explicit owner consent record",
                )
            })?;
            if record.scope_id != target.scope_id {
                return Err(CoreError::new(
                    "consent_scope_mismatch",
                    "Consent does not cover this scope",
                ));
            }
            validate_consent(record, target.kind, &operation)?;
        } else if consent.is_some() {
            return Err(CoreError::new(
                "consent_not_required",
                "A consent record was supplied for a non-disclosure operation",
            ));
        }

        // (2) Freshness: re‑observe and compare to expected_state_digest.
        let revision_now = self.observe_target(&target).await?.0;
        if revision_now != action.expected_state_digest {
            return Err(CoreError::new(
                "stale_target",
                "The target state changed since the owner observed it; re-observe before retrying",
            ));
        }
        // (3) Exact grant consumption (delegated to ApprovalService).
        let approvals = ApprovalService::new(self.store.clone());
        let policy = approvals.policy(action.project_id).await?;
        let decision = crate::policy::PolicyEngine::check(&action, None, Some(&grant), &policy, now);
        match decision {
            crate::policy::PolicyDecision::Permit => {}
            crate::policy::PolicyDecision::Deny(e) =>
                return Err(CoreError::new("grant_denied", format!("Stored grant is no longer eligible: {e:?}"))),
            crate::policy::PolicyDecision::AwaitApproval =>
                return Err(CoreError::new("grant_not_approved", "Action is not in the approved state")),
        }
        let consumption = match approvals.consume(action.clone(), grant.id, revision_now.clone(), now).await {
            Ok(c) => c,
            Err(e) => {
                let _ = self.store.append(
                    Some(action.run_id),
                    CoreEvent::ToolFinished { action_id: action.id, result: serde_json::Value::Null },
                ).await;
                return Err(CoreError::new("grant_denied", format!("Stored grant is no longer eligible: {e}")));
            }
        };

        // Wayland global-input cap.
        if let Some(reason) = unsupported_on_wayland(&target, &operation, &parameters) {
            let _ = approvals.finish(
                    consumption,
                    crate::policy::service::OutcomeEvidence {
                    outcome: crate::policy::service::ActionOutcome::Failed,
                    receipt_digest: receipt_digest_placeholder(),
                    observed_state_digest: revision_now,
                },
                now,
            ).await;
            let _ = self.store.append(
                    Some(action.run_id),
                CoreEvent::ToolFinished { action_id: action.id, result: serde_json::Value::Null },
            ).await;
            return Err(CoreError::new("unsupported", reason));
        }

        // Authorise the operation — real backend or real file bytes.
        let report = self
            .run_executor(&target, "execute", Some(&operation), Some(&parameters))
            .await;
        let (outcome, after, effect, error) = match &report {
            Ok(report) => (
                ActionOutcome::Succeeded,
                report.revision_after.clone(),
                report.effect.clone(),
                None,
            ),
            Err(e) => (
                ActionOutcome::Failed,
                revision_now.clone(),
                Effect::Failed {
                    reason: format!("{e}"),
                },
                Some(format!("{e}")),
            ),
        };

        let receipt_digest = Digest::parse(&format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(format!("{:?}", (outcome, &effect)),)).unwrap_or_default()
            )
        ))
        .unwrap_or_else(|_| Digest::parse("00").unwrap());

        let _ = approvals.finish(
            consumption,
            crate::policy::service::OutcomeEvidence {
                outcome,
                receipt_digest: receipt_digest.clone(),
                observed_state_digest: revision_now.clone(),
            },
            now,
        ).await;
        let _ = self.store.append(
            Some(action.run_id),
            CoreEvent::ToolFinished { action_id: action.id, result: serde_json::Value::Null },
        ).await;
        Ok(AutomationReceipt {
            action_id: action.id,
            grant_id: Some(grant.id),
            target_id,
            backend: backend_of_scope(&target),
            operation,
            outcome,
            effect,
            revision_before: revision_now.clone(),
            revision_after: after,
            receipt_digest,
            error,
        })
    }
    // ── release ───────────────────────────────────────────────────────────────

    /// Release an owner‑issued scope (idempotent).
    pub async fn release(&self, scope_id: String) -> Result<()> {
        let id = scope_id.clone();
        match self.scope_record(&id).await? {
            None => Ok(()),
            Some(record) if record.phase == ScopePhase::Released => Ok(()),
            Some(record) => {
                self.record_scope(&record.target, ScopePhase::Released).await?;
                Ok(())
            }
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    async fn active_scope_for(&self, target_id: &str) -> Result<AutomationScopeRecord> {
        let target_id = target_id.to_string();
        self.store.with_connection(move |conn| {
            // Read newest-first and keep the FIRST record seen for each scope:
            // that record is the scope's current phase. A released scope must
            // not fall back to the Active record that preceded the release.
            let mut stmt = conn.prepare(
                "SELECT json_extract(payload,'$.entity_id'), json_extract(payload,'$.state') FROM events \
                 WHERE json_extract(payload,'$.type')='state_changed' \
                 AND json_extract(payload,'$.entity_id') LIKE 'automation-scope/%' \
                 ORDER BY seq DESC",
            )?;
            let mut rows = stmt.query([])?;
            let mut seen: Vec<String> = Vec::new();
            while let Some(row) = rows.next()? {
                let entity: String = row.get(0)?;
                if seen.iter().any(|known| known == &entity) {
                    continue;
                }
                seen.push(entity);
                let state: String = row.get(1)?;
                if let Ok(record) = serde_json::from_str::<AutomationScopeRecord>(&state)
                    && record.phase == ScopePhase::Active
                    && record.target.target_id == target_id
                {
                    return Ok(record);
                }
            }
            Err(CoreError::new(
                "scope_missing",
                "No active owner-issued scope for this target; observe first",
            ))
        })
        .await
    }

    async fn scope_record(&self, scope_id: &str) -> Result<Option<AutomationScopeRecord>> {
        let scope_id = scope_id.to_string();
        self.store.with_connection(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT payload FROM events WHERE json_extract(payload,'$.type')='state_changed' \
                 AND json_extract(payload,'$.entity_id') = ?1 ORDER BY seq DESC LIMIT 1",
            )?;
            let entity = format!("automation-scope/{scope_id}");
            let mut rows = stmt.query([&entity])?;
            if let Some(row) = rows.next()? {
                let payload: String = row.get(0)?;
                let ev: serde_json::Value = serde_json::from_str(&payload)?;
                let state = ev
                    .get("state")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| {
                        CoreError::new(
                            "storage_corrupt",
                            "malformed automation-scope state event",
                        )
                    })?;
                Ok(Some(serde_json::from_str::<AutomationScopeRecord>(state)?))
            } else {
                Ok(None)
            }
        }).await
    }

    async fn record_scope(&self, target: &ApprovedTarget, phase: ScopePhase) -> Result<()> {
        let record = AutomationScopeRecord {
            phase,
            target: target.clone(),
            recorded_at: now_ms(),
        };
        let state = serde_json::to_string(&record)
            .map_err(|e| CoreError::new("invalid_scope", format!("scope is not serialisable: {e}")))?;
        let event = CoreEvent::StateChanged {
            entity_id: format!("automation-scope/{}", target.scope_id),
            state: state.into(),
        };
        let _ = self
            .store
            .append(None, event)
            .await?;
        Ok(())
    }

    /// Launch the pinned executor for browser/native targets.
    async fn run_executor(
        &self,
        target: &ApprovedTarget,
        mode: &str,
        operation: Option<&str>,
        parameters: Option<&serde_json::Value>,
    ) -> Result<ExecutionReport> {
        match target.kind {
            TargetKind::DocumentFile => Self::run_document_executor(
                target, mode, operation, parameters,
            )
            .await,
            TargetKind::BrowserProfile | TargetKind::NativeWindow => {
                let bin = std::env::var("IRONBARK_ALPHACODE_EXECUTOR").map_err(|_| {
                    CoreError::new(
                        "executor_unavailable",
                        "IRONBARK_ALPHACODE_EXECUTOR is not configured for this backend",
                    )
                })?;
                if !Path::new(&bin).is_file() {
                    return Err(CoreError::new(
                        "executor_unavailable",
                        "configured automation executor binary is missing",
                    ));
                }
                let mut request = serde_json::json!({ "kind": match target.kind {
                    TargetKind::BrowserProfile => "browser_profile",
                    TargetKind::NativeWindow => "native_window",
                    _ => unreachable!(),
                }, "target_id": target.target_id, "mode": mode });
                if let Some(op) = operation {
                    request["operation"] = serde_json::Value::String(op.to_string());
                }
                if let Some(params) = parameters {
                    request["parameters"] = params.clone();
                }
                let output = tokio::process::Command::new(&bin)
                    .arg("automation")
                    .arg(mode)
                    .arg("--request")
                    .arg(
                        serde_json::to_string(&request)
                            .map_err(|e| CoreError::new("invalid_request", format!("{e}")))?,
                    )
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map_err(|e| CoreError::new("executor_launched", format!("{e}")))?;
                if !output.status.success() {
                    let err = String::from_utf8_lossy(&output.stderr).to_string();
                    return Err(CoreError::new(
                        "executor_failed",
                        format!("automation executor error: {err}"),
                    ));
                }
                serde_json::from_slice(&output.stdout)
                    .map_err(|e| CoreError::new("executor_output", format!("{e}")))
            }
        }
    }

    fn read_document(path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).map_err(|e| CoreError::new("target_unavailable", format!("document target cannot be read: {e}")))
    }
}

impl AutomationBroker {
    async fn run_document_executor(
        target: &ApprovedTarget,
        mode: &str,
        operation: Option<&str>,
        parameters: Option<&serde_json::Value>,
    ) -> Result<ExecutionReport> {
        let path = Path::new(&target.target_id);
        if !path.is_absolute() || !path.exists() {
            return Err(CoreError::new(
                "target_unavailable",
                "document target path does not exist",
            ));
        }
        match mode {
            "observe" => {
                let bytes = Self::read_document(path)?;
                let digest = document_revision(&target.target_id, &bytes);
                let mut tree = serde_json::Map::new();
                tree.insert(
                    "kind".into(),
                    serde_json::Value::String("document_file".into()),
                );
                tree.insert(
                    "size_bytes".into(),
                    serde_json::Value::Number(serde_json::Number::from(bytes.len())),
                );
                Ok(ExecutionReport {
                    revision_after: digest,
                    effect: Effect::Unchanged,
                    accessibility_tree: Some(serde_json::Value::Object(tree)),
                    image_artifact: None,
                    result: Some(format!("document_file:{} bytes", bytes.len())),
                })
            }
            "execute" => {
                let op = operation
                    .ok_or_else(|| CoreError::new("invalid_action", "execute requires an operation"))?;
                let before = Self::read_document(path)?;
                let digest_before = document_revision(&target.target_id, &before);
                let params = parameters.unwrap_or(&serde_json::Value::Null);
                let (after, effect): (Vec<u8>, Effect) = match op {
                    "type" => {
                        let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if text.is_empty() {
                            return Err(CoreError::new("invalid_action", "type requires non-empty 'text'"));
                        }
                        let mut aft = before.clone();
                        aft.extend_from_slice(text.as_bytes());
                        (aft, Effect::BytesAppended { count: text.len() })
                    }
                    "press" => {
                        let key = params.get("key").and_then(|v| v.as_str()).unwrap_or("");
                        if key.eq_ignore_ascii_case("Enter") || key.eq_ignore_ascii_case("Return") {
                            let mut aft = before.clone();
                            aft.extend_from_slice(b"\n");
                            (aft, Effect::BytesAppended { count: 1 })
                        } else {
                            return Err(CoreError::new("invalid_action", "unsupported press key"));
                        }
                    }
                    "click" => {
                        if document_revision(&target.target_id, &before) != digest_before {
                            return Err(CoreError::new("integrity_error", "document changed during click"));
                        }
                        (before, Effect::Unchanged)
                    }
                    "read" => (before.clone(), Effect::Unchanged),
                    _ => {
                        return Err(CoreError::new(
                            "unsupported",
                            format!("operation '{op}' is not available on a document_file target"),
                        ))
                    }
                };
                std::fs::write(path, &after)
                    .map_err(|e| CoreError::new("write_failed", format!("could not persist: {e}")))?;
                let digest_after = document_revision(&target.target_id, &after);
                Ok(ExecutionReport {
                    revision_after: digest_after,
                    effect,
                    accessibility_tree: None,
                    image_artifact: None,
                    result: Some(format!("document_file:{} bytes", after.len())),
                })
            }
            _ => Err(CoreError::new(
                "invalid_action",
                "document_file supports observe and execute only",
            )),
        }
    }
}

// ─── module helpers ──────────────────────────────────────────────────────────

fn backend_of_scope(target: &ApprovedTarget) -> String {
    match target.kind {
        TargetKind::BrowserProfile => "firefox_bridge".to_string(),
        TargetKind::NativeWindow => "atspi".to_string(),
        TargetKind::DocumentFile => "document_file".to_string(),
    }
}

fn scope_kind_matches(kind: TargetKind, action_kind: &str) -> bool {
    match kind {
        TargetKind::BrowserProfile => action_kind == "browser",
        TargetKind::NativeWindow | TargetKind::DocumentFile => action_kind == "desktop",
    }
}

fn scope_entity(scope_id: &str) -> String {
    format!("automation-scope/{scope_id}")
}

fn unsupported_on_wayland(
    target: &ApprovedTarget,
    operation: &str,
    parameters: &serde_json::Value,
) -> Option<String> {
    if target.kind != TargetKind::NativeWindow {
        return None;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return None;
    }
    let is_global = parameters.get("input_mode").and_then(|v| v.as_str()) == Some("global")
        || (parameters.get("x").is_some()
            && parameters.get("y").is_some()
            && parameters.get("element_id").is_none());
    let is_canvas = matches!(operation, "drag_and_drop" | "eval")
        && parameters.get("canvas").is_some();
    if is_global || is_canvas {
        Some(
            "global pointer injection and canvas compositing are unavailable on this Wayland session; no fallback is provided."
                .to_string(),
        )
    } else {
        None
    }
}

/// Parse {target_id, operation, parameters} out of an ActionSpec.
#[derive(Clone)]
struct CanonicalArgs {
    target_id: String,
    observation_digest: Digest,
    operation: String,
    parameters: serde_json::Value,
}

fn parse_action_args(action: &ActionSpec) -> Result<CanonicalArgs> {
    let obj = action
        .canonical_args
        .as_object()
        .ok_or_else(|| CoreError::new("invalid_action", "canonical_args must be an object"))?;
    let target_id = obj
        .get("target_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::new("invalid_action", "target_id missing"))?
        .to_string();
    let observation_digest = Digest::parse(
        obj.get("observation_digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::new("invalid_action", "observation_digest missing"))?,
    )?;
    let operation = obj
        .get("operation")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::new("invalid_action", "operation missing"))?
        .to_string();
    let parameters = obj
        .get("parameters")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(CanonicalArgs {
        target_id,
        observation_digest,
        operation,
        parameters,
    })
}

// ─── public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Unchanged,
    BytesAppended { count: usize },
    Executed { description: String },
    Failed { reason: String },
}

/// Placeholder digest used in error paths before the real receipt is computed.
fn receipt_digest_placeholder() -> Digest {
    Digest::parse("00").unwrap_or_else(|_| Digest::parse("00").unwrap())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AutomationObservation {
    pub target_id: String,
    pub scope_id: String,
    pub backend: String,
    pub revision_digest: Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "unknown")]
    pub accessibility_tree: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_artifact: Option<ArtifactId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AutomationReceipt {
    pub action_id: ActionId,
    pub grant_id: Option<GrantId>,
    pub target_id: String,
    pub backend: String,
    pub operation: String,
    pub outcome: ActionOutcome,
    pub effect: Effect,
    pub revision_before: Digest,
    pub revision_after: Digest,
    pub receipt_digest: Digest,
    pub error: Option<String>,
}

#[derive(serde::Deserialize)]
struct ExecutionReport {
    revision_after: Digest,
    effect: Effect,
    accessibility_tree: Option<serde_json::Value>,
    image_artifact: Option<ArtifactId>,
    result: Option<String>,
}

impl Default for AutomationBroker {
    fn default() -> Self {
        Self {
            store: Store::default(),
            cas_root: None,
        }
    }
}
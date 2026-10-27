//! J3: advisory use of local judgments in actual assistant decisions.
//!
//! The advisor is an authority gate, not a dispatcher. Given a `JudgmentInput`,
//! the caller's already-policy-filtered `eligible` candidate set and the
//! committed `ProjectPolicy`, it applies the deterministic policy first and
//! consults the durable local judgment (J1: `JudgmentRecord`) and its held-out
//! qualification (J2: `QualificationRecord`) only for the residual semantic
//! uncertainty. It returns advice and nothing else:
//!
//! - It never dispatches an action, never approves anything and never records
//!   a mutation.
//! - It never enlarges the eligible candidate set: the choice it may advise is
//!   always a subset of the caller's `eligible` set intersected with the
//!   committed policy.
//! - A judgment of a domain that is not `enabled` per the stored qualification
//!   record never yields a choice.
//! - An unavailable local judgment (no recorded result, unqualified or
//!   outaged) yields `Unavailable`; there is deliberately no paid-judging
//!   fallback, so no route is ever called.
//! - Every granted advisory choice is recorded in the existing `judgments`
//!   table under a deterministic (name-derived) decision id, so a replay of the
//!   same input/policy/choice is an idempotent upsert and the owner can inspect
//!   exactly what was advised.

use std::collections::BTreeSet;

use rusqlite::{params, OptionalExtension};
use sha2::{Digest as _Sha2, Sha256};
use uuid::Uuid;

use crate::contracts::*;
use crate::error::{CoreError, Result};
use crate::judgments::QualificationStore;
use crate::store::{events::now_ms, Store};

/// Deterministic (name-based, RFC 4122 v5-shaped) advisory decision id. The
/// same project/domain/version/input/eligible/policy-version/choice always maps
/// to the same UUID, mirroring `qualification::decision_id_for`'s idempotency.
const ADVISORY_NAMESPACE: &[u8] = b"ironbark:jev:advisory:v1\x1f";

#[derive(Clone)]
pub struct JudgmentAdvisor {
    store: Store,
    project: ProjectId,
}

impl JudgmentAdvisor {
    pub fn new(store: Store, project: ProjectId) -> Result<Self> {
        Ok(Self { store, project })
    }

    /// The advisory authority gate. See the module documentation: this returns
    /// advice for the *already-obtained* local judgment of `input.decision_id`
    /// and never calls out to a judge, a provider or a paid route.
    pub async fn advise(
        &self,
        input: JudgmentInput,
        eligible: Vec<String>,
        policy: &ProjectPolicy,
    ) -> Result<AdvisoryOutcome> {
        // 1. Deterministic policy identity/version first. Advice is only ever
        //    valid under the project's committed policy; a stale or foreign
        //    policy is refused rather than silently narrowed.
        if policy.project_id != self.project {
            return Err(CoreError::new("policy_project_mismatch", "policy does not belong to the advisor's project"));
        }
        if self.latest_policy_version().await? != Some(policy.version as i64) {
            return Err(CoreError::new("stale_policy", "advisor policy version is not the project's committed policy"));
        }

        // 2. Deterministic eligibility. The judged candidates are intersected
        //    with the caller's eligible set (never enlarged) and, for a route
        //    choice, re-checked against the committed policy's route rules:
        //    local-only, allowed connections and unknown-cost spend policy.
        let eligible_set: BTreeSet<&str> = eligible.iter().map(String::as_str).collect();
        let route_choice = parse_domain(&input.domain) == Some(JudgmentDomain::AllowedRouteChoice);
        let candidates_in_eligible: Vec<String> = input
            .candidates
            .iter()
            .filter(|candidate| eligible_set.contains(candidate.as_str()))
            .cloned()
            .collect();
        let admitted: Vec<String> = candidates_in_eligible
            .iter()
            .filter(|candidate| !route_choice || policy_admits_route(candidate.as_str(), policy))
            .cloned()
            .collect();
        if admitted.is_empty() {
            // Deterministic policy already resolved the decision (or denied
            // every candidate): there is nothing for the local judgment to add.
            return Ok(AdvisoryOutcome::Unavailable);
        }

        // 3. Consult the local judgment only for residual semantic uncertainty:
        //    the domain must be enabled by a held-out qualification record
        //    (J2) and the exact judged record (J1) must exist. Anything else is
        //    an honest unavailability, never a fabricated choice.
        let Some(domain) = parse_domain(&input.domain) else {
            return Ok(AdvisoryOutcome::Unavailable);
        };
        let Some(qualification) = QualificationStore::new(self.store.clone()).current(domain).await? else {
            return Ok(AdvisoryOutcome::Unavailable);
        };
        let Some(judgment) = self.recorded_judgment(input.decision_id).await? else {
            // The local judge produced no durable result for this decision:
            // unavailable, and no paid route is consulted as a substitute.
            return Ok(AdvisoryOutcome::Unavailable);
        };

        // The recorded judgment must be about exactly this input and the
        // qualified identity; a mismatched record is not silently accepted.
        if judgment.domain != input.domain
            || judgment.domain_version != input.domain_version
            || judgment.input_digest != input.input_digest
            || judgment.candidates != input.candidates
        {
            return Ok(AdvisoryOutcome::NeedsPlanner { reason: "judgment_input_mismatch".into(), decision_id: None });
        }
        if judgment.model_identity != qualification.model_identity {
            return Ok(AdvisoryOutcome::NeedsPlanner { reason: "judgment_identity_mismatch".into(), decision_id: None });
        }

        // 4. Apply the judgment's state and, where it produced a choice, its
        //    authority against the admitted candidate set.
        match judgment.state {
            JudgmentState::Available => {
                let Some(choice) = judgment.choice.clone() else {
                    return Ok(AdvisoryOutcome::NeedsPlanner { reason: "judgment_abstained".into(), decision_id: None });
                };
                if !admitted.iter().any(|candidate| candidate == &choice) {
                    // A judgment can never add a route that policy did not
                    // admit, nor one outside the caller's eligible set.
                    let reason = if candidates_in_eligible.iter().any(|candidate| candidate == &choice) {
                        "choice_not_policy_allowed"
                    } else {
                        "choice_not_eligible"
                    };
                    return Ok(AdvisoryOutcome::NeedsPlanner { reason: reason.into(), decision_id: None });
                }
                let decision_id =
                    self.record_advice(&input, &admitted, policy, &qualification, &judgment, &choice).await?;
                Ok(AdvisoryOutcome::QualifiedChoice { choice, decision_id })
            }
            // Invalid and abstained results are honest non-answers: escalate to
            // the main planner under unchanged policy instead of dispatching.
            JudgmentState::Invalid => Ok(AdvisoryOutcome::NeedsPlanner { reason: "judgment_invalid".into(), decision_id: None }),
            JudgmentState::Abstain => Ok(AdvisoryOutcome::NeedsPlanner { reason: "judgment_abstained".into(), decision_id: None }),
            // Unqualified or unobserved runtime has no advisory authority.
            JudgmentState::Unqualified => Ok(AdvisoryOutcome::Unavailable),
            JudgmentState::Unavailable => Ok(AdvisoryOutcome::Unavailable),
        }
    }

    /// The version of the project's latest committed policy, if any.
    async fn latest_policy_version(&self) -> Result<Option<i64>> {
        let project = self.project;
        self.store
            .with_connection(move |conn| {
                let version: Option<i64> = conn
                    .query_row(
                        "SELECT version FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
                        [project.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(version)
            })
            .await
    }

    /// The durable local judgment (J1) recorded for exactly this decision, if
    /// one exists. Absence is an honest outage/no-result, never an invented one.
    async fn recorded_judgment(&self, decision_id: DecisionId) -> Result<Option<JudgmentRecord>> {
        self.store
            .with_connection(move |conn| {
                let body: Option<String> = conn
                    .query_row("SELECT body FROM judgments WHERE decision_id=?1", [decision_id.to_string()], |row| row.get(0))
                    .optional()?;
                match body {
                    Some(body) => Ok(Some(serde_json::from_str(&body).map_err(|_| {
                        CoreError::new("storage_corrupt", "persisted judgment body is not a valid JudgmentRecord")
                    })?)),
                    None => Ok(None),
                }
            })
            .await
    }

    /// Persist the granted advisory decision as a `JudgmentRecord` under a
    /// deterministic name-derived decision id. The row is the owner-inspectable
    /// record of exactly what was advised under which policy/qualification.
    async fn record_advice(
        &self,
        input: &JudgmentInput,
        admitted: &[String],
        policy: &ProjectPolicy,
        qualification: &QualificationRecord,
        judgment: &JudgmentRecord,
        choice: &str,
    ) -> Result<DecisionId> {
        let decision_id = advisory_decision_id(self.project, input, admitted, policy, choice);
        let record = JudgmentRecord {
            decision_id,
            domain: input.domain.clone(),
            domain_version: input.domain_version.clone(),
            candidates: input.candidates.clone(),
            input_refs: input.input_refs.clone(),
            input_digest: input.input_digest.clone(),
            criterion: input.criterion.clone(),
            route_policy_digest: input.route_policy_digest.clone(),
            model_identity: judgment.model_identity.clone(),
            runtime_identity_digest: judgment.runtime_identity_digest.clone(),
            prompt_digest: judgment.prompt_digest.clone(),
            candidate_codes: judgment.candidate_codes.clone(),
            choice: Some(choice.to_string()),
            state: JudgmentState::Available,
            score: judgment.score.clone(),
            latency_ms: judgment.latency_ms,
            qualification_digest: Some(qualification.calibrator_digest.clone()),
            reason: Some("advisory_qualified".into()),
        };
        let project = self.project;
        let input_digest = record.input_digest.as_str().to_string();
        let model_identity = record.model_identity.clone();
        let prompt_digest = record.prompt_digest.as_str().to_string();
        let state = "available";
        let body = serde_json::to_string(&record)?;
        let created_at = now_ms();
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO judgments(decision_id, project_id, run_id, state, input_digest, model_identity, prompt_digest, body, created_at)
                     VALUES(?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(decision_id) DO UPDATE SET state=excluded.state, body=excluded.body, created_at=excluded.created_at",
                    params![
                        decision_id.to_string(),
                        project.to_string(),
                        state,
                        input_digest,
                        model_identity,
                        prompt_digest,
                        body,
                        created_at,
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(decision_id)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn parse_domain(domain: &str) -> Option<JudgmentDomain> {
    serde_json::from_value(serde_json::Value::String(domain.to_string())).ok()
}

/// Whether the committed policy admits this route candidate, mirroring
/// `knowledge::validate_route`: the canonical local connection is always
/// admitted, a remote connection must be an allowed one, and under
/// `free_only` with unknown cost disallowed only the local route (whose cost is
/// locally known) is spendable.
fn policy_admits_route(candidate: &str, policy: &ProjectPolicy) -> bool {
    let local = candidate == "local";
    if policy.local_only {
        return local;
    }
    if !local && !policy.allowed_connection_ids.iter().any(|allowed| allowed == candidate) {
        return false;
    }
    if matches!(policy.spend_policy, SpendPolicy::FreeOnly) && !policy.allow_unknown_cost && !local {
        return false;
    }
    true
}

/// Deterministic advisory decision id bound to the project, the exact input,
/// the admitted eligible set and the policy version. Same inputs → same UUID.
fn advisory_decision_id(
    project: ProjectId,
    input: &JudgmentInput,
    admitted: &[String],
    policy: &ProjectPolicy,
    choice: &str,
) -> DecisionId {
    let mut hasher = Sha256::new();
    hasher.update(ADVISORY_NAMESPACE);
    hasher.update(project.to_string().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(input.domain.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(input.domain_version.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(input.input_digest.as_str().as_bytes());
    hasher.update(b"\x1f");
    for candidate in admitted {
        hasher.update(candidate.as_bytes());
        hasher.update(b"\x1e");
    }
    hasher.update(b"\x1f");
    hasher.update(policy.version.to_string().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(choice.as_bytes());
    let digest = Sha256::digest(hasher.finalize());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50; // RFC 4122 version 5 (name-based)
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    DecisionId(Uuid::from_bytes(bytes))
}

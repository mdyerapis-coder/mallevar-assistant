//! J2: held-out qualification for local JEV judgment domains.
//!
//! Invariants (mirrored 1:1 by the TS `qualificationGate` in
//! `packages/adapters/src/jev/domains/types.ts`):
//! - A domain is enabled ONLY when the holdout has >= 200 rows, acceptance
//!   coverage >= 30%, every required class is represented in holdout, the
//!   report is internally consistent, and the one-sided 95% upper confidence
//!   bound on accepted-choice error is <= 5%. Low coverage cannot hide wrong
//!   decisions: the error bound is computed over accepted choices only, and
//!   the coverage gate is independent of it.
//! - The observed runtime identity must equal the claimed identity, else the
//!   report is blocked (model-identity mismatch) — a green badge is never
//!   issued for an unobserved or different model.
//! - A version/identity change revokes any prior enablement: the stale
//!   identity is persisted as blocked with a version-change reason and the
//!   old record can never read back as enabled.
//! - An uncalibrated score never auto-selects: enablement only makes a domain
//!   advisory-eligible; auto-selection authority requires the separate J3
//!   calibrator bound to the exact dataset and identity.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _Sha2, Sha256};
use schemars::JsonSchema;
use ts_rs::TS;

use crate::contracts::*;
use crate::error::{CoreError, Result};
use crate::store::Store;

pub const MIN_HOLDOUT: u32 = 200;
const MIN_ACCEPTANCE_COVERAGE: f64 = 0.30;
const MAX_UPPER_ERROR_BOUND: f64 = 0.05;
const REQUIRED_CLASSES: [&str; 4] = ["positive", "negative", "ambiguous", "hostile"];

// ── Wire DTOs (snake_case, byte-compatible with the TS EvaluationReport) ─────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ClassMetrics {
    pub total: u32,
    pub accepted: u32,
    pub wrong: u32,
    pub abstained: u32,
    pub invalid: u32,
    pub unavailable: u32,
    pub unqualified: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HoldoutMetrics {
    pub size: u32,
    pub accepted: u32,
    pub wrong_accepted: u32,
    pub abstained: u32,
    pub invalid: u32,
    pub unavailable: u32,
    pub unqualified: u32,
    pub coverage: f64,
    pub upper_error_bound: f64,
    pub by_class: std::collections::BTreeMap<String, ClassMetrics>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct LatencyMs {
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
}

/// The TS `EvaluationReport`, re-derivable and re-gated here. `state` and
/// `reasons` on the wire are ignored: the core re-derives them from the raw
/// counts before persisting anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReportInput {
    pub domain: JudgmentDomain,
    pub domain_version: String,
    pub dataset_digest: Digest,
    pub prompt_digest: Digest,
    pub split_digests: SplitDigests,
    pub observed_identity_digest: Digest,
    pub observed_model_identity: String,
    pub claimed_identity: Digest,
    pub confidence_method: String,
    pub holdout: HoldoutMetrics,
    pub latency_ms: LatencyMs,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::contracts::wire::option_i64")]
    #[schemars(with = "Option<crate::contracts::wire::WireI64Schema>")]
    #[ts(optional, type = "string")]
    pub evaluated_at: Option<i64>,
}


#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SplitDigests {
    pub development: Digest,
    pub calibration: Digest,
    pub holdout: Digest,
}

// ── Statistics: mirrored from the TS implementation exactly ─────────────────

fn binom_cdf(x: i64, n: i64, p: f64) -> f64 {
    if n <= 0 { return 1.0; }
    if x < 0 { return 0.0; }
    if x >= n || p <= 0.0 { return 1.0; }
    if p >= 1.0 { return if x >= n { 1.0 } else { 0.0 }; }
    let log_failure = (-p).ln_1p();
    let mut log_term = n as f64 * log_failure;
    let mut log_cdf = f64::NEG_INFINITY;
    for k in 0..=x {
        if k > 0 {
            log_term += ((n - k + 1) as f64).ln() - (k as f64).ln() + p.ln() - log_failure;
        }
        let max = log_cdf.max(log_term);
        log_cdf = max + (log_cdf.min(log_term) - max).exp().ln_1p();
    }
    log_cdf.exp().min(1.0)
}

/// One-sided 95% Clopper-Pearson upper bound on the binomial failure rate for
/// `wrong` failures out of `accepted` accepted choices. Zero accepted choices
/// yield the conservative maximum, never an invented number.
pub fn binom_upper95(wrong: i64, accepted: i64) -> Result<f64> {
    if wrong < 0 || accepted < 0 || wrong > accepted {
        return Err(CoreError::new("metrics_inconsistent", "0 <= wrong_accepted <= accepted is required"));
    }
    if accepted == 0 { return Ok(1.0); }
    if wrong >= accepted { return Ok(1.0); }
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if binom_cdf(wrong, accepted, mid) >= 0.05 { lo = mid; } else { hi = mid; }
    }
    Ok(hi)
}

// ── The deterministic enablement gate ────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct GateVerdict {
    pub state: QualificationState,
    pub reasons: Vec<String>,
}

/// Re-derives the gate from raw counts. The stored or reported state is never
/// trusted. Mirrors `qualificationGate` in the TS adapter exactly.
pub fn gate(report: &EvaluationReportInput) -> GateVerdict {
    let mut reasons: Vec<String> = Vec::new();
    let h = &report.holdout;
    if h.size < MIN_HOLDOUT { reasons.push("insufficient_holdout".into()); }
    if h.size > 0 {
        let coverage = h.accepted as f64 / h.size as f64;
        if (coverage - h.coverage).abs() > 1e-9 { reasons.push("metrics_inconsistent".into()); }
        if coverage < MIN_ACCEPTANCE_COVERAGE - 1e-12 { reasons.push("insufficient_coverage".into()); }
    }
    match binom_upper95(h.wrong_accepted as i64, h.accepted as i64) {
        Ok(bound) => {
            if (bound - h.upper_error_bound).abs() > 1e-9 { reasons.push("metrics_inconsistent".into()); }
        }
        Err(_) => reasons.push("metrics_inconsistent".into()),
    }
    if h.upper_error_bound > MAX_UPPER_ERROR_BOUND + 1e-12 { reasons.push("error_bound_exceeded".into()); }
    for cls in REQUIRED_CLASSES {
        let metrics = h.by_class.get(cls);
        if metrics.map_or(true, |m| m.total < 1) { reasons.push(format!("class_missing:{cls}")); }
    }
    if report.observed_identity_digest != report.claimed_identity {
        reasons.push("model_identity_mismatch".into());
    }
    GateVerdict { state: if reasons.is_empty() { QualificationState::Enabled } else { QualificationState::Blocked }, reasons }
}

// ── Store ────────────────────────────────────────────────────────────────────

/// Persists qualification decisions in the `judgments` table. Decision IDs are
/// deterministic per (domain, model identity, prompt digest) so re-qualifying
/// the same identity is idempotent, and a new identity writes a new row that
/// revokes (blocks) the previous one.
#[derive(Clone)]
pub struct QualificationStore { pub store: Store }

impl QualificationStore {
    pub fn new(store: Store) -> Self { Self { store } }

    fn validate(report: &EvaluationReportInput) -> Result<()> {
        if report.holdout.size < report.holdout.accepted + report.holdout.abstained
            + report.holdout.invalid + report.holdout.unavailable + report.holdout.unqualified {
            return Err(CoreError::new("metrics_inconsistent", "holdout buckets exceed holdout size"));
        }
        if report.holdout.wrong_accepted > report.holdout.accepted {
            return Err(CoreError::new("metrics_inconsistent", "wrong_accepted exceeds accepted"));
        }
        for (name, value) in [("coverage", report.holdout.coverage), ("upper_error_bound", report.holdout.upper_error_bound)] {
            if !value.is_finite() {
                return Err(CoreError::new("metrics_inconsistent", &format!("{name} is not finite")));
            }
        }
        Ok(())
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    /// Persists the gate outcome for the report's exact identity. Any prior
    /// enablement of a DIFFERENT identity is revoked first (persisted as
    /// blocked with `version_change`); a prior record of the same identity is
    /// replaced by the latest outcome.
    pub async fn enable(&self, report: EvaluationReportInput, calibrator_digest: Digest) -> Result<QualificationRecord> {
        Self::validate(&report)?;
        let verdict = gate(&report);
        let now = report.evaluated_at.unwrap_or_else(Self::now_ms);

        self.store.with_transaction(move |tx| -> Result<QualificationRecord> {
            // Revoke stale enablement of any other identity in the same domain.
            let mut stmt = tx.prepare(
                "SELECT decision_id, body FROM judgments WHERE decision_id LIKE 'qual:%' AND state='enabled'",
            ).map_err(|e| CoreError::new("storage_error", e.to_string()))?;
            let mut rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }).map_err(|e| CoreError::new("storage_error", e.to_string()))?;
            while let Some(row) = rows.next() {
                let (decision_id, body_str) = row.map_err(|e| CoreError::new("storage_error", e.to_string()))?;
                let stored: QualificationRecord = serde_json::from_str(&body_str)
                    .map_err(|_| CoreError::new("storage_corrupt", "persisted qualification body is not valid JSON"))?;
                if stored.domain == report.domain && stored.model_identity != report.observed_model_identity {
                    let blocked = QualificationRecord { state: QualificationState::Blocked, ..stored };
                    let body = serde_json::to_string(&blocked).map_err(|e| CoreError::new("invalid_dto", e.to_string()))?;
                    tx.execute("UPDATE judgments SET state='blocked', body=?2, created_at=?3 WHERE decision_id=?1",
                        params![decision_id, body, now])
                        .map_err(|e| CoreError::new("storage_error", e.to_string()))?;
                }
            }
            let record = QualificationRecord {
                domain: report.domain,
                model_identity: report.observed_model_identity.clone(),
                prompt_digest: report.prompt_digest.clone(),
                state: verdict.state,
                upper_error_bound: report.holdout.upper_error_bound,
                acceptance_coverage: report.holdout.coverage,
                holdout_size: report.holdout.size,
                evaluated_at: now,
                calibrator_digest: calibrator_digest.clone(),
            };
            tx.execute(
                "INSERT INTO judgments(decision_id, project_id, run_id, state, input_digest, model_identity, prompt_digest, body, created_at)
                 VALUES(?1, NULL, NULL, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(decision_id) DO UPDATE SET state=excluded.state, body=excluded.body, created_at=excluded.created_at",
                params![
                    decision_id_for(record.domain, &record.model_identity, &record.prompt_digest),
                    state_str(&record.state),
                    report.dataset_digest.as_str(),
                    record.model_identity,
                    record.prompt_digest.as_str(),
                    serde_json::to_string(&record).map_err(|e| CoreError::new("invalid_dto", e.to_string()))?,
                    now,
                ],
            ).map_err(|e| CoreError::new("storage_error", e.to_string()))?;
            Ok(record)
        }).await
    }

    /// The currently enabled record for a domain, if any. A blocked (revoked
    /// or failed) identity never reads back as enabled.
    pub async fn current(&self, domain: JudgmentDomain) -> Result<Option<QualificationRecord>> {
        self.store.with_connection(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT body FROM judgments WHERE decision_id LIKE 'qual:%'
                 AND state='enabled' AND model_identity IS NOT NULL
                 ORDER BY created_at DESC, decision_id DESC LIMIT 1",
            ).map_err(|e| CoreError::new("storage_error", e.to_string()))?;
            let mut rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| CoreError::new("storage_error", e.to_string()))?;
            let mut found: Option<QualificationRecord> = None;
            while let Some(row) = rows.next() {
                let body = row.map_err(|e| CoreError::new("storage_error", e.to_string()))?;
                let record: QualificationRecord = serde_json::from_str(&body)
                    .map_err(|_| CoreError::new("storage_corrupt", "persisted qualification body is not valid JSON"))?;
                if record.domain != domain { continue; }
                found = Some(record);
                break;
            }
            Ok(found)
        }).await
    }

    /// All qualification records for a domain: the live enabled one (if any)
    /// plus the blocked (revoked/failed) ones, newest first.
    pub async fn list(&self, domain: JudgmentDomain) -> Result<DomainQualifications> {
        self.store.with_connection(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT body FROM judgments WHERE decision_id LIKE 'qual:%'
                 ORDER BY created_at DESC, decision_id DESC",
            ).map_err(|e| CoreError::new("storage_error", e.to_string()))?;
            let mut rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| CoreError::new("storage_error", e.to_string()))?;
            let mut enabled: Option<QualificationRecord> = None;
            let mut blocked: Vec<QualificationRecord> = Vec::new();
            while let Some(row) = rows.next() {
                let body = row.map_err(|e| CoreError::new("storage_error", e.to_string()))?;
                let record: QualificationRecord = serde_json::from_str(&body)
                    .map_err(|_| CoreError::new("storage_corrupt", "persisted qualification body is not valid JSON"))?;
                if record.domain != domain { continue; }
                match record.state {
                    QualificationState::Enabled if enabled.is_none() => enabled = Some(record),
                    QualificationState::Enabled => {}
                    QualificationState::Blocked => blocked.push(record),
                }
            }
            Ok(DomainQualifications { enabled, blocked })
        }).await
    }
}

/// All qualification records for a domain: the live enabled one (if any)
/// plus the blocked (revoked/failed) ones, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DomainQualifications {
    pub enabled: Option<QualificationRecord>,
    pub blocked: Vec<QualificationRecord>,
}

fn state_str(state: &QualificationState) -> &'static str {
    match state { QualificationState::Enabled => "enabled", QualificationState::Blocked => "blocked" }
}

/// Deterministic decision ID: the same (domain, identity, prompt) always maps
/// to the same row, so re-qualification is an idempotent upsert.
fn decision_id_for(domain: JudgmentDomain, model_identity: &str, prompt_digest: &Digest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ironbark:jev:qualification:v1\x1f");
    hasher.update(serde_json::to_string(&domain).expect("JudgmentDomain serializes").as_bytes());
    hasher.update(b"\x1f");
    hasher.update(model_identity.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(prompt_digest.as_str().as_bytes());
    format!("qual:{}", hex(&Sha256::digest(hasher.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes { out.push_str(&format!("{byte:02x}")); }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(s: &str) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(s.as_bytes());
        Digest::parse(&hex(hasher.finalize().as_slice())).expect("sha256 hex parses")
    }

    fn metrics(accepted: u32, wrong: u32, size: u32) -> ClassMetrics {
        ClassMetrics { total: size, accepted, wrong, abstained: size.saturating_sub(accepted), invalid: 0, unavailable: 0, unqualified: 0 }
    }

    fn holdout(accepted: u32, wrong: u32, size: u32) -> HoldoutMetrics {
        let coverage = if size > 0 { accepted as f64 / size as f64 } else { 0.0 };
        let bound = binom_upper95(wrong as i64, accepted as i64).expect("valid counts");
        HoldoutMetrics {
            size, accepted, wrong_accepted: wrong, abstained: size - accepted, invalid: 0, unavailable: 0, unqualified: 0,
            coverage, upper_error_bound: bound,
            by_class: [
                ("positive", metrics(accepted / 2, wrong / 2, size / 2)),
                ("negative", metrics(accepted / 2, wrong.saturating_sub(wrong / 2), size / 2)),
                ("ambiguous", metrics(0, 0, (size / 2).saturating_sub(1))),
                ("hostile", metrics(0, 0, 1)),
            ].into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    fn report(domain: JudgmentDomain, observed: &str, claimed: &str, h: HoldoutMetrics) -> EvaluationReportInput {
        EvaluationReportInput {
            domain,
            domain_version: "1".into(),
            dataset_digest: digest("dataset"),
            prompt_digest: digest("prompt"),
            split_digests: SplitDigests { development: digest("dev"), calibration: digest("cal"), holdout: digest("hold") },
            observed_identity_digest: digest(observed),
            observed_model_identity: observed.into(),
            claimed_identity: digest(claimed),
            confidence_method: "none".into(),
            holdout: h,
            latency_ms: LatencyMs { mean: 1.0, p50: 1.0, p95: 1.0 },
            evaluated_at: Some(1_000),
        }
    }

    fn passing_report(domain: JudgmentDomain, identity: &str) -> EvaluationReportInput {
        report(domain, identity, identity, holdout(100, 0, 200))
    }

    #[test]
    fn binom_upper95_edge_cases() {
        assert_eq!(binom_upper95(0, 0).unwrap(), 1.0, "no accepted choices: conservative maximum");
        assert_eq!(binom_upper95(5, 5).unwrap(), 1.0, "all wrong: bound is 1");
        let zero_errors = binom_upper95(0, 100).unwrap();
        // Rule of three: upper bound ~ 3/100 for 0/100 at 95% one-sided.
        assert!(zero_errors > 0.029 && zero_errors < 0.031, "0/100 upper bound is ~3%, got {zero_errors}");
        let some_errors = binom_upper95(3, 80).unwrap();
        // 3/80 has a known one-sided 95% Clopper-Pearson bound near 0.0941.
        assert!(some_errors > 0.093 && some_errors < 0.096, "3/80 upper bound near 9.4%, got {some_errors}");
        assert!(binom_upper95(5, 4).is_err(), "wrong > accepted is inconsistent");
    }

    #[test]
    fn error_bounds_match_exact_combinatorial_references() {
        // Independent Decimal bisection of sum(C(n,k) p^k (1-p)^(n-k)) = 0.05.
        for (wrong, accepted, reference) in [
            (177, 200, 0.920114754926111),
            (112, 200, 0.6192709347928696),
            (1, 2000, 0.002369713367656844),
            (1800, 2000, 0.9108444162227793),
        ] {
            let actual = binom_upper95(wrong, accepted).unwrap();
            assert!((actual - reference).abs() < 1e-10, "{wrong}/{accepted}: {actual} != {reference}");
        }
    }

    #[test]
    fn gate_enables_only_when_all_conditions_hold() {
        let verdict = gate(&passing_report(JudgmentDomain::TaskIntent, "id-a"));
        assert_eq!(verdict.state, QualificationState::Enabled, "clean report passes: {:?}", verdict.reasons);
        assert!(verdict.reasons.is_empty());
    }

    #[test]
    fn gate_blocks_low_coverage_even_with_zero_errors() {
        // 10 accepted of 200 (coverage 5%) with zero wrong: coverage and the
        // small-sample confidence bound both independently block the domain.
        let h = holdout(10, 0, 200);
        let verdict = gate(&report(JudgmentDomain::TaskIntent, "id-a", "id-a", h));
        assert_eq!(verdict.state, QualificationState::Blocked);
        assert!(verdict.reasons.contains(&"insufficient_coverage".into()), "{:?}", verdict.reasons);
        // With only 10 accepted and 0 errors, the 95% bound is ~30% > 5%,
        // so both insufficient_coverage AND error_bound_exceeded fire.
        assert!(verdict.reasons.contains(&"error_bound_exceeded".into()), "{:?}", verdict.reasons);
    }

    #[test]
    fn gate_blocks_wrong_decisions_that_exceed_the_bound() {
        // 9 wrong of 100 accepted: bound ~0.16 > 5% -> blocked.
        let h = holdout(100, 9, 200);
        let verdict = gate(&report(JudgmentDomain::TaskIntent, "id-a", "id-a", h));
        assert_eq!(verdict.state, QualificationState::Blocked);
        assert!(verdict.reasons.contains(&"error_bound_exceeded".into()), "{:?}", verdict.reasons);
    }

    #[test]
    fn gate_blocks_small_holdout_and_missing_classes() {
        let small = holdout(60, 0, 100);
        let verdict = gate(&report(JudgmentDomain::TaskIntent, "id-a", "id-a", small));
        assert!(verdict.reasons.contains(&"insufficient_holdout".into()), "{:?}", verdict.reasons);

        let mut missing = holdout(100, 0, 200);
        missing.by_class.remove("hostile");
        let verdict = gate(&report(JudgmentDomain::TaskIntent, "id-a", "id-a", missing));
        assert!(verdict.reasons.contains(&"class_missing:hostile".into()), "{:?}", verdict.reasons);
    }

    #[test]
    fn gate_blocks_inconsistent_metrics() {
        let mut bad = holdout(100, 0, 200);
        bad.coverage = 0.99; // claimed 99%, computed 50%
        let verdict = gate(&report(JudgmentDomain::TaskIntent, "id-a", "id-a", bad));
        assert!(verdict.reasons.contains(&"metrics_inconsistent".into()), "{:?}", verdict.reasons);
    }

    #[test]
    fn gate_blocks_identity_mismatch() {
        let verdict = gate(&report(JudgmentDomain::TaskIntent, "observed-model", "claimed-model", holdout(100, 0, 200)));
        assert_eq!(verdict.state, QualificationState::Blocked);
        assert!(verdict.reasons.contains(&"model_identity_mismatch".into()), "{:?}", verdict.reasons);
    }

    #[tokio::test]
    async fn enablement_revoked_on_identity_change() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&tmp.path().join("core.db")).await.expect("open store");
        let qs = QualificationStore::new(store);

        let first = qs.enable(passing_report(JudgmentDomain::TaskIntent, "Qwen2.5-0.5B-Instruct-Q8_0.gguf"), digest("cal-a")).await.expect("enable v1");
        assert_eq!(first.state, QualificationState::Enabled);

        // A different observed identity (version/identity change): the stale
        // identity must not stay enabled.
        let mut second = passing_report(JudgmentDomain::TaskIntent, "Qwen3-4B-Instruct-2507-Q4_K_M.gguf");
        second.evaluated_at = Some(2_000);
        let second = qs.enable(second, digest("cal-b")).await.expect("enable v2");
        assert_eq!(second.state, QualificationState::Enabled);

        let listing = qs.list(JudgmentDomain::TaskIntent).await.expect("list");
        assert!(listing.enabled.is_some(), "new identity is enabled");
        assert_eq!(listing.enabled.as_ref().unwrap().model_identity, "Qwen3-4B-Instruct-2507-Q4_K_M.gguf");
        let stale: Vec<String> = listing.blocked.iter().map(|r| r.model_identity.clone()).collect();
        assert!(stale.contains(&"Qwen2.5-0.5B-Instruct-Q8_0.gguf".to_string()),
            "old identity was revoked to blocked: {stale:?}");
        // The old identity reads back as NOT enabled.
        assert_ne!(qs.current(JudgmentDomain::TaskIntent).await.unwrap().unwrap().model_identity, "Qwen2.5-0.5B-Instruct-Q8_0.gguf");
    }

    #[tokio::test]
    async fn blocked_report_persists_and_never_reads_as_enabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&tmp.path().join("core.db")).await.expect("open store");
        let qs = QualificationStore::new(store);

        let bad = report(JudgmentDomain::EvidenceNeeded, "id-a", "id-b", holdout(100, 0, 200));
        let record = qs.enable(bad, digest("cal")).await.expect("persist blocked");
        assert_eq!(record.state, QualificationState::Blocked, "mismatch persists as blocked");
        assert!(qs.current(JudgmentDomain::EvidenceNeeded).await.unwrap().is_none(), "no enabled record for a mismatched identity");
        let listing = qs.list(JudgmentDomain::EvidenceNeeded).await.unwrap();
        assert!(listing.enabled.is_none());
        assert_eq!(listing.blocked.len(), 1);
    }

    #[tokio::test]
    async fn wrong_answers_persist_as_blocked_without_double_counting_accepted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&tmp.path().join("core.db")).await.expect("open store");
        let qs = QualificationStore::new(store);
        let wrong = report(JudgmentDomain::TaskIntent, "id-a", "id-a", holdout(100, 9, 200));

        let record = qs.enable(wrong, digest("cal")).await.expect("persist measured wrong answers");
        assert_eq!(record.state, QualificationState::Blocked);
        assert_eq!(record.acceptance_coverage, 0.5);
        assert!(record.upper_error_bound > MAX_UPPER_ERROR_BOUND);
        assert!(qs.current(JudgmentDomain::TaskIntent).await.unwrap().is_none());
        assert_eq!(qs.list(JudgmentDomain::TaskIntent).await.unwrap().blocked, vec![record]);
    }

    #[tokio::test]
    async fn same_identity_requalification_is_idempotent_upsert() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&tmp.path().join("core.db")).await.expect("open store");
        let qs = QualificationStore::new(store);

        let first = qs.enable(passing_report(JudgmentDomain::TaskIntent, "id-a"), digest("cal")).await.unwrap();
        let second = qs.enable(passing_report(JudgmentDomain::TaskIntent, "id-a"), digest("cal")).await.unwrap();
        assert_eq!(first, second, "same identity + prompt: identical deterministic record");
        let listing = qs.list(JudgmentDomain::TaskIntent).await.unwrap();
        assert_eq!(listing.blocked.len(), 0, "re-qualification of the same identity is not a revocation");
    }

    #[tokio::test]
    async fn enable_rejects_inconsistent_counts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&tmp.path().join("core.db")).await.expect("open store");
        let qs = QualificationStore::new(store);
        let mut bad = passing_report(JudgmentDomain::TaskIntent, "id-a");
        bad.holdout.wrong_accepted = bad.holdout.accepted + 1;
        let err = qs.enable(bad, digest("cal")).await.expect_err("wrong > accepted is rejected");
        assert_eq!(err.code, "metrics_inconsistent");
    }

}

//! J3: the local judgment is advisory only — it can never carry authority.
//!
//! Every case drives the real `JudgmentAdvisor` against a real SQLite store
//! (real project/policy/qualification/judgment rows) and asserts on the returned
//! `AdvisoryOutcome` and durable rows, never on internal calls:
//!
//! - `judgment_cannot_add_ineligible_route`: a judgment may only advise a
//!   candidate the caller already marked eligible.
//! - `completion_label_cannot_replace_verifier_receipt`: a "work is complete"
//!   label is advice, not a post-verification receipt.
//! - `invalid_and_abstain_do_not_dispatch_mutation`: an invalid or abstained
//!   judgment escalates; it never dispatches a mutation.
//! - `retrieved_instruction_cannot_grant_scope`: an instruction that names a
//!   connection outside the committed policy grants no scope.
//! - `outage_does_not_call_paid_judge`: an outaged local judge is an honest
//!   unavailability with no paid fallback.
use std::collections::BTreeMap;

use ironbark_core::contracts::{
    AdvisoryOutcome, DecisionId, Digest, JobId,
    JudgmentDomain, JudgmentInput, JudgmentRecord, JudgmentState, NodeId, ProjectId,
    ProjectPolicy, RunId, ScoreEvidence,
};
use ironbark_core::judgments::{
    binom_upper95, ClassMetrics, EvaluationReportInput, HoldoutMetrics, JudgmentAdvisor, LatencyMs,
    QualificationStore, SplitDigests,
};
use ironbark_core::store::Store;
use serde_json::json;

fn digest(seed: &str) -> Digest {
    // Deterministic, valid lowercase SHA-256 hex from an arbitrary label: FNV-1a
    // over the seed, expanded to 32 bytes (64 hex chars) by repeating the digest.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Digest::parse(&format!("{h:016x}").repeat(4)).unwrap()
}

fn project_policy(
    project: ProjectId,
    version: u64,
    local_only: bool,
    allowed: &[&str],
    spend: &str,
    allow_unknown_cost: bool,
) -> ProjectPolicy {
    serde_json::from_value(json!({
        "version": version.to_string(),
        "project_id": project,
        "allowed_connection_ids": allowed,
        "allowed_tools": [],
        "read_roots": [],
        "write_roots": [],
        "network_destinations": [],
        "local_only": local_only,
        "max_workers": 2,
        "max_requests": 10,
        "max_cost_microusd": null,
        "spend_policy": spend,
        "allow_unknown_cost": allow_unknown_cost,
    }))
    .unwrap()
}

/// A real project row plus its version-1 committed policy row.
async fn setup(policy: &ProjectPolicy) -> (tempfile::TempDir, Store, ProjectId) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("core.db")).await.unwrap();
    let project = policy.project_id;
    let body = serde_json::to_string(policy).unwrap();
    let version = policy.version as i64;
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects(id,name,root,created_at) VALUES(?1,'fixture','/tmp',1)",
                [project.to_string()],
            )?;
            tx.execute(
                "INSERT INTO policies(project_id,version,body) VALUES(?1,?2,?3)",
                rusqlite::params![project.to_string(), version, body],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (tmp, store, project)
}

fn class_metrics(size: u32) -> ClassMetrics {
    ClassMetrics { total: size, accepted: size, wrong: 0, abstained: 0, invalid: 0, unavailable: 0, unqualified: 0 }
}

/// A clean report: 100 accepted of 200 holdout, zero wrong, every required class
/// present, observed identity == claimed identity. It enables the domain.
fn passing_report(domain: JudgmentDomain, identity: &str) -> EvaluationReportInput {
    let (accepted, size) = (100u32, 200u32);
    let coverage = accepted as f64 / size as f64;
    let upper_error_bound = binom_upper95(0, accepted as i64).unwrap();
    EvaluationReportInput {
        domain,
        domain_version: "1".into(),
        dataset_digest: digest("d"),
        prompt_digest: digest("p"),
        split_digests: SplitDigests { development: digest("a"), calibration: digest("b"), holdout: digest("c") },
        observed_identity_digest: digest("e"),
        observed_model_identity: identity.into(),
        claimed_identity: digest("e"),
        confidence_method: "none".into(),
        holdout: HoldoutMetrics {
            size,
            accepted,
            wrong_accepted: 0,
            abstained: size - accepted,
            invalid: 0,
            unavailable: 0,
            unqualified: 0,
            coverage,
            upper_error_bound,
            by_class: [("positive", class_metrics(50)), ("negative", class_metrics(50)), ("ambiguous", class_metrics(50)), ("hostile", class_metrics(50))]
                .into_iter()
                .map(|(name, metrics)| (name.to_string(), metrics))
                .collect(),
        },
        latency_ms: LatencyMs { mean: 1.0, p50: 1.0, p95: 1.0 },
        evaluated_at: Some(1_000),
    }
}

/// Enable a domain through the real qualification gate.
async fn qualify(store: &Store, domain: JudgmentDomain, identity: &str) {
    let record = QualificationStore::new(store.clone())
        .enable(passing_report(domain, identity), digest("f"))
        .await
        .unwrap();
    assert_eq!(record.state, ironbark_core::contracts::QualificationState::Enabled, "fixture report must enable the domain");
}

fn judgment_input(domain: &str, candidates: &[&str]) -> JudgmentInput {
    JudgmentInput {
        decision_id: DecisionId::new(),
        domain: domain.into(),
        domain_version: "1".into(),
        candidates: candidates.iter().map(|c| c.to_string()).collect(),
        input_refs: vec![],
        input_digest: digest("a"),
        criterion: "fixture criterion".into(),
        route_policy_digest: digest("b"),
    }
}

fn judgment(
    input: &JudgmentInput,
    identity: &str,
    state: JudgmentState,
    choice: Option<&str>,
) -> JudgmentRecord {
    let candidate_codes = input
        .candidates
        .iter()
        .enumerate()
        .map(|(index, label)| (format!("C{index}"), label.clone()))
        .collect::<BTreeMap<String, String>>();
    JudgmentRecord {
        decision_id: input.decision_id,
        domain: input.domain.clone(),
        domain_version: input.domain_version.clone(),
        candidates: input.candidates.clone(),
        input_refs: input.input_refs.clone(),
        input_digest: input.input_digest.clone(),
        criterion: input.criterion.clone(),
        route_policy_digest: input.route_policy_digest.clone(),
        model_identity: identity.into(),
        runtime_identity_digest: digest("e"),
        prompt_digest: digest("p"),
        candidate_codes,
        choice: choice.map(|c| c.to_string()),
        state,
        score: ScoreEvidence::None {},
        latency_ms: 7,
        qualification_digest: None,
        reason: None,
    }
}

/// Persist the J1 `JudgmentRecord` exactly as the recorder does (its own row,
/// keyed by the decision's UUID).
async fn record_judgment(store: &Store, record: &JudgmentRecord) {
    let decision_id = record.decision_id.to_string();
    let state = serde_json::to_value(record.state).unwrap().as_str().unwrap().to_string();
    let input_digest = record.input_digest.as_str().to_string();
    let model_identity = record.model_identity.clone();
    let prompt_digest = record.prompt_digest.as_str().to_string();
    let body = serde_json::to_string(record).unwrap();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO judgments(decision_id, project_id, run_id, state, input_digest, model_identity, prompt_digest, body, created_at)
                 VALUES(?1, NULL, NULL, ?2, ?3, ?4, ?5, ?6, 1)",
                rusqlite::params![decision_id, state, input_digest, model_identity, prompt_digest, body],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn count(store: &Store, sql: &str) -> i64 {
    let sql = sql.to_string();
    store
        .with_connection(move |conn| conn.query_row(&sql, [], |row| row.get::<_, i64>(0)).map_err(Into::into))
        .await
        .unwrap()
}

const ADVISORY_ROWS: &str = "SELECT count(*) FROM judgments WHERE body LIKE '%advisory_qualified%'";

#[tokio::test]
async fn judgment_cannot_add_ineligible_route() {
    let project = ProjectId::new();
    // Policy admits both routes (paid allowed, unknown cost allowed); the only
    // narrowing available is the caller's eligible set.
    let policy = project_policy(project, 1, false, &["local", "alpha"], "paid_allowed", true);
    let (_tmp, store, project) = setup(&policy).await;
    qualify(&store, JudgmentDomain::AllowedRouteChoice, "model-a").await;

    let input = judgment_input("allowed_route_choice", &["local", "alpha"]);
    let eligible_before = vec!["local".to_string()];
    // The local judgment picks a route the caller never marked eligible.
    record_judgment(&store, &judgment(&input, "model-a", JudgmentState::Available, Some("alpha"))).await;

    let advisor = JudgmentAdvisor::new(store.clone(), project).unwrap();
    let outcome = advisor.advise(input.clone(), eligible_before.clone(), &policy).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::NeedsPlanner { reason: "choice_not_eligible".into(), decision_id: None });
    // Advice can never enlarge the eligible set; the only choice offered was
    // outside it and was refused.
    let eligible_after = match &outcome {
        AdvisoryOutcome::QualifiedChoice { choice, .. } => vec![choice.clone()],
        _ => eligible_before.clone(),
    };
    assert_eq!(eligible_after, eligible_before);
    assert_eq!(count(&store, ADVISORY_ROWS).await, 0, "an ineligible choice is never recorded or advised");
    assert_eq!(count(&store, "SELECT count(*) FROM actions").await, 0, "the advisor never dispatches");
}

#[tokio::test]
async fn completion_label_cannot_replace_verifier_receipt() {
    let project = ProjectId::new();
    let policy = project_policy(project, 1, false, &["local"], "free_only", false);
    let (_tmp, store, project) = setup(&policy).await;
    qualify(&store, JudgmentDomain::HandoffCompleteness, "model-a").await;

    // A live run that nothing has verified yet.
    let (run, job, node) = (RunId::new(), JobId::new(), NodeId::new());
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','running')",
                rusqlite::params![job.to_string(), project.to_string()],
            )?;
            tx.execute(
                "INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,'{}','running')",
                rusqlite::params![node.to_string(), job.to_string()],
            )?;
            tx.execute(
                "INSERT INTO runs(id,node_id,state,body) VALUES(?1,?2,'running','{}')",
                rusqlite::params![run.to_string(), node.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let input = judgment_input("handoff_completeness", &["complete", "incomplete"]);
    let eligible = vec!["complete".to_string(), "incomplete".to_string()];
    // The local judgment declares the handoff complete — a "work is done" label.
    record_judgment(&store, &judgment(&input, "model-a", JudgmentState::Available, Some("complete"))).await;

    let advisor = JudgmentAdvisor::new(store.clone(), project).unwrap();
    let outcome = advisor.advise(input.clone(), eligible.clone(), &policy).await.unwrap();
    let decision_id = match &outcome {
        AdvisoryOutcome::QualifiedChoice { choice, decision_id } => {
            assert_eq!(choice, "complete");
            *decision_id
        }
        other => panic!("expected advisory advice, got {other:?}"),
    };

    // The grant is advice only: a completion label is not a verifier receipt.
    let completion_label_produced_receipt = count(&store, "SELECT count(*) FROM checkpoints").await;
    assert_eq!(completion_label_produced_receipt, 0, "a completion label never becomes a verifier receipt");
    assert_eq!(count(&store, "SELECT count(*) FROM actions").await, 0);
    let state: String = store
        .with_connection(move |conn| {
            conn.query_row("SELECT state FROM runs WHERE id=?1", [run.to_string()], |row| row.get(0)).map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(state, "running", "a completion label does not advance the run");

    // The granted advice is recorded durably under a deterministic decision id.
    assert_eq!(count(&store, ADVISORY_ROWS).await, 1, "the granted advice is a durable record");
    let replayed = advisor.advise(input, eligible, &policy).await.unwrap();
    assert_eq!(
        replayed,
        AdvisoryOutcome::QualifiedChoice { choice: "complete".into(), decision_id },
        "re-advising the same input/policy/choice is idempotent"
    );
    assert_eq!(count(&store, ADVISORY_ROWS).await, 1, "the replay upserts the same row");
}

#[tokio::test]
async fn invalid_and_abstain_do_not_dispatch_mutation() {
    let project = ProjectId::new();
    let policy = project_policy(project, 1, false, &["local"], "free_only", false);
    let (_tmp, store, project) = setup(&policy).await;
    qualify(&store, JudgmentDomain::TaskIntent, "model-a").await;
    let advisor = JudgmentAdvisor::new(store.clone(), project).unwrap();

    let invalid_input = judgment_input("task_intent", &["research", "code"]);
    let eligible = vec!["research".to_string(), "code".to_string()];
    record_judgment(&store, &judgment(&invalid_input, "model-a", JudgmentState::Invalid, None)).await;
    let invalid = advisor.advise(invalid_input, eligible.clone(), &policy).await.unwrap();
    assert_eq!(invalid, AdvisoryOutcome::NeedsPlanner { reason: "judgment_invalid".into(), decision_id: None });

    let abstain_input = judgment_input("task_intent", &["research", "code"]);
    record_judgment(&store, &judgment(&abstain_input, "model-a", JudgmentState::Abstain, None)).await;
    let abstain = advisor.advise(abstain_input, eligible, &policy).await.unwrap();
    assert_eq!(abstain, AdvisoryOutcome::NeedsPlanner { reason: "judgment_abstained".into(), decision_id: None });

    // Neither state dispatched a mutation or produced a recorded decision.
    let invalid_judgment_mutation_count =
        count(&store, "SELECT count(*) FROM actions").await + count(&store, "SELECT count(*) FROM action_outcomes").await;
    assert_eq!(invalid_judgment_mutation_count, 0);
    assert_eq!(count(&store, ADVISORY_ROWS).await, 0);
}

#[tokio::test]
async fn retrieved_instruction_cannot_grant_scope() {
    let project = ProjectId::new();
    // The committed policy allows only the canonical local connection.
    let policy = project_policy(project, 1, false, &["local"], "paid_allowed", true);
    let (_tmp, store, project) = setup(&policy).await;
    qualify(&store, JudgmentDomain::AllowedRouteChoice, "model-a").await;

    // A retrieved instruction tries to name a connection outside the policy scope.
    let input = judgment_input("allowed_route_choice", &["local", "cloud-paid"]);
    let eligible = vec!["local".to_string(), "cloud-paid".to_string()];
    record_judgment(&store, &judgment(&input, "model-a", JudgmentState::Available, Some("cloud-paid"))).await;

    let advisor = JudgmentAdvisor::new(store.clone(), project).unwrap();
    let outcome = advisor.advise(input, eligible, &policy).await.unwrap();

    assert_eq!(outcome, AdvisoryOutcome::NeedsPlanner { reason: "choice_not_policy_allowed".into(), decision_id: None });
    // The instruction granted no scope: no grant, action or recorded advice exists.
    assert_eq!(count(&store, "SELECT count(*) FROM grants").await, 0);
    assert_eq!(count(&store, "SELECT count(*) FROM actions").await, 0);
    assert_eq!(count(&store, ADVISORY_ROWS).await, 0);
}

#[tokio::test]
async fn outage_does_not_call_paid_judge() {
    let project = ProjectId::new();
    let policy = project_policy(project, 1, false, &["local"], "free_only", false);
    let (_tmp, store, project) = setup(&policy).await;
    qualify(&store, JudgmentDomain::TaskIntent, "model-a").await;

    // No local judgment was ever recorded: the local judging service is out.
    let input = judgment_input("task_intent", &["research", "code"]);
    let eligible = vec!["research".to_string(), "code".to_string()];
    let judgments_before = count(&store, "SELECT count(*) FROM judgments").await;

    let advisor = JudgmentAdvisor::new(store.clone(), project).unwrap();
    let outcome = advisor.advise(input, eligible, &policy).await.unwrap();
    assert_eq!(outcome, AdvisoryOutcome::Unavailable, "an outage is an honest unavailability");

    // No paid judge was consulted as a fallback.
    let paid_judge_requests = count(&store, "SELECT count(*) FROM requests").await
        + count(&store, "SELECT count(*) FROM judgments WHERE model_identity LIKE '%paid%'").await;
    assert_eq!(paid_judge_requests, 0, "an outage never falls back to a paid judge");
    assert_eq!(count(&store, "SELECT count(*) FROM judgments").await, judgments_before, "no advisory row is written on an outage");
    assert_eq!(count(&store, "SELECT count(*) FROM actions").await, 0);
}

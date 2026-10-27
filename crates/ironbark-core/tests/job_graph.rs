//! H6 job-graph invariants tested against real SQLite and the real scheduler.
//!
//! - `cycle_rejected_before_dispatch`: a cyclic graph is refused by validation
//!   before a single `jobs`/`nodes`/`runs` row exists.
//! - `failed_dependency_blocks_consumers_not_siblings`: a consumer of a failed
//!   node is durably failed; a sibling of the healthy branch still runs.
//! - `max_two_workers_queues_third`: the concurrency ceiling admits two workers
//!   and leaves the third run `queued`.
//! - `reviewer_cannot_write`: a read-only node resolves to a read-only lease
//!   and can never be handed a writer lease.
//! - `skill_or_mcp_cannot_expand_policy`: a node that allows a route outside the
//!   policy is rejected; scoped tools/skills cannot widen policy.
//! - `user_pin_is_not_semantic_preference`: a route pin is only legal inside the
//!   policy and the node's own allowed routes; it never changes them.
use std::collections::HashMap;

use ironbark_core::contracts::{
    ArtifactId, CheckReceipt, Citation, CitationLocator, CitedExcerpt, ContextPackage, Digest,
    DisclosurePolicy, HarnessKind, JobGraph, JobId, JobNode, NodeId, ProjectId, ProjectPolicy,
    ResultManifest, RoutePin, RunId, RunState, SessionRef, WorkspaceMode,
};
use ironbark_core::policy::{TaskAuthority, TaskScope};
use ironbark_core::scheduler::{
    build_handoff, Budgets, GraphValidator, HandoffRequest, Scheduler, DEFAULT_MAX_WORKERS,
};
use ironbark_core::store::{events::now_ms, Store};
use serde_json::json;
use tempfile::tempdir;

const POLICY_VERSION: u64 = 1;

fn hex(seed: &str) -> Digest {
    Digest::parse(&seed.repeat(64)).unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    project: ProjectId,
    parent_run: RunId,
    policy: ProjectPolicy,
}

fn policy_value(project: ProjectId, max_workers: u16, max_requests: u32, max_cost_microusd: Option<u64>, allow_unknown_cost: bool) -> ProjectPolicy {
    serde_json::from_value(json!({
        "version": POLICY_VERSION.to_string(),
        "project_id": project,
        "allowed_connection_ids": ["local", "local-alt"],
        "allowed_tools": ["file_write", "integration", "file_read"],
        "read_roots": [],
        "write_roots": [],
        "network_destinations": [],
        "local_only": true,
        "max_workers": max_workers,
        "max_requests": max_requests,
        "max_cost_microusd": max_cost_microusd.map(|value| value.to_string()),
        "spend_policy": "paid_allowed",
        "allow_unknown_cost": allow_unknown_cost,
    }))
    .unwrap()
}

/// A project, its version-1 policy and one completed parent run the task
/// authority names (a terminal parent never occupies a worker slot or the
/// queued set under test).
async fn setup(max_workers: u16, max_requests: u32, max_cost_microusd: Option<u64>, allow_unknown_cost: bool) -> Fixture {
    let dir = tempdir().unwrap();
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let project = ProjectId::new();
    let parent_run = RunId::new();
    let policy = policy_value(project, max_workers, max_requests, max_cost_microusd, allow_unknown_cost);
    let policy_body = serde_json::to_string(&policy).unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let job = JobId::new().to_string();
    let node = NodeId::new().to_string();
    let (project_str, run_str) = (project.to_string(), parent_run.to_string());
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects(id,name,root,created_at) VALUES(?1,'fixture',?2,1)",
                rusqlite::params![project_str, root],
            )?;
            tx.execute(
                "INSERT INTO policies(project_id,version,body) VALUES(?1,?2,?3)",
                rusqlite::params![project_str, POLICY_VERSION as i64, policy_body],
            )?;
            tx.execute(
                "INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![job, project_str],
            )?;
            tx.execute(
                "INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![node, job],
            )?;
            tx.execute(
                "INSERT INTO runs(id,node_id,state,body) VALUES(?1,?2,'succeeded','{}')",
                rusqlite::params![run_str, node],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    Fixture { _dir: dir, store, project, parent_run, policy }
}

fn authority(fixture: &Fixture) -> TaskAuthority {
    TaskAuthority {
        run_id: fixture.parent_run,
        project_id: fixture.project,
        owner_approval_id: ironbark_core::contracts::GrantId::new(),
        scope: TaskScope {
            read_roots: fixture.policy.read_roots.clone(),
            write_roots: fixture.policy.write_roots.clone(),
            tools: fixture.policy.allowed_tools.clone(),
            connection_ids: fixture.policy.allowed_connection_ids.clone(),
            network_destinations: fixture.policy.network_destinations.clone(),
        },
        policy_version: POLICY_VERSION,
        expires_at: now_ms() + 3_600_000,
    }
}

fn node(id: NodeId, role: &str, dependencies: &[NodeId], allowed_routes: &[&str], mode: WorkspaceMode) -> JobNode {
    JobNode {
        id,
        role_id: role.to_string(),
        dependencies: dependencies.to_vec(),
        route_pin: None,
        allowed_routes: allowed_routes.iter().map(|route| (*route).to_string()).collect(),
        inputs: vec![],
        acceptance: vec!["review".into()],
        workspace_mode: mode,
    }
}

fn graph(project: ProjectId, nodes: Vec<JobNode>) -> JobGraph {
    JobGraph { id: JobId::new(), project_id: project, version: 1, nodes }
}

async fn count(store: &Store, sql: &str) -> i64 {
    let sql = sql.to_string();
    store
        .with_connection(move |conn| conn.query_row(&sql, [], |row| row.get::<_, i64>(0)).map_err(Into::into))
        .await
        .unwrap()
}

async fn run_state(store: &Store, run: RunId) -> RunState {
    let raw: String = store
        .with_connection(move |conn| {
            conn.query_row("SELECT state FROM runs WHERE id=?1", [run.to_string()], |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    serde_json::from_value(json!(raw)).unwrap()
}

async fn run_body(store: &Store, run: RunId) -> serde_json::Value {
    let body: String = store
        .with_connection(move |conn| {
            conn.query_row("SELECT body FROM runs WHERE id=?1", [run.to_string()], |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    serde_json::from_str(&body).unwrap()
}

async fn set_run_state(store: &Store, run: RunId, next: RunState) {
    let raw = serde_json::to_value(next).unwrap().as_str().unwrap().to_string();
    store
        .with_transaction(move |tx| {
            tx.execute("UPDATE runs SET state=?2 WHERE id=?1", rusqlite::params![run.to_string(), raw])?;
            Ok(())
        })
        .await
        .unwrap();
}

/// node id -> run id for the runs of one job.
async fn runs_of_job(store: &Store, job: JobId) -> HashMap<String, RunId> {
    store
        .with_connection(move |conn| {
            let mut stmt = conn.prepare("SELECT n.id, r.id FROM runs r JOIN nodes n ON n.id=r.node_id WHERE n.job_id=?1")?;
            let rows = stmt.query_map([job.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut map = HashMap::new();
            for row in rows {
                let (node, run) = row?;
                map.insert(node, RunId(uuid::Uuid::parse_str(&run).unwrap()));
            }
            Ok(map)
        })
        .await
        .unwrap()
}

async fn running_workers(store: &Store, project: ProjectId) -> i64 {
    let project = project.to_string();
    store
        .with_connection(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE j.project_id=?1 AND r.state IN ('ready','running','awaiting_approval','verifying','reviewing')",
                [project],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn cycle_rejected_before_dispatch() {
    let f = setup(2, 16, None, true).await;
    let before = (
        count(&f.store, "SELECT COUNT(*) FROM jobs").await,
        count(&f.store, "SELECT COUNT(*) FROM nodes").await,
        count(&f.store, "SELECT COUNT(*) FROM runs").await,
    );
    let a = NodeId::new();
    let b = NodeId::new();
    let proposed = graph(
        f.project,
        vec![
            node(a, "implementer", &[b], &["local"], WorkspaceMode::Writer),
            node(b, "reviewer", &[a], &["local"], WorkspaceMode::ReadOnly),
        ],
    );

    let error = GraphValidator::builtin().validate(&proposed, &f.policy).unwrap_err();
    assert_eq!(error.code, "graph_cycle");

    // Validation is the gate: no job/node/run row was created, so nothing exists
    // that could ever be dispatched.
    let cycle_dispatch_count = count(&f.store, "SELECT COUNT(*) FROM runs").await - before.2;
    assert_eq!(cycle_dispatch_count, 0);
    assert_eq!(count(&f.store, "SELECT COUNT(*) FROM jobs").await, before.0);
    assert_eq!(count(&f.store, "SELECT COUNT(*) FROM nodes").await, before.1);
}

#[tokio::test]
async fn failed_dependency_blocks_consumers_not_siblings() {
    let f = setup(2, 16, None, true).await;
    let a = NodeId::new();
    let b = NodeId::new();
    let c = NodeId::new();
    let d = NodeId::new();
    let proposed = graph(
        f.project,
        vec![
            node(a, "implementer", &[], &["local"], WorkspaceMode::Writer),
            node(b, "reviewer", &[a], &["local"], WorkspaceMode::ReadOnly),
            node(c, "researcher", &[a], &["local"], WorkspaceMode::ReadOnly),
            node(d, "implementer", &[b], &["local"], WorkspaceMode::Writer),
        ],
    );
    let validated = GraphValidator::builtin().validate(&proposed, &f.policy).unwrap();
    let job = Scheduler::submit(&f.store, &validated, &authority(&f)).await.unwrap();
    let runs = runs_of_job(&f.store, job).await;
    let (ra, rb, rc, rd) = (runs[&a.to_string()], runs[&b.to_string()], runs[&c.to_string()], runs[&d.to_string()]);

    let budgets = Budgets::new(f.store.clone(), f.project);
    let order = vec![ra, rb, rc, rd];
    let first = budgets.admit(&f.policy, &order).await.unwrap();
    assert_eq!(first.admitted, vec![ra], "only the root has its dependency satisfied");
    assert!(first.queued.contains(&rb) && first.queued.contains(&rc) && first.queued.contains(&rd));

    // A succeeded; the reviewer on that branch failed.
    set_run_state(&f.store, ra, RunState::Succeeded).await;
    set_run_state(&f.store, rb, RunState::Failed).await;

    let second = budgets.admit(&f.policy, &order).await.unwrap();
    assert_eq!(second.admitted, vec![rc], "the healthy sibling still runs");
    assert_eq!(run_state(&f.store, rc).await, RunState::Ready);
    let blocked: Vec<RunId> = second.blocked.iter().map(|entry| entry.run_id).collect();
    assert_eq!(blocked, vec![rd], "the consumer of the failed branch is blocked");
    assert_eq!(run_state(&f.store, rd).await, RunState::Failed);
}

#[tokio::test]
async fn max_two_workers_queues_third() {
    let f = setup(2, 16, None, true).await;
    assert_eq!(f.policy.max_workers, DEFAULT_MAX_WORKERS);
    let n1 = NodeId::new();
    let n2 = NodeId::new();
    let n3 = NodeId::new();
    let proposed = graph(
        f.project,
        vec![
            node(n1, "implementer", &[], &["local"], WorkspaceMode::Writer),
            node(n2, "implementer", &[], &["local-alt"], WorkspaceMode::Writer),
            node(n3, "implementer", &[], &["local"], WorkspaceMode::Writer),
        ],
    );
    let validated = GraphValidator::builtin().validate(&proposed, &f.policy).unwrap();
    let job = Scheduler::submit(&f.store, &validated, &authority(&f)).await.unwrap();
    let runs = runs_of_job(&f.store, job).await;

    let budgets = Budgets::new(f.store.clone(), f.project);
    let order = vec![runs[&n1.to_string()], runs[&n2.to_string()], runs[&n3.to_string()]];
    let admission = budgets.admit(&f.policy, &order).await.unwrap();

    assert_eq!(admission.admitted.len(), 2);
    assert_eq!(admission.queued, vec![order[2]]);
    let workers = running_workers(&f.store, f.project).await;
    assert_eq!(workers, 2);
    let third_state = run_state(&f.store, order[2]).await;
    assert_eq!(third_state, RunState::Queued);
}

#[tokio::test]
async fn reviewer_cannot_write() {
    let f = setup(2, 16, None, true).await;
    let reviewer = NodeId::new();
    let implementer = NodeId::new();
    let reviewer_node = node(reviewer, "reviewer", &[], &["local"], WorkspaceMode::ReadOnly);
    let implementer_node = node(implementer, "implementer", &[], &["local"], WorkspaceMode::Writer);
    let proposed = graph(f.project, vec![reviewer_node.clone(), implementer_node.clone()]);
    let validated = GraphValidator::builtin().validate(&proposed, &f.policy).unwrap();
    let job = Scheduler::submit(&f.store, &validated, &authority(&f)).await.unwrap();
    let runs = runs_of_job(&f.store, job).await;

    // The declared mode is durable on the run.
    let recorded = run_body(&f.store, runs[&reviewer.to_string()]).await;
    assert_eq!(recorded["workspace_mode"], json!("read_only"));

    assert_eq!(Scheduler::lease_mode(&reviewer_node), WorkspaceMode::ReadOnly);
    assert_eq!(
        Scheduler::ensure_writer_allowed(&reviewer_node).unwrap_err().code,
        "readonly_node_cannot_write",
        "a read-only node is never handed a writer lease"
    );
    assert_eq!(Scheduler::lease_mode(&implementer_node), WorkspaceMode::Writer);
    assert!(Scheduler::ensure_writer_allowed(&implementer_node).is_ok());
}

#[tokio::test]
async fn skill_or_mcp_cannot_expand_policy() {
    let f = setup(2, 16, None, true).await;
    // A node that allows a route no policy connection covers (as an MCP server or
    // skill would propose) is rejected rather than silently widened.
    let widened = graph(
        f.project,
        vec![node(NodeId::new(), "implementer", &[], &["mcp-remote"], WorkspaceMode::Writer)],
    );
    let error = GraphValidator::builtin().validate(&widened, &f.policy).unwrap_err();
    assert_eq!(error.code, "route_not_allowed");
    assert_eq!(count(&f.store, "SELECT COUNT(*) FROM runs").await, 1, "only the fixture parent run exists; no node was dispatched");

    // The same node restricted to the policy's own route is fine.
    let scoped = graph(
        f.project,
        vec![node(NodeId::new(), "implementer", &[], &["local"], WorkspaceMode::Writer)],
    );
    assert!(GraphValidator::builtin().validate(&scoped, &f.policy).is_ok());
}

#[tokio::test]
async fn user_pin_is_not_semantic_preference() {
    let f = setup(2, 16, None, true).await;
    let id = NodeId::new();
    let mut pinned = node(id, "implementer", &[], &["local"], WorkspaceMode::Writer);
    pinned.route_pin = Some(RoutePin { connection_id: "local".into(), model_id: "fixture".into(), harness: Some(HarnessKind::Omp) });
    let validated = GraphValidator::builtin().validate(&graph(f.project, vec![pinned.clone()]), &f.policy).unwrap();
    let resolved = validated.node(id).unwrap();
    assert_eq!(resolved.allowed_routes, vec!["local".to_string()], "a pin never adds or removes routes");
    assert_eq!(resolved.route_pin.as_ref().unwrap().connection_id, "local");

    // A pin outside the policy cannot expand it.
    let mut foreign = node(NodeId::new(), "implementer", &[], &["local"], WorkspaceMode::Writer);
    foreign.route_pin = Some(RoutePin { connection_id: "evil-remote".into(), model_id: "fixture".into(), harness: None });
    assert_eq!(
        GraphValidator::builtin().validate(&graph(f.project, vec![foreign]), &f.policy).unwrap_err().code,
        "pin_not_allowed"
    );

    // A pin outside the node's own allowed routes is refused too.
    let mut mismatch = node(NodeId::new(), "implementer", &[], &["local-alt"], WorkspaceMode::Writer);
    mismatch.route_pin = Some(RoutePin { connection_id: "local".into(), model_id: "fixture".into(), harness: None });
    assert_eq!(
        GraphValidator::builtin().validate(&graph(f.project, vec![mismatch]), &f.policy).unwrap_err().code,
        "pin_not_allowed"
    );
}

#[tokio::test]
async fn unknown_route_cost_requires_allow_unknown_cost() {
    let f = setup(2, 16, Some(1_000), false).await;
    let id = NodeId::new();
    let proposed = graph(f.project, vec![node(id, "implementer", &[], &["local"], WorkspaceMode::Writer)]);
    let validated = GraphValidator::builtin().validate(&proposed, &f.policy).unwrap();
    let job = Scheduler::submit(&f.store, &validated, &authority(&f)).await.unwrap();
    let run = runs_of_job(&f.store, job).await[&id.to_string()];

    let budgets = Budgets::new(f.store.clone(), f.project);
    let admission = budgets.admit(&f.policy, &[run]).await.unwrap();
    assert!(admission.admitted.is_empty());
    assert_eq!(admission.blocked.len(), 1);
    assert_eq!(admission.blocked[0].reason, "unknown_cost_not_allowed");
    assert_eq!(run_state(&f.store, run).await, RunState::Queued);

    // The same route is admitted once the policy tolerates unknown cost.
    let mut permissive = f.policy.clone();
    permissive.allow_unknown_cost = true;
    let admission = budgets.admit(&permissive, &[run]).await.unwrap();
    assert_eq!(admission.admitted, vec![run]);
}

#[tokio::test]
async fn request_budget_gates_admission() {
    // The fixture parent run already consumed one request, so a budget of two
    // admits exactly one more node.
    let f = setup(4, 2, None, true).await;
    let n1 = NodeId::new();
    let n2 = NodeId::new();
    let proposed = graph(
        f.project,
        vec![
            node(n1, "implementer", &[], &["local"], WorkspaceMode::Writer),
            node(n2, "implementer", &[], &["local-alt"], WorkspaceMode::Writer),
        ],
    );
    let validated = GraphValidator::builtin().validate(&proposed, &f.policy).unwrap();
    let job = Scheduler::submit(&f.store, &validated, &authority(&f)).await.unwrap();
    let runs = runs_of_job(&f.store, job).await;
    let budgets = Budgets::new(f.store.clone(), f.project);
    let admission = budgets.admit(&f.policy, &[runs[&n1.to_string()], runs[&n2.to_string()]]).await.unwrap();
    assert_eq!(admission.admitted.len(), 1);
    assert_eq!(admission.blocked[0].reason, "request_budget_exhausted");
}

#[tokio::test]
async fn submission_refuses_expired_or_foreign_authority() {
    let f = setup(2, 16, None, true).await;
    let proposed = graph(
        f.project,
        vec![node(NodeId::new(), "implementer", &[], &["local"], WorkspaceMode::Writer)],
    );
    let validated = GraphValidator::builtin().validate(&proposed, &f.policy).unwrap();

    let mut expired = authority(&f);
    expired.expires_at = now_ms() - 1;
    assert_eq!(
        Scheduler::submit(&f.store, &validated, &expired).await.unwrap_err().code,
        "authority_expired"
    );

    let mut foreign = authority(&f);
    foreign.project_id = ProjectId::new();
    assert_eq!(
        Scheduler::submit(&f.store, &validated, &foreign).await.unwrap_err().code,
        "authority_mismatch"
    );

    let mut unknown_run = authority(&f);
    unknown_run.run_id = RunId::new();
    assert_eq!(
        Scheduler::submit(&f.store, &validated, &unknown_run).await.unwrap_err().code,
        "authority_run_unknown"
    );
    assert_eq!(count(&f.store, "SELECT COUNT(*) FROM runs").await, 1, "only the fixture parent run exists");

    // The scope must cover every route the graph allows.
    let mut narrow = authority(&f);
    narrow.scope.connection_ids = vec!["local".into()];
    let wide = graph(
        f.project,
        vec![node(NodeId::new(), "implementer", &[], &["local-alt"], WorkspaceMode::Writer)],
    );
    let wide = GraphValidator::builtin().validate(&wide, &f.policy).unwrap();
    assert_eq!(
        Scheduler::submit(&f.store, &wide, &narrow).await.unwrap_err().code,
        "authority_scope_insufficient"
    );
}

#[tokio::test]
async fn handoff_filters_disclosure_and_never_fabricates_a_session() {
    let f = setup(2, 16, None, true).await;
    let upstream = ResultManifest {
        run_id: RunId::new(),
        native_session: Some(SessionRef {
            harness: HarnessKind::Omp,
            native_id: "native-1".into(),
            native_version: "18.8.9".into(),
            project_id: f.project,
            cwd: "/tmp".into(),
            resolved_route: RoutePin { connection_id: "local".into(), model_id: "fixture".into(), harness: Some(HarnessKind::Omp) },
        }),
        artifacts: vec![ArtifactId::new()],
        checks: vec![CheckReceipt { name: "review".into(), passed: true, evidence_digest: Some(hex("c")), details: "ok".into() }],
        acceptance_satisfied: true,
        unresolved: vec!["does the diff compile?".into()],
    };
    let excerpt = |text: &str, disclosure: DisclosurePolicy| -> CitedExcerpt {
        CitedExcerpt {
            text: text.to_string(),
            citation: Citation {
                source_id: "src-1".into(),
                revision_digest: hex("a"),
                locator: CitationLocator {
                    page: None,
                    section: None,
                    line_start: Some(1),
                    line_end: Some(1),
                    sheet: None,
                    cell_range: None,
                    bounding_box: None,
                    time_start_ms: None,
                    time_end_ms: None,
                },
            },
            project_id: f.project,
            disclosure,
        }
    };
    let excerpts = vec![
        excerpt("local only", DisclosurePolicy::LocalOnly),
        excerpt("allowed to local", DisclosurePolicy::AllowedConnections { connection_ids: vec!["local".into()] }),
        excerpt("public", DisclosurePolicy::ExplicitPublic),
    ];

    let request = HandoffRequest {
        upstream_run: upstream.run_id,
        upstream: &upstream,
        excerpts: &excerpts,
        policy: &f.policy,
        route_connection_id: Some("local"),
    };
    let package: ContextPackage = build_handoff(&request).unwrap();
    assert_eq!(package.excerpts.len(), 2, "a LocalOnly excerpt is never handed to a route");
    assert!(package.excerpts.iter().all(|kept| kept.text != "local only"));
    assert!(package.artifacts.contains(&upstream.artifacts[0]));
    assert!(package.originating_sessions.is_empty(), "no native session is fabricated or forwarded");
    assert_eq!(package.unresolved_questions, upstream.unresolved);
    assert!(package.constraints.iter().any(|constraint| constraint.contains("review: pass")));

    // An in-core handoff may still see LocalOnly content; a route-bound excerpt
    // is only visible to its named connection.
    let in_core = HandoffRequest { route_connection_id: None, ..request };
    let package = build_handoff(&in_core).unwrap();
    assert_eq!(package.excerpts.len(), 2);
    assert!(package.excerpts.iter().any(|kept| kept.text == "local only"));
}

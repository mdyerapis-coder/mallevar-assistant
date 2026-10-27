//! H6 role authority boundary: a role/skill/MCP narrows the policy, it never
//! widens it. Every denial is typed and every activation/exclusion is durable.

use ironbark_core::contracts::{
    ActionSpec, CapabilityObservation, CapabilityState, ConnectionConfig, Digest, ProjectPolicy, ProjectId, RoutePin, RunId,
};
use ironbark_core::knowledge::{chunks, extractors::ExtractedUnit, locators};
use ironbark_core::policy::{PolicyDecision, PolicyEngine, PolicyError, TaskAuthority};
use ironbark_core::roles::activation::resolve;
use ironbark_core::roles::registry::RoleRegistry;
use ironbark_core::store::Store;
use ironbark_core::tools::mcp::{McpApprovalBehaviour, McpAuth, McpConnector, McpConnectorDescriptor, McpToolDeclaration, McpTransport};
use ironbark_core::tools::registry::ToolRegistry;
use serde_json::{json, Value};
use uuid::Uuid;

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

/// A far-future expiry: the tool registry checks expiry against the real clock.
const FAR: i64 = 4_000_000_000_000;

async fn base() -> (Store, tempfile::TempDir, ProjectId) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("state.sqlite")).await.unwrap();
    let project = ProjectId::new();
    let root = temp.path().to_string_lossy().into_owned();
    let project_str = project.to_string();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects(id,name,root,created_at) VALUES(?1,'test',?2,0)",
                rusqlite::params![project_str, root],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (store, temp, project)
}

fn policy_value(project: ProjectId, tools: &[&str], root: &str, connections: &[&str], local_only: bool) -> ProjectPolicy {
    parse(json!({
        "version": "1", "project_id": project,
        "allowed_connection_ids": connections, "allowed_tools": tools,
        "read_roots": [root], "write_roots": [root],
        "network_destinations": [],
        "local_only": local_only, "max_workers": 2, "max_requests": 10,
        "max_cost_microusd": null, "spend_policy": "free_only", "allow_unknown_cost": false
    }))
}

async fn put_policy(store: &Store, policy: &ProjectPolicy) {
    let body = serde_json::to_string(policy).unwrap();
    let project = policy.project_id;
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO policies(project_id,version,body) VALUES(?1,1,?2)",
                rusqlite::params![project.to_string(), body],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn put_run(store: &Store, project: ProjectId, role_id: &str, route: Option<RoutePin>) -> RunId {
    let job = Uuid::new_v4().to_string();
    let node = Uuid::new_v4().to_string();
    let run = RunId::new();
    let node_body = json!({
        "id": node, "role_id": role_id, "dependencies": [], "route_pin": route,
        "allowed_routes": [], "inputs": [], "acceptance": [], "workspace_mode": "writer"
    })
    .to_string();
    let run_str = run.to_string();
    store
        .with_transaction(move |tx| {
            tx.execute("INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','queued')", rusqlite::params![job, project.to_string()])?;
            tx.execute("INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,?3,'queued')", rusqlite::params![node, job, node_body])?;
            tx.execute("INSERT INTO runs(id,node_id,body,state) VALUES(?1,?2,'{}','queued')", rusqlite::params![run_str, node])?;
            Ok(())
        })
        .await
        .unwrap();
    run
}

fn role_definition(id: &str, tool_scope: &[&str], source_scope: &[&str], workers: u16, requests: u32) -> ironbark_core::contracts::RoleDefinition {
    parse(json!({
        "id": id, "instructions": "do the scoped work",
        "required_capabilities": [], "allowed_harnesses": ["omp"],
        "allowed_routes": [], "source_scope": source_scope, "tool_scope": tool_scope,
        "delegated_limits": { "max_workers": workers, "max_requests": requests, "max_cost_microusd": null }
    }))
}

fn authority(run: RunId, project: ProjectId, tools: &[&str], root: &str, expires_at: i64) -> TaskAuthority {
    parse(json!({
        "run_id": run, "project_id": project, "owner_approval_id": Uuid::new_v4(),
        "scope": { "read_roots": [root], "write_roots": [root], "tools": tools, "connection_ids": [], "network_destinations": [] },
        // Wire contract: u64/i64 fields are canonical decimal STRINGS.
        "policy_version": "1", "expires_at": expires_at.to_string()
    }))
}

async fn receipt_count(store: &Store, project: ProjectId) -> usize {
    store
        .with_connection(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM import_receipts WHERE project_id=?1",
                rusqlite::params![project.to_string()],
                |row| row.get(0),
            )?;
            Ok(count as usize)
        })
        .await
        .unwrap()
}

// A role narrows the committed policy; a scope entry beyond `allowed_tools` is
// refused at activation and leaves no durable activation behind.
#[tokio::test]
async fn role_cannot_widen_policy() {
    let (store, temp, project) = base().await;
    let policy = policy_value(project, &["file_read"], temp.path().to_str().unwrap(), &[], false);
    put_policy(&store, &policy).await;
    let registry = RoleRegistry::new(store.clone());

    let widening = role_definition("implementer", &["file_write"], &[], 1, 4);
    let digest = registry.preview(project, &widening).await.unwrap();
    let error = registry.activate(widening, digest).await.err().unwrap();
    assert_eq!(error.code, "role_tool_beyond_policy");
    assert_eq!(receipt_count(&store, project).await, 0);
    assert!(registry.known_ids(project).await.unwrap().is_empty());

    // Positive control: a scope inside the policy activates and persists.
    let ok = role_definition("implementer", &["file_read"], &[], 1, 4);
    let digest = registry.preview(project, &ok).await.unwrap();
    assert_eq!(registry.activate(ok, digest).await.unwrap(), "implementer");
    assert_eq!(registry.known_ids(project).await.unwrap(), vec!["implementer".to_string()]);
    assert_eq!(receipt_count(&store, project).await, 1);
    assert!(RoleRegistry::builtin_ids().contains(&"verifier".to_string()));
}

// A changed definition cannot ride an old preview: activation is bound to the
// exact digest that was reviewed.
#[tokio::test]
async fn changed_definition_requires_a_fresh_preview() {
    let (store, temp, project) = base().await;
    let policy = policy_value(project, &["file_read"], temp.path().to_str().unwrap(), &[], false);
    put_policy(&store, &policy).await;
    let registry = RoleRegistry::new(store.clone());

    let original = role_definition("reviewer", &["file_read"], &[], 1, 2);
    let digest = registry.preview(project, &original).await.unwrap();
    let mutated = role_definition("reviewer", &["file_read"], &[], 1, 9);
    let error = registry.activate(mutated, digest).await.err().unwrap();
    assert_eq!(error.code, "role_definition_changed");
    assert_eq!(receipt_count(&store, project).await, 0);
}

// Delegated limits bound concurrency and requests, and can never exceed the
// policy ceilings they sit under.
#[tokio::test]
async fn delegated_limits_bound_concurrency_and_requests() {
    let (store, temp, project) = base().await;
    let policy = policy_value(project, &["file_read"], temp.path().to_str().unwrap(), &[], false);
    put_policy(&store, &policy).await;
    let registry = RoleRegistry::new(store.clone());

    let too_many_workers = role_definition("implementer", &["file_read"], &[], 5, 4);
    let digest = registry.preview(project, &too_many_workers).await.unwrap();
    assert_eq!(registry.activate(too_many_workers, digest).await.err().unwrap().code, "role_workers_beyond_policy");

    let too_many_requests = role_definition("implementer", &["file_read"], &[], 2, 99);
    let digest = registry.preview(project, &too_many_requests).await.unwrap();
    assert_eq!(registry.activate(too_many_requests, digest).await.err().unwrap().code, "role_requests_beyond_policy");

    // Within both ceilings, the role admits only up to its own delegated limit.
    let ok = role_definition("implementer", &["file_read"], &[], 1, 3);
    let digest = registry.preview(project, &ok).await.unwrap();
    registry.activate(ok, digest).await.unwrap();
    let active = resolve(&store, project, "implementer").await.unwrap();
    assert!(active.admit_workers(1).is_ok());
    assert_eq!(active.admit_workers(2).unwrap_err().code, "role_workers_exceeded");
    assert!(active.admit_requests(3).is_ok());
    assert_eq!(active.admit_requests(4).unwrap_err().code, "role_requests_exceeded");
}

// A skill/MCP entry declares tools; it cannot grant authority. An unlisted tool
// is refused by the policy engine, and a non-activated connector fails closed.
#[tokio::test]
async fn skill_or_mcp_cannot_grant_authority() {
    let (store, temp, project) = base().await;
    let root = temp.path().to_str().unwrap().to_owned();
    let policy = policy_value(project, &["exec"], &root, &[], false);
    put_policy(&store, &policy).await;
    let run = put_run(&store, project, "", None).await;

    let descriptor = McpConnectorDescriptor {
        id: "demo".into(),
        transport: McpTransport::LocalStdio { command: "/bin/echo".into(), args: vec![] },
        tools: vec![McpToolDeclaration {
            namespace: "demo".into(),
            name: "exec".into(),
            input_schema: json!({ "type": "object" }),
            auth: McpAuth::None,
            approval: McpApprovalBehaviour::RequireApproval,
        }],
    };
    let mut connector = McpConnector::new(descriptor.clone());
    // No invocation before explicit activation.
    assert_eq!(connector.invoke("demo/exec", json!({})).unwrap_err().code, "mcp_not_activated");
    connector.activate(Digest::parse(&"a".repeat(64)).unwrap(), 1_000).unwrap();
    // Failure-closed namespace/tool resolution.
    assert_eq!(connector.invoke("foreign/exec", json!({})).unwrap_err().code, "mcp_unknown_namespace");
    assert_eq!(connector.invoke("demo/nope", json!({})).unwrap_err().code, "mcp_unknown_tool");

    let invocation = connector.invoke("demo/exec", json!({})).unwrap();
    assert_eq!(invocation.tool.qualified_name(), "demo/exec");

    // An MCP-qualified name is never a policy tool: a typed denial, no authority.
    let qualified = invocation.action(run, project, 1, Digest::parse(&"b".repeat(64)).unwrap());
    assert_eq!(qualified.kind, "demo/exec");
    assert_eq!(PolicyEngine::check(&qualified, None, None, &policy, 1_000), PolicyDecision::Deny(PolicyError::ForbiddenTool));

    // The core kind the connector maps to is only permitted by a real task
    // authority that lists it. An empty authority scope is a ForbiddenTool.
    let bare = authority(run, project, &[], &root, 2_000);
    let action = ActionSpec {
        id: ironbark_core::contracts::ActionId::new(),
        run_id: run,
        project_id: project,
        kind: "exec".into(),
        canonical_args: json!({ "argv": ["true"], "cwd": root.clone() }),
        scope: json!({ "tools": ["exec"], "paths": [root.clone()] }),
        destination: None,
        policy_version: 1,
        expected_state_digest: Digest::parse(&"b".repeat(64)).unwrap(),
    };
    assert_eq!(PolicyEngine::check(&action, Some(&bare), None, &policy, 1_000), PolicyDecision::Deny(PolicyError::ForbiddenTool));

    // Positive control: only a task authority that lists the tool permits it.
    let granting = authority(run, project, &["exec"], &root, 2_000);
    assert_eq!(PolicyEngine::check(&action, Some(&granting), None, &policy, 1_000), PolicyDecision::Permit);
}

// Source scope is enforced through disclosure filtering: a role sees only its
// own sources, and a LocalOnly source never reaches a cloud route.
#[tokio::test]
async fn source_scope_is_enforced_through_disclosure_filtering() {
    let (store, temp, project) = base().await;
    let root = temp.path().to_str().unwrap().to_owned();
    let policy = policy_value(project, &["file_read"], &root, &["cloud-1"], false);
    put_policy(&store, &policy).await;

    let source_a = Uuid::new_v4().to_string();
    let source_b = Uuid::new_v4().to_string();
    let rev_a = Uuid::new_v4().to_string();
    let rev_b = Uuid::new_v4().to_string();
    let local_only = chunks::disclosure_json(&parse(json!({ "type": "local_only" })));
    let public = chunks::disclosure_json(&parse(json!({ "type": "explicit_public" })));
    let project_for_insert = project;
    let (a, b) = (source_a.clone(), source_b.clone());
    store
        .with_transaction(move |tx| {
            chunks::ensure_fts(tx)?;
            for (source, revision, disclosure) in [
                (&a, &rev_a, &local_only),
                (&b, &rev_b, &public),
            ] {
                tx.execute("INSERT INTO sources(id,project_id,uri) VALUES(?1,?2,'file://x')", rusqlite::params![source, project_for_insert.to_string()])?;
                tx.execute(
                    "INSERT INTO source_revisions(id,source_id,digest,created_at) VALUES(?1,?2,?3,0)",
                    rusqlite::params![revision, source, "c".repeat(64)],
                )?;
                chunks::insert_chunk(tx, revision, &project_for_insert.to_string(), disclosure, &ExtractedUnit { locator_key: locators::locator_key(&locators::section("p")), content: "report body".into() })?;
            }
            Ok(())
        })
        .await
        .unwrap();

    let role = role_definition("reviewer", &["file_read"], &[source_a.as_str()], 1, 2);
    let preview = RoleRegistry::new(store.clone()).preview(project, &role).await.unwrap();
    RoleRegistry::new(store.clone()).activate(role, preview).await.unwrap();
    let active = resolve(&store, project, "reviewer").await.unwrap();

    let cloud = RoutePin { connection_id: "cloud-1".into(), model_id: "m".into(), harness: None };
    let local = RoutePin { connection_id: "local".into(), model_id: "m".into(), harness: None };

    let cloud_ranked = {
        let policy = policy.clone();
        let cloud = cloud.clone();
        store.with_connection(move |conn| chunks::rank_lexical(conn, project, &cloud, &policy, "report", 10)).await.unwrap()
    };
    let local_ranked = {
        let policy = policy.clone();
        let local = local.clone();
        store.with_connection(move |conn| chunks::rank_lexical(conn, project, &local, &policy, "report", 10)).await.unwrap()
    };

    // Disclosure filtering: the LocalOnly source never reaches the cloud route.
    let cloud_cites = chunks::cites_from_ranked(&cloud_ranked, project).unwrap();
    let cloud_sources: Vec<String> = cloud_cites.iter().map(|c| c.citation.source_id.clone()).collect();
    assert_eq!(cloud_sources, vec![source_b.clone()]);

    // Scope filtering: the out-of-scope public source is dropped even locally.
    let local_cites = chunks::cites_from_ranked(&local_ranked, project).unwrap();
    let mut local_sources: Vec<String> = local_cites.iter().map(|c| c.citation.source_id.clone()).collect();
    local_sources.sort();
    let mut expected = vec![source_a.clone(), source_b.clone()];
    expected.sort();
    assert_eq!(local_sources, expected);

    let scoped: Vec<String> = active.scope_excerpts(&local_cites).iter().map(|c| c.citation.source_id.clone()).collect();
    assert_eq!(scoped, vec![source_a.clone()]);
    assert!(active.scope_excerpts(&cloud_cites).is_empty());
    assert!(active.admits_source(&source_a));
    assert!(!active.admits_source(&source_b));

    // The rendered per-harness profile names the harness, allowlist and scope.
    let profile = active.render(ironbark_core::contracts::HarnessKind::Omp, &policy).unwrap();
    assert_eq!(profile.tool_allowlist, vec!["file_read".to_string()]);
    assert_eq!(profile.source_scope, vec![source_a.clone()]);
    assert!(active.render(ironbark_core::contracts::HarnessKind::Codex, &policy).is_err());
}

// An expired or foreign task authority is refused, and the tool registry never
// lists tools for one.
#[tokio::test]
async fn expired_and_foreign_authority_is_refused() {
    let (store, temp, project) = base().await;
    let root = temp.path().to_str().unwrap().to_owned();
    let policy = policy_value(project, &["exec"], &root, &[], false);
    put_policy(&store, &policy).await;
    let run = put_run(&store, project, "", None).await;

    let action = ActionSpec {
        id: ironbark_core::contracts::ActionId::new(),
        run_id: run,
        project_id: project,
        kind: "exec".into(),
        canonical_args: json!({ "argv": ["true"], "cwd": root.clone() }),
        scope: json!({ "tools": ["exec"], "paths": [root.clone()] }),
        destination: None,
        policy_version: 1,
        expected_state_digest: Digest::parse(&"b".repeat(64)).unwrap(),
    };

    let expired = authority(run, project, &["exec"], &root, 1_000);
    assert_eq!(PolicyEngine::check(&action, Some(&expired), None, &policy, 2_000), PolicyDecision::Deny(PolicyError::ExpiredAuthority));

    let foreign = authority(RunId::new(), project, &["exec"], &root, 2_000);
    assert_eq!(PolicyEngine::check(&action, Some(&foreign), None, &policy, 1_000), PolicyDecision::Deny(PolicyError::AuthorityMismatch));

    // The tool registry refuses the same authorities with typed denials.
    let registry = ToolRegistry::new(store.clone());
    let expired_run_authority = authority(run, project, &["exec"], &root, 1_000);
    assert_eq!(
        registry.list_for_task(run, &expired_run_authority, &policy).await.err().unwrap().code,
        "expired_authority"
    );
    let foreign_authority = authority(RunId::new(), project, &["exec"], &root, 2_000);
    assert_eq!(
        registry.list_for_task(run, &foreign_authority, &policy).await.err().unwrap().code,
        "authority_mismatch"
    );
}

// Route capability gating: a tool needing an unavailable capability is excluded
// with a recorded reason, and listing never grants invocation.
#[tokio::test]
async fn tool_route_capability_excludes_with_recorded_reason() {
    let (store, temp, project) = base().await;
    let root = temp.path().to_str().unwrap().to_owned();
    let policy = policy_value(project, &["file_read", "vision_describe"], &root, &["cloud-1"], false);
    put_policy(&store, &policy).await;

    let route = RoutePin { connection_id: "cloud-1".into(), model_id: "m".into(), harness: None };
    let config = ConnectionConfig {
        id: "cloud-1".into(),
        protocol: ironbark_core::contracts::ProviderProtocol::OpenaiCompletions,
        endpoint: "https://allowed.example".into(),
        models: vec![ironbark_core::contracts::ModelDescriptor {
            id: "m".into(),
            context_limit: 4096,
            output_limit: 512,
            capabilities: vec![
                CapabilityObservation { name: "tools".into(), state: CapabilityState::Qualified, implementation_version: "1".into(), evidence_digest: None, observed_at: 0 },
                CapabilityObservation { name: "vision".into(), state: CapabilityState::Unavailable, implementation_version: "1".into(), evidence_digest: None, observed_at: 0 },
            ],
        }],
        auth_ref: None,
        allowed_projects: vec![project],
    };
    let config_body = serde_json::to_string(&config).unwrap();
    store
        .with_transaction(move |tx| {
            tx.execute("INSERT INTO provider_references(id,config) VALUES('cloud-1',?1)", rusqlite::params![config_body])?;
            Ok(())
        })
        .await
        .unwrap();

    let run = put_run(&store, project, "implementer", Some(route)).await;
    let role = role_definition("implementer", &["file_read", "vision_describe"], &[], 2, 8);
    let preview = RoleRegistry::new(store.clone()).preview(project, &role).await.unwrap();
    RoleRegistry::new(store.clone()).activate(role, preview).await.unwrap();

    let grant = authority(run, project, &["file_read", "vision_describe"], &root, FAR);
    let listing = ToolRegistry::new(store.clone()).list_for_task(run, &grant, &policy).await.unwrap();
    let available: Vec<String> = listing.available.iter().map(|d| d.name.clone()).collect();
    assert_eq!(available, vec!["file_read".to_string()]);
    assert_eq!(listing.excluded.len(), 1);
    assert_eq!(listing.excluded[0].name, "vision_describe");
    assert_eq!(listing.excluded[0].reason, "capability_unavailable:vision");

    // The exclusion reason is durable: an event records it.
    let recorded = store
        .with_connection(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM events WHERE payload LIKE '%tool_excluded%'",
                [],
                |row| row.get(0),
            )?;
            Ok(count)
        })
        .await
        .unwrap();
    assert_eq!(recorded, 1);

    // A named role that is not activated is a typed denial, never an unscoped list.
    let other_run = put_run(&store, project, "researcher", None).await;
    let other_grant = authority(other_run, project, &["file_read"], &root, FAR);
    assert_eq!(
        ToolRegistry::new(store.clone()).list_for_task(other_run, &other_grant, &policy).await.err().unwrap().code,
        "role_not_active"
    );
}

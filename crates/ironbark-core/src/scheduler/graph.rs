//! H6: deterministic DAG validation and durable submission.
//!
//! A planner (or the owner) proposes a `JobGraph`; nothing about it is trusted
//! until `GraphValidator::validate` has checked it against the project policy:
//! duplicate ids, unknown roles, routes (or pins) outside the policy, missing
//! dependencies, dependencies that can never succeed, cycles, and an absurd node
//! count are all rejected *before* any durable row exists. The validator surfaces
//! a deterministic topological order so submission is reproducible.
//!
//! `Scheduler::submit` is the only path that turns a validated graph into
//! durable `jobs`/`nodes`/`runs` rows. It refuses unless the caller's
//! `TaskAuthority` covers the project, matches the current policy version, names
//! a run that belongs to the project, and is unexpired; the authority's
//! `TaskScope` then bounds the routes (and therefore the credentials and
//! destinations) each node's run may use. Runs are born `queued`; only the
//! transition table in `state.rs` may move them afterwards.

use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::{params, OptionalExtension};

use crate::contracts::{JobGraph, JobId, JobNode, NodeId, ProjectId, ProjectPolicy, WorkspaceMode};
use crate::error::{CoreError, Result};
use crate::policy::TaskAuthority;
use crate::store::{
    events::{append_tx, now_ms, redacted_json},
    Store,
};

/// The core-owned role ids. User specialists are added by the caller (the role
/// registry's `known_ids`) when constructing a validator, so activation stays
/// owned by H6's role registry; a graph that names a role the validator was not
/// told about is rejected rather than silently planned.
pub const BUILTIN_ROLE_IDS: &[&str] = &["planner", "researcher", "implementer", "reviewer", "verifier"];

/// A validated, topologically ordered graph. Holding one is proof that every
/// structural and policy check passed; submission recomputes nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedGraph {
    pub job_id: JobId,
    pub project_id: ProjectId,
    pub version: u64,
    /// Deterministic topological order (ties broken by node id).
    pub order: Vec<NodeId>,
    /// The nodes, in `order`, so wave scheduling is reproducible.
    pub nodes: Vec<JobNode>,
    pub max_workers: u16,
}

impl ValidatedGraph {
    pub fn node(&self, id: NodeId) -> Option<&JobNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

/// Validates a proposed graph against a project policy. It carries the role ids
/// the caller knows to be active for the project (builtins ∪ activated roles).
#[derive(Debug, Clone, Default)]
pub struct GraphValidator {
    roles: BTreeSet<String>,
}

impl GraphValidator {
    /// Build a validator that recognises exactly `role_ids`.
    pub fn new(role_ids: impl IntoIterator<Item = String>) -> Self {
        Self { roles: role_ids.into_iter().collect() }
    }

    /// A validator that recognises only the core-owned builtin roles.
    pub fn builtin() -> Self {
        Self::new(BUILTIN_ROLE_IDS.iter().map(|id| (*id).to_string()))
    }

    /// The absolute number of nodes a policy may ever admit. Concurrency
    /// (`max_workers`) bounds how many run at once and the request budget bounds
    /// how many may ever run; their product rejects an absurd graph before a
    /// single durable row is created. A zero worker ceiling is read as the
    /// scheduler default (2).
    fn admission_limit(policy: &ProjectPolicy) -> usize {
        let workers = if policy.max_workers == 0 { super::budgets::DEFAULT_MAX_WORKERS as usize } else { usize::from(policy.max_workers) };
        let requests = usize::try_from(policy.max_requests.max(1)).unwrap_or(usize::MAX);
        workers.saturating_mul(requests)
    }

    pub fn validate(&self, graph: &JobGraph, policy: &ProjectPolicy) -> Result<ValidatedGraph> {
        if graph.project_id != policy.project_id {
            return Err(CoreError::new(
                "graph_project_mismatch",
                "The graph belongs to a different project than the policy it was validated against",
            ));
        }
        if graph.nodes.is_empty() {
            return Err(CoreError::new("empty_graph", "A job graph must declare at least one node"));
        }
        if graph.nodes.len() > Self::admission_limit(policy) {
            return Err(CoreError::new(
                "graph_too_large",
                "The graph declares more nodes than the policy's worker and request ceilings can ever admit",
            ));
        }

        // Ids, duplicates and role knowledge.
        let mut known: HashSet<NodeId> = HashSet::new();
        for node in &graph.nodes {
            if !known.insert(node.id) {
                return Err(CoreError::new("duplicate_node_id", "A job graph may not declare the same node id twice"));
            }
            if !self.roles.contains(&node.role_id) {
                return Err(CoreError::new(
                    "unknown_role_id",
                    "A node names a role that is not active for this project; activate the role before planning it",
                ));
            }
        }

        // Dependencies, routes and pins.
        for node in &graph.nodes {
            for dependency in &node.dependencies {
                if !known.contains(dependency) {
                    return Err(CoreError::new("unknown_dependency", "A node depends on a node id that is not in the graph"));
                }
                if *dependency == node.id {
                    return Err(CoreError::new("graph_cycle", "A node may not depend on itself"));
                }
            }
            for route in &node.allowed_routes {
                if !policy.allowed_connection_ids.iter().any(|allowed| allowed == route) {
                    // A skill, MCP server or any other scope source cannot widen
                    // the project policy: only the policy's connections are legal.
                    return Err(CoreError::new(
                        "route_not_allowed",
                        "A node allows a route that is not in the project policy; scoped tools and skills cannot expand policy",
                    ));
                }
            }
            if let Some(pin) = &node.route_pin {
                let pinned = &pin.connection_id;
                if !policy.allowed_connection_ids.iter().any(|allowed| allowed == pinned) {
                    return Err(CoreError::new(
                        "pin_not_allowed",
                        "A node pins a route that is not in the project policy; a user pin cannot expand policy",
                    ));
                }
                if !node.allowed_routes.is_empty()
                    && !node.allowed_routes.iter().any(|route| route == pinned)
                {
                    return Err(CoreError::new(
                        "pin_not_allowed",
                        "A node pins a route outside its own allowed routes; a pin never widens what the node may use",
                    ));
                }
            }
        }

        // A dependency that can never succeed would block its consumers forever.
        // A node with no permitted route (an empty node scope over an empty
        // policy connection set) is such a node: reject it now, deterministically.
        if policy.allowed_connection_ids.is_empty() {
            for node in &graph.nodes {
                if !node.dependencies.is_empty() {
                    return Err(CoreError::new(
                        "dependency_cannot_succeed",
                        "A dependency has no permitted route under the policy, so its consumers could never run",
                    ));
                }
            }
        }

        // Deterministic topological order (Kahn, smallest id first).
        let mut indegree: HashMap<NodeId, usize> = HashMap::new();
        let mut dependents: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for node in &graph.nodes {
            indegree.entry(node.id).or_insert(0);
            for dependency in &node.dependencies {
                *indegree.entry(node.id).or_insert(0) += 1;
                dependents.entry(*dependency).or_default().push(node.id);
            }
        }
        let by_id: HashMap<NodeId, &JobNode> = graph.nodes.iter().map(|node| (node.id, node)).collect();
        let mut ready: BTreeSet<String> = indegree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| id.to_string())
            .collect();
        let mut order: Vec<NodeId> = Vec::with_capacity(graph.nodes.len());
        while let Some(next) = ready.iter().next().cloned() {
            ready.remove(&next);
            let id = NodeId(uuid::Uuid::parse_str(&next).map_err(|_| CoreError::new("storage_corrupt", "Invalid node id"))?);
            order.push(id);
            if let Some(children) = dependents.get(&id) {
                // Sort so the ready set is offered in a stable id order.
                let mut children: Vec<NodeId> = children.clone();
                children.sort_by_key(|child| child.to_string());
                for child in children {
                    let degree = indegree.get_mut(&child).expect("known node");
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert(child.to_string());
                    }
                }
            }
        }
        if order.len() != graph.nodes.len() {
            return Err(CoreError::new("graph_cycle", "The job graph contains a dependency cycle and can never start"));
        }

        let nodes: Vec<JobNode> = order
            .iter()
            .map(|id| (*by_id.get(id).expect("ordered node is known")).clone())
            .collect();
        let max_workers = if policy.max_workers == 0 { super::budgets::DEFAULT_MAX_WORKERS } else { policy.max_workers };
        Ok(ValidatedGraph {
            job_id: graph.id,
            project_id: graph.project_id,
            version: graph.version,
            order,
            nodes,
            max_workers,
        })
    }
}

/// The scheduler namespace: durable submission of a validated graph.
pub struct Scheduler;

impl Scheduler {
    /// The only lease a node may hold is the one its declared workspace mode
    /// permits (also the durable `workspace_mode` recorded on the run).
    pub fn lease_mode(node: &JobNode) -> WorkspaceMode {
        node.workspace_mode
    }

    /// A reviewer/verifier node is read-only: it must never be handed a writer
    /// lease, so any caller that intends to write first asks here.
    pub fn ensure_writer_allowed(node: &JobNode) -> Result<()> {
        match node.workspace_mode {
            WorkspaceMode::Writer => Ok(()),
            WorkspaceMode::ReadOnly => Err(CoreError::new(
                "readonly_node_cannot_write",
                "A read-only node can never hold a writer lease",
            )),
        }
    }

    /// Create the durable `jobs`/`nodes`/`runs` chain for every node, in
    /// dependency order, refusing unless the authority covers the project, the
    /// current policy version and a real run in that project, and is unexpired.
    /// The authority's `TaskScope` bounds each run's routes, tools, roots and
    /// destinations. Runs are born `queued`.
    pub async fn submit(store: &Store, graph: &ValidatedGraph, authority: &TaskAuthority) -> Result<JobId> {
        if authority.project_id != graph.project_id {
            return Err(CoreError::new("authority_mismatch", "The task authority covers a different project than the graph"));
        }
        if authority.expires_at <= now_ms() {
            return Err(CoreError::new("authority_expired", "The task authority has expired; request fresh owner approval"));
        }
        for node in &graph.nodes {
            for route in &node.allowed_routes {
                if !authority.scope.connection_ids.iter().any(|allowed| allowed == route) {
                    return Err(CoreError::new(
                        "authority_scope_insufficient",
                        "The task authority does not cover a route the graph allows",
                    ));
                }
            }
            if let Some(pin) = &node.route_pin
                && !authority.scope.connection_ids.iter().any(|allowed| allowed == &pin.connection_id)
            {
                return Err(CoreError::new(
                    "authority_scope_insufficient",
                    "The task authority does not cover the route a node pins",
                ));
            }
        }

        let project = graph.project_id;
        let project_str = project.to_string();
        let job_id = graph.job_id;
        let job_str = job_id.to_string();
        let version = graph.version;
        let order: Vec<String> = graph.order.iter().map(|id| id.to_string()).collect();

        // Node/run rows are minted here; the dependency order is preserved in
        // `position` so a reader can re-derive the plan without the graph DTO.
        let mut node_rows: Vec<(String, String, String, String, String)> = Vec::with_capacity(graph.nodes.len());
        for (position, node) in graph.nodes.iter().enumerate() {
            let node_id = node.id;
            let run_id = crate::contracts::RunId::new();
            let node_body = redacted_json(&serde_json::json!({
                "version": 1,
                "position": position,
                "role_id": node.role_id.clone(),
                "dependencies": node.dependencies.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                "allowed_routes": node.allowed_routes.clone(),
                "inputs": node.inputs.clone(),
                "acceptance": node.acceptance.clone(),
                "workspace_mode": node.workspace_mode,
            }))?;
            let run_body = redacted_json(&serde_json::json!({
                "version": 1,
                "node_id": node_id.to_string(),
                "role_id": node.role_id.clone(),
                "workspace_mode": node.workspace_mode,
                "allowed_routes": node.allowed_routes.clone(),
                "route_cost_microusd": serde_json::Value::Null,
                // The bounded scope the node's run may act within.
                "scope": {
                    "tools": authority.scope.tools.clone(),
                    "read_roots": authority.scope.read_roots.clone(),
                    "write_roots": authority.scope.write_roots.clone(),
                    "network_destinations": authority.scope.network_destinations.clone(),
                },
            }))?;
            node_rows.push((node_id.to_string(), run_id.to_string(), node_body, run_body, position.to_string()));
        }
        let role_ids: Vec<String> = graph.nodes.iter().map(|node| node.role_id.clone()).collect();
        let job_body = redacted_json(&serde_json::json!({
            "version": version,
            "node_count": graph.nodes.len(),
            "order": order,
            "roles": role_ids,
        }))?;
        let expected_policy_version = authority.policy_version;
        let authority_run = authority.run_id.to_string();

        store
            .with_transaction(move |tx| {
                let exists: Option<String> = tx
                    .query_row("SELECT id FROM projects WHERE id=?1", [project_str.clone()], |row| row.get(0))
                    .optional()?;
                if exists.is_none() {
                    return Err(CoreError::new("project_missing", "Project does not exist; create it before submitting a graph"));
                }
                // The authority must name a real run in this project.
                let run_project: Option<String> = tx
                    .query_row(
                        "SELECT j.project_id FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE r.id=?1",
                        [authority_run.clone()],
                        |row| row.get(0),
                    )
                    .optional()?;
                match run_project {
                    None => return Err(CoreError::new("authority_run_unknown", "The task authority names a run that does not exist")),
                    Some(run_project) if run_project != project_str => {
                        return Err(CoreError::new("authority_mismatch", "The task authority names a run from another project"));
                    }
                    Some(_) => {}
                }
                // The authority must be bound to the policy version in force.
                let current: Option<i64> = tx
                    .query_row(
                        "SELECT version FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
                        [project_str.clone()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(current) = current
                    && u64::try_from(current).unwrap_or(u64::MAX) != expected_policy_version
                {
                    return Err(CoreError::new(
                        "policy_version_mismatch",
                        "The task authority was issued under a different policy version",
                    ));
                }
                tx.execute(
                    "INSERT INTO jobs (id,project_id,body,state) VALUES (?1,?2,?3,'queued')",
                    params![job_str, project_str, job_body],
                )?;
                for (node_str, run_str, node_body, run_body, _position) in node_rows {
                    tx.execute(
                        "INSERT INTO nodes (id,job_id,body,state) VALUES (?1,?2,?3,'queued')",
                        params![node_str, job_str, node_body],
                    )?;
                    tx.execute(
                        "INSERT INTO runs (id,node_id,state,body) VALUES (?1,?2,'queued',?3)",
                        params![run_str, node_str, run_body],
                    )?;
                    let run_id = crate::contracts::RunId(
                        uuid::Uuid::parse_str(&run_str).map_err(|_| CoreError::new("storage_corrupt", "Invalid run id"))?,
                    );
                    append_tx(
                        tx,
                        None,
                        Some(project),
                        Some(run_id),
                        &crate::contracts::CoreEvent::StateChanged {
                            entity_id: run_str,
                            state: "queued (graph_submitted)".into(),
                        },
                    )?;
                }
                Ok(())
            })
            .await?;
        Ok(job_id)
    }
}

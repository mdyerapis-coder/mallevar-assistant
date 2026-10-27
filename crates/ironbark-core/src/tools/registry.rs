//! Scoped tool listing.
//!
//! A listing is an intersection, never a grant: the tools surfaced for a task
//! are the intersection of the task authority's scope, the active role's scope
//! and the committed policy. Listing a tool here NEVER authorises invocation —
//! every invocation still passes the policy engine with its own task authority.
//! A tool whose `required_capabilities` are not available on the requested
//! route is excluded, and the exclusion reason is recorded durably as an event
//! so the omission is auditable rather than silent.

use rusqlite::{params, OptionalExtension};
use serde_json::json;

use crate::contracts::{Capability, CapabilityState, ConnectionConfig, JobNode, ProjectPolicy, RunId, ToolDescriptor};
use crate::error::{CoreError, Result};
use crate::policy::TaskAuthority;
use crate::roles::registry::RoleRegistry;
use crate::store::events::{append_tx, now_ms};
use crate::store::Store;

/// One core tool: its descriptor plus the route capabilities it needs to run.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub descriptor: ToolDescriptor,
    pub required_capabilities: Vec<Capability>,
}

/// A tool that was in scope but not surfaced, with the recorded reason.
#[derive(Debug, Clone, PartialEq)]
pub struct ExcludedTool {
    pub name: String,
    pub reason: String,
}

/// The full listing: what a task may list, and what was excluded and why.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolListing {
    pub available: Vec<ToolDescriptor>,
    pub excluded: Vec<ExcludedTool>,
}

/// Registry over the core tool catalog, scoped per task.
#[derive(Clone)]
pub struct ToolRegistry {
    store: Store,
}

impl ToolRegistry {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// The core tool catalog. Descriptors are stable; the catalog is small and
    /// built per call so `input_schema` stays plain JSON.
    pub fn catalog() -> Vec<CatalogEntry> {
        vec![
            entry("file_read", "Read a file inside the task's read roots", Capability::Tools),
            entry("file_write", "Write a file inside the task's write roots", Capability::Tools),
            entry("search", "Search the workspace for text", Capability::Tools),
            entry("publish", "Publish an artifact to an approved destination", Capability::Tools),
            entry("vision_describe", "Describe an image attachment", Capability::Vision),
            entry("transcribe", "Transcribe an audio attachment", Capability::Transcription),
            entry("embed", "Compute a local embedding", Capability::Embeddings),
        ]
    }

    /// The descriptors a task may list. Convenience over [`Self::list_for_task`]
    /// that drops the excluded set (the reasons are still recorded as events).
    pub async fn for_task(&self, run: RunId, authority: &TaskAuthority, policy: &ProjectPolicy) -> Result<Vec<ToolDescriptor>> {
        Ok(self.list_for_task(run, authority, policy).await?.available)
    }

    /// The scoped listing for a task, recording every exclusion as an event.
    pub async fn list_for_task(&self, run: RunId, authority: &TaskAuthority, policy: &ProjectPolicy) -> Result<ToolListing> {
        if authority.project_id != policy.project_id {
            return Err(CoreError::new("authority_mismatch", "the task authority does not belong to this project"));
        }
        if authority.run_id != run {
            return Err(CoreError::new("authority_mismatch", "the task authority does not belong to this run"));
        }
        if authority.policy_version != policy.version {
            return Err(CoreError::new("policy_version_mismatch", "the task authority binds a different policy version"));
        }
        if authority.expires_at <= now_ms() {
            return Err(CoreError::new("expired_authority", "the task authority has expired"));
        }

        // The node owns the role id and the requested route.
        let node = self.node_for_run(run).await?;
        let node = match node {
            Some(node) => node,
            None => return Err(CoreError::new("run_not_found", "the run or its node is missing")),
        };
        if node.project_id != authority.project_id {
            return Err(CoreError::new("authority_mismatch", "the run does not belong to the authority's project"));
        }

        // A role narrows the listing. A named role that is not activated is a
        // typed denial rather than an unscoped listing.
        let role_scope: Option<Vec<String>> = if node.node.role_id.is_empty() {
            None
        } else {
            let registry = RoleRegistry::new(self.store.clone());
            let active = registry
                .active(authority.project_id, &node.node.role_id)
                .await?
                .ok_or_else(|| CoreError::new("role_not_active", format!("role `{}` is not activated in this project", node.node.role_id)))?;
            Some(active.definition.tool_scope)
        };

        let route = node.node.route_pin.clone();
        let capabilities = self.route_capabilities(&route).await?;

        let mut available = Vec::new();
        let mut excluded = Vec::new();
        for catalog in Self::catalog() {
            let name = catalog.descriptor.name.clone();
            if !policy.allowed_tools.iter().any(|allowed| allowed == &name) {
                continue;
            }
            if !authority.scope.tools.iter().any(|allowed| allowed == &name) {
                continue;
            }
            if let Some(scope) = &role_scope
                && !scope.iter().any(|allowed| allowed == &name)
            {
                continue;
            }
            if let Some(missing) = missing_capability(&catalog.required_capabilities, &capabilities) {
                excluded.push(ExcludedTool { name, reason: format!("capability_unavailable:{missing}") });
            } else {
                available.push(catalog.descriptor);
            }
        }

        if !excluded.is_empty() {
            self.record_exclusions(run, authority.project_id, &excluded).await?;
        }
        Ok(ToolListing { available, excluded })
    }

    /// Load the run's node body and owning project.
    async fn node_for_run(&self, run: RunId) -> Result<Option<NodeContext>> {
        self.store
            .with_connection(move |conn| {
                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT n.body, j.project_id FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE r.id=?1",
                        params![run.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((body, project)) = row else {
                    return Ok(None);
                };
                let node: JobNode = serde_json::from_str(&body)
                    .map_err(|_| CoreError::new("storage_corrupt", "the run's node body is not a valid job node"))?;
                let project_id = uuid::Uuid::parse_str(&project)
                    .map(crate::contracts::ProjectId)
                    .map_err(|_| CoreError::new("storage_corrupt", "invalid persisted UUID"))?;
                Ok(Some(NodeContext { node, project_id }))
            })
            .await
    }

    /// The capabilities the requested route actually offers. An unknown route
    /// or a route with no committed model offers nothing, so any tool needing a
    /// capability is excluded rather than assumed available.
    async fn route_capabilities(&self, route: &Option<crate::contracts::RoutePin>) -> Result<Vec<String>> {
        let Some(route) = route else {
            return Ok(Vec::new());
        };
        let connection_id = route.connection_id.clone();
        let model_id = route.model_id.clone();
        self.store
            .with_connection(move |conn| {
                let config: Option<String> = conn
                    .query_row("SELECT config FROM provider_references WHERE id=?1", params![connection_id], |row| row.get(0))
                    .optional()?;
                let Some(config) = config else {
                    return Ok(Vec::new());
                };
                let Ok(config) = serde_json::from_str::<ConnectionConfig>(&config) else {
                    return Ok(Vec::new());
                };
                let Some(model) = config.models.iter().find(|model| model.id == model_id) else {
                    return Ok(Vec::new());
                };
                Ok(model
                    .capabilities
                    .iter()
                    .filter(|observation| observation.state == CapabilityState::Qualified)
                    .map(|observation| observation.name.clone())
                    .collect())
            })
            .await
    }

    async fn record_exclusions(&self, run: RunId, project: crate::contracts::ProjectId, excluded: &[ExcludedTool]) -> Result<()> {
        let events: Vec<crate::contracts::CoreEvent> = excluded
            .iter()
            .map(|tool| crate::contracts::CoreEvent::Error {
                code: "tool_excluded".into(),
                message: format!("tool `{}` was excluded from the task listing", tool.name),
                details: json!({ "name": tool.name, "reason": tool.reason }),
            })
            .collect();
        self.store
            .with_transaction(move |tx| {
                for event in &events {
                    append_tx(tx, None, Some(project), Some(run), event)?;
                }
                Ok(())
            })
            .await
    }
}

struct NodeContext {
    node: JobNode,
    project_id: crate::contracts::ProjectId,
}

fn entry(name: &str, description: &str, capability: Capability) -> CatalogEntry {
    CatalogEntry {
        descriptor: ToolDescriptor {
            name: name.into(),
            description: description.into(),
            input_schema: json!({ "type": "object" }),
        },
        required_capabilities: vec![capability],
    }
}

/// The first required capability the route does not offer, if any.
fn missing_capability(required: &[Capability], available: &[String]) -> Option<String> {
    for capability in required {
        let name = capability_name(*capability);
        if !available.iter().any(|offered| offered == &name) {
            return Some(name);
        }
    }
    None
}

fn capability_name(capability: Capability) -> String {
    serde_json::to_value(capability)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

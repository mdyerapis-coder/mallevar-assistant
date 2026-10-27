//! Resolving an activated role and rendering it for a harness.
//!
//! Rendering produces an OWNED profile payload: the harness name, the tool
//! allowlist and the source scope the role may use. It never reads or writes an
//! operator config file — the payload is returned to the caller, which is
//! responsible for any native schema-specific agent definition it generates.
//! The allowlist is the intersection of the role scope with the policy, so a
//! rendered profile can only ever be narrower than the committed policy.

use serde::{Deserialize, Serialize};

use crate::contracts::{
    Capability, CitedExcerpt, DelegatedLimits, HarnessKind, ProjectId, ProjectPolicy, RoleDefinition,
};
use crate::error::{CoreError, Result};
use crate::store::Store;

use super::registry::{ActiveDefinition, RoleRegistry};

/// An activated role resolved for a project: the definition plus the digest
/// and instant that activation committed. Holding it is not authority; every
/// action still passes the policy engine with its own task authority.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRole {
    pub role_id: String,
    pub project_id: ProjectId,
    pub definition: RoleDefinition,
    pub definition_digest: crate::contracts::Digest,
    pub activated_at: i64,
}

impl From<ActiveDefinition> for ActiveRole {
    fn from(active: ActiveDefinition) -> Self {
        Self {
            role_id: active.role_id,
            project_id: active.project_id,
            definition: active.definition,
            definition_digest: active.definition_digest,
            activated_at: active.activated_at,
        }
    }
}

/// The owned per-harness profile payload. It names the harness, the tool
/// allowlist and the source scope; it is data the caller renders into a native
/// agent definition, never a file this module touched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProfile {
    pub harness: HarnessKind,
    pub role_id: String,
    pub instructions: String,
    pub tool_allowlist: Vec<String>,
    pub source_scope: Vec<String>,
    pub allowed_routes: Vec<String>,
    pub required_capabilities: Vec<Capability>,
    pub delegated_limits: DelegatedLimits,
}

impl ActiveRole {
    /// Render this role into an owned profile payload for `harness`. Refuses a
    /// harness the role did not declare. The tool allowlist is the intersection
    /// of the role's `tool_scope` with `policy.allowed_tools`, ordered by the
    /// role scope, so rendering can never widen the policy.
    pub fn render(&self, harness: HarnessKind, policy: &ProjectPolicy) -> Result<HarnessProfile> {
        if !self.definition.allowed_harnesses.contains(&harness) {
            return Err(CoreError::new(
                "role_harness_not_allowed",
                format!("role `{}` does not declare harness {harness:?}", self.role_id),
            ));
        }
        let tool_allowlist = self
            .definition
            .tool_scope
            .iter()
            .filter(|tool| policy.allowed_tools.iter().any(|allowed| allowed == *tool))
            .cloned()
            .collect();
        let allowed_routes = self
            .definition
            .allowed_routes
            .iter()
            .filter(|route| policy.allowed_connection_ids.iter().any(|allowed| allowed == *route))
            .cloned()
            .collect();
        Ok(HarnessProfile {
            harness,
            role_id: self.role_id.clone(),
            instructions: self.definition.instructions.clone(),
            tool_allowlist,
            source_scope: self.definition.source_scope.clone(),
            allowed_routes,
            required_capabilities: self.definition.required_capabilities.clone(),
            delegated_limits: self.definition.delegated_limits.clone(),
        })
    }

    /// The sources this role may disclose. Fail-closed: an empty scope admits
    /// nothing, so a role that names no sources sees no source content.
    pub fn admits_source(&self, source_id: &str) -> bool {
        self.definition.source_scope.iter().any(|allowed| allowed == source_id)
    }

    /// Narrow already-disclosure-filtered excerpts to the sources this role's
    /// scope permits. Retrieval has already dropped content the route may not
    /// disclose; this only ever removes more, never less.
    pub fn scope_excerpts(&self, excerpts: &[CitedExcerpt]) -> Vec<CitedExcerpt> {
        excerpts
            .iter()
            .filter(|excerpt| self.admits_source(&excerpt.citation.source_id))
            .cloned()
            .collect()
    }

    /// Admit a requested worker fan-out under the role's hard ceiling. A role
    /// is not an approval; this only bounds what the role may propose.
    pub fn admit_workers(&self, requested: u16) -> Result<()> {
        if requested > self.definition.delegated_limits.max_workers {
            return Err(CoreError::new(
                "role_workers_exceeded",
                format!(
                    "role `{}` may delegate at most {} workers",
                    self.role_id, self.definition.delegated_limits.max_workers
                ),
            ));
        }
        Ok(())
    }

    /// Admit a requested request count under the role's hard ceiling.
    pub fn admit_requests(&self, requested: u32) -> Result<()> {
        if requested > self.definition.delegated_limits.max_requests {
            return Err(CoreError::new(
                "role_requests_exceeded",
                format!(
                    "role `{}` may delegate at most {} requests",
                    self.role_id, self.definition.delegated_limits.max_requests
                ),
            ));
        }
        Ok(())
    }
}

/// Resolve the currently-active role for a project, if any. A missing
/// activation is a typed denial, never a default role.
pub async fn resolve(store: &Store, project: ProjectId, role_id: &str) -> Result<ActiveRole> {
    let registry = RoleRegistry::new(store.clone());
    registry
        .active(project, role_id)
        .await?
        .map(ActiveRole::from)
        .ok_or_else(|| CoreError::new("role_not_active", format!("role `{role_id}` is not activated in this project")))
}

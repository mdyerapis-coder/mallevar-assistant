mod actions;
mod grants;
pub mod routes;
pub mod service;
pub mod file_sink;

use crate::contracts::{ActionSpec, ApprovalGrant, GrantId, ProjectId, ProjectPolicy, RunId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TaskScope {
    pub read_roots: Vec<String>,
    pub write_roots: Vec<String>,
    pub tools: Vec<String>,
    pub connection_ids: Vec<String>,
    pub network_destinations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TaskAuthority {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub owner_approval_id: GrantId,
    pub scope: TaskScope,
    #[serde(with = "crate::contracts::wire::u64")] #[schemars(with = "crate::contracts::wire::WireU64Schema")] #[ts(type = "string")] pub policy_version: u64,
    #[serde(with = "crate::contracts::wire::i64")] #[schemars(with = "crate::contracts::wire::WireI64Schema")] #[ts(type = "string")] pub expires_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyError {
    InvalidAction,
    ProjectMismatch,
    PolicyVersionMismatch,
    ForbiddenTool,
    ForbiddenPath,
    ForbiddenDestination,
    StaleGrant,
    ExpiredGrant,
    RevokedGrant,
    ExpiredAuthority,
    AuthorityMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", content = "reason", rename_all = "snake_case")]
pub enum PolicyDecision {
    Permit,
    AwaitApproval,
    Deny(PolicyError),
}

pub struct PolicyEngine;

impl PolicyEngine {
    pub fn check(
        action: &ActionSpec,
        authority: Option<&TaskAuthority>,
        grant: Option<&ApprovalGrant>,
        policy: &ProjectPolicy,
        now_unix_ms: i64,
    ) -> PolicyDecision {
        if action.project_id != policy.project_id {
            return PolicyDecision::Deny(PolicyError::ProjectMismatch);
        }
        if action.policy_version != policy.version {
            return PolicyDecision::Deny(PolicyError::PolicyVersionMismatch);
        }
        let validated = match actions::validate(action, policy) {
            Ok(value) => value,
            Err(error) => return PolicyDecision::Deny(error),
        };
        if let Some(grant) = grant {
            return match grants::validate(action, grant, policy, now_unix_ms) {
                Ok(()) => PolicyDecision::Permit,
                Err(error) => PolicyDecision::Deny(error),
            };
        }
        if let Some(authority) = authority {
            if authority.project_id != action.project_id || authority.run_id != action.run_id {
                return PolicyDecision::Deny(PolicyError::AuthorityMismatch);
            }
            if authority.policy_version != policy.version {
                return PolicyDecision::Deny(PolicyError::PolicyVersionMismatch);
            }
            if authority.expires_at <= now_unix_ms {
                return PolicyDecision::Deny(PolicyError::ExpiredAuthority);
            }
            if !validated.consequential {
                if !authority.scope.tools.iter().any(|tool| tool == &action.kind) {
                    return PolicyDecision::Deny(PolicyError::ForbiddenTool);
                }
                if let Some(path) = validated.path.as_deref() {
                    let roots = if action.kind == "file_write" {
                        &authority.scope.write_roots
                    } else {
                        &authority.scope.read_roots
                    };
                    if !actions::within_roots(path, roots) {
                        return PolicyDecision::Deny(PolicyError::ForbiddenPath);
                    }
                }
                if let Some(destination) = action.destination.as_deref()
                    && !routes::destination_allowed(destination, &authority.scope.network_destinations) {
                        return PolicyDecision::Deny(PolicyError::ForbiddenDestination);
                    }
                return PolicyDecision::Permit;
            }
        }
        PolicyDecision::AwaitApproval
    }
}

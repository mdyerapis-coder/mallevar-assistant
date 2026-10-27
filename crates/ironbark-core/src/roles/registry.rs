//! Core-owned role definitions.
//!
//! A role definition is scoped instructions, not an authority. This registry
//! stores core-owned definitions only: it persists to the durable store and
//! NEVER reads or writes an operator config file, so a user/native agent file
//! is never overwritten by activation. Activation is preview-bound: the caller
//! reviews a preview ([[RoleRegistry::preview]]) and activates exactly that
//! definition digest, so a definition that changed after review cannot be
//! activated without a fresh preview. Activation also refuses any definition
//! whose `tool_scope`, `allowed_routes` or `delegated_limits` exceed the
//! committed project policy — a role narrows policy, it never widens it.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest as ShaDigest, Sha256};

use crate::contracts::{Digest, ProjectId, ProjectPolicy, RoleDefinition};
use crate::error::{CoreError, Result};
use crate::store::events::{now_ms, redacted_json};
use crate::store::Store;

/// The canonical core-owned role ids. User specialists are added at runtime by
/// activation and are additional to these; the builtins always exist.
pub const BUILTIN_ROLE_IDS: &[&str] = &["planner", "researcher", "implementer", "reviewer", "verifier"];

/// A role id naming a non-core namespace may never be activated here: a
/// user/native role file is not owned by this registry.
const RESERVED_PREFIXES: &[&str] = &["user:", "native:", "local:", "file:"];

/// An activated role: the definition bound to the digest and instant that
/// activation committed. This is scoping, never authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveDefinition {
    pub role_id: String,
    pub project_id: ProjectId,
    pub definition: RoleDefinition,
    pub definition_digest: Digest,
    pub activated_at: i64,
}

/// The reviewed activation preview body persisted under `import_previews`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RolePreview {
    role_id: String,
    project_id: ProjectId,
    definition_digest: Digest,
    definition: RoleDefinition,
    created_at: i64,
}

/// The durable activation receipt body persisted under `import_receipts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleReceipt {
    role_id: String,
    project_id: ProjectId,
    definition_digest: Digest,
    definition: RoleDefinition,
    activated_at: i64,
}

/// Registry over core-owned role definitions. Construction is cheap and shares
/// the one store authority; the definitions themselves live in durable rows.
#[derive(Clone)]
pub struct RoleRegistry {
    store: Store,
}

impl RoleRegistry {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// The canonical core-owned role ids. Always present, independent of any
    /// project's activations.
    pub fn builtin_ids() -> Vec<String> {
        BUILTIN_ROLE_IDS.iter().map(|id| (*id).to_string()).collect()
    }

    /// Deterministic content digest of a definition; the identity activation
    /// binds. Any change to the definition changes this digest.
    pub fn definition_digest(definition: &RoleDefinition) -> Result<Digest> {
        let bound = serde_json::to_vec(&serde_json::json!({
            "format": 1,
            "kind": "role",
            "definition": definition,
        }))?;
        sha256_hex(&bound)
    }

    /// The preview digest that binds one definition digest to one project.
    pub fn preview_digest(project: ProjectId, definition_digest: &Digest) -> Result<Digest> {
        let bound = serde_json::to_vec(&serde_json::json!({
            "format": 1,
            "kind": "role",
            "project_id": project,
            "definition_digest": definition_digest,
        }))?;
        sha256_hex(&bound)
    }

    /// Review an activation preview for `definition` in `project`. Persists the
    /// preview durably and returns its digest; a definition changed after this
    /// review no longer matches, so it needs a fresh preview.
    pub async fn preview(&self, project: ProjectId, definition: &RoleDefinition) -> Result<Digest> {
        validate_role_id(&definition.id)?;
        let definition_digest = Self::definition_digest(definition)?;
        let digest = Self::preview_digest(project, &definition_digest)?;
        let record = RolePreview {
            role_id: definition.id.clone(),
            project_id: project,
            definition_digest,
            definition: definition.clone(),
            created_at: now_ms(),
        };
        let body = redacted_json(&record)?;
        let created_at = record.created_at;
        let key = digest.clone();
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO import_previews(digest,project_id,body,created_at) VALUES(?1,?2,?3,?4) ON CONFLICT(digest) DO NOTHING",
                    params![key.as_str(), project.to_string(), body, created_at],
                )?;
                Ok(())
            })
            .await?;
        Ok(digest)
    }

    /// Activate exactly the reviewed definition. Refuses: an unknown/changed
    /// preview; a definition whose scope or limits exceed the committed policy;
    /// and a changed definition whose preview is not newer than the activation
    /// it would replace. Activation never grants authority — the resulting
    /// definition only scopes work that still needs its own task authority.
    pub async fn activate(&self, definition: RoleDefinition, preview_digest: Digest) -> Result<String> {
        validate_role_id(&definition.id)?;
        let definition_digest = Self::definition_digest(&definition)?;
        let requested = preview_digest.clone();
        let (preview, current) = self
            .store
            .with_connection(move |conn| {
                let preview = load_preview(conn, &requested)?;
                let current = match &preview {
                    Some(preview) => active_receipt(conn, preview.project_id, &preview.role_id)?,
                    None => None,
                };
                Ok((preview, current))
            })
            .await?;
        let Some(preview) = preview else {
            return Err(CoreError::new("role_preview_missing", "review a role activation preview in this project first"));
        };
        if preview.definition_digest != definition_digest {
            return Err(CoreError::new(
                "role_definition_changed",
                "the definition changed since it was reviewed; review a fresh activation preview",
            ));
        }
        if let Some(current) = &current
            && current.definition_digest != definition_digest
            && preview.created_at <= current.activated_at
        {
            return Err(CoreError::new(
                "role_stale_preview",
                "a changed definition requires a preview created after the activation it replaces",
            ));
        }
        let policy = crate::knowledge::latest_policy(&self.store, preview.project_id)
            .await?
            .ok_or_else(|| CoreError::new("policy_missing", "the project has no committed policy to scope this role"))?;
        ensure_within_policy(&definition, &policy)?;
        let project = preview.project_id;
        let role_id = definition.id.clone();
        let receipt = RoleReceipt {
            role_id: role_id.clone(),
            project_id: project,
            definition_digest,
            definition,
            activated_at: now_ms(),
        };
        let body = redacted_json(&receipt)?;
        let activated_at = receipt.activated_at;
        let key = preview_digest.clone();
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO import_receipts(digest,project_id,body,applied_at) VALUES(?1,?2,?3,?4) ON CONFLICT(digest) DO NOTHING",
                    params![key.as_str(), project.to_string(), body, activated_at],
                )?;
                Ok(())
            })
            .await?;
        Ok(role_id)
    }

    /// The currently-active definition for a role id in a project, if any. The
    /// latest activation wins; a changed definition only ever replaces one
    /// whose activation predates its preview.
    pub async fn active(&self, project: ProjectId, role_id: &str) -> Result<Option<ActiveDefinition>> {
        let role_id = role_id.to_string();
        let receipt = self
            .store
            .with_connection(move |conn| active_receipt(conn, project, &role_id))
            .await?;
        Ok(receipt.map(|receipt| ActiveDefinition {
            role_id: receipt.role_id,
            project_id: receipt.project_id,
            definition: receipt.definition,
            definition_digest: receipt.definition_digest,
            activated_at: receipt.activated_at,
        }))
    }

    /// Every role id activated in this project, sorted and deduplicated. Used
    /// by graph validation to reject unknown role ids.
    pub async fn known_ids(&self, project: ProjectId) -> Result<Vec<String>> {
        let mut ids: Vec<String> = self
            .store
            .with_connection(move |conn| {
                let mut stmt = conn.prepare("SELECT body FROM import_receipts WHERE project_id=?1")?;
                let mut rows = stmt.query(params![project.to_string()])?;
                let mut ids = Vec::new();
                while let Some(row) = rows.next()? {
                    let body: String = row.get(0)?;
                    if let Ok(receipt) = serde_json::from_str::<RoleReceipt>(&body) {
                        ids.push(receipt.role_id);
                    }
                }
                Ok(ids)
            })
            .await?;
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

/// A role id must name a core-owned definition here; a user/native file is not
/// this registry's to write, so its namespace is refused outright.
fn validate_role_id(role_id: &str) -> Result<()> {
    if role_id.is_empty() {
        return Err(CoreError::new("role_invalid_id", "a role id must not be empty"));
    }
    if RESERVED_PREFIXES.iter().any(|prefix| role_id.starts_with(prefix)) {
        return Err(CoreError::new(
            "role_not_core_owned",
            "a user/native role file is not owned by this registry; pick a core-owned role id",
        ));
    }
    Ok(())
}

/// A role narrows policy; it can never widen it. Every scope entry and every
/// delegated limit must fit inside the committed project policy.
pub fn ensure_within_policy(definition: &RoleDefinition, policy: &ProjectPolicy) -> Result<()> {
    for tool in &definition.tool_scope {
        if !policy.allowed_tools.iter().any(|allowed| allowed == tool) {
            return Err(CoreError::new(
                "role_tool_beyond_policy",
                format!("role `{}` requests tool `{tool}` that the project policy does not allow", definition.id),
            ));
        }
    }
    for route in &definition.allowed_routes {
        if !policy.allowed_connection_ids.iter().any(|allowed| allowed == route) {
            return Err(CoreError::new(
                "role_route_beyond_policy",
                format!("role `{}` requests route `{route}` that the project policy does not allow", definition.id),
            ));
        }
    }
    if definition.delegated_limits.max_workers > policy.max_workers {
        return Err(CoreError::new(
            "role_workers_beyond_policy",
            format!(
                "role `{}` delegates {} workers above the policy ceiling of {}",
                definition.id, definition.delegated_limits.max_workers, policy.max_workers
            ),
        ));
    }
    if definition.delegated_limits.max_requests > policy.max_requests {
        return Err(CoreError::new(
            "role_requests_beyond_policy",
            format!(
                "role `{}` delegates {} requests above the policy ceiling of {}",
                definition.id, definition.delegated_limits.max_requests, policy.max_requests
            ),
        ));
    }
    Ok(())
}

fn load_preview(conn: &rusqlite::Connection, digest: &Digest) -> Result<Option<RolePreview>> {
    let body: Option<String> = conn
        .query_row("SELECT body FROM import_previews WHERE digest=?1", params![digest.as_str()], |row| row.get(0))
        .optional()?;
    match body {
        Some(body) => match serde_json::from_str::<RolePreview>(&body) {
            Ok(record) => Ok(Some(record)),
            Err(_) => Err(CoreError::new("role_preview_invalid", "the reviewed preview is not a role activation preview")),
        },
        None => Ok(None),
    }
}

fn active_receipt(conn: &rusqlite::Connection, project: ProjectId, role_id: &str) -> Result<Option<RoleReceipt>> {
    let mut stmt = conn.prepare("SELECT body FROM import_receipts WHERE project_id=?1")?;
    let mut rows = stmt.query(params![project.to_string()])?;
    let mut best: Option<RoleReceipt> = None;
    while let Some(row) = rows.next()? {
        let body: String = row.get(0)?;
        let Ok(receipt) = serde_json::from_str::<RoleReceipt>(&body) else {
            continue;
        };
        if receipt.role_id != role_id {
            continue;
        }
        if best.as_ref().is_none_or(|current| receipt.activated_at >= current.activated_at) {
            best = Some(receipt);
        }
    }
    Ok(best)
}

fn sha256_hex(bytes: &[u8]) -> Result<Digest> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Digest::parse(&format!("{:x}", hasher.finalize()))
}

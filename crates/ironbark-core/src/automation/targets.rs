//! Data types and pure-scope rules for automation targets.
//!
//! `TargetKind` mirrors the H8 spec (`BrowserProfile` | `NativeWindow`) and adds
//! `DocumentFile` — the core-owned, byte-verified model of an owner-selected
//! editable document that the offline tests drive against a real temp file.

use crate::contracts::{Digest, ProjectPolicy};
use crate::error::{CoreError, Result};
use crate::policy::routes;
use schemars::JsonSchema;
use ts_rs::TS;
use serde::{Deserialize, Serialize};

/// A target kind the broker may scope authority over. `DocumentFile` is the
/// core-owned, byte-verified document model (the offline-qualifiable backend);
/// `BrowserProfile` and `NativeWindow` are live backends that require the
/// pinned AlphaCode executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    BrowserProfile,
    NativeWindow,
    DocumentFile,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::BrowserProfile => "browser_profile",
            TargetKind::NativeWindow => "native_window",
            TargetKind::DocumentFile => "document_file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ScopeRole {
    /// The operator's own adapter: the only caller that may open a scope.
    OwnerOperator,
    /// A bounded coding worker: receives broker requests only, never the host
    /// session bus or the operator's personal browser.
    CodingWorker,
}

/// The operator-issued scope for one automation target. Existence of a matching,
/// unexpired, un-released scope is the owner's standing consent for that exact
/// (kind, target) pair; a worker can never mint one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovedTarget {
    pub scope_id: String,
    pub role: ScopeRole,
    pub kind: TargetKind,
    pub target_id: String,
    #[serde(with = "crate::contracts::wire::i64")] #[schemars(with = "crate::contracts::wire::WireI64Schema")] #[ts(type = "string")] pub expires_at: i64,
}

/// The reserved identity of the operator's live session bus / personal browser.
/// Only `OwnerOperator` may ever be scoped to it; a coding worker is denied at
/// scope time and again on every execute (defense in depth).
pub const HOST_SESSION_TARGET: &str = "host-session";

/// The durable scope state, persisted under entity
/// `automation-scope/<scope_id>` as the `state` of a `CoreEvent::StateChanged`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationScopeRecord {
    pub phase: ScopePhase,
    pub target: ApprovedTarget,
    pub recorded_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ScopePhase {
    Active,
    Released,
}

/// `state` payload is the JSON of [`AutomationScopeRecord`]; the entity carries
/// the `scope_id`. Extract the id back out of the entity string for lookups.
pub fn scope_of_scope(entity_id: &str) -> String {
    entity_id.strip_prefix("automation-scope/").unwrap_or(entity_id).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetClass {
    /// The target has no local byte state (a live browser tab / AT-SPI window).
    External,
    /// The target's state is a document the core can hash (temp file or profile
    /// directory whose bytes the broker can read without a display).
    Hashable,
}

impl TargetKind {
    /// Whether a `global`-input / coordinate operation against this target is
    /// forbidden by the local display-server policy. Wayland excludes pointer
    /// compositing for coordinate injection; the document model is exempt (its
    /// input is the file bytes, never injected).
    pub fn denies_global_input(self) -> bool {
        matches!(self, TargetKind::NativeWindow) && std::env::var_os("WAYLAND_DISPLAY").is_some()
    }
}

/// Scope check shared by observe and execute: the target must be a real,
/// non-empty, unexpired scope and the caller's role must be permitted for it.
pub fn check_target_in_scope(target: &ApprovedTarget, now_unix_ms: i64) -> Result<()> {
    if target.scope_id.is_empty() {
        return Err(CoreError::new("invalid_target", "Scope id is empty"));
    }
    if target.target_id.is_empty() {
        return Err(CoreError::new("invalid_target", "Target id is empty"));
    }
    if target.expires_at <= now_unix_ms {
        return Err(CoreError::new("scope_expired", "The owner-issued scope is no longer current"));
    }
    if target.role == ScopeRole::CodingWorker && target.target_id == HOST_SESSION_TARGET {
        return Err(CoreError::new(
            "scope_violation",
            "A coding worker is limited to owner-approved disposable targets; the host session bus and personal browser are owner-only",
        ));
    }
    Ok(())
}

/// `ActionSpec.destination` must stay within the project policy's network
/// destinations (loopback-gated when `local_only`), mirroring `PolicyEngine`.
pub fn check_destination(destination: &str, policy: &ProjectPolicy) -> Result<()> {
    if !routes::destination_allowed(destination, &policy.network_destinations)
        || (policy.local_only && !routes::is_loopback_destination(destination))
    {
        return Err(CoreError::new(
            "policy_denied",
            "Action destination is outside the project policy's allowed network destinations",
        ));
    }
    Ok(())
}

/// The byte-verified document revision: SHA-256 of `canonical_path \0 <bytes>`.
/// Canonicalised (symlinks resolved, `..` collapsed) so a renamed/moved file or
/// differing path spelling cannot silently change the digest. Appending text
/// changes the hash; a focus `click` does not.
pub fn document_revision(path: &str, bytes: &[u8]) -> Digest {
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest as _;
    // Resolve the canonical absolute path so the digest is stable across path
    // spellings (a real effect: a moved document is a different observable).
    let canonical = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    hasher.update(canonical.as_bytes());
    hasher.update([0u8]);
    hasher.update(bytes);
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        hex.push_str(&format!("{:02x}", byte));
    }
    Digest::parse(&hex).expect("sha256 hex is always a valid digest")
}
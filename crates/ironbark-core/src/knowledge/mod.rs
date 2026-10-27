//! P3/P4/P5: immutable cited knowledge (ingestion, extraction, retrieval,
//! deletion, memory, embeddings).
//!
//! Invariants:
//! - A source revision is immutable and content-addressed. Re-ingesting the
//!   same bytes is idempotent; new bytes publish a NEW revision and old
//!   citations keep resolving to the exact historic text (P3).
//! - Retrieval applies project/access/disclosure SQL filtering BEFORE any
//!   ranking so LocalOnly and foreign-project content cannot reach a cloud
//!   route or another project's context (P4).
//! - Memory has separate candidate/approved/forgotten records tied to
//!   originating evidence. A proposal is not retrievable before approval, and
//!   retrieved memory is advisory text: it can never expand tools (P5).
//! - Embeddings are optional, tagged by full model identity/dimensions/
//!   revision, and never block chat or FTS: a lexical result is explicitly
//!   labelled lexical (P5).
//!
//! Services: `KnowledgeService` (ingest/retrieve/open/delete/context),
//! `CitationResolver` (open), `MemoryService` (propose/decide/forget),
//! `EmbeddingIndex` (index/semantic retrieve). The leaf files own the logic;
//! this file owns the public types + the shared store helpers so there is one
//! convention for policy/route/project access.

pub mod chunks;
pub mod context;
pub mod deletion;
pub mod embeddings;
pub mod extractors;
pub mod index_jobs;
pub mod locators;
pub mod memory;
pub mod retrieval;
pub mod sources;

use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use ts_rs::TS;
use uuid::Uuid;
use crate::{contracts::*, error::{CoreError, Result}, store::Store};

/// One ranked, disclosure-checked excerpt. The `CitedExcerpt` already carries
/// the project and disclosure of the revision that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RetrievalExcerpt { pub excerpt: CitedExcerpt, pub rank: f64, pub lexical: bool }

/// A retrieval result. `mode` is `Lexical` when the query resolved through the
/// FTS fallback (no qualifying vector path was available) and `Embedding` when
/// local cosine ranking of authorised candidates was used. A lexical result is
/// always explicitly labelled, never silently called semantic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RetrievalResult { Lexical { excerpts: Vec<RetrievalExcerpt> }, Embedding { model: EmbeddingModelIdentity, excerpts: Vec<RetrievalExcerpt> } }
impl RetrievalResult {
    pub fn excerpts(&self) -> &[RetrievalExcerpt] {
        match self { RetrievalResult::Lexical { excerpts } | RetrievalResult::Embedding { excerpts, .. } => excerpts }
    }
}

/// P3+P4 ingestion, retrieval, citation opening and deletion. `cas_root` is the
/// project CAS root (the same root `CoreServices` uses) so `Attachment` inputs
/// resolve through the project-scoped object store.
#[derive(Clone)]
pub struct KnowledgeService { pub(crate) store: Store, pub(crate) cas_root: PathBuf }
/// P3 citation opening. Never substitutes the current revision for a deleted
/// one; it returns the explicit `Deleted`/`Missing` view instead.
#[derive(Clone)]
pub struct CitationResolver { pub(crate) store: Store }
/// P5 explicit owner memory.
#[derive(Clone)]
pub struct MemoryService { pub(crate) store: Store }
/// P5 optional versioned local embeddings.
#[derive(Clone)]
pub struct EmbeddingIndex { pub(crate) store: Store }

impl KnowledgeService {
    pub fn new(store: Store, cas_root: PathBuf) -> Self { Self { store, cas_root } }
}
impl CitationResolver {
    pub fn new(store: Store) -> Self { Self { store } }
}
impl MemoryService {
    pub fn new(store: Store) -> Self { Self { store } }
}
impl EmbeddingIndex {
    pub fn new(store: Store) -> Self { Self { store } }
}

pub(crate) fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| CoreError::new("storage_corrupt", "Invalid persisted UUID"))
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The latest committed policy for a project, if any.
pub(crate) async fn latest_policy(store: &Store, project: ProjectId) -> Result<Option<ProjectPolicy>> {
    store.with_connection(move |conn| {
        let body: Option<String> = conn.query_row(
            "SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
            [project.to_string()],
            |row| row.get(0),
        ).optional()?;
        match body {
            Some(body) => Ok(Some(serde_json::from_str(&body).map_err(|_| CoreError::new("storage_corrupt", "persisted policy is not valid JSON"))?)),
            None => Ok(None),
        }
    }).await
}

/// The latest committed policy, refusing operations on a project that has none.
/// Retrieval, context and embedding ranking all require a committed policy;
/// there is no default-permissive fallback.
pub(crate) async fn require_policy(store: &Store, project: ProjectId) -> Result<ProjectPolicy> {
    latest_policy(store, project).await?
        .ok_or_else(|| CoreError::new("policy_missing", "project has no committed policy; the operation is refused"))
}

/// Validate a route against a committed policy. A non-local route must be in
/// `allowed_connection_ids`; a local_only project admits only the local route.
pub(crate) fn validate_route(policy: &ProjectPolicy, route: &RoutePin) -> Result<()> {
    if policy.local_only {
        if route.connection_id != "local" {
            return Err(CoreError::new("local_only_route", "a local_only project may only use the local route"));
        }
        return Ok(());
    }
    if route.connection_id != "local" && !policy.allowed_connection_ids.iter().any(|c| c == &route.connection_id) {
        return Err(CoreError::new("route_not_allowed", "route connection is not in the committed policy's allowed connections"));
    }
    Ok(())
}

/// A knowledge command names a project explicitly; the project must exist or
/// the whole operation is refused (no implicit project creation).
pub(crate) fn ensure_project(conn: &rusqlite::Connection, project: ProjectId) -> Result<()> {
    let name: Option<String> = conn.query_row("SELECT name FROM projects WHERE id=?1", [project.to_string()], |row| row.get(0)).optional()?;
    name.map(|_| ()).ok_or_else(|| CoreError::new("project_not_found", "knowledge operation names a project that is not registered"))
}

pub(crate) fn ensure_project_tx(tx: &rusqlite::Transaction<'_>, project: ProjectId) -> Result<()> {
    let name: Option<String> = tx.query_row("SELECT name FROM projects WHERE id=?1", [project.to_string()], |row| row.get(0)).optional()?;
    name.map(|_| ()).ok_or_else(|| CoreError::new("project_not_found", "knowledge operation names a project that is not registered"))
}

use rusqlite::OptionalExtension;

//! Durable conversation projections. This module never launches a provider or
//! grants run acceptance; the parent core owns qualification and C4 receipts.
pub mod branches;
pub mod turns;
pub mod search;
pub mod exports;

use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use ts_rs::TS;
use uuid::Uuid;
use crate::{contracts::*, error::{CoreError, Result}, store::{Store, events::{now_ms, redacted_json}}};

#[derive(Clone)]
pub struct ConversationService { pub(crate) store: Store, pub(crate) context_gate: Option<std::sync::Arc<dyn ContextGate>> }
/// Native dependency injection only: this validator is never a public DTO,
/// model-generated approval, or caller-supplied boolean. Parent supplies P4.
pub trait ContextGate: Send + Sync {
    fn validate(&self, tx: &Transaction<'_>, project: ProjectId, route: &RoutePin, policy: &ProjectPolicy, context: &ContextPackage) -> Result<()>;
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct BranchProvenance {
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub original_message_id: Option<MessageId>,
    pub preserved_run_ids: Vec<RunId>,
    pub prior_effects_undone: bool,
}
impl Default for BranchProvenance {
    fn default() -> Self { Self { operation: "root".into(), original_message_id: None, preserved_run_ids: vec![], prior_effects_undone: false } }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MessageView {
    pub id: MessageId, pub origin_branch_id: BranchId, pub role: String,
    pub parts: Vec<ContentPart>, pub status: String,
    #[serde(with = "crate::contracts::wire::i64")] #[schemars(with = "crate::contracts::wire::WireI64Schema")] #[ts(type = "string")] pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub run_id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub snapshot: Option<TurnSnapshot>,
    pub receipts: Vec<EventEnvelope>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct BranchView { pub id: BranchId, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub parent_id: Option<BranchId>, pub provenance: BranchProvenance, pub messages: Vec<MessageView> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ConversationHistory { pub id: ConversationId, pub project_id: ProjectId, pub title: String, pub archived: bool, pub branches: Vec<BranchView> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TurnSnapshot { pub route: RoutePin, pub context: ContextPackage, pub disclosure_digest: Digest, pub policy: ProjectPolicy, pub artifact_receipts: Vec<ArtifactReceipt> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReceipt { pub artifact_id: ArtifactId, pub digest: Digest, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub origin_run_id: Option<RunId> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct QueuedTurn { pub run_id: RunId, pub branch_id: BranchId, pub user_message_id: MessageId, pub assistant_message_id: MessageId, pub replayed: bool }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TurnDispatch { pub run_id: RunId, pub request_id: Uuid, pub branch_id: BranchId, pub user_message_id: MessageId, pub assistant_message_id: MessageId, pub snapshot: TurnSnapshot, pub messages: Vec<MessageView> }
/// Resolution asserts only the expected run set, never authorizes a state
/// transition. Terminal run state must already be committed by C4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RunResolution { pub run_ids: Vec<RunId> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ConversationSummary { pub id: ConversationId, pub title: String, pub archived: bool, #[serde(with = "crate::contracts::wire::i64")] #[schemars(with = "crate::contracts::wire::WireI64Schema")] #[ts(type = "string")] pub created_at: i64 }
pub use exports::{ConversationExport, ExportFormat};

impl ConversationService {
    pub fn new(store: Store) -> Self { Self { store, context_gate: None } }
    pub fn with_context_gate(store: Store, gate: std::sync::Arc<dyn ContextGate>) -> Self { Self { store, context_gate: Some(gate) } }
    pub async fn create(&self, project: ProjectId) -> Result<ConversationId> {
        self.store.with_transaction(move |tx| {
            let id = ConversationId::new();
            let branch = BranchId::new();
            tx.execute("INSERT INTO conversations(id,project_id,created_at) VALUES(?1,?2,?3)", params![id.to_string(),project.to_string(),now_ms()])?;
            tx.execute("INSERT INTO branches(id,conversation_id,provenance) VALUES(?1,?2,?3)", params![branch.to_string(),id.to_string(),redacted_json(&BranchProvenance::default())?])?;
            Ok(id)
        }).await
    }
    pub async fn rename(&self, id: ConversationId, title: String) -> Result<()> {
        let title = crate::store::events::redact_text(title.trim());
        if title.is_empty() || title.len() > 512 { return Err(CoreError::new("invalid_title", "Title must have 1 to 512 bytes")); }
        self.store.with_transaction(move |tx| {
            ensure_visible(tx, id, false)?;
            tx.execute("UPDATE conversations SET title=?2 WHERE id=?1", params![id.to_string(),title])?;
            Ok(())
        }).await
    }
    pub async fn archive(&self, id: ConversationId) -> Result<()> {
        self.store.with_transaction(move |tx| {
            ensure_visible(tx, id, false)?;
            observed_terminal(tx, id)?;
            tx.execute("UPDATE conversations SET archived_at=?2 WHERE id=?1", params![id.to_string(),now_ms()])?;
            Ok(())
        }).await
    }
    /// Logical owner deletion hides history and exports. Execution receipts are
    /// retained for audit; physical audit-retention erasure is a separate policy.
    pub async fn delete(&self, id: ConversationId, resolution: RunResolution) -> Result<()> {
        self.store.with_transaction(move |tx| {
            ensure_visible(tx, id, false)?;
            let mut expected = resolution.run_ids;
            expected.sort_by_key(|r| r.to_string());
            if expected != run_ids(tx, id)? { return Err(CoreError::new("resolution_conflict", "Resolution must cover exactly the conversation's runs")); }
            observed_terminal(tx, id)?;
            tx.execute("UPDATE conversations SET deleted_at=?2 WHERE id=?1", params![id.to_string(),now_ms()])?;
            Ok(())
        }).await
    }
}
pub(crate) fn parse_uuid(value: &str) -> Result<Uuid> { Uuid::parse_str(value).map_err(|_| CoreError::new("storage_corrupt", "Invalid persisted UUID")) }
pub(crate) fn ensure_visible(tx: &Transaction<'_>, id: ConversationId, writable: bool) -> Result<ProjectId> {
    let value: Option<(String, Option<i64>, Option<i64>)> = tx.query_row("SELECT project_id,archived_at,deleted_at FROM conversations WHERE id=?1", [id.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    match value {
        Some((project, archived, None)) if !writable || archived.is_none() => Ok(ProjectId(parse_uuid(&project)?)),
        Some((_, Some(_), None)) => Err(CoreError::new("conversation_archived", "Archived conversation is read only")),
        _ => Err(CoreError::new("conversation_missing", "Conversation is missing or deleted")),
    }
}
pub(crate) fn run_ids(tx: &Transaction<'_>, id: ConversationId) -> Result<Vec<RunId>> {
    let mut stmt = tx.prepare("SELECT run_id FROM conversation_turns WHERE conversation_id=?1 ORDER BY run_id")?;
    let rows = stmt.query_map([id.to_string()], |r| r.get::<_, String>(0))?;
    rows.map(|r| parse_uuid(&r?).map(RunId)).collect()
}
pub(crate) fn observed_terminal(tx: &Transaction<'_>, id: ConversationId) -> Result<()> {
    let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM conversation_turns t JOIN runs r ON r.id=t.run_id WHERE t.conversation_id=?1 AND r.state NOT IN ('succeeded','failed','cancelled'))", [id.to_string()], |r| r.get(0))?;
    let uncertain: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM conversation_turns t JOIN actions a ON a.run_id=t.run_id LEFT JOIN action_outcomes o ON o.action_id=a.id WHERE t.conversation_id=?1 AND (a.state NOT IN ('denied','revoked','succeeded','failed') OR (a.state IN ('succeeded','failed') AND (o.outcome IS NULL OR o.outcome<>a.state)) OR o.outcome='uncertain'))", [id.to_string()], |r| r.get(0))?;
    if active || uncertain { return Err(CoreError::new("run_reconciliation_required", "C4 must observe stop and reconcile tool outcomes before archive/delete")); }
    Ok(())
}

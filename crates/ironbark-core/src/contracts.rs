use crate::judgments::{DomainQualifications, EvaluationReportInput};
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use ts_rs::TS;
use uuid::Uuid;
use crate::error::{CoreError, Result};
use crate::conversations::{
    ConversationExport, ConversationHistory, ConversationSummary, ExportFormat, QueuedTurn, RunResolution,
};
use crate::automation::targets::ApprovedTarget;
use crate::automation::{AutomationObservation, AutomationReceipt};
use crate::store::backup::BackupManifest;
use crate::store::imports::{ImportPreview, ImportReceipt, ImportSource};
use crate::store::objects::CapacityReport;
use crate::store::retention::RetentionReport;
use crate::scheduler::RunReceipt;
use crate::connections::webgpt::{
    WebGptStatus, WebGptSubmissionReceipt, WebGptTurnInput, WebGptTurnOutput,
};

// Protocol 2 exact wire contract: canonical decimal-string 64-bit integers.
pub mod wire;

macro_rules! uuid_id {
    ($($name:ident),* $(,)?) => { $(
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name { pub fn new() -> Self { Self(Uuid::new_v4()) } }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) } }
    )* };
}
uuid_id!(ProjectId, ConversationId, BranchId, MessageId, JobId, NodeId, RunId, ActionId, GrantId, ArtifactId, DecisionId);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, JsonSchema, TS)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Digest(#[schemars(regex(pattern = "^[0-9a-f]{64}$"))] String);
impl Digest {
    pub fn parse(value: &str) -> Result<Self> {
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(CoreError::new("invalid_digest", "digest must be lowercase SHA-256 hex"));
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str { &self.0 }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind { Omp, Codex, Opencode, Claude, Hermes, Alphacode }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState { Unknown, Unavailable, Unqualified, Qualified }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct CapabilityObservation { pub name: String, pub state: CapabilityState, pub implementation_version: String, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub evidence_digest: Option<Digest>, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub observed_at: i64 }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RoutePin { pub connection_id: String, pub model_id: String, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub harness: Option<HarnessKind> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SpendPolicy { FreeOnly, PaidAllowed }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProjectPolicy {
    #[serde(with = "wire::u64")] #[schemars(with = "String")] #[ts(type = "string")] pub version: u64, pub project_id: ProjectId,
    pub allowed_connection_ids: Vec<String>, pub allowed_tools: Vec<String>,
    pub read_roots: Vec<String>, pub write_roots: Vec<String>, pub network_destinations: Vec<String>,
    pub local_only: bool, pub max_workers: u16, pub max_requests: u32,
    #[serde(default, with = "wire::option_u64", skip_serializing_if = "Option::is_none")] #[schemars(with = "Option<String>")] #[ts(optional, type = "string")] pub max_cost_microusd: Option<u64>,
    pub spend_policy: SpendPolicy, pub allow_unknown_cost: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum RunState { Queued, Ready, Running, AwaitingApproval, Blocked, Reviewing, Verifying, Paused, Cancelling, Cancelled, Succeeded, Failed }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    pub id: ActionId, pub run_id: RunId, pub project_id: ProjectId, pub kind: String,
    #[ts(type = "unknown")] pub canonical_args: serde_json::Value, #[ts(type = "unknown")] pub scope: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub destination: Option<String>,
    #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub policy_version: u64, pub expected_state_digest: Digest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalGrant {
    pub id: GrantId, pub action_id: ActionId, pub action_digest: Digest,
    #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub policy_version: u64, pub expected_state_digest: Digest,
    #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_i64")] #[schemars(with = "Option<wire::WireI64Schema>")] #[ts(optional, type = "string")] pub revoked_at: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRef { pub harness: HarnessKind, pub native_id: String, pub native_version: String, pub project_id: ProjectId, pub cwd: String, pub resolved_route: RoutePin }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct CitationLocator {
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub line_start: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub line_end: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub sheet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub cell_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub bounding_box: Option<[f64; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_u64")] #[schemars(with = "Option<wire::WireU64Schema>")] #[ts(optional, type = "string")] pub time_start_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_u64")] #[schemars(with = "Option<wire::WireU64Schema>")] #[ts(optional, type = "string")] pub time_end_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Citation { pub source_id: String, pub revision_digest: Digest, pub locator: CitationLocator }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct CitedExcerpt { pub text: String, pub citation: Citation, pub project_id: ProjectId, pub disclosure: DisclosurePolicy }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ContextPackage { pub digest: Digest, pub brief: String, pub constraints: Vec<String>, pub excerpts: Vec<CitedExcerpt>, pub artifacts: Vec<ArtifactId>, pub unresolved_questions: Vec<String>, pub originating_sessions: Vec<SessionRef> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct CheckReceipt { pub name: String, pub passed: bool, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub evidence_digest: Option<Digest>, pub details: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ResultManifest { pub run_id: RunId, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub native_session: Option<SessionRef>, pub artifacts: Vec<ArtifactId>, pub checks: Vec<CheckReceipt>, pub acceptance_satisfied: bool, pub unresolved: Vec<String> }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentPart { Text { text: String }, Image { uri: String, mime_type: String }, Audio { uri: String, mime_type: String }, Artifact { artifact_id: ArtifactId } }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolDescriptor { pub name: String, pub description: String, #[ts(type = "unknown")] pub input_schema: serde_json::Value }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct OpenSessionInput { pub run_id: RunId, pub project_id: ProjectId, pub cwd: String, pub route: RoutePin, pub context: ContextPackage, pub policy: ProjectPolicy, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub resume: Option<SessionRef> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TurnInput { pub request_id: Uuid, pub message_id: MessageId, pub content: Vec<ContentPart>, pub allowed_tools: Vec<ToolDescriptor>, pub budget: ProjectPolicy }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision { Allow, Deny }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PermissionAnswer { pub native_request_id: String, pub action: ActionSpec, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub grant: Option<ApprovalGrant>, pub decision: PermissionDecision }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HarnessDescriptor { pub harness: HarnessKind, pub version: String, pub transport: String, pub route_compatibility: Vec<String>, pub approval: CapabilityState, pub cancel: CapabilityState, pub resume: CapabilityState, pub child_observability: CapabilityState, pub capabilities: Vec<CapabilityObservation> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol { OpenaiCompletions, OpenaiResponses, Anthropic, Gemini, Ollama, LlamaCpp, Custom }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthRef { SecretService { service: String, account: String }, Bitwarden { item_id: String, field: String }, NativeSession { harness: HarnessKind, profile: String } }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Capability { Text, Tools, Vision, Audio, Embeddings, Transcription, Reasoning }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor { pub id: String, pub context_limit: u32, pub output_limit: u32, pub capabilities: Vec<CapabilityObservation> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ConnectionConfig { pub id: String, pub protocol: ProviderProtocol, pub endpoint: String, pub models: Vec<ModelDescriptor>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub auth_ref: Option<AuthRef>, pub allowed_projects: Vec<ProjectId> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ConnectionDescriptor { pub id: String, pub protocol: ProviderProtocol, pub endpoint: String, pub models: Vec<ModelDescriptor>, pub capabilities: Vec<CapabilityObservation>, pub availability: CapabilityState, pub auth_available: bool }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRole { System, User, Assistant, Tool }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderToolCall { pub id: String, pub name: String, #[ts(type = "unknown")] pub arguments: serde_json::Value }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderMessage { pub role: ProviderRole, pub content: Vec<ContentPart>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub tool_call_id: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub tool_calls: Option<Vec<ProviderToolCall>>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional, type = "unknown")] pub sdk_content: Option<serde_json::Value>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub sdk_route: Option<RoutePin>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub tool_result_error: Option<bool> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderTurnInput { pub run_id: RunId, pub request_id: Uuid, pub message_id: MessageId, pub route: RoutePin, pub messages: Vec<ProviderMessage>, pub required_capabilities: Vec<Capability>, pub tool_schemas: Vec<ToolDescriptor>, pub disclosure_digest: Digest, pub policy: ProjectPolicy, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub max_output_tokens: Option<u32> }
// Never persisted or exported through UI IPC. Only inherited worker stdio uses this DTO.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkerProviderTurnInput { #[serde(flatten)] #[ts(flatten)] pub input: ProviderTurnInput, pub credential_lease_id: String, pub connection: ConnectionConfig }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct BillingObservation { pub state: BillingState, #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_u64")] #[schemars(with = "Option<wire::WireU64Schema>")] #[ts(optional, type = "string")] pub cost_microusd: Option<u64> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum BillingState { Unknown, Reported }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource { Provider, Unknown }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct UsageObservation { #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub input_tokens: Option<u32>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub output_tokens: Option<u32>, #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_u64")] #[schemars(with = "Option<wire::WireU64Schema>")] #[ts(optional, type = "string")] pub cost_microusd: Option<u64>, pub billing: BillingObservation, pub source: UsageSource }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScoreEvidence { None {}, RawTokenLogProb { value: f64, token: String, token_position: u32 }, CalibratedSelectiveRisk { upper_error_bound: f64, dataset_digest: Digest, calibrator_digest: Digest } }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum JudgmentState { Available, Invalid, Abstain, Unqualified, Unavailable }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct JudgmentInput { pub decision_id: DecisionId, pub domain: String, pub domain_version: String, pub candidates: Vec<String>, pub input_refs: Vec<String>, pub input_digest: Digest, pub criterion: String, pub route_policy_digest: Digest }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct JudgmentRecord { pub decision_id: DecisionId, pub domain: String, pub domain_version: String, pub candidates: Vec<String>, pub input_refs: Vec<String>, pub input_digest: Digest, pub criterion: String, pub route_policy_digest: Digest, pub model_identity: String, pub runtime_identity_digest: Digest, pub prompt_digest: Digest, pub candidate_codes: std::collections::BTreeMap<String, String>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub choice: Option<String>, pub state: JudgmentState, pub score: ScoreEvidence, #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub latency_ms: u64, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub qualification_digest: Option<Digest>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub reason: Option<String> }

// ── C4: state transitions, checkpoints, reconciliation ───────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPhase { Planned, PreMutation, PostMutation, PostVerification, PreConsequential }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRecord { pub run_id: RunId, pub phase: CheckpointPhase, pub state_digest: Digest, #[ts(type = "unknown")] pub manifest: serde_json::Value, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub created_at: i64 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReport { pub run_id: RunId, pub run_state: RunState, #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_u64")] #[schemars(with = "Option<wire::WireU64Schema>")] #[ts(optional, type = "string")] pub recorded_pid: Option<u64>, #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_i64")] #[schemars(with = "Option<wire::WireI64Schema>")] #[ts(optional, type = "string")] pub recorded_start_time: Option<i64>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub checkpoint: Option<CheckpointRecord>, #[ts(type = "unknown")] pub observed_workspace: serde_json::Value, pub pending_mutation: bool, pub uncertain_actions: Vec<String>, pub recommended: Vec<String> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDecision { AcceptObserved, ResumeReadOnly, ContinueFromCheckpoint, Abandon }

// ── P3: immutable source ingestion and real extraction ───────────────────────────
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceInput { File { path: String }, Attachment { artifact_id: ArtifactId }, Fetched { url: String, receipt: Digest } }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DisclosurePolicy { LocalOnly, AllowedConnections { connection_ids: Vec<String> }, ExplicitPublic }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SourceRevision { pub source_id: String, pub project_id: ProjectId, pub digest: Digest, pub mime: String, pub extractor_id: String, pub extractor_version: String, pub locators: usize, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub created_at: i64 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CitationView { Content { text: String }, Deleted, Missing }

// ── P4: filter-before-ranking retrieval and deletion semantics ────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum DeletionScope { Source, Project, All }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DeletionReceipt { pub source_id: String, pub chunks_removed: usize, pub vectors_removed: usize, pub revisions_removed: usize, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub applied_at: i64 }

// ── P5: approved memory and optional versioned local embeddings ───────────────────
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MemoryCandidate { pub project_id: ProjectId, pub text: String, pub provenance: String, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub input_refs: Option<Vec<String>> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDecision { Approve, EditAndApprove, Reject }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecord { pub id: String, pub project_id: ProjectId, pub text: String, pub state: String, pub provenance: String, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub created_at: i64 }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingModelIdentity { pub connection_id: String, pub model_id: String, pub version: String, #[ts(type = "number")] pub dimensions: u32 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingIndexReceipt { pub revision_id: String, pub model: EmbeddingModelIdentity, pub chunks_indexed: usize, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub indexed_at: i64 }

// ── H6: roles, scoped tools and deterministic graph orchestration ────────────────

/// Which workspace a node needs: a reviewer/verifier reads, the implementer writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode { ReadOnly, Writer }

/// The hard ceilings a role may delegate to the work it starts. A role definition
/// is not itself an approval: these limits only bound what the role may propose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegatedLimits {
    pub max_workers: u16,
    pub max_requests: u32,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire::option_u64")] #[schemars(with = "Option<wire::WireU64Schema>")] #[ts(optional, type = "string")] pub max_cost_microusd: Option<u64>,
}

/// A scoped role: instructions plus the routes, harnesses, sources and tools it
/// is allowed to touch. Activation requires an import/activation preview, and
/// activation never grants authority — it only scopes what the role may do
/// inside the existing task policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RoleDefinition {
    pub id: String,
    pub instructions: String,
    pub required_capabilities: Vec<Capability>,
    pub allowed_harnesses: Vec<HarnessKind>,
    pub allowed_routes: Vec<String>,
    pub source_scope: Vec<String>,
    pub tool_scope: Vec<String>,
    pub delegated_limits: DelegatedLimits,
}

/// One node of the deterministic DAG. `dependencies` are node ids that must
/// reach `Succeeded` before this node may start; `acceptance` names the checks
/// the node's result must satisfy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct JobNode {
    pub id: NodeId,
    pub role_id: String,
    pub dependencies: Vec<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub route_pin: Option<RoutePin>,
    pub allowed_routes: Vec<String>,
    pub inputs: Vec<String>,
    pub acceptance: Vec<String>,
    pub workspace_mode: WorkspaceMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct JobGraph {
    pub id: JobId,
    pub project_id: ProjectId,
    #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub version: u64,
    pub nodes: Vec<JobNode>,
}

// ── J2/J3: domain qualification and advisory judgments ────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum JudgmentDomain { TaskIntent, RetrievalRelevance, HandoffCompleteness, EvidenceNeeded, AllowedRouteChoice }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum QualificationState { Enabled, Blocked }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct QualificationRecord { pub domain: JudgmentDomain, pub model_identity: String, pub prompt_digest: Digest, pub state: QualificationState, pub upper_error_bound: f64, pub acceptance_coverage: f64, #[ts(type = "number")] pub holdout_size: u32, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub evaluated_at: i64, pub calibrator_digest: Digest }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdvisoryOutcome { QualifiedChoice { choice: String, decision_id: DecisionId },
    /// The advisor could not choose. `decision_id` names the recorded judgment
    /// that was consulted, when one exists, so the planner can inspect it.
    NeedsPlanner { reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] #[ts(optional)] decision_id: Option<DecisionId> },
    Unavailable }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreEvent {
    TurnStarted { message_id: MessageId },
    TextDelta { message_id: MessageId, delta: String },
    TextReplace { message_id: MessageId, text: String },
    ReasoningDelta { message_id: MessageId, delta: String },
    ProviderToolCall { message_id: MessageId, call: ProviderToolCall },
    ProviderMessage { message_id: MessageId, #[ts(type = "unknown")] content: serde_json::Value, route: RoutePin },
    ToolStarted { action: ActionSpec },
    ToolFinished { action_id: ActionId, #[ts(type = "unknown")] result: serde_json::Value },
    ApprovalNeeded { action: ActionSpec },
    StateChanged { entity_id: String, state: String },
    SessionBound { session: SessionRef },
    ArtifactAdded { artifact_id: ArtifactId, digest: Digest },
    JudgmentRecorded { judgment: Box<JudgmentRecord> },
    UsageObserved { usage: UsageObservation },
    Error { code: String, message: String, #[ts(type = "unknown")] details: serde_json::Value },
    TurnFinished { message_id: MessageId, status: RunState, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] stop_reason: Option<String> },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WorkerEvent { #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub session_id: Option<String>, pub turn_id: String, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub tool_id: Option<String>, pub run_id: RunId, pub payload: CoreEvent }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope { #[ts(type = "1")] #[schemars(range(min = 1, max = 1))] pub protocol: u8, #[serde(with = "wire::u64")] #[schemars(with = "String")] #[ts(type = "string")] pub seq: u64, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub request_id: Option<Uuid>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub project_id: Option<ProjectId>, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] pub run_id: Option<RunId>, #[serde(with = "wire::i64")] #[schemars(with = "String")] #[ts(type = "string")] pub timestamp: i64, pub payload: CoreEvent }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Project { pub id: ProjectId, pub name: String, pub root: String, #[serde(with = "wire::i64")] #[schemars(with = "wire::WireI64Schema")] #[ts(type = "string")] pub created_at: i64 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
// `ConversationQueueTurn` carries a full optional `ContextPackage`, which makes this wire
// enum large. Boxing is not possible (ts-rs `#[ts(optional)]` needs a plain `Option`, and
// serde/TS must stay wire-compatible), so the size is accepted rather than hidden behind a
// second indirection layer.
#[allow(clippy::large_enum_variant)]
pub enum CoreCommand { Status {}, Events { #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] after: u64 }, ProjectList {}, ProjectCreate { name: String, root: String }, Doctor {}, Shutdown {},
    ProjectPolicySet { project_id: ProjectId, policy: ProjectPolicy },
    ProjectPolicyShow { project_id: ProjectId },
    ArtifactRead { project_id: ProjectId, artifact_id: ArtifactId },
    ImportPreview { project_id: ProjectId, source: ImportSource },
    ImportApply { project_id: ProjectId, preview_digest: Digest },
    BackupCreate { destination: String },
    RetentionSweep {},
    ObjectCapacity { project_id: ProjectId },
    ConversationCreate { project_id: ProjectId },
    ConversationHistory { conversation_id: ConversationId },
    ConversationRename { conversation_id: ConversationId, title: String },
    ConversationSearch { project_id: ProjectId, query: String, include_archived: bool, #[ts(type = "number")] limit: u32 },
    ConversationExport { conversation_id: ConversationId, format: ExportFormat },
    ConversationEdit { message_id: MessageId, parts: Vec<ContentPart> },
    ConversationQueueTurn { conversation_id: ConversationId, branch_id: BranchId, parts: Vec<ContentPart>, pin: RoutePin, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] context: Option<ContextPackage> },
    ConversationRegenerate { message_id: MessageId, pin: RoutePin },
    ConversationArchive { conversation_id: ConversationId },
    ConversationDelete { conversation_id: ConversationId, resolution: RunResolution },
    RunInterrupt { run_id: RunId },
    RunCreate { project_id: ProjectId, task: String },
    RunStart { project_id: ProjectId, run_id: RunId, pin: RoutePin,
        #[serde(default)] native_session: Option<SessionRef>,
        #[serde(default)] artifacts: Vec<ArtifactId>,
        #[serde(default, with = "wire::option_u64")] #[schemars(with = "Option<wire::WireU64Schema>")] #[ts(optional, type = "string")] recorded_pid: Option<u64>,
        #[serde(default, with = "wire::option_i64")] #[schemars(with = "Option<wire::WireI64Schema>")] #[ts(optional, type = "string")] recorded_start_time: Option<i64> },
    RunPause { project_id: ProjectId, run_id: RunId },
    RunResume { project_id: ProjectId, run_id: RunId },
    RunCheckpoint { project_id: ProjectId, run_id: RunId, phase: CheckpointPhase, manifest: ResultManifest },
    RunInspect { project_id: ProjectId, run_id: RunId },
    RunReconcile { project_id: ProjectId, run_id: RunId, decision: RecoveryDecision },
    RunCancel { project_id: ProjectId, run_id: RunId },
    RunObserveStop { project_id: ProjectId, run_id: RunId },
    RunNativeDone { project_id: ProjectId, run_id: RunId, receipt: String },
    KnowledgeIngest { project_id: ProjectId, source: SourceInput, disclosure: DisclosurePolicy },
    KnowledgeRetrieve { project_id: ProjectId, query: String, #[ts(type = "number")] limit: u32 },
    KnowledgeOpenCitation { project_id: ProjectId, citation: Citation },
    KnowledgeDeleteSource { project_id: ProjectId, source_id: String },
    KnowledgeBuildContext { project_id: ProjectId, query: String, #[ts(type = "number")] limit: u32 },
    MemoryPropose { project_id: ProjectId, text: String, source: String },
    MemoryDecide { project_id: ProjectId, candidate_id: String, decision: MemoryDecision, #[serde(skip_serializing_if = "Option::is_none")] #[ts(optional)] edited_text: Option<String> },
    MemoryForget { project_id: ProjectId, record_id: String },
    KnowledgeSearchMemory { project_id: ProjectId, query: String, #[ts(type = "number")] limit: u32 },
    KnowledgeEmbedIndex { project_id: ProjectId, model: EmbeddingModelIdentity },
    KnowledgeEmbedQuery { project_id: ProjectId, model: EmbeddingModelIdentity, query: String },
    WebGptProbe { connection: ConnectionConfig },
    WebGptActivate { connection_id: String },
    WebGptSubmit { run_id: RunId, content: Vec<ContentPart>, idempotency_key: String },
    WebGptResubmit { run_id: RunId, receipt_digest: Digest },
    WebGptTurn { run_id: RunId, input: WebGptTurnInput },
    JudgmentQualify { domain: JudgmentDomain, report: EvaluationReportInput, calibrator_digest: Digest },
    JudgmentQualificationList { domain: JudgmentDomain },
    AutomationObserve { target: ApprovedTarget },
    AutomationExecute { action: ActionSpec, grant: ApprovalGrant, expected_state_digest: Digest },
    AutomationRelease { scope_id: String } }

/// Authenticated read of a CAS object over the control socket. The object bytes are
/// base64-encoded in the JSON reply so raw, digest-verified bytes can cross the
/// 1 MiB frame; the CLI decodes and streams them to stdout. Objects beyond the
/// recoverable IPC budget are refused with `artifact_too_large_for_ipc`, never truncated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
pub struct ArtifactRead { pub digest: Digest, #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub bytes: u64, pub data: String }

/// The typed success payload of a `CoreReply::Ok`. One variant per public
/// `CoreCommand`, tagged by the command's serde `type` via the `command` field,
/// carrying that command's actual leaf result type. A reply can only ever
/// unwrap the variant whose command matches the request; a mismatched or echoed
/// payload fails serde (unknown/missing command tag) and the TS validator
/// (`packages/contracts/src/validate.ts` + `ResultOf<C>`) before any consumer
/// sees it. Intentional user JSON stays an explicitly bounded `serde_json::Value`
/// inside the leaf DTOs (e.g. `canonical_args`), never this envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandResult {
    Status { value: StatusReport },
    Events { value: Vec<EventEnvelope> },
    ProjectList { value: Vec<Project> },
    ProjectCreate { value: Project },
    Doctor { value: DoctorReport },
    Shutdown { value: ShutdownReceipt },
    ProjectPolicySet { value: ProjectPolicy },
    ProjectPolicyShow { value: ProjectPolicy },
    ArtifactRead { value: ArtifactRead },
    ImportPreview { value: ImportPreview },
    ImportApply { value: ImportReceipt },
    BackupCreate { value: BackupManifest },
    RetentionSweep { value: RetentionReport },
    ObjectCapacity { value: CapacityReport },
    ConversationCreate { value: ConversationId },
    ConversationHistory { value: ConversationHistory },
    ConversationRename { value: Empty },
    ConversationSearch { value: Vec<ConversationSummary> },
    ConversationExport { value: ConversationExport },
    ConversationEdit { value: BranchId },
    ConversationQueueTurn { value: QueuedTurn },
    ConversationRegenerate { value: QueuedTurn },
    ConversationArchive { value: Empty },
    ConversationDelete { value: Empty },
    RunInterrupt { value: Empty },
    RunCreate { value: RunReceipt },
    RunStart { value: RunState },
    RunPause { value: RunState },
    RunResume { value: RunState },
    RunCheckpoint { value: ArtifactId },
    RunInspect { value: RecoveryReport },
    RunReconcile { value: Empty },
    RunCancel { value: RunState },
    RunObserveStop { value: RunState },
    RunNativeDone { value: RunState },
    KnowledgeIngest { value: SourceRevision },
    KnowledgeRetrieve { value: Vec<CitedExcerpt> },
    KnowledgeOpenCitation { value: CitationView },
    KnowledgeDeleteSource { value: DeletionReceipt },
    KnowledgeBuildContext { value: ContextPackage },
    MemoryPropose { value: String },
    MemoryDecide { value: MemoryRecord },
    MemoryForget { value: MemoryRecord },
    KnowledgeSearchMemory { value: Vec<MemoryRecord> },
    KnowledgeEmbedIndex { value: EmbeddingModelIdentity },
    KnowledgeEmbedQuery { value: String },
    WebGptProbe { value: WebGptStatus },
    WebGptActivate { value: WebGptStatus },
    WebGptSubmit { value: WebGptSubmissionReceipt },
    WebGptResubmit { value: WebGptSubmissionReceipt },
    WebGptTurn { value: WebGptTurnOutput },
    JudgmentQualify { value: QualificationRecord },
    JudgmentQualificationList { value: DomainQualifications },
    AutomationObserve { value: AutomationObservation },
    AutomationExecute { value: AutomationReceipt },
    AutomationRelease { value: Empty },
}

impl CommandResult {
    /// The leaf result value as JSON, for CLI/consumer display. The command tag
    /// is the caller's concern; every variant carries exactly one `value`.
    pub fn leaf_json(&self) -> serde_json::Value {
        macro_rules! leaf {
            ($($variant:ident),* $(,)?) => {
                match self {
                    $(CommandResult::$variant { value } => serde_json::to_value(value).unwrap_or(serde_json::Value::Null),)*
                }
            }
        }
        leaf!(Status, Events, ProjectList, ProjectCreate, Doctor, Shutdown, ProjectPolicySet, ProjectPolicyShow, ArtifactRead,
            ImportPreview, ImportApply, BackupCreate, RetentionSweep, ObjectCapacity, ConversationCreate, ConversationHistory,
            ConversationRename, ConversationSearch, ConversationExport, ConversationEdit, ConversationQueueTurn,
            ConversationRegenerate, ConversationArchive, ConversationDelete, RunInterrupt, RunCreate, RunStart, RunPause,
            RunResume, RunCheckpoint, RunInspect, RunReconcile, RunCancel, RunObserveStop, RunNativeDone, KnowledgeIngest,
            KnowledgeRetrieve, KnowledgeOpenCitation, KnowledgeDeleteSource, KnowledgeBuildContext, MemoryPropose,
            MemoryDecide, MemoryForget, KnowledgeSearchMemory, KnowledgeEmbedIndex, KnowledgeEmbedQuery, WebGptProbe,
            WebGptActivate, WebGptSubmit, WebGptResubmit, WebGptTurn, JudgmentQualify, JudgmentQualificationList,
            AutomationObserve, AutomationExecute, AutomationRelease)
    }
}

/// The unit result of commands whose leaf outcome carries no value. Serde
/// renders it `{}`, matching the pre-envelope wire shape of those replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// `Status {}` reply: durable counters, all on the exact numeric wire (counts
/// are SQLite integers, not JS-safe by construction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    pub protocol: u8,
    #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub projects: u64,
    #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub last_seq: u64,
    #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub abandoned_requests: u64,
}

/// `Doctor {}` reply: live SQLite integrity facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DoctorReport {
    pub protocol: u8,
    pub sqlite_integrity: String,
    pub foreign_keys: bool,
    #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] pub schema_version: u64,
    pub journal_mode: String,
}

/// `Shutdown {}` reply: the daemon acknowledged the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ShutdownReceipt { pub shutting_down: bool }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreReply { Ok { value: CommandResult }, Error { code: String, message: String, #[ts(type = "unknown")] details: serde_json::Value } }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
// `Request` embeds the full command by design (the frame IS the command envelope). Boxing
// would force a deref at every `ipc.rs` call site and is not warranted for a control-plane
// message that serde heap-allocates anyway.
#[allow(clippy::large_enum_variant)]
pub enum ClientFrame { Hello { #[ts(type = "1")] #[schemars(range(min = 1, max = 1))] protocol: u8, session_nonce: String }, Request { request_id: Uuid, command: CoreCommand }, Subscribe { #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] after_seq: u64 } }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerFrame { Hello { #[ts(type = "1")] #[schemars(range(min = 1, max = 1))] protocol: u8 }, Reply { request_id: Uuid, reply: CoreReply }, Event { event: Box<EventEnvelope> }, Error { code: String, message: String, #[serde(with = "wire::u64")] #[schemars(with = "wire::WireU64Schema")] #[ts(type = "string")] after_seq: u64 } }

// Root schema excludes private native-worker credential lease DTOs.
#[derive(JsonSchema)]
pub struct ContractSchema { pub client: ClientFrame, pub server: ServerFrame, pub policy: ProjectPolicy, pub context: ContextPackage, pub result: ResultManifest, pub provider: ConnectionConfig, pub judgment_input: JudgmentInput, pub worker_event: WorkerEvent, pub open_session: OpenSessionInput, pub turn: TurnInput, pub permission: PermissionAnswer }

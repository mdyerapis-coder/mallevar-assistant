use std::{fs, path::PathBuf};
use ironbark_core::contracts::*;
use ironbark_core::judgments::qualification::{ClassMetrics, DomainQualifications, EvaluationReportInput, GateVerdict, HoldoutMetrics, LatencyMs, SplitDigests};
use ironbark_core::policy::service::ActionOutcome;
use ironbark_core::connections::webgpt::{WebGptActivationState, WebGptStatus, WebGptSubmissionReceipt, WebGptSubmissionState, WebGptTurnInput, WebGptTurnOutput};
use ironbark_core::conversations::{ArtifactReceipt, BranchProvenance, BranchView, ConversationExport, ConversationHistory, ConversationSummary, ExportFormat, MessageView, QueuedTurn, RunResolution, TurnDispatch, TurnSnapshot};
use ironbark_core::store::backup::{BackupManifest, BackupObject, RestoreReport};
use ironbark_core::store::imports::{ImportFile, ImportFormat, ImportPreview, ImportReceipt, ImportSource};
use ironbark_core::policy::{TaskAuthority, TaskScope};
use ironbark_core::scheduler::RunReceipt;
use ironbark_core::store::objects::CapacityReport;
use ironbark_core::store::retention::RetentionReport;
use ironbark_core::automation::{ApprovedTarget, AutomationObservation, AutomationReceipt, Effect, TargetKind, ScopeRole, ScopePhase, ConsentKind, ConsentSource, ConsentRecord};
use ts_rs::TS;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let directory = root.join("packages/contracts");
    fs::create_dir_all(directory.join("src"))?;
    let mut output = String::from("// Generated from ironbark-core::contracts by export_contracts. Do not edit.\n// Protocol 1. Unix timestamps are integer milliseconds; micro-USD never floats.\n");
    macro_rules! emit { ($($name:ty),* $(,)?) => { $( output.push_str("export "); output.push_str(&<$name>::decl()); output.push('\n'); )* }; }
    emit!(ProjectId, ConversationId, BranchId, MessageId, JobId, NodeId, RunId, ActionId, GrantId, ArtifactId, DecisionId,
        Digest, HarnessKind, CapabilityState, CapabilityObservation, RoutePin, SpendPolicy, ProjectPolicy, RunState,
        ActionSpec, ApprovalGrant, SessionRef, CitationLocator, Citation, CitedExcerpt, ContextPackage, CheckReceipt,
        ResultManifest, ContentPart, ToolDescriptor, OpenSessionInput, TurnInput, PermissionDecision, PermissionAnswer,
        HarnessDescriptor, ProviderProtocol, AuthRef, Capability, ModelDescriptor, ConnectionConfig, ConnectionDescriptor,
        ProviderRole, ProviderToolCall, ProviderMessage, ProviderTurnInput, WorkerProviderTurnInput, BillingObservation,
        BillingState, UsageSource, UsageObservation, ScoreEvidence, JudgmentState, JudgmentInput, JudgmentRecord,
        CoreEvent, WorkerEvent, EventEnvelope, Project, CoreCommand, CoreReply, CommandResult,
        StatusReport, DoctorReport, ShutdownReceipt, Empty, ClientFrame, ServerFrame,
        ConversationSummary, BranchView, MessageView, BranchProvenance, ConversationHistory, ConversationExport, ExportFormat,
        QueuedTurn, TurnDispatch, TurnSnapshot, ArtifactReceipt, RunResolution,
        ImportSource, ImportFormat, ImportFile, ImportPreview, ImportReceipt, CapacityReport, RetentionReport,
        BackupObject, BackupManifest, RestoreReport, ArtifactRead,
        CheckpointPhase, CheckpointRecord, RecoveryReport, RecoveryDecision, RunReceipt,
        JobGraph, JobNode, RoleDefinition, DelegatedLimits, WorkspaceMode, TaskAuthority, TaskScope,
        SourceInput, DisclosurePolicy, SourceRevision, CitationView,
        DeletionScope, DeletionReceipt,
        MemoryCandidate, MemoryDecision, MemoryRecord, EmbeddingModelIdentity, EmbeddingIndexReceipt,
        JudgmentDomain, QualificationState, QualificationRecord, DomainQualifications, AdvisoryOutcome,
        EvaluationReportInput, ClassMetrics, SplitDigests, LatencyMs, GateVerdict, HoldoutMetrics,
        // Policy service outcomes referenced by automation receipts.
        ActionOutcome,
        // WebGPT DTOs
        WebGptStatus, WebGptSubmissionReceipt, WebGptTurnInput, WebGptTurnOutput,
        WebGptActivationState, WebGptSubmissionState,
        // Automation DTOs
        ApprovedTarget, AutomationObservation, AutomationReceipt, Effect,
        TargetKind, ScopeRole, ScopePhase,
        ConsentKind, ConsentSource, ConsentRecord);
    // Interfaces describe real worker implementations and import only generated DTOs.
    output.push_str("\nexport interface HarnessAdapter {\n  describe(): Promise<HarnessDescriptor>;\n  open(input: OpenSessionInput): Promise<SessionRef>;\n  turn(session: SessionRef, input: TurnInput): AsyncIterable<WorkerEvent>;\n  answerPermission(input: PermissionAnswer): Promise<void>;\n  interrupt(session: SessionRef): Promise<void>;\n  exportContext(session: SessionRef): Promise<ContextPackage>;\n  close(session: SessionRef): Promise<void>;\n}\nexport interface ProviderAdapter {\n  describe(connection: ConnectionConfig): Promise<ConnectionDescriptor>;\n  stream(input: WorkerProviderTurnInput, signal: AbortSignal): AsyncIterable<WorkerEvent>;\n}\nexport interface CoreClient {\n  request<C extends CoreCommand>(command: C, requestId?: string): Promise<ResultOf<C>>;\n  events(afterSeq: string): AsyncIterable<EventEnvelope>;\n}\n// The typed value of one command's reply: the CommandResult variant whose\n// `command` tag matches the request, unwrapped to its leaf. A result whose tag\n// does not match the request (mismatched or echoed) cannot satisfy this type\n// and is rejected at runtime by packages/contracts/src/validate.ts.\nexport type ResultOf<C extends CoreCommand> = Extract<CommandResult, { command: C[\"type\"] }>[\"value\"];\n");
    fs::write(directory.join("src/generated.ts"), output)?;
    let mut schema = serde_json::to_value(schemars::schema_for!(ContractSchema))?;
    schema.as_object_mut().ok_or("schema root is not an object")?.insert("x-ironbark-protocol".into(), serde_json::json!(1));
    fs::write(directory.join("schema.json"), format!("{}\n", serde_json::to_string_pretty(&schema)?))?;
    println!("Exported protocol 1 contracts and public schema (no worker lease material)");
    Ok(())
}

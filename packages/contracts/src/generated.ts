// Generated from ironbark-core::contracts by export_contracts. Do not edit.
// Protocol 1. Unix timestamps are integer milliseconds; micro-USD never floats.
export type ProjectId = string;
export type ConversationId = string;
export type BranchId = string;
export type MessageId = string;
export type JobId = string;
export type NodeId = string;
export type RunId = string;
export type ActionId = string;
export type GrantId = string;
export type ArtifactId = string;
export type DecisionId = string;
export type Digest = string;
export type HarnessKind = "omp" | "codex" | "opencode" | "claude" | "hermes" | "alphacode";
export type CapabilityState = "unknown" | "unavailable" | "unqualified" | "qualified";
export type CapabilityObservation = { name: string, state: CapabilityState, implementation_version: string, evidence_digest?: Digest, observed_at: string, };
export type RoutePin = { connection_id: string, model_id: string, harness?: HarnessKind, };
export type SpendPolicy = "free_only" | "paid_allowed";
export type ProjectPolicy = { version: string, project_id: ProjectId, allowed_connection_ids: Array<string>, allowed_tools: Array<string>, read_roots: Array<string>, write_roots: Array<string>, network_destinations: Array<string>, local_only: boolean, max_workers: number, max_requests: number, max_cost_microusd?: string, spend_policy: SpendPolicy, allow_unknown_cost: boolean, };
export type RunState = "queued" | "ready" | "running" | "awaiting_approval" | "blocked" | "reviewing" | "verifying" | "paused" | "cancelling" | "cancelled" | "succeeded" | "failed";
export type ActionSpec = { id: ActionId, run_id: RunId, project_id: ProjectId, kind: string, canonical_args: unknown, scope: unknown, destination?: string, policy_version: string, expected_state_digest: Digest, };
export type ApprovalGrant = { id: GrantId, action_id: ActionId, action_digest: Digest, policy_version: string, expected_state_digest: Digest, expires_at: string, revoked_at?: string, };
export type SessionRef = { harness: HarnessKind, native_id: string, native_version: string, project_id: ProjectId, cwd: string, resolved_route: RoutePin, };
export type CitationLocator = { page?: number, section?: string, line_start?: number, line_end?: number, sheet?: string, cell_range?: string, bounding_box?: [number, number, number, number], time_start_ms?: string, time_end_ms?: string, };
export type Citation = { source_id: string, revision_digest: Digest, locator: CitationLocator, };
export type CitedExcerpt = { text: string, citation: Citation, project_id: ProjectId, disclosure: DisclosurePolicy, };
export type ContextPackage = { digest: Digest, brief: string, constraints: Array<string>, excerpts: Array<CitedExcerpt>, artifacts: Array<ArtifactId>, unresolved_questions: Array<string>, originating_sessions: Array<SessionRef>, };
export type CheckReceipt = { name: string, passed: boolean, evidence_digest?: Digest, details: string, };
export type ResultManifest = { run_id: RunId, native_session?: SessionRef, artifacts: Array<ArtifactId>, checks: Array<CheckReceipt>, acceptance_satisfied: boolean, unresolved: Array<string>, };
export type ContentPart = { "type": "text", text: string, } | { "type": "image", uri: string, mime_type: string, } | { "type": "audio", uri: string, mime_type: string, } | { "type": "artifact", artifact_id: ArtifactId, };
export type ToolDescriptor = { name: string, description: string, input_schema: unknown, };
export type OpenSessionInput = { run_id: RunId, project_id: ProjectId, cwd: string, route: RoutePin, context: ContextPackage, policy: ProjectPolicy, resume?: SessionRef, };
export type TurnInput = { request_id: string, message_id: MessageId, content: Array<ContentPart>, allowed_tools: Array<ToolDescriptor>, budget: ProjectPolicy, };
export type PermissionDecision = "allow" | "deny";
export type PermissionAnswer = { native_request_id: string, action: ActionSpec, grant?: ApprovalGrant, decision: PermissionDecision, };
export type HarnessDescriptor = { harness: HarnessKind, version: string, transport: string, route_compatibility: Array<string>, approval: CapabilityState, cancel: CapabilityState, resume: CapabilityState, child_observability: CapabilityState, capabilities: Array<CapabilityObservation>, };
export type ProviderProtocol = "openai_completions" | "openai_responses" | "anthropic" | "gemini" | "ollama" | "llama_cpp" | "custom";
export type AuthRef = { "type": "secret_service", service: string, account: string, } | { "type": "bitwarden", item_id: string, field: string, } | { "type": "native_session", harness: HarnessKind, profile: string, };
export type Capability = "text" | "tools" | "vision" | "audio" | "embeddings" | "transcription" | "reasoning";
export type ModelDescriptor = { id: string, context_limit: number, output_limit: number, capabilities: Array<CapabilityObservation>, };
export type ConnectionConfig = { id: string, protocol: ProviderProtocol, endpoint: string, models: Array<ModelDescriptor>, auth_ref?: AuthRef, allowed_projects: Array<ProjectId>, };
export type ConnectionDescriptor = { id: string, protocol: ProviderProtocol, endpoint: string, models: Array<ModelDescriptor>, capabilities: Array<CapabilityObservation>, availability: CapabilityState, auth_available: boolean, };
export type ProviderRole = "system" | "user" | "assistant" | "tool";
export type ProviderToolCall = { id: string, name: string, arguments: unknown, };
export type ProviderMessage = { role: ProviderRole, content: Array<ContentPart>, tool_call_id?: string, tool_calls?: Array<ProviderToolCall>, sdk_content?: unknown, sdk_route?: RoutePin, tool_result_error?: boolean, };
export type ProviderTurnInput = { run_id: RunId, request_id: string, message_id: MessageId, route: RoutePin, messages: Array<ProviderMessage>, required_capabilities: Array<Capability>, tool_schemas: Array<ToolDescriptor>, disclosure_digest: Digest, policy: ProjectPolicy, max_output_tokens?: number, };
export type WorkerProviderTurnInput = { credential_lease_id: string, connection: ConnectionConfig, run_id: RunId, request_id: string, message_id: MessageId, route: RoutePin, messages: Array<ProviderMessage>, required_capabilities: Array<Capability>, tool_schemas: Array<ToolDescriptor>, disclosure_digest: Digest, policy: ProjectPolicy, max_output_tokens?: number, };
export type BillingObservation = { state: BillingState, cost_microusd?: string, };
export type BillingState = "unknown" | "reported";
export type UsageSource = "provider" | "unknown";
export type UsageObservation = { input_tokens?: number, output_tokens?: number, cost_microusd?: string, billing: BillingObservation, source: UsageSource, };
export type ScoreEvidence = { "type": "none", } | { "type": "raw_token_log_prob", value: number, token: string, token_position: number, } | { "type": "calibrated_selective_risk", upper_error_bound: number, dataset_digest: Digest, calibrator_digest: Digest, };
export type JudgmentState = "available" | "invalid" | "abstain" | "unqualified" | "unavailable";
export type JudgmentInput = { decision_id: DecisionId, domain: string, domain_version: string, candidates: Array<string>, input_refs: Array<string>, input_digest: Digest, criterion: string, route_policy_digest: Digest, };
export type JudgmentRecord = { decision_id: DecisionId, domain: string, domain_version: string, candidates: Array<string>, input_refs: Array<string>, input_digest: Digest, criterion: string, route_policy_digest: Digest, model_identity: string, runtime_identity_digest: Digest, prompt_digest: Digest, candidate_codes: { [key in string]?: string }, choice?: string, state: JudgmentState, score: ScoreEvidence, latency_ms: string, qualification_digest?: Digest, reason?: string, };
export type CoreEvent = { "type": "turn_started", message_id: MessageId, } | { "type": "text_delta", message_id: MessageId, delta: string, } | { "type": "text_replace", message_id: MessageId, text: string, } | { "type": "reasoning_delta", message_id: MessageId, delta: string, } | { "type": "provider_tool_call", message_id: MessageId, call: ProviderToolCall, } | { "type": "provider_message", message_id: MessageId, content: unknown, route: RoutePin, } | { "type": "tool_started", action: ActionSpec, } | { "type": "tool_finished", action_id: ActionId, result: unknown, } | { "type": "approval_needed", action: ActionSpec, } | { "type": "state_changed", entity_id: string, state: string, } | { "type": "session_bound", session: SessionRef, } | { "type": "artifact_added", artifact_id: ArtifactId, digest: Digest, } | { "type": "judgment_recorded", judgment: JudgmentRecord, } | { "type": "usage_observed", usage: UsageObservation, } | { "type": "error", code: string, message: string, details: unknown, } | { "type": "turn_finished", message_id: MessageId, status: RunState, stop_reason?: string, };
export type WorkerEvent = { session_id?: string, turn_id: string, tool_id?: string, run_id: RunId, payload: CoreEvent, };
export type EventEnvelope = { protocol: 1, seq: string, request_id?: string, project_id?: ProjectId, run_id?: RunId, timestamp: string, payload: CoreEvent, };
export type Project = { id: ProjectId, name: string, root: string, created_at: string, };
export type CoreCommand = { "type": "status", } | { "type": "events", after: string, } | { "type": "project_list", } | { "type": "project_create", name: string, root: string, } | { "type": "doctor", } | { "type": "shutdown", } | { "type": "project_policy_set", project_id: ProjectId, policy: ProjectPolicy, } | { "type": "project_policy_show", project_id: ProjectId, } | { "type": "artifact_read", project_id: ProjectId, artifact_id: ArtifactId, } | { "type": "import_preview", project_id: ProjectId, source: ImportSource, } | { "type": "import_apply", project_id: ProjectId, preview_digest: Digest, } | { "type": "backup_create", destination: string, } | { "type": "retention_sweep", } | { "type": "object_capacity", project_id: ProjectId, } | { "type": "conversation_create", project_id: ProjectId, } | { "type": "conversation_history", conversation_id: ConversationId, } | { "type": "conversation_rename", conversation_id: ConversationId, title: string, } | { "type": "conversation_search", project_id: ProjectId, query: string, include_archived: boolean, limit: number, } | { "type": "conversation_export", conversation_id: ConversationId, format: ExportFormat, } | { "type": "conversation_edit", message_id: MessageId, parts: Array<ContentPart>, } | { "type": "conversation_queue_turn", conversation_id: ConversationId, branch_id: BranchId, parts: Array<ContentPart>, pin: RoutePin, context?: ContextPackage, } | { "type": "conversation_regenerate", message_id: MessageId, pin: RoutePin, } | { "type": "conversation_archive", conversation_id: ConversationId, } | { "type": "conversation_delete", conversation_id: ConversationId, resolution: RunResolution, } | { "type": "run_interrupt", run_id: RunId, } | { "type": "run_create", project_id: ProjectId, task: string, } | { "type": "run_start", project_id: ProjectId, run_id: RunId, pin: RoutePin, native_session: SessionRef | null, artifacts: Array<ArtifactId>, recorded_pid?: string, recorded_start_time?: string, } | { "type": "run_pause", project_id: ProjectId, run_id: RunId, } | { "type": "run_resume", project_id: ProjectId, run_id: RunId, } | { "type": "run_checkpoint", project_id: ProjectId, run_id: RunId, phase: CheckpointPhase, manifest: ResultManifest, } | { "type": "run_inspect", project_id: ProjectId, run_id: RunId, } | { "type": "run_reconcile", project_id: ProjectId, run_id: RunId, decision: RecoveryDecision, } | { "type": "run_cancel", project_id: ProjectId, run_id: RunId, } | { "type": "run_observe_stop", project_id: ProjectId, run_id: RunId, } | { "type": "run_native_done", project_id: ProjectId, run_id: RunId, receipt: string, } | { "type": "knowledge_ingest", project_id: ProjectId, source: SourceInput, disclosure: DisclosurePolicy, } | { "type": "knowledge_retrieve", project_id: ProjectId, query: string, limit: number, } | { "type": "knowledge_open_citation", project_id: ProjectId, citation: Citation, } | { "type": "knowledge_delete_source", project_id: ProjectId, source_id: string, } | { "type": "knowledge_build_context", project_id: ProjectId, query: string, limit: number, } | { "type": "memory_propose", project_id: ProjectId, text: string, source: string, } | { "type": "memory_decide", project_id: ProjectId, candidate_id: string, decision: MemoryDecision, edited_text?: string, } | { "type": "memory_forget", project_id: ProjectId, record_id: string, } | { "type": "knowledge_search_memory", project_id: ProjectId, query: string, limit: number, } | { "type": "knowledge_embed_index", project_id: ProjectId, model: EmbeddingModelIdentity, } | { "type": "knowledge_embed_query", project_id: ProjectId, model: EmbeddingModelIdentity, query: string, } | { "type": "web_gpt_probe", connection: ConnectionConfig, } | { "type": "web_gpt_activate", connection_id: string, } | { "type": "web_gpt_submit", run_id: RunId, content: Array<ContentPart>, idempotency_key: string, } | { "type": "web_gpt_resubmit", run_id: RunId, receipt_digest: Digest, } | { "type": "web_gpt_turn", run_id: RunId, input: WebGptTurnInput, } | { "type": "judgment_qualify", domain: JudgmentDomain, report: EvaluationReportInput, calibrator_digest: Digest, } | { "type": "judgment_qualification_list", domain: JudgmentDomain, } | { "type": "automation_observe", target: ApprovedTarget, } | { "type": "automation_execute", action: ActionSpec, grant: ApprovalGrant, expected_state_digest: Digest, } | { "type": "automation_release", scope_id: string, };
export type CoreReply = { "type": "ok", value: CommandResult, } | { "type": "error", code: string, message: string, details: unknown, };
export type CommandResult = { "command": "status", value: StatusReport, } | { "command": "events", value: Array<EventEnvelope>, } | { "command": "project_list", value: Array<Project>, } | { "command": "project_create", value: Project, } | { "command": "doctor", value: DoctorReport, } | { "command": "shutdown", value: ShutdownReceipt, } | { "command": "project_policy_set", value: ProjectPolicy, } | { "command": "project_policy_show", value: ProjectPolicy, } | { "command": "artifact_read", value: ArtifactRead, } | { "command": "import_preview", value: ImportPreview, } | { "command": "import_apply", value: ImportReceipt, } | { "command": "backup_create", value: BackupManifest, } | { "command": "retention_sweep", value: RetentionReport, } | { "command": "object_capacity", value: CapacityReport, } | { "command": "conversation_create", value: ConversationId, } | { "command": "conversation_history", value: ConversationHistory, } | { "command": "conversation_rename", value: Empty, } | { "command": "conversation_search", value: Array<ConversationSummary>, } | { "command": "conversation_export", value: ConversationExport, } | { "command": "conversation_edit", value: BranchId, } | { "command": "conversation_queue_turn", value: QueuedTurn, } | { "command": "conversation_regenerate", value: QueuedTurn, } | { "command": "conversation_archive", value: Empty, } | { "command": "conversation_delete", value: Empty, } | { "command": "run_interrupt", value: Empty, } | { "command": "run_create", value: RunReceipt, } | { "command": "run_start", value: RunState, } | { "command": "run_pause", value: RunState, } | { "command": "run_resume", value: RunState, } | { "command": "run_checkpoint", value: ArtifactId, } | { "command": "run_inspect", value: RecoveryReport, } | { "command": "run_reconcile", value: Empty, } | { "command": "run_cancel", value: RunState, } | { "command": "run_observe_stop", value: RunState, } | { "command": "run_native_done", value: RunState, } | { "command": "knowledge_ingest", value: SourceRevision, } | { "command": "knowledge_retrieve", value: Array<CitedExcerpt>, } | { "command": "knowledge_open_citation", value: CitationView, } | { "command": "knowledge_delete_source", value: DeletionReceipt, } | { "command": "knowledge_build_context", value: ContextPackage, } | { "command": "memory_propose", value: string, } | { "command": "memory_decide", value: MemoryRecord, } | { "command": "memory_forget", value: MemoryRecord, } | { "command": "knowledge_search_memory", value: Array<MemoryRecord>, } | { "command": "knowledge_embed_index", value: EmbeddingModelIdentity, } | { "command": "knowledge_embed_query", value: string, } | { "command": "web_gpt_probe", value: WebGptStatus, } | { "command": "web_gpt_activate", value: WebGptStatus, } | { "command": "web_gpt_submit", value: WebGptSubmissionReceipt, } | { "command": "web_gpt_resubmit", value: WebGptSubmissionReceipt, } | { "command": "web_gpt_turn", value: WebGptTurnOutput, } | { "command": "judgment_qualify", value: QualificationRecord, } | { "command": "judgment_qualification_list", value: DomainQualifications, } | { "command": "automation_observe", value: AutomationObservation, } | { "command": "automation_execute", value: AutomationReceipt, } | { "command": "automation_release", value: Empty, };
export type StatusReport = { protocol: number, projects: string, last_seq: string, abandoned_requests: string, };
export type DoctorReport = { protocol: number, sqlite_integrity: string, foreign_keys: boolean, schema_version: string, journal_mode: string, };
export type ShutdownReceipt = { shutting_down: boolean, };
export type Empty = Record<string, never>;
export type ClientFrame = { "type": "hello", protocol: 1, session_nonce: string, } | { "type": "request", request_id: string, command: CoreCommand, } | { "type": "subscribe", after_seq: string, };
export type ServerFrame = { "type": "hello", protocol: 1, } | { "type": "reply", request_id: string, reply: CoreReply, } | { "type": "event", event: EventEnvelope, } | { "type": "error", code: string, message: string, after_seq: string, };
export type ConversationSummary = { id: ConversationId, title: string, archived: boolean, created_at: string, };
export type BranchView = { id: BranchId, parent_id?: BranchId, provenance: BranchProvenance, messages: Array<MessageView>, };
export type MessageView = { id: MessageId, origin_branch_id: BranchId, role: string, parts: Array<ContentPart>, status: string, created_at: string, run_id?: RunId, snapshot?: TurnSnapshot, receipts: Array<EventEnvelope>, };
export type BranchProvenance = { operation: string, original_message_id?: MessageId, preserved_run_ids: Array<RunId>, prior_effects_undone: boolean, };
export type ConversationHistory = { id: ConversationId, project_id: ProjectId, title: string, archived: boolean, branches: Array<BranchView>, };
export type ConversationExport = { conversation_id: ConversationId, format: ExportFormat, content: string, };
export type ExportFormat = "json" | "markdown";
export type QueuedTurn = { run_id: RunId, branch_id: BranchId, user_message_id: MessageId, assistant_message_id: MessageId, replayed: boolean, };
export type TurnDispatch = { run_id: RunId, request_id: string, branch_id: BranchId, user_message_id: MessageId, assistant_message_id: MessageId, snapshot: TurnSnapshot, messages: Array<MessageView>, };
export type TurnSnapshot = { route: RoutePin, context: ContextPackage, disclosure_digest: Digest, policy: ProjectPolicy, artifact_receipts: Array<ArtifactReceipt>, };
export type ArtifactReceipt = { artifact_id: ArtifactId, digest: Digest, origin_run_id?: RunId, };
export type RunResolution = { run_ids: Array<RunId>, };
export type ImportSource = { path: string, format: ImportFormat, };
export type ImportFormat = { "type": "document" } | { "type": "native_transcript", origin: string, };
export type ImportFile = { path: string, digest: Digest, bytes: string, device: string, inode: string, };
export type ImportPreview = { digest: Digest, project_id: ProjectId, kind: string, files: Array<ImportFile>, additive: boolean, archived_only: boolean, warnings: Array<string>, };
export type ImportReceipt = { preview_digest: Digest, project_id: ProjectId, kind: string, origin: string, source_digest: Digest, artifact_id?: ArtifactId, source_id?: string, connection_id?: string, archived_only: boolean, applied_at: string, };
export type CapacityReport = { bytes: string, soft_bytes: string, hard_bytes: string, soft_exceeded: boolean, };
export type RetentionReport = { removed_objects: string, removed_staging_files: string, reclaimed_bytes: string, retained_objects: string, };
export type BackupObject = { digest: Digest, bytes: string, };
export type BackupManifest = { format: number, created_at: string, database_digest: Digest, database_bytes: string, objects: Array<BackupObject>, };
export type RestoreReport = { database_digest: Digest, restored_objects: string, blocked_runs: string, stopped: boolean, namespace: string, };
export type ArtifactRead = { digest: Digest, bytes: string, data: string, };
export type CheckpointPhase = "planned" | "pre_mutation" | "post_mutation" | "post_verification" | "pre_consequential";
export type CheckpointRecord = { run_id: RunId, phase: CheckpointPhase, state_digest: Digest, manifest: unknown, created_at: string, };
export type RecoveryReport = { run_id: RunId, run_state: RunState, recorded_pid?: string, recorded_start_time?: string, checkpoint?: CheckpointRecord, observed_workspace: unknown, pending_mutation: boolean, uncertain_actions: Array<string>, recommended: Array<string>, };
export type RecoveryDecision = "accept_observed" | "resume_read_only" | "continue_from_checkpoint" | "abandon";
export type RunReceipt = { run_id: RunId, node_id: NodeId, job_id: JobId, project_id: ProjectId, state: RunState, };
export type JobGraph = { id: JobId, project_id: ProjectId, version: string, nodes: Array<JobNode>, };
export type JobNode = { id: NodeId, role_id: string, dependencies: Array<NodeId>, route_pin?: RoutePin, allowed_routes: Array<string>, inputs: Array<string>, acceptance: Array<string>, workspace_mode: WorkspaceMode, };
export type RoleDefinition = { id: string, instructions: string, required_capabilities: Array<Capability>, allowed_harnesses: Array<HarnessKind>, allowed_routes: Array<string>, source_scope: Array<string>, tool_scope: Array<string>, delegated_limits: DelegatedLimits, };
export type DelegatedLimits = { max_workers: number, max_requests: number, max_cost_microusd?: string, };
export type WorkspaceMode = "read_only" | "writer";
export type TaskAuthority = { run_id: RunId, project_id: ProjectId, owner_approval_id: GrantId, scope: TaskScope, policy_version: string, expires_at: string, };
export type TaskScope = { read_roots: Array<string>, write_roots: Array<string>, tools: Array<string>, connection_ids: Array<string>, network_destinations: Array<string>, };
export type SourceInput = { "type": "file", path: string, } | { "type": "attachment", artifact_id: ArtifactId, } | { "type": "fetched", url: string, receipt: Digest, };
export type DisclosurePolicy = { "type": "local_only" } | { "type": "allowed_connections", connection_ids: Array<string>, } | { "type": "explicit_public" };
export type SourceRevision = { source_id: string, project_id: ProjectId, digest: Digest, mime: string, extractor_id: string, extractor_version: string, locators: number, created_at: string, };
export type CitationView = { "type": "content", text: string, } | { "type": "deleted" } | { "type": "missing" };
export type DeletionScope = "source" | "project" | "all";
export type DeletionReceipt = { source_id: string, chunks_removed: number, vectors_removed: number, revisions_removed: number, applied_at: string, };
export type MemoryCandidate = { project_id: ProjectId, text: string, provenance: string, input_refs?: Array<string>, };
export type MemoryDecision = "approve" | "edit_and_approve" | "reject";
export type MemoryRecord = { id: string, project_id: ProjectId, text: string, state: string, provenance: string, created_at: string, };
export type EmbeddingModelIdentity = { connection_id: string, model_id: string, version: string, dimensions: number, };
export type EmbeddingIndexReceipt = { revision_id: string, model: EmbeddingModelIdentity, chunks_indexed: number, indexed_at: string, };
export type JudgmentDomain = "task_intent" | "retrieval_relevance" | "handoff_completeness" | "evidence_needed" | "allowed_route_choice";
export type QualificationState = "enabled" | "blocked";
export type QualificationRecord = { domain: JudgmentDomain, model_identity: string, prompt_digest: Digest, state: QualificationState, upper_error_bound: number, acceptance_coverage: number, holdout_size: number, evaluated_at: string, calibrator_digest: Digest, };
export type DomainQualifications = { enabled: QualificationRecord | null, blocked: Array<QualificationRecord>, };
export type AdvisoryOutcome = { "type": "qualified_choice", choice: string, decision_id: DecisionId, } | { "type": "needs_planner", reason: string, decision_id?: DecisionId, } | { "type": "unavailable" };
export type EvaluationReportInput = { domain: JudgmentDomain, domain_version: string, dataset_digest: Digest, prompt_digest: Digest, split_digests: SplitDigests, observed_identity_digest: Digest, observed_model_identity: string, claimed_identity: Digest, confidence_method: string, holdout: HoldoutMetrics, latency_ms: LatencyMs, evaluated_at?: string, };
export type ClassMetrics = { total: number, accepted: number, wrong: number, abstained: number, invalid: number, unavailable: number, unqualified: number, };
export type SplitDigests = { development: Digest, calibration: Digest, holdout: Digest, };
export type LatencyMs = { mean: number, p50: number, p95: number, };
export type GateVerdict = { state: QualificationState, reasons: Array<string>, };
export type HoldoutMetrics = { size: number, accepted: number, wrong_accepted: number, abstained: number, invalid: number, unavailable: number, unqualified: number, coverage: number, upper_error_bound: number, by_class: { [key in string]?: ClassMetrics }, };
export type ActionOutcome = "succeeded" | "failed" | "uncertain";
export type WebGptStatus = { state: WebGptActivationState, sidecar_version?: string, profile_digest?: Digest, reason?: string, };
export type WebGptSubmissionReceipt = { digest: Digest, run_id: RunId, idempotency_key: string, state: WebGptSubmissionState, 
/**
 * SHA-256 of the canonical content; a same-key/different-content submit is
 * a conflict, never a silent overwrite.
 */
submission_digest: Digest, 
/**
 * The native message id exactly as the sidecar wrote it. Absent when the
 * send could not be confirmed.
 */
native_message_id?: string, 
/**
 * The pinned sidecar version that produced this receipt.
 */
sidecar_version?: string, 
/**
 * The profile digest at send time, binding the receipt to the exact
 * activation the turn ran under.
 */
profile_digest?: Digest, 
/**
 * Image parts the sidecar received, as CAS objects owned by the run's
 * project; each digest is the image bytes' content digest.
 */
image_digests?: Array<Digest>, 
/**
 * The prompt text (all text parts, in order), kept for the audit trail.
 */
prompt_text: string, timestamp: string, 
/**
 * Set on a superseded view: the digest of the resubmit receipt.
 */
superseded_by?: Digest, 
/**
 * Set on a duplicate view: the digest of the original send's receipt.
 */
original?: Digest, 
/**
 * Human-readable cause for non-submitted states (never a secret).
 */
reason?: string, };
export type WebGptTurnInput = { content: Array<ContentPart>, 
/**
 * Native conversation to continue, when the sidecar reports one. A turn
 * never opens a fresh chat implicitly: continuation only follows a
 * sidecar-confirmed conversation id, never a retry of a failed send.
 */
conversation_id?: string, };
export type WebGptTurnOutput = { run_id: RunId, 
/**
 * The native message id exactly as the sidecar wrote it (its
 * confirmation file — not core-generated).
 */
native_message_id: string, 
/**
 * Native conversation identifier as reported by the sidecar.
 */
conversation_id: string, 
/**
 * The native model label as reported by the sidecar; the core never
 * substitutes its own identity.
 */
model: string, 
/**
 * The native reply text exactly as the sidecar returned it.
 */
response_text: string, sidecar_version: string, profile_digest: Digest, timestamp: string, };
export type WebGptActivationState = "not_installed" | "not_activated" | "activated" | "unavailable";
export type WebGptSubmissionState = "submitted" | "failed" | "duplicate" | "superseded";
export type ApprovedTarget = { scope_id: string, role: ScopeRole, kind: TargetKind, target_id: string, expires_at: string, };
export type AutomationObservation = { target_id: string, scope_id: string, backend: string, revision_digest: Digest, accessibility_tree?: unknown, image_artifact: ArtifactId | null, };
export type AutomationReceipt = { action_id: ActionId, grant_id: GrantId | null, target_id: string, backend: string, operation: string, outcome: ActionOutcome, effect: Effect, revision_before: Digest, revision_after: Digest, receipt_digest: Digest, error: string | null, };
export type Effect = "unchanged" | { "bytes_appended": { count: number, } } | { "executed": { description: string, } } | { "failed": { reason: string, } };
export type TargetKind = "browser_profile" | "native_window" | "document_file";
export type ScopeRole = "owner_operator" | "coding_worker";
export type ScopePhase = "active" | "released";
export type ConsentKind = "screenshot" | "get_cookies" | "set_cookies" | "clipboard_read" | "clipboard_write" | "file_upload";
export type ConsentSource = "owner" | "page_world";
export type ConsentRecord = { source: ConsentSource, kind: ConsentKind, scope_id: string, 
/**
 * SHA-256 of the disclosed subject (target identity / URL / file path).
 */
digest: Digest, };

export interface HarnessAdapter {
  describe(): Promise<HarnessDescriptor>;
  open(input: OpenSessionInput): Promise<SessionRef>;
  turn(session: SessionRef, input: TurnInput): AsyncIterable<WorkerEvent>;
  answerPermission(input: PermissionAnswer): Promise<void>;
  interrupt(session: SessionRef): Promise<void>;
  exportContext(session: SessionRef): Promise<ContextPackage>;
  close(session: SessionRef): Promise<void>;
}
export interface ProviderAdapter {
  describe(connection: ConnectionConfig): Promise<ConnectionDescriptor>;
  stream(input: WorkerProviderTurnInput, signal: AbortSignal): AsyncIterable<WorkerEvent>;
}
export interface CoreClient {
  request<C extends CoreCommand>(command: C, requestId?: string): Promise<ResultOf<C>>;
  events(afterSeq: string): AsyncIterable<EventEnvelope>;
}
// The typed value of one command's reply: the CommandResult variant whose
// `command` tag matches the request, unwrapped to its leaf. A result whose tag
// does not match the request (mismatched or echoed) cannot satisfy this type
// and is rejected at runtime by packages/contracts/src/validate.ts.
export type ResultOf<C extends CoreCommand> = Extract<CommandResult, { command: C["type"] }>["value"];

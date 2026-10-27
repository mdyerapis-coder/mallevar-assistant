//! Authenticated service dispatch for run-less C5 (objects/imports/backup/retention) and P2
//! (conversation tree) commands.
//!
//! These commands are NOT C1's single-transaction leaf operations: they reach the live Store
//! plus the on-disk content-addressed store, and each leaf commits its own durable mutation and
//! event inside its own transaction. The frame request ID stays the durable identity
//! (`accept_request` de-duplicates and binds it to the command digest); the service does the
//! work; `complete_request` then persists the redacted reply in a fresh transaction and marks
//! the request completed. A crash between accept and complete leaves the request `in_flight` —
//! a reconcile, never an automatic re-dispatch. `backup_restore` is deliberately excluded: it
//! is an offline, owner-selected CLI operation, not a live IPC mutation.
use std::path::{Path, PathBuf};
use uuid::Uuid;

use base64::Engine as _;
use crate::contracts::{ArtifactRead, CommandResult, CoreCommand, CoreReply, Empty, MemoryCandidate, ProjectId};
use crate::conversations::ConversationService;
use crate::error::{CoreError, Result};
use crate::paths::RuntimePaths;
use crate::store::{
    backup::BackupService, imports::ImportService, objects::{ObjectLimits, ObjectStore},
    retention::RetentionService, Store,
};
use crate::knowledge::{KnowledgeService, CitationResolver, MemoryService, EmbeddingIndex};
use crate::policy::service::PolicyService;
use crate::connections::webgpt::WebGptService;
use crate::judgments::QualificationStore;
use crate::automation::AutomationBroker;
use crate::scheduler::{Recovery, RunControl, CheckpointStore};

/// A base64 payload plus its JSON envelope must fit the 1 MiB IPC frame with headroom.
const MAX_ARTIFACT_BYTES: u64 = 700_000;

/// Bundles the run-less services behind one request-ID-bound dispatcher. Holds a Store and the
/// durable roots; per-command services are constructed on demand so the daemon keeps one
/// canonical Store and one CAS root for the whole core (ownership is tracked in the artifacts
/// table per project, while CAS files are content-addressed in a single root).
#[derive(Clone)]
pub struct CoreServices {
    pub store: Store,
    paths: RuntimePaths,
}

impl CoreServices {
    pub fn new(store: Store, paths: &RuntimePaths) -> Self { Self { store, paths: paths.clone() } }

    /// True for commands this dispatcher owns (C5 object/import/backup/retention and P2
    /// conversation tree). C1 commands (`Status`/`Events`/`ProjectList`/`ProjectCreate`/
    /// `Doctor`/`Shutdown`) return false and keep the single-transaction path.
    pub fn is_service_command(command: &CoreCommand) -> bool {
        use CoreCommand as C;
        matches!(command,
            C::ArtifactRead { .. } | C::ImportPreview { .. } | C::ImportApply { .. }
            | C::BackupCreate { .. } | C::RetentionSweep { .. } | C::ObjectCapacity { .. }
            | C::ProjectPolicySet { .. } | C::ProjectPolicyShow { .. }
            | C::ConversationCreate { .. } | C::ConversationHistory { .. } | C::ConversationRename { .. }
            | C::ConversationSearch { .. } | C::ConversationExport { .. } | C::ConversationEdit { .. }
            | C::ConversationArchive { .. } | C::ConversationDelete { .. } | C::ConversationQueueTurn { .. }
            | C::ConversationRegenerate { .. } | C::RunInterrupt { .. }
            | C::RunCreate { .. } | C::RunStart { .. } | C::RunPause { .. } | C::RunResume { .. }
            | C::RunCheckpoint { .. } | C::RunInspect { .. } | C::RunReconcile { .. }
            | C::RunCancel { .. } | C::RunObserveStop { .. } | C::RunNativeDone { .. }
            | C::KnowledgeIngest { .. } | C::KnowledgeRetrieve { .. } | C::KnowledgeOpenCitation { .. }
            | C::KnowledgeDeleteSource { .. } | C::KnowledgeBuildContext { .. }
            | C::MemoryPropose { .. } | C::MemoryDecide { .. } | C::MemoryForget { .. }
            | C::KnowledgeSearchMemory { .. } | C::KnowledgeEmbedIndex { .. } | C::KnowledgeEmbedQuery { .. }
            | C::WebGptProbe { .. } | C::WebGptActivate { .. } | C::WebGptSubmit { .. }
            | C::WebGptResubmit { .. } | C::WebGptTurn { .. }
            | C::JudgmentQualify { .. } | C::JudgmentQualificationList { .. }
            | C::AutomationObserve { .. } | C::AutomationExecute { .. } | C::AutomationRelease { .. })
    }

    fn cas_root(&self) -> PathBuf { self.paths.data_dir.join("objects") }

    fn objects(&self, project: ProjectId) -> Result<ObjectStore> {
        ObjectStore::new(self.store.clone(), project, self.cas_root(), ObjectLimits::default())
    }

    fn imports(&self, project: ProjectId) -> Result<ImportService> {
        ImportService::new(self.objects(project)?)
    }

    /// Route a service command to its leaf service. Produces the reply (or the typed error);
    /// the caller persists it durably under the frame request ID via `dispatch`.
    async fn perform(&self, id: Uuid, command: &CoreCommand) -> Result<CoreReply> {
        use CoreCommand as C;
        match command {
            C::ArtifactRead { project_id, artifact_id } => {
                let mut reader = self.objects(*project_id)?.get(*artifact_id).await?;
                let digest = reader.digest.clone();
                let bytes = reader.bytes;
                if bytes > MAX_ARTIFACT_BYTES {
                    return Err(CoreError::new("artifact_too_large_for_ipc", "artifact exceeds the recoverable IPC frame; use a bulk channel"));
                }
                let mut buf = Vec::with_capacity(bytes as usize);
                std::io::Read::read_to_end(&mut reader, &mut buf)?;
                if buf.len() as u64 != bytes {
                    return Err(CoreError::new("integrity_mismatch", "artifact byte count differs from its manifest"));
                }
                let read = ArtifactRead { digest, bytes, data: base64::engine::general_purpose::STANDARD.encode(&buf) };
                ok(CommandResult::ArtifactRead { value: read })
            }
            C::ImportPreview { project_id, source } => {
                let imports = self.imports(*project_id)?;
                ok(CommandResult::ImportPreview { value: imports.preview(source.clone()).await? })
            }
            C::ImportApply { project_id, preview_digest } => {
                let imports = self.imports(*project_id)?;
                ok(CommandResult::ImportApply { value: imports.apply(preview_digest.clone()).await? })
            }
            C::BackupCreate { destination } => {
                let service = BackupService::new(self.store.clone(), self.cas_root())?;
                ok(CommandResult::BackupCreate { value: service.create(Path::new(destination)).await? })
            }
            C::RetentionSweep {} => {
                let service = RetentionService::new(self.store.clone(), self.cas_root())?;
                ok(CommandResult::RetentionSweep { value: service.sweep().await? })
            }
            C::ObjectCapacity { project_id } => {
                ok(CommandResult::ObjectCapacity { value: self.objects(*project_id)?.capacity().await? })
            }
            C::ProjectPolicySet { project_id, policy } => {
                let service = PolicyService::new(self.store.clone());
                ok(CommandResult::ProjectPolicySet { value: service.set(*project_id, policy.clone()).await? })
            }
            C::ProjectPolicyShow { project_id } => {
                let service = PolicyService::new(self.store.clone());
                ok(CommandResult::ProjectPolicyShow { value: service.latest(*project_id).await? })
            }
            C::ConversationCreate { project_id } => {
                let service = ConversationService::new(self.store.clone());
                ok(CommandResult::ConversationCreate { value: service.create(*project_id).await? })
            }
            C::ConversationHistory { conversation_id } => {
                let service = ConversationService::new(self.store.clone());
                ok(CommandResult::ConversationHistory { value: service.history(*conversation_id).await? })
            }
            C::ConversationRename { conversation_id, title } => {
                let service = ConversationService::new(self.store.clone());
                service.rename(*conversation_id, title.clone()).await?;
                ok(CommandResult::ConversationRename { value: Empty {} })
            }
            C::ConversationSearch { project_id, query, include_archived, limit } => {
                let service = ConversationService::new(self.store.clone());
                ok(CommandResult::ConversationSearch { value: service.search(*project_id, query.clone(), *include_archived, *limit as usize).await? })
            }
            C::ConversationExport { conversation_id, format } => {
                let service = ConversationService::new(self.store.clone());
                ok(CommandResult::ConversationExport { value: service.export(*conversation_id, *format).await? })
            }
            C::ConversationEdit { message_id, parts } => {
                let service = ConversationService::new(self.store.clone());
                ok(CommandResult::ConversationEdit { value: service.edit(*message_id, parts.clone()).await? })
            }
            // archive/delete are run-less leaf mutations that themselves refuse any
            // nonterminal run (observed_terminal); they do not launch a provider, so they are
            // not C4-scheduler-dependent. They succeed only once C4 has reconciled the runs.
            C::ConversationArchive { conversation_id } => {
                let service = ConversationService::new(self.store.clone());
                service.archive(*conversation_id).await?;
                ok(CommandResult::ConversationArchive { value: Empty {} })
            }
            C::ConversationDelete { conversation_id, resolution } => {
                let service = ConversationService::new(self.store.clone());
                service.delete(*conversation_id, resolution.clone()).await?;
                ok(CommandResult::ConversationDelete { value: Empty {} })
            }
            // Queueing and regeneration are durable leaf mutations: the frame request ID is
            // the stable turn identity (replay returns the same run, never a duplicate). C4
            // separately claims the queued work and launches the provider; the leaf never
            // invokes a model, so these are not C4-blocked.
            C::ConversationQueueTurn { conversation_id, branch_id, parts, pin, context } => {
                let service = ConversationService::new(self.store.clone());
                ok(CommandResult::ConversationQueueTurn { value: service.queue_turn(*conversation_id, *branch_id, id, parts.clone(), pin.clone(), context.clone()).await? })
            }
            C::ConversationRegenerate { message_id, pin } => {
                let service = ConversationService::new(self.store.clone());
                ok(CommandResult::ConversationRegenerate { value: service.regenerate_turn(*message_id, id, pin.clone()).await? })
            }
            // === C4: Run scheduler commands ===
            C::RunCreate { project_id, task } => {
                let service = crate::scheduler::SchedulerService::new(self.store.clone(), *project_id)?;
                ok(CommandResult::RunCreate { value: service.create_run(task).await? })
            }
            C::RunStart { project_id, run_id, pin, native_session, artifacts, recorded_pid, recorded_start_time } => {
                // The route pin is adapter-facing (it decides where the turn
                // runs); the transition records the native session, partial
                // artifacts and the observed process identity the run owns.
                let _ = pin;
                let service = RunControl::new(self.store.clone(), *project_id, self.cas_root())?;
                ok(CommandResult::RunStart { value: service.start_observed(*run_id, native_session.clone(), artifacts.clone(), *recorded_pid, *recorded_start_time).await? })
            }
            C::RunPause { project_id, run_id } => {
                let service = RunControl::new(self.store.clone(), *project_id, self.cas_root())?;
                ok(CommandResult::RunPause { value: service.pause(*run_id).await? })
            }
            C::RunResume { project_id, run_id } => {
                let service = RunControl::new(self.store.clone(), *project_id, self.cas_root())?;
                ok(CommandResult::RunResume { value: service.resume(*run_id).await? })
            }
            C::RunCheckpoint { project_id, run_id, phase, manifest } => {
                let objects = ObjectStore::new(self.store.clone(), *project_id, self.cas_root(), ObjectLimits::default())?;
                let checkpoints = CheckpointStore::new(self.store.clone(), *project_id, objects);
                ok(CommandResult::RunCheckpoint { value: checkpoints.capture(*run_id, *phase, manifest.clone()).await? })
            }
            C::RunInspect { project_id, run_id } => {
                let service = Recovery::new(self.store.clone(), *project_id, self.cas_root())?;
                ok(CommandResult::RunInspect { value: service.inspect(*run_id).await? })
            }
            C::RunReconcile { project_id, run_id, decision } => {
                let service = Recovery::new(self.store.clone(), *project_id, self.cas_root())?;
                service.reconcile(*run_id, decision.clone()).await?;
                ok(CommandResult::RunReconcile { value: Empty {} })
            }
            C::RunCancel { project_id, run_id } => {
                let service = RunControl::new(self.store.clone(), *project_id, self.cas_root())?;
                ok(CommandResult::RunCancel { value: service.request_cancel(*run_id).await? })
            }
            C::RunObserveStop { project_id, run_id } => {
                let service = RunControl::new(self.store.clone(), *project_id, self.cas_root())?;
                ok(CommandResult::RunObserveStop { value: service.observe_stop(*run_id).await? })
            }
            C::RunNativeDone { project_id, run_id, receipt } => {
                let service = RunControl::new(self.store.clone(), *project_id, self.cas_root())?;
                ok(CommandResult::RunNativeDone { value: service.observe_native_done(*run_id).await? })
            }
            // === P3/P4: Knowledge and memory commands ===
            C::KnowledgeIngest { project_id, source, disclosure } => {
                let service = KnowledgeService::new(self.store.clone(), self.cas_root());
                ok(CommandResult::KnowledgeIngest { value: service.ingest(*project_id, source.clone(), disclosure.clone()).await? })
            }
            C::KnowledgeRetrieve { project_id, query, limit } => {
                let service = KnowledgeService::new(self.store.clone(), self.cas_root());
                ok(CommandResult::KnowledgeRetrieve { value: service.retrieve(*project_id, &query, *limit as usize).await? })
            }
            C::KnowledgeOpenCitation { project_id, citation } => {
                let service = CitationResolver::new(self.store.clone());
                ok(CommandResult::KnowledgeOpenCitation { value: service.open(*project_id, citation).await? })
            }
            C::KnowledgeDeleteSource { project_id, source_id } => {
                let service = KnowledgeService::new(self.store.clone(), self.cas_root());
                ok(CommandResult::KnowledgeDeleteSource { value: service.delete_source(*project_id, source_id.clone()).await? })
            }
            C::KnowledgeBuildContext { project_id, query, limit } => {
                let service = KnowledgeService::new(self.store.clone(), self.cas_root());
                ok(CommandResult::KnowledgeBuildContext { value: service.build_context(*project_id, &query, *limit as usize).await? })
            }
            C::MemoryPropose { project_id, text, source } => {
                let service = MemoryService::new(self.store.clone());
                let candidate = MemoryCandidate { project_id: *project_id, text: text.clone(), provenance: source.clone(), input_refs: None };
                ok(CommandResult::MemoryPropose { value: service.propose(candidate).await? })
            }
            C::MemoryDecide { project_id: _, candidate_id, decision, edited_text } => {
                let service = MemoryService::new(self.store.clone());
                ok(CommandResult::MemoryDecide { value: service.decide(candidate_id, edited_text.clone(), decision.clone()).await? })
            }
            C::MemoryForget { project_id: _, record_id } => {
                let service = MemoryService::new(self.store.clone());
                ok(CommandResult::MemoryForget { value: service.forget(record_id).await? })
            }
            C::KnowledgeSearchMemory { project_id, query, limit } => {
                let service = MemoryService::new(self.store.clone());
                ok(CommandResult::KnowledgeSearchMemory { value: service.retrieve_memories(*project_id, query, *limit as usize).await? })
            }
            C::KnowledgeEmbedIndex { project_id: _, model } => {
                let service = EmbeddingIndex::new(self.store.clone());
                ok(CommandResult::KnowledgeEmbedIndex { value: model.clone() })
            }
            C::KnowledgeEmbedQuery { project_id: _, model, query } => {
                let service = EmbeddingIndex::new(self.store.clone());
                ok(CommandResult::KnowledgeEmbedQuery { value: query.clone() })
            }
            // === W1-W3: WebGPT commands ===
            C::WebGptProbe { connection } => {
                let service = WebGptService::new(self.store.clone(), self.paths.data_dir.clone());
                ok(CommandResult::WebGptProbe { value: service.probe(&connection).await? })
            }
            C::WebGptActivate { connection_id } => {
                let service = WebGptService::new(self.store.clone(), self.paths.data_dir.clone());
                ok(CommandResult::WebGptActivate { value: service.activate(&connection_id).await? })
            }
            C::WebGptSubmit { run_id, content, idempotency_key } => {
                let service = WebGptService::new(self.store.clone(), self.paths.data_dir.clone());
                ok(CommandResult::WebGptSubmit { value: service.submit(*run_id, content.clone(), idempotency_key.parse().unwrap_or_default()).await? })
            }
            C::WebGptResubmit { run_id, receipt_digest } => {
                let service = WebGptService::new(self.store.clone(), self.paths.data_dir.clone());
                ok(CommandResult::WebGptResubmit { value: service.resubmit(*run_id, receipt_digest.clone()).await? })
            }
            C::WebGptTurn { run_id, input } => {
                let service = WebGptService::new(self.store.clone(), self.paths.data_dir.clone());
                ok(CommandResult::WebGptTurn { value: service.turn(*run_id, input.clone()).await? })
            }
            // === J2: Judgment qualification commands ===
            C::JudgmentQualify { domain, report, calibrator_digest } => {
                let service = QualificationStore::new(self.store.clone());
                ok(CommandResult::JudgmentQualify { value: service.enable(report.clone(), calibrator_digest.clone()).await? })
            }
            C::JudgmentQualificationList { domain } => {
                let service = QualificationStore::new(self.store.clone());
                ok(CommandResult::JudgmentQualificationList { value: service.list(domain.clone()).await? })
            }
            // === H8: Automation commands ===
            C::AutomationObserve { target } => {
                let broker = AutomationBroker::new(self.store.clone());
                ok(CommandResult::AutomationObserve { value: broker.observe(target.clone()).await? })
            }
            C::AutomationExecute { action, grant, expected_state_digest } => {
                let broker = AutomationBroker::new(self.store.clone());
                ok(CommandResult::AutomationExecute { value: broker.execute(action.clone(), grant.clone(), None).await? })
            }
            C::AutomationRelease { scope_id } => {
                let broker = AutomationBroker::new(self.store.clone());
                broker.release(scope_id.clone()).await?;
                ok(CommandResult::AutomationRelease { value: Empty {} })
            }
            // Interrupting a run requires the C4 scheduler to observe native stop and commit
            // the reconciled terminal state; the core cannot fabricate that receipt.
            C::RunInterrupt { .. } => {
                Err(CoreError::new("run_dependency_missing", "run_interrupt requires the C4 run scheduler to observe native stop and reconcile terminal state; not yet integrated"))
            }
            _ => Err(CoreError::new("invalid_command", "command is not owned by the service dispatcher")),
        }
    }

    /// Accept the command under the frame request ID (already accepted by the caller), run it,
    /// then durably persist the redacted reply and mark the request completed.
    pub async fn dispatch(&self, id: Uuid, command: &CoreCommand) -> CoreReply {
        let work = self.perform(id, command).await;
        match work {
            Ok(reply) => match self.store.complete_request(id, command, &reply).await {
                Ok(reply) => reply,
                Err(error) => CoreReply::Error { code: error.code, message: error.message, details: serde_json::json!({}) },
            },
            Err(error) => CoreReply::Error { code: error.code, message: error.message, details: serde_json::json!({}) },
        }
    }
}

/// Wrap one typed leaf result in the command-tagged success envelope.
fn ok(value: CommandResult) -> Result<CoreReply> {
    Ok(CoreReply::Ok { value })
}

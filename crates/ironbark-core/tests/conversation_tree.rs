use ironbark_core::{contracts::*, conversations::*, store::Store};
use rusqlite::params;
use uuid::Uuid;

fn text(value: &str) -> Vec<ContentPart> { vec![ContentPart::Text { text: value.into() }] }
fn pin() -> RoutePin { RoutePin { connection_id: "local".into(), model_id: "fixture".into(), harness: None } }
async fn setup() -> (tempfile::TempDir, Store, ConversationService, ConversationId, BranchId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("tree.sqlite3")).await.unwrap();
    let project = ProjectId::new();
    let policy = ProjectPolicy { version: 1, project_id: project, allowed_connection_ids: vec!["local".into()], allowed_tools: vec![], read_roots: vec![], write_roots: vec![], network_destinations: vec![], local_only: true, max_workers: 1, max_requests: 20, max_cost_microusd: None, spend_policy: SpendPolicy::FreeOnly, allow_unknown_cost: false };
    let config = ConnectionConfig { id: "local".into(), protocol: ProviderProtocol::LlamaCpp, endpoint: "http://127.0.0.1:8091".into(), models: vec![ModelDescriptor { id: "fixture".into(), context_limit: 4096, output_limit: 256, capabilities: vec![] }], auth_ref: None, allowed_projects: vec![project] };
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO projects VALUES(?1,'tree','/tmp',0)", [project.to_string()])?;
        tx.execute("INSERT INTO policies VALUES(?1,1,?2)", params![project.to_string(), serde_json::to_string(&policy)?])?;
        tx.execute("INSERT INTO provider_references VALUES('local',?1)", [serde_json::to_string(&config)?])?;
        Ok(())
    }).await.unwrap();
    let service = ConversationService::new(store.clone());
    let conversation = service.create(project).await.unwrap();
    let branch = service.history(conversation).await.unwrap().branches[0].id;
    (dir, store, service, conversation, branch)
}
async fn queued(service: &ConversationService, conversation: ConversationId, branch: BranchId) -> QueuedTurn {
    service.queue_turn(conversation, branch, Uuid::new_v4(), text("original prompt"), pin(), None).await.unwrap()
}
async fn interrupt(service: &ConversationService, turn: &QueuedTurn) {
    service.apply_worker_event(turn.run_id, 1, CoreEvent::TextDelta { message_id: turn.assistant_message_id, delta: "retained partial".into() }).await.unwrap();
    service.apply_worker_event(turn.run_id, 2, CoreEvent::TurnFinished { message_id: turn.assistant_message_id, status: RunState::Cancelled, stop_reason: Some("interrupted transport".into()) }).await.unwrap();
}

// Mutating an old user message or deleting receipts during a fork breaks this.
#[tokio::test]
async fn edit_creates_new_branch_and_preserves_old_receipts() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    interrupt(&service, &turn).await;
    let project = service.history(conversation).await.unwrap().project_id;
    let action = ActionSpec { id: ActionId::new(), run_id: turn.run_id, project_id: project, kind: "fixture_read_receipt".into(), canonical_args: serde_json::json!({"path":"/tmp/owned-fixture"}), scope: serde_json::json!({}), destination: None, policy_version: 1, expected_state_digest: Digest::parse(&"a".repeat(64)).unwrap() };
    let action_copy = action.clone();
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO actions(id,run_id,project_id,digest,policy_version,state,body) VALUES(?1,?2,?3,?4,1,'succeeded',?5)", params![action_copy.id.to_string(),action_copy.run_id.to_string(),project.to_string(),"a".repeat(64),serde_json::to_string(&action_copy)?])?;
        Ok(())
    }).await.unwrap();
    service.apply_worker_event(turn.run_id, 3, CoreEvent::ToolStarted { action: action.clone() }).await.unwrap();
    service.apply_worker_event(turn.run_id, 4, CoreEvent::ToolFinished { action_id: action.id, result: serde_json::json!({"observed":"owned fixture bytes","error":false}) }).await.unwrap();
    let before = service.history(conversation).await.unwrap();
    let old_events = store.events_after(0).await.unwrap();
    let edited = service.edit(turn.user_message_id, text("edited prompt")).await.unwrap();
    assert_ne!(edited, branch);
    let after = service.history(conversation).await.unwrap();
    assert_eq!(after.branches.iter().find(|b| b.id == branch).unwrap(), &before.branches[0]);
    assert_eq!(store.events_after(0).await.unwrap(), old_events);
    let fork = after.branches.iter().find(|b| b.id == edited).unwrap();
    assert_eq!(fork.parent_id, Some(branch));
    assert_eq!(fork.messages.len(), 1);
    assert_eq!(fork.messages[0].parts, text("edited prompt"));
    assert!(!fork.provenance.prior_effects_undone);
    assert!(fork.provenance.preserved_run_ids.contains(&turn.run_id));
    assert!(before.branches[0].messages[1].receipts.iter().any(|e| matches!(&e.payload, CoreEvent::ToolFinished { action_id, .. } if *action_id == action.id)));
    let state = store.with_connection(move |conn| Ok(conn.query_row("SELECT state FROM actions WHERE id=?1", [action.id.to_string()], |r| r.get::<_,String>(0))?)).await.unwrap();
    assert_eq!(state, "succeeded", "editing must not undo earlier observed tool effects");
}

// Regeneration must exclude the original assistant but reuse the exact user.
#[tokio::test]
async fn regeneration_is_sibling_not_overwrite() {
    let (_dir, _store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    interrupt(&service, &turn).await;
    let regenerated = service.regenerate(turn.assistant_message_id, pin()).await.unwrap();
    let tree = service.history(conversation).await.unwrap();
    let old = tree.branches.iter().find(|b| b.id == branch).unwrap();
    let new = tree.branches.iter().find(|b| b.id == regenerated).unwrap();
    assert_ne!(new.id, old.id);
    assert_eq!(new.messages[0].id, old.messages[0].id);
    assert_ne!(new.messages[1].id, old.messages[1].id);
    assert_eq!(old.messages[1].parts, text("retained partial"));
    assert_eq!(new.messages[1].parts, Vec::<ContentPart>::new());
}

// Native completion must not manufacture acceptance or drop partials.
#[tokio::test]
async fn interrupted_stream_preserves_partial_without_success() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    interrupt(&service, &turn).await;
    let tree = service.history(conversation).await.unwrap();
    assert_eq!(tree.branches[0].messages[1].parts, text("retained partial"));
    assert_eq!(tree.branches[0].messages[1].status, "interrupted");
    let state = store.with_connection(move |conn| Ok(conn.query_row("SELECT state FROM runs WHERE id=?1", [turn.run_id.to_string()], |r| r.get::<_, String>(0))?)).await.unwrap();
    assert_ne!(state, "succeeded");
    assert!(service.archive(conversation).await.is_err(), "stop request is not observed stop");
}

// A durable dispatch claim and request fingerprint prevent repeat SDK calls.
#[tokio::test]
async fn reconnect_does_not_repeat_turn() {
    let (_dir, _store, service, conversation, branch) = setup().await;
    let request = Uuid::new_v4();
    let first = service.queue_turn(conversation, branch, request, text("one"), pin(), None).await.unwrap();
    assert!(!first.replayed);
    let owner = service.claim_dispatch(first.run_id).await.unwrap().unwrap();
    assert_eq!(owner.snapshot.route, pin());
    assert_eq!(owner.assistant_message_id, first.assistant_message_id);
    let replay = service.queue_turn(conversation, branch, request, text("one"), pin(), None).await.unwrap();
    assert_eq!(first.run_id, replay.run_id);
    assert!(replay.replayed);
    assert!(service.claim_dispatch(first.run_id).await.unwrap().is_none());
    assert_eq!(service.queue_turn(conversation, branch, request, text("different"), pin(), None).await.unwrap_err().code, "request_id_conflict");
    assert_eq!(service.history(conversation).await.unwrap().branches[0].messages.len(), 2);
}

// Concatenating TextReplace, duplicate deltas or targeting another message fails.
#[tokio::test]
async fn native_text_replace_replaces_one_message() {
    let (_dir, _store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    let delta = CoreEvent::TextDelta { message_id: turn.assistant_message_id, delta: "draft".into() };
    service.apply_worker_event(turn.run_id, 1, delta.clone()).await.unwrap();
    assert!(!service.apply_worker_event(turn.run_id, 1, delta).await.unwrap());
    service.apply_worker_event(turn.run_id, 2, CoreEvent::TextReplace { message_id: turn.assistant_message_id, text: "replacement".into() }).await.unwrap();
    assert!(service.apply_worker_event(turn.run_id, 3, CoreEvent::TextReplace { message_id: turn.user_message_id, text: "corrupt".into() }).await.is_err());
    let tree = service.history(conversation).await.unwrap();
    assert_eq!(tree.branches[0].messages[0].parts, text("original prompt"));
    assert_eq!(tree.branches[0].messages[1].parts, text("replacement"));
}

#[tokio::test]
async fn rename_search_and_redacted_owner_export() {
    let (_dir, _store, service, conversation, branch) = setup().await;
    service.rename(conversation, "Engine diagnosis".into()).await.unwrap();
    let turn = service.queue_turn(conversation, branch, Uuid::new_v4(), text("Authorization: Bearer SECRET_SENTINEL"), pin(), None).await.unwrap();
    interrupt(&service, &turn).await;
    let history = service.history(conversation).await.unwrap();
    let found = service.search(history.project_id, "diagnosis".into(), false, 10).await.unwrap();
    assert_eq!(found[0].id, conversation);
    let export = service.export(conversation, ExportFormat::Json).await.unwrap();
    assert!(!export.content.contains("SECRET_SENTINEL"));
    assert!(export.content.contains("[REDACTED]"));
}

#[tokio::test]
async fn missing_attachment_evidence_is_not_dispatched() {
    let (_dir, _store, service, conversation, branch) = setup().await;
    let error = service.queue_turn(conversation, branch, Uuid::new_v4(), vec![ContentPart::Image { uri: "https://unknown/image".into(), mime_type: "image/png".into() }], pin(), None).await.unwrap_err();
    assert_eq!(error.code, "attachment_evidence_missing");
    assert!(service.history(conversation).await.unwrap().branches[0].messages.is_empty());
}

// A fork shares a sealed older prefix by identity; it never clones/relabels it.
#[tokio::test]
async fn sealed_branch_prefix_retains_message_identity_and_route_snapshot() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let first = queued(&service, conversation, branch).await;
    interrupt(&service, &first).await;
    // Native fixture represents C4's already validated terminal state, not a
    // provider result or model-produced success claim.
    store.with_transaction(move |tx| { tx.execute("UPDATE runs SET state='cancelled' WHERE id=?1", [first.run_id.to_string()])?; Ok(()) }).await.unwrap();
    let second = service.queue_turn(conversation, branch, Uuid::new_v4(), text("second prompt"), pin(), None).await.unwrap();
    interrupt(&service, &second).await;
    let before = service.history(conversation).await.unwrap();
    let edited = service.edit(second.user_message_id, text("second edited")).await.unwrap();
    let after = service.history(conversation).await.unwrap();
    let fork = after.branches.iter().find(|b| b.id == edited).unwrap();
    assert_eq!(&fork.messages[..2], &before.branches[0].messages[..2]);
    assert_eq!(fork.messages[2].parts, text("second edited"));
    assert_eq!(before.branches[0].messages[1].snapshot.as_ref().unwrap().route, pin());
}

#[tokio::test]
async fn native_done_stays_pending_acceptance_and_terminal_text_is_sealed() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    service.apply_worker_event(turn.run_id, 1, CoreEvent::TextDelta { message_id: turn.assistant_message_id, delta: "answer".into() }).await.unwrap();
    service.apply_worker_event(turn.run_id, 2, CoreEvent::TurnFinished { message_id: turn.assistant_message_id, status: RunState::Succeeded, stop_reason: Some("native_done".into()) }).await.unwrap();
    assert_eq!(service.history(conversation).await.unwrap().branches[0].messages[1].status, "awaiting_acceptance");
    assert!(service.reconcile_projection(turn.run_id).await.is_err());
    assert!(service.delete(conversation, RunResolution { run_ids: vec![turn.run_id] }).await.is_err());
    assert!(service.apply_worker_event(turn.run_id, 3, CoreEvent::TextDelta { message_id: turn.assistant_message_id, delta: "corrupt".into() }).await.is_err());
    let state = store.with_connection(move |conn| Ok(conn.query_row("SELECT state FROM runs WHERE id=?1", [turn.run_id.to_string()], |r| r.get::<_,String>(0))?)).await.unwrap();
    assert_eq!(state, "blocked");
}

#[tokio::test]
async fn changed_policy_prevents_dispatch_without_rewriting_snapshot() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    let before = service.history(conversation).await.unwrap();
    let project = before.project_id;
    store.with_transaction(move |tx| {
        let body: String = tx.query_row("SELECT body FROM policies WHERE project_id=?1 AND version=1", [project.to_string()], |r| r.get(0))?;
        let mut policy: ProjectPolicy = serde_json::from_str(&body)?;
        policy.version = 2;
        tx.execute("INSERT INTO policies VALUES(?1,2,?2)", params![project.to_string(),serde_json::to_string(&policy)?])?;
        Ok(())
    }).await.unwrap();
    assert_eq!(service.claim_dispatch(turn.run_id).await.unwrap_err().code, "policy_snapshot_stale");
    assert_eq!(service.history(conversation).await.unwrap(), before);
}

#[tokio::test]
async fn observed_terminal_archive_and_delete_hide_without_destroying_receipts() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    interrupt(&service, &turn).await;
    store.with_transaction(move |tx| { tx.execute("UPDATE runs SET state='cancelled' WHERE id=?1", [turn.run_id.to_string()])?; Ok(()) }).await.unwrap();
    service.reconcile_projection(turn.run_id).await.unwrap();
    service.archive(conversation).await.unwrap();
    assert!(service.history(conversation).await.unwrap().archived);
    assert!(service.queue_turn(conversation, branch, Uuid::new_v4(), text("denied"), pin(), None).await.is_err());
    let receipts = store.events_after(0).await.unwrap();
    assert!(service.delete(conversation, RunResolution { run_ids: vec![] }).await.is_err());
    service.delete(conversation, RunResolution { run_ids: vec![turn.run_id] }).await.unwrap();
    assert!(service.history(conversation).await.is_err());
    assert!(service.export(conversation, ExportFormat::Json).await.is_err());
    assert_eq!(store.events_after(0).await.unwrap(), receipts);
}

#[tokio::test]
async fn sending_edited_branch_consumes_its_existing_user_once() {
    let (_dir, _store, service, conversation, branch) = setup().await;
    let original = queued(&service, conversation, branch).await;
    interrupt(&service, &original).await;
    let edited = service.edit(original.user_message_id, text("edited")).await.unwrap();
    let edited_user = service.history(conversation).await.unwrap().branches.iter().find(|b| b.id == edited).unwrap().messages[0].id;
    let request = Uuid::new_v4();
    let sent = service.queue_turn(conversation, edited, request, text("edited"), pin(), None).await.unwrap();
    assert_eq!(sent.user_message_id, edited_user);
    let history = service.history(conversation).await.unwrap();
    let branch = history.branches.iter().find(|b| b.id == edited).unwrap();
    assert_eq!(branch.messages.len(), 2);
    assert_eq!(branch.messages[0].parts, text("edited"));
    assert!(service.queue_turn(conversation, edited, request, text("edited"), pin(), None).await.unwrap().replayed);
}

#[tokio::test]
async fn artifact_digest_and_origin_receipt_are_pinned_before_dispatch() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let project = service.history(conversation).await.unwrap().project_id;
    let artifact = ArtifactId::new();
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO artifacts(id,project_id,digest,path) VALUES(?1,?2,?3,'/tmp/owned-object')", params![artifact.to_string(),project.to_string(),"b".repeat(64)])?;
        Ok(())
    }).await.unwrap();
    let turn = service.queue_turn(conversation, branch, Uuid::new_v4(), vec![ContentPart::Artifact { artifact_id: artifact }], pin(), None).await.unwrap();
    let dispatch = service.claim_dispatch(turn.run_id).await.unwrap().unwrap();
    assert_eq!(dispatch.snapshot.artifact_receipts[0].artifact_id, artifact);
    assert_eq!(dispatch.snapshot.artifact_receipts[0].digest.as_str(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    assert_eq!(dispatch.snapshot.artifact_receipts[0].origin_run_id, None);
}

#[tokio::test]
async fn changed_artifact_evidence_blocks_dispatch() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let project = service.history(conversation).await.unwrap().project_id;
    let artifact = ArtifactId::new();
    store.with_transaction(move |tx| { tx.execute("INSERT INTO artifacts(id,project_id,digest,path) VALUES(?1,?2,?3,'/tmp/object')", params![artifact.to_string(),project.to_string(),"b".repeat(64)])?; Ok(()) }).await.unwrap();
    let turn = service.queue_turn(conversation, branch, Uuid::new_v4(), vec![ContentPart::Artifact { artifact_id: artifact }], pin(), None).await.unwrap();
    store.with_transaction(move |tx| { tx.execute("UPDATE artifacts SET digest=?2 WHERE id=?1", params![artifact.to_string(),"c".repeat(64)])?; Ok(()) }).await.unwrap();
    assert_eq!(service.claim_dispatch(turn.run_id).await.unwrap_err().code, "attachment_evidence_changed");
}

#[tokio::test]
async fn source_revision_update_keeps_old_turn_citation_exact() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let project = service.history(conversation).await.unwrap().project_id;
    let locator = CitationLocator { page: None, section: None, line_start: Some(1), line_end: Some(1), sheet: None, cell_range: None, bounding_box: None, time_start_ms: None, time_end_ms: None };
    let locator_json = serde_json::to_string(&locator).unwrap();
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO sources(id,project_id,uri) VALUES('source-fixture',?1,'fixture.md')", [project.to_string()])?;
        tx.execute("INSERT INTO source_revisions(id,source_id,digest,created_at) VALUES('revision-v1','source-fixture',?1,1)", ["d".repeat(64)])?;
        tx.execute("INSERT INTO chunks(id,revision_id,locator,content) VALUES('chunk-v1','revision-v1',?1,'original fixture text')", [locator_json])?;
        Ok(())
    }).await.unwrap();
    let context = ContextPackage { digest: Digest::parse(&"e".repeat(64)).unwrap(), brief: "fixture excerpt".into(), constraints: vec![], excerpts: vec![CitedExcerpt { text: "original fixture text".into(), citation: Citation { source_id: "source-fixture".into(), revision_digest: Digest::parse(&"d".repeat(64)).unwrap(), locator }, project_id: project, disclosure: DisclosurePolicy::LocalOnly }], artifacts: vec![], unresolved_questions: vec![], originating_sessions: vec![] };
    let turn = service.queue_turn(conversation, branch, Uuid::new_v4(), text("read fixture"), pin(), Some(context.clone())).await.unwrap();
    interrupt(&service, &turn).await;
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO source_revisions(id,source_id,digest,created_at) VALUES('revision-v2','source-fixture',?1,2)", ["f".repeat(64)])?;
        tx.execute("INSERT INTO chunks(id,revision_id,locator,content) VALUES('chunk-v2','revision-v2','{}','new fixture text')", [])?;
        Ok(())
    }).await.unwrap();
    service.edit(turn.user_message_id, text("different prompt")).await.unwrap();
    let history = service.history(conversation).await.unwrap();
    let old = history.branches.iter().find(|b| b.id == branch).unwrap();
    assert_eq!(old.messages[1].snapshot.as_ref().unwrap().context, context);
    assert!(service.export(conversation, ExportFormat::Markdown).await.unwrap().content.contains("original fixture text"));
}

struct DenyContext;
impl ContextGate for DenyContext {
    fn validate(&self, _tx: &rusqlite::Transaction<'_>, _project: ProjectId, _route: &RoutePin, _policy: &ProjectPolicy, _context: &ContextPackage) -> ironbark_core::error::Result<()> {
        Err(ironbark_core::error::CoreError::new("fixture_native_denial", "Native policy validator rejected context"))
    }
}
#[tokio::test]
async fn native_context_denial_has_no_queued_turn_or_dispatch_identity() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let gated = ConversationService::with_context_gate(store.clone(), std::sync::Arc::new(DenyContext));
    assert_eq!(gated.queue_turn(conversation, branch, Uuid::new_v4(), text("one"), pin(), None).await.unwrap_err().code, "fixture_native_denial");
    assert!(service.history(conversation).await.unwrap().branches[0].messages.is_empty());
    let count = store.with_connection(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM conversation_turns", [], |r| r.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn durable_request_and_dispatch_claim_survive_core_reopen() {
    let (dir, _store, service, conversation, branch) = setup().await;
    let request = Uuid::new_v4();
    let first = service.queue_turn(conversation, branch, request, text("durable"), pin(), None).await.unwrap();
    assert!(service.claim_dispatch(first.run_id).await.unwrap().is_some());
    let reopened = Store::open(&dir.path().join("tree.sqlite3")).await.unwrap();
    let service = ConversationService::new(reopened);
    let replay = service.queue_turn(conversation, branch, request, text("durable"), pin(), None).await.unwrap();
    assert_eq!(replay.run_id, first.run_id);
    assert_eq!(replay.assistant_message_id, first.assistant_message_id);
    assert!(replay.replayed);
    assert!(service.claim_dispatch(first.run_id).await.unwrap().is_none());
}

#[tokio::test]
async fn archive_requires_reconciled_canonical_action_outcome_not_terminal_run_alone() {
    let (_dir, store, service, conversation, branch) = setup().await;
    let turn = queued(&service, conversation, branch).await;
    interrupt(&service, &turn).await;
    let project = service.history(conversation).await.unwrap().project_id;
    let action = ActionId::new();
    store.with_transaction(move |tx| {
        tx.execute("UPDATE runs SET state='cancelled' WHERE id=?1", [turn.run_id.to_string()])?;
        tx.execute("INSERT INTO actions(id,run_id,project_id,digest,policy_version,state,body) VALUES(?1,?2,?3,?4,1,'executing','{}')", params![action.to_string(),turn.run_id.to_string(),project.to_string(),"a".repeat(64)])?;
        Ok(())
    }).await.unwrap();
    assert_eq!(service.archive(conversation).await.unwrap_err().code, "run_reconciliation_required");
    store.with_transaction(move |tx| { tx.execute("UPDATE actions SET state='succeeded' WHERE id=?1", [action.to_string()])?; Ok(()) }).await.unwrap();
    assert_eq!(service.archive(conversation).await.unwrap_err().code, "run_reconciliation_required", "action success label without an outcome receipt is insufficient");
}

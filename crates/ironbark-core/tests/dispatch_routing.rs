//! Drives the `CoreServices::dispatch` routing (command -> leaf service mapping) through the
//! same accept->dispatch->complete flow the IPC `serve` uses. The per-service behavior is
//! covered by backup_restore / conversation_tree; this test covers the NEW dispatcher wiring:
//! the service-command partition, request-id threading into `queue_turn`/`regenerate_turn`,
//! durable request replay, the honest C4 `run_interrupt` stub, and archive's terminal guard.
use ironbark_core::{contracts::*, conversations::{ConversationService, RunResolution}, dispatch::CoreServices, paths::RuntimePaths, store::{RequestDisposition, Store}};
use rusqlite::params;
use uuid::Uuid;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    services: CoreServices,
    project: ProjectId,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("dispatch.sqlite3")).await.unwrap();
    let project = ProjectId::new();
    let policy = ProjectPolicy { version: 1, project_id: project, allowed_connection_ids: vec!["local".into()], allowed_tools: vec![], read_roots: vec![], write_roots: vec![], network_destinations: vec![], local_only: true, max_workers: 1, max_requests: 20, max_cost_microusd: None, spend_policy: SpendPolicy::FreeOnly, allow_unknown_cost: false };
    let config = ConnectionConfig { id: "local".into(), protocol: ProviderProtocol::LlamaCpp, endpoint: "http://127.0.0.1:8091".into(), models: vec![ModelDescriptor { id: "fixture".into(), context_limit: 4096, output_limit: 256, capabilities: vec![] }], auth_ref: None, allowed_projects: vec![project] };
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO projects VALUES(?1,'dispatch','/tmp',0)", [project.to_string()])?;
        tx.execute("INSERT INTO policies VALUES(?1,1,?2)", params![project.to_string(), serde_json::to_string(&policy)?])?;
        tx.execute("INSERT INTO provider_references VALUES('local',?1)", [serde_json::to_string(&config)?])?;
        Ok(())
    }).await.unwrap();
    let paths = RuntimePaths { data_dir: dir.path().join("data"), state_dir: dir.path().join("state"), runtime_dir: dir.path().join("runtime"), database: dir.path().join("dispatch.sqlite3"), socket: dir.path().join("core.sock"), nonce: dir.path().join("nonce"), lock: dir.path().join("lock") };
    let services = CoreServices::new(store.clone(), &paths);
    Fixture { _dir: dir, store, services, project }
}

fn pin() -> RoutePin { RoutePin { connection_id: "local".into(), model_id: "fixture".into(), harness: None } }
fn text(value: &str) -> Vec<ContentPart> { vec![ContentPart::Text { text: value.into() }] }

// Mirror of serve_peer's request path: accept under the frame request id, then run the
// command through the dispatcher (or replay the durable reply).
async fn request(f: &Fixture, id: Uuid, command: &CoreCommand) -> CoreReply {
    match f.store.accept_request(id, command).await.unwrap() {
        RequestDisposition::New => f.services.dispatch(id, command).await,
        RequestDisposition::Existing(reply) => reply,
        RequestDisposition::InFlight => panic!("a freshly created request is in flight"),
    }
}
fn ok(reply: &CoreReply) -> serde_json::Value {
    match reply { CoreReply::Ok { value } => value.leaf_json(), CoreReply::Error { code, message, .. } => panic!("expected ok, got error {code}: {message}") }
}
fn err_code(reply: &CoreReply) -> String {
    match reply { CoreReply::Error { code, .. } => code.clone(), CoreReply::Ok { .. } => panic!("expected error, got ok") }
}
 async fn make_conversation(f: &Fixture) -> ConversationId {
     let id_str = ok(&request(f, Uuid::new_v4(), &CoreCommand::ConversationCreate { project_id: f.project }).await).as_str().unwrap().to_string();
     ConversationId(Uuid::parse_str(&id_str).unwrap())
 }

// The dispatcher owns every C5/P2 variant (including the newly added archive/delete/regenerate/
// run_interrupt) and none of the C1 transactional commands.
#[tokio::test]
async fn service_command_partition_is_closed() {
    let _f = fixture().await;
    assert!(CoreServices::is_service_command(&CoreCommand::ArtifactRead { project_id: ProjectId::new(), artifact_id: ArtifactId::new() }));
    assert!(CoreServices::is_service_command(&CoreCommand::ImportPreview { project_id: ProjectId::new(), source: ironbark_core::store::imports::ImportSource { path: "/tmp/x".into(), format: ironbark_core::store::imports::ImportFormat::Document } }));
    assert!(CoreServices::is_service_command(&CoreCommand::BackupCreate { destination: "/tmp/b".into() }));
    assert!(CoreServices::is_service_command(&CoreCommand::RetentionSweep {}));
    assert!(CoreServices::is_service_command(&CoreCommand::ObjectCapacity { project_id: ProjectId::new() }));
    assert!(CoreServices::is_service_command(&CoreCommand::ConversationCreate { project_id: ProjectId::new() }));
    assert!(CoreServices::is_service_command(&CoreCommand::ConversationArchive { conversation_id: ConversationId::new() }));
    assert!(CoreServices::is_service_command(&CoreCommand::ConversationDelete { conversation_id: ConversationId::new(), resolution: RunResolution { run_ids: vec![] } }));
    assert!(CoreServices::is_service_command(&CoreCommand::ConversationRegenerate { message_id: MessageId::new(), pin: pin() }));
    assert!(CoreServices::is_service_command(&CoreCommand::RunInterrupt { run_id: RunId::new() }));
    assert!(!CoreServices::is_service_command(&CoreCommand::Status {}));
    assert!(!CoreServices::is_service_command(&CoreCommand::ProjectList {}));
    assert!(!CoreServices::is_service_command(&CoreCommand::Shutdown {}));
}

// conversation_create routes to the real leaf and returns the durable conversation id; the
// request id cannot be reused for a different command.
#[tokio::test]
async fn conversation_create_routes_to_leaf() {
    let f = fixture().await;
    let id = Uuid::new_v4();
    let reply = request(&f, id, &CoreCommand::ConversationCreate { project_id: f.project }).await;
    let conversation = ok(&reply).as_str().unwrap().to_string();
    // A different command cannot reuse the same request id (digest binding).
    let clash = f.store.accept_request(id, &CoreCommand::ProjectList {}).await;
    assert!(matches!(clash, Err(e) if e.code == "request_id_conflict"));
    // history round-trips through the dispatcher and names the conversation + project.
    let history = ok(&request(&f, Uuid::new_v4(), &CoreCommand::ConversationHistory { conversation_id: ConversationId(Uuid::parse_str(&conversation).unwrap()) }).await);
    assert_eq!(history["id"], conversation);
    assert_eq!(history["project_id"], f.project.to_string());
    assert_eq!(history["branches"].as_array().unwrap().len(), 1);
}

// queue_turn threads the frame request id as the stable turn identity; replay returns the same
// run and never creates a duplicate turn.
#[tokio::test]
async fn queue_turn_threads_request_id_and_replays() {
    let f = fixture().await;
    let conversation = make_conversation(&f).await;
    let branch = ConversationService::new(f.store.clone()).history(conversation).await.unwrap().branches[0].id;
    let command = CoreCommand::ConversationQueueTurn { conversation_id: conversation, branch_id: branch, parts: text("dispatch routing"), pin: pin(), context: None };
    let turn_id = Uuid::new_v4();
    let first = request(&f, turn_id, &command).await;
    let run_id = ok(&first)["run_id"].as_str().unwrap().to_string();
    // Replay under the same request id is a durable reply, not a second turn.
    let replay = request(&f, turn_id, &command).await;
    assert_eq!(ok(&replay)["run_id"], run_id);
    // Exactly one turn row exists for the conversation (no duplicate from the replay).
    let count: i64 = {
        let s = f.store.clone();
        s.with_connection(move |conn| Ok(conn.query_row("SELECT count(*) FROM conversation_turns WHERE conversation_id=?1", [conversation.to_string()], |r| r.get(0)).unwrap())).await.unwrap()
    };
    assert_eq!(count, 1);
}

// regenerate is the stable-id leaf call (sibling branch), routed through the dispatcher.
#[tokio::test]
async fn regenerate_routes_to_leaf_sibling() {
    let f = fixture().await;
    let conversation = make_conversation(&f).await;
    let service = ConversationService::new(f.store.clone());
    let branch = service.history(conversation).await.unwrap().branches[0].id;
    let turn = service.queue_turn(conversation, branch, Uuid::new_v4(), text("original"), pin(), None).await.unwrap();
    // The dispatcher must route conversation_regenerate to the stable-id leaf.
    let reply = request(&f, Uuid::new_v4(), &CoreCommand::ConversationRegenerate { message_id: turn.assistant_message_id, pin: pin() }).await;
    let value = ok(&reply);
    // Sibling: same user, new assistant, new branch, new run.
    assert_eq!(value["user_message_id"], turn.user_message_id.to_string());
    assert_ne!(value["assistant_message_id"], turn.assistant_message_id.to_string());
    assert_ne!(value["branch_id"], branch.to_string());
    assert_ne!(value["run_id"], turn.run_id.to_string());
}

// run_interrupt cannot be fabricated by the core; it surfaces the exact C4 dependency.
#[tokio::test]
async fn run_interrupt_is_honest_c4_dependency() {
    let f = fixture().await;
    let reply = request(&f, Uuid::new_v4(), &CoreCommand::RunInterrupt { run_id: RunId::new() }).await;
    assert_eq!(err_code(&reply), "run_dependency_missing");
}

// archive refuses a conversation with an active (queued) run: the dispatcher must not
// manufacture a terminal state on the caller's behalf.
#[tokio::test]
async fn archive_refuses_nonterminal_run() {
    let f = fixture().await;
    let conversation = make_conversation(&f).await;
    let service = ConversationService::new(f.store.clone());
    let branch = service.history(conversation).await.unwrap().branches[0].id;
    // A freshly queued turn is active (state=queued), so archive must refuse.
    service.queue_turn(conversation, branch, Uuid::new_v4(), text("active"), pin(), None).await.unwrap();
    let reply = request(&f, Uuid::new_v4(), &CoreCommand::ConversationArchive { conversation_id: conversation }).await;
    assert_eq!(err_code(&reply), "run_reconciliation_required");
}

// backup_create routes to the real backup leaf and returns a protocol-1 manifest.
#[tokio::test]
async fn backup_create_routes_to_leaf() {
    let f = fixture().await;
    let destination = f._dir.path().join("backup-out");
    let reply = request(&f, Uuid::new_v4(), &CoreCommand::BackupCreate { destination: destination.to_string_lossy().into_owned() }).await;
    let manifest = ok(&reply);
    assert_eq!(manifest["format"], 1);
    assert!(manifest["objects"].as_array().is_some());
}

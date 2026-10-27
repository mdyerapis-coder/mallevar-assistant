//! C5 boundary tests. No fixtures replace the production backup or restore path.
use std::{fs, io::{Cursor, Read}};
use ironbark_core::{contracts::*, paths::RuntimePaths, store::{Store, objects::{ObjectStore, ObjectLimits}, backup::BackupService, retention::RetentionService, imports::{ImportService, ImportSource}}};
use ironbark_core::knowledge::KnowledgeService;
use rusqlite::params;
use tempfile::TempDir;

struct Fixture { temp: TempDir, store: Store, objects: ObjectStore, project: ProjectId, backup: BackupService }
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("core.sqlite3")).await.unwrap();
        let project = ProjectId::new();
        store.with_transaction(move |tx| { tx.execute("INSERT INTO projects VALUES(?1,'backup-test','/not-a-native-profile',0)", [project.to_string()])?; Ok(()) }).await.unwrap();
        let root = temp.path().join("objects");
        let objects = ObjectStore::new(store.clone(), project, root.clone(), ObjectLimits::default()).unwrap();
        let backup = BackupService::new(store.clone(), root).unwrap();
        Self { temp, store, objects, project, backup }
    }
    fn target(&self) -> RuntimePaths {
        let root = self.temp.path().join("new-namespace");
        let data_dir = root.join("data"); let state_dir = root.join("state"); let runtime_dir = root.join("runtime");
        RuntimePaths { database: data_dir.join("core.sqlite3"), socket: runtime_dir.join("core.sock"), nonce: runtime_dir.join("session.nonce"), lock: runtime_dir.join("core.lock"), data_dir, state_dir, runtime_dir }
    }
    async fn references(&self, artifact: ArtifactId) -> RunId {
        let project = self.project;
        let run = ironbark_core::scheduler::service::SchedulerService::new(self.store.clone(),project).unwrap().create_run("capture historical backup evidence").await.unwrap().run_id;
        self.store.with_transaction(move |tx| {
            let digest: String = tx.query_row("SELECT digest FROM artifacts WHERE id=?1", [artifact.to_string()], |r|r.get(0))?;
            tx.execute("INSERT INTO sources VALUES('old-source',?1,'archive:original')", [project.to_string()])?;
            tx.execute("INSERT INTO source_revisions VALUES('old-revision','old-source',?1,0)", [&digest])?;
            tx.execute("UPDATE runs SET state='running' WHERE id=?1",[run.to_string()])?;
            // The checkpoint itself is captured below by its real producer.
            Ok(())
        }).await.unwrap();
        ironbark_core::scheduler::checkpoints::CheckpointStore::new(self.store.clone(),project,self.objects.clone())
            .capture(run,CheckpointPhase::Planned,ResultManifest{run_id:run,native_session:None,artifacts:vec![artifact],checks:vec![],acceptance_satisfied:false,unresolved:vec!["historical assertion; no verification claimed".into()]})
            .await.unwrap();
        run
    }
}

// Catch a live-query manifest, premature GC, or latest-revision-only backup.
#[tokio::test]
async fn backup_preserves_referenced_objects_consistently() {
    let f = Fixture::new().await;
    let old = f.objects.put(Cursor::new(b"old cited revision".to_vec()), 100).await.unwrap();
    f.references(old).await;
    let new = f.objects.put(Cursor::new(b"new revision".to_vec()), 100).await.unwrap();
    f.store.with_transaction(move |tx| { let digest:String=tx.query_row("SELECT digest FROM artifacts WHERE id=?1",[new.to_string()],|r|r.get(0))?; tx.execute("INSERT INTO source_revisions VALUES('new-revision','old-source',?1,1)",[digest])?; Ok(()) }).await.unwrap();
    RetentionService::new(f.store.clone(), f.temp.path().join("objects")).unwrap().sweep().await.unwrap();
    let destination = f.temp.path().join("backup");
    let manifest = f.backup.create(&destination).await.unwrap();
    assert_eq!(manifest.objects.len(), 3); // two revisions + actual checkpoint link bytes
    let target=f.target(); BackupService::restore(&destination,&target).await.unwrap();
    let restored=Store::open(&target.database).await.unwrap();
    let objects=ObjectStore::new(restored.clone(),f.project,target.data_dir.join("objects"),ObjectLimits::default()).unwrap();
    let mut reader=objects.get(old).await.unwrap(); let mut bytes=Vec::new(); reader.read_to_end(&mut bytes).unwrap(); assert_eq!(bytes,b"old cited revision");
    let counts=restored.with_connection(|conn| Ok((conn.query_row("SELECT count(*) FROM source_revisions",[],|r|r.get::<_,i64>(0))?, conn.query_row("SELECT count(*) FROM checkpoints",[],|r|r.get::<_,i64>(0))?))).await.unwrap();
    assert_eq!(counts,(2,1));
}

// Catch validation after installation, partial restore, or trusting filename hashes.
#[tokio::test]
async fn missing_or_corrupt_object_aborts_restore() {
    for missing in [false,true] {
        let f=Fixture::new().await; f.objects.put(Cursor::new(b"durable evidence".to_vec()),100).await.unwrap();
        let backup=f.temp.path().join("backup"); let manifest=f.backup.create(&backup).await.unwrap();
        let object=backup.join("objects").join(manifest.objects[0].digest.as_str());
        if missing { fs::remove_file(object).unwrap(); } else { fs::write(object,b"corrupt evidence").unwrap(); }
        let native=f.temp.path().join("native-transcripts"); fs::create_dir(&native).unwrap(); fs::write(native.join("keep"),b"native sentinel").unwrap();
        let target=f.target(); let error=BackupService::restore(&backup,&target).await.unwrap_err();
        assert_eq!(error.code,"IntegrityMismatch"); assert!(!target.data_dir.exists()); assert!(!target.state_dir.exists()); assert!(!target.runtime_dir.exists());
        assert_eq!(fs::read(native.join("keep")).unwrap(),b"native sentinel");
    }
}

#[tokio::test]
async fn restored_runs_do_not_auto_resume() {
    let f=Fixture::new().await; let artifact=f.objects.put(Cursor::new(b"checkpoint".to_vec()),100).await.unwrap(); let run=f.references(artifact).await;
    let backup=f.temp.path().join("backup"); f.backup.create(&backup).await.unwrap(); let target=f.target(); let report=BackupService::restore(&backup,&target).await.unwrap();
    assert_eq!(report.blocked_runs,1); assert!(!target.socket.exists()); assert!(!target.nonce.exists());
    let conn=rusqlite::Connection::open(&target.database).unwrap(); let state:String=conn.query_row("SELECT state FROM runs WHERE id=?1",[run.to_string()],|r|r.get(0)).unwrap(); assert_eq!(state,"blocked");
}

// Catch copying runtime/vault material and leaving secret bytes on SQLite free pages.
#[tokio::test]
async fn export_excludes_secret_leases() {
    let f=Fixture::new().await; let sentinel="C5_SECRET_SENTINEL_39ca2896b9c64b1d";
    fs::write(f.temp.path().join("session.nonce"),sentinel).unwrap();
    let project=f.project;
    f.store.with_transaction(move |tx| {
        let config=ConnectionConfig{id:"secret-test".into(),protocol:ProviderProtocol::OpenaiCompletions,endpoint:format!("https://legacy:{sentinel}@example.test/v1"),models:vec![],auth_ref:Some(AuthRef::SecretService{service:"ironbark".into(),account:"test".into()}),allowed_projects:vec![project]};
        tx.execute("INSERT INTO provider_references VALUES('secret-test',?1)",[serde_json::to_string(&config)?])?; Ok(())
    }).await.unwrap();
    let backup=f.temp.path().join("backup"); f.backup.create(&backup).await.unwrap();
    for name in ["core.sqlite3","manifest.json"] { assert!(!fs::read(backup.join(name)).unwrap().windows(sentinel.len()).any(|w|w==sentinel.as_bytes())); }
    assert!(!backup.join("session.nonce").exists());
    let conn=rusqlite::Connection::open(backup.join("core.sqlite3")).unwrap(); let config:String=conn.query_row("SELECT config FROM provider_references WHERE id='secret-test'",[],|r|r.get(0)).unwrap(); assert!(config.contains("secret_service")); assert!(config.contains("REDACTED"));
    drop(conn);
    let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    let restored=rusqlite::Connection::open(target.database).unwrap();
    let config:String=restored.query_row("SELECT config FROM provider_references WHERE id='secret-test'",[],|r|r.get(0)).unwrap();
    let config:ConnectionConfig=serde_json::from_str(&config).unwrap();
    assert!(config.auth_ref.is_none());assert!(config.allowed_projects.is_empty());
}

#[tokio::test]
async fn changed_import_preview_requires_review() {
    let f=Fixture::new().await; let path=f.temp.path().join("history.json");
    fs::write(&path,br#"{"kind":"native_transcript","origin":"codex","text":"old transcript"}"#).unwrap();
    let imports=ImportService::new(f.objects.clone()).unwrap();
    let preview=imports.preview(ImportSource::document(path.clone())).await.unwrap();
    fs::write(&path,br#"{"kind":"native_transcript","origin":"codex","text":"changed transcript"}"#).unwrap();
    assert_eq!(imports.apply(preview.digest).await.unwrap_err().code,"ImportChanged");
    let count=f.store.with_connection(|c|Ok(c.query_row("SELECT count(*) FROM conversations",[],|r|r.get::<_,i64>(0))?)).await.unwrap(); assert_eq!(count,0);
}

#[tokio::test]
async fn existing_namespace_is_untouched() {
    let f=Fixture::new().await; let backup=f.temp.path().join("backup"); f.backup.create(&backup).await.unwrap(); let target=f.target();
    fs::create_dir_all(&target.data_dir).unwrap(); fs::write(target.data_dir.join("keep"),b"do not overwrite").unwrap();
    assert_eq!(BackupService::restore(&backup,&target).await.unwrap_err().code,"NamespaceExists");
    assert_eq!(fs::read(target.data_dir.join("keep")).unwrap(),b"do not overwrite");
}

#[tokio::test]
async fn streaming_item_and_store_caps_do_not_publish_partial_rows() {
    let f=Fixture::new().await;
    assert_eq!(f.objects.put(Cursor::new(vec![0;101]),100).await.unwrap_err().code,"ItemTooLarge");
    let objects=ObjectStore::new(f.store.clone(),f.project,f.temp.path().join("objects"),ObjectLimits { item_bytes:100,soft_bytes:3,hard_bytes:6 }).unwrap();
    let id=objects.put(Cursor::new(b"1234".to_vec()),100).await.unwrap(); assert!(objects.capacity().await.unwrap().soft_exceeded);
    assert_eq!(objects.put(Cursor::new(b"5678".to_vec()),100).await.unwrap_err().code,"StoreFull");
    // Same digest does not count twice against physical capacity.
    objects.put(Cursor::new(b"1234".to_vec()),100).await.unwrap();
    let mut data=String::new(); objects.get(id).await.unwrap().read_to_string(&mut data).unwrap(); assert_eq!(data,"1234");
    let another=ProjectId::new(); f.store.with_transaction(move |tx|{tx.execute("INSERT INTO projects VALUES(?1,'other','/',0)",[another.to_string()])?;Ok(())}).await.unwrap();
    let foreign=ObjectStore::new(f.store.clone(),another,f.temp.path().join("objects"),ObjectLimits::default()).unwrap(); assert_eq!(foreign.get(id).await.err().unwrap().code,"ArtifactNotFound");
}

#[tokio::test]
async fn transcript_import_is_additive_archived_and_idempotent() {
    let f=Fixture::new().await; let path=f.temp.path().join("native.json"); let original=br#"{"kind":"native_transcript","origin":"codex/session-42","text":"archived content"}"#;
    fs::write(&path,original).unwrap(); let service=ImportService::new(f.objects.clone()).unwrap();
    let preview=service.preview(ImportSource::document(path.clone())).await.unwrap(); let receipt=service.apply(preview.digest.clone()).await.unwrap(); let again=service.apply(preview.digest).await.unwrap(); assert_eq!(receipt,again);
    assert_eq!(fs::read(path).unwrap(),original);
    assert!(receipt.archived_only); assert!(receipt.artifact_id.is_some());
    let counts=f.store.with_connection(|c|Ok((c.query_row("SELECT count(*) FROM runs",[],|r|r.get::<_,i64>(0))?,c.query_row("SELECT count(*) FROM conversations",[],|r|r.get::<_,i64>(0))?))).await.unwrap(); assert_eq!(counts,(0,0));
}

// Catch collecting real checkpoint link bytes or historical source revisions.
#[tokio::test]
async fn retention_marks_old_citations_and_canonical_checkpoint_objects() {
    let f=Fixture::new().await;
    let citation=f.objects.put(Cursor::new(b"citation evidence".to_vec()),100).await.unwrap();
    let orphan=f.objects.put(Cursor::new(b"unreferenced garbage".to_vec()),100).await.unwrap();
    let run=f.references(citation).await;
    let (citation_digest,checkpoint_digest,orphan_digest)=f.store.with_transaction(move |tx|{
        let c:String=tx.query_row("SELECT digest FROM source_revisions WHERE id='old-revision'",[],|r|r.get(0))?;
        let p:String=tx.query_row("SELECT digest FROM artifacts WHERE run_id=?1",[run.to_string()],|r|r.get(0))?;
        let o:String=tx.query_row("SELECT digest FROM artifacts WHERE id=?1",[orphan.to_string()],|r|r.get(0))?;
        tx.execute("DELETE FROM artifacts WHERE id=?1",[orphan.to_string()])?;
        Ok((c,p,o))
    }).await.unwrap();
    let original_checkpoint=fs::read(f.temp.path().join("objects").join(&checkpoint_digest)).unwrap();
    let report=RetentionService::new(f.store.clone(),f.temp.path().join("objects")).unwrap().sweep().await.unwrap();
    assert_eq!(report.removed_objects,1);assert_eq!(report.retained_objects,2);
    assert_eq!(fs::read(f.temp.path().join("objects").join(citation_digest)).unwrap(),b"citation evidence");
    assert_eq!(fs::read(f.temp.path().join("objects").join(&checkpoint_digest)).unwrap(),original_checkpoint);
    assert!(!f.temp.path().join("objects").join(orphan_digest).exists());
    let backup=f.temp.path().join("backup");assert_eq!(f.backup.create(&backup).await.unwrap().objects.len(),2);
    let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    assert_eq!(fs::read(target.data_dir.join("objects").join(checkpoint_digest)).unwrap(),original_checkpoint);
}

#[tokio::test]
async fn source_sidecar_change_is_bound_to_import_preview() {
    let f=Fixture::new().await;let sidecar=f.temp.path().join("source.txt");let path=f.temp.path().join("source.json");
    fs::write(&sidecar,b"original source revision").unwrap();fs::write(&path,br#"{"kind":"source","uri":"file:source.txt","path":"source.txt"}"#).unwrap();
    let imports=ImportService::new(f.objects.clone()).unwrap();let preview=imports.preview(ImportSource::document(path.clone())).await.unwrap();
    assert_eq!(preview.files.len(),2);fs::write(&sidecar,b"changed source revision").unwrap();
    assert_eq!(imports.apply(preview.digest).await.unwrap_err().code,"ImportChanged");
    let count=f.store.with_connection(|c|Ok(c.query_row("SELECT count(*) FROM sources",[],|r|r.get::<_,i64>(0))?)).await.unwrap();assert_eq!(count,0);
    let preview=imports.preview(ImportSource::document(path)).await.unwrap();let receipt=imports.apply(preview.digest).await.unwrap();
    let mut bytes=Vec::new();f.objects.get(receipt.artifact_id.unwrap()).await.unwrap().read_to_end(&mut bytes).unwrap();assert_eq!(bytes,b"changed source revision");
}

#[tokio::test]
async fn restore_revokes_unconsumed_authority_and_blocks_all_nonterminal_work() {
    let f=Fixture::new().await;let artifact=f.objects.put(Cursor::new(b"grant evidence".to_vec()),100).await.unwrap();let run=f.references(artifact).await;
    let project=f.project;let action=ActionId::new();let grant=GrantId::new();
    f.store.with_transaction(move |tx|{
        let digest="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let action_body=serde_json::to_string(&ActionSpec{id:action,run_id:run,project_id:project,kind:"file_write".into(),canonical_args:serde_json::json!({"path":"/external","content_digest":digest}),scope:serde_json::json!({"tools":["file_write"],"paths":["/external"]}),destination:None,policy_version:1,expected_state_digest:Digest::parse(digest)?})?;
        let grant_body=serde_json::to_string(&ApprovalGrant{id:grant,action_id:action,action_digest:Digest::parse(digest)?,policy_version:1,expected_state_digest:Digest::parse(digest)?,expires_at:i64::MAX,revoked_at:None})?;
        tx.execute("INSERT INTO actions VALUES(?1,?2,?3,?4,1,'approved',?5)",params![action.to_string(),run.to_string(),project.to_string(),digest,action_body])?;
        tx.execute("INSERT INTO grants(id,action_id,action_digest,expires_at,body) VALUES(?1,?2,?3,9223372036854775807,?4)",params![grant.to_string(),action.to_string(),digest,grant_body])?;
        Ok(())
    }).await.unwrap();
    let backup=f.temp.path().join("backup");f.backup.create(&backup).await.unwrap();let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    let conn=rusqlite::Connection::open(target.database).unwrap();
    let revoked:i64=conn.query_row("SELECT count(*) FROM grants WHERE revoked_at IS NOT NULL",[],|r|r.get(0)).unwrap();assert_eq!(revoked,1);
    let state:String=conn.query_row("SELECT state FROM actions WHERE id=?1",[action.to_string()],|r|r.get(0)).unwrap();assert_eq!(state,"blocked");
}

#[tokio::test]
async fn import_refuses_symlink_source_without_mutating_native_target() {
    let f=Fixture::new().await;let native=f.temp.path().join("native.json");let content=br#"{"kind":"native_transcript","origin":"codex","text":"original native session"}"#;
    fs::write(&native,content).unwrap();let link=f.temp.path().join("link.json");std::os::unix::fs::symlink(&native,&link).unwrap();
    let imports=ImportService::new(f.objects.clone()).unwrap();assert_eq!(imports.preview(ImportSource::document(link)).await.unwrap_err().code,"unsafe_path");assert_eq!(fs::read(native).unwrap(),content);
}

// The writer uses a real second SQLite connection. Compare references from the
// completed snapshot independently with its manifest, while the live DB changes.
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn online_backup_manifest_matches_snapshot_during_concurrent_writes() {
    use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
    let f=Fixture::new().await;let artifact=f.objects.put(Cursor::new(b"stable online object".to_vec()),100).await.unwrap();
    let digest=f.store.with_connection(move |c|Ok(c.query_row("SELECT digest FROM artifacts WHERE id=?1",[artifact.to_string()],|r|r.get::<_,String>(0))?)).await.unwrap();
    let alternate=f.objects.put(Cursor::new(b"concurrent revision only".to_vec()),100).await.unwrap();
    let alternate_digest=f.store.with_transaction(move |tx|{
        let digest:String=tx.query_row("SELECT digest FROM artifacts WHERE id=?1",[alternate.to_string()],|r|r.get(0))?;
        tx.execute("DELETE FROM artifacts WHERE id=?1",[alternate.to_string()])?;Ok(digest)
    }).await.unwrap();
    let (started_tx,started_rx)=std::sync::mpsc::channel();
    let path=f.temp.path().join("core.sqlite3");let stop=Arc::new(AtomicBool::new(false));let worker_stop=stop.clone();let project=f.project;let writer_digest=alternate_digest;
    let writer=std::thread::spawn(move ||{
        let conn=rusqlite::Connection::open(path).unwrap();conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        let mut iteration=0;
        while !worker_stop.load(Ordering::Acquire) {
            conn.execute("INSERT OR REPLACE INTO artifacts(id,project_id,run_id,digest,path) VALUES('online-row',?1,NULL,?2,?2)",params![project.to_string(),writer_digest]).unwrap();
            conn.execute("DELETE FROM artifacts WHERE id='online-row'",[]).unwrap();iteration+=1;
            if iteration==1 {started_tx.send(()).unwrap();}
        } iteration
    });
    if let Err(error)=started_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        stop.store(true,Ordering::Release);let _=writer.join();panic!("real concurrent writer did not start: {error}");
    }
    let result=f.backup.create(&f.temp.path().join("online-backup")).await;
    stop.store(true,Ordering::Release);let iterations=writer.join().unwrap();assert!(iterations>0);let manifest=result.unwrap();
    let conn=rusqlite::Connection::open(f.temp.path().join("online-backup/core.sqlite3")).unwrap();
    let mut stmt=conn.prepare("SELECT DISTINCT digest FROM artifacts ORDER BY digest").unwrap();let snapshot=stmt.query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap();
    assert_eq!(snapshot,manifest.objects.iter().map(|o|o.digest.as_str().to_owned()).collect::<Vec<_>>());assert!(snapshot.contains(&digest));
}

#[tokio::test]
async fn export_refuses_unreviewed_columns_instead_of_guessing_their_secrecy() {
    let f=Fixture::new().await;let sentinel="C5_METADATA_SECRET_5df462b83f12401d";
    f.store.with_connection(move |conn|{
        conn.execute("ALTER TABLE provider_references ADD COLUMN opaque_secret TEXT",[])?;
        conn.execute("INSERT INTO provider_references(id,config,opaque_secret) VALUES('metadata','{}',?1)",[sentinel])?;Ok(())
    }).await.unwrap();
    let backup=f.temp.path().join("backup");
    assert_eq!(f.backup.create(&backup).await.unwrap_err().code,"UnknownBackupSchema");
    assert!(!backup.exists());
}

#[tokio::test]
async fn unreviewed_schema_or_sql_trigger_cannot_be_exported() {
    for trigger in [false,true] {
        let f=Fixture::new().await;
        f.store.with_connection(move |conn|{
            if trigger {conn.execute_batch("CREATE TRIGGER replay_after_restore AFTER UPDATE ON runs BEGIN UPDATE runs SET state='running' WHERE id=NEW.id; END;")?;}
            else {conn.execute_batch("CREATE TABLE native_credential_leases(material TEXT); INSERT INTO native_credential_leases VALUES('never-export');")?;}
            Ok(())
        }).await.unwrap();
        let backup=f.temp.path().join("unsafe-backup");let error=f.backup.create(&backup).await.unwrap_err();
        assert!(matches!(error.code.as_str(),"UnknownBackupSchema"|"IntegrityMismatch"));assert!(!backup.exists());
    }
}

#[tokio::test]
async fn actual_native_jsonl_import_is_archived_redacted_not_resumable() {
    let f=Fixture::new().await;let path=f.temp.path().join("native-session.jsonl");let sentinel="C5_NATIVE_LEASE_183c660571f04fcb";
    let bytes=format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"native-42\",\"credential_lease_id\":\"{sentinel}\"}}}}\n{{\"type\":\"message\",\"text\":\"native transcript content\"}}\n");
    fs::write(&path,&bytes).unwrap();let imports=ImportService::new(f.objects.clone()).unwrap();
    let preview=imports.preview(ImportSource::native_transcript(path.clone(),"codex/native-42".into())).await.unwrap();
    let receipt=imports.apply(preview.digest).await.unwrap();assert!(receipt.archived_only);
    let mut archive=String::new();f.objects.get(receipt.artifact_id.unwrap()).await.unwrap().read_to_string(&mut archive).unwrap();
    assert!(!archive.contains(sentinel));assert!(archive.contains("native-42"));assert!(archive.contains("native transcript content"));assert!(archive.contains("REDACTED"));
    assert_eq!(fs::read_to_string(path).unwrap(),bytes);
    let count=f.store.with_connection(|c|Ok(c.query_row("SELECT count(*) FROM runs",[],|r|r.get::<_,i64>(0))?)).await.unwrap();assert_eq!(count,0);
}

#[tokio::test]
async fn replacing_import_with_identical_bytes_requires_identity_review() {
    let f=Fixture::new().await;let path=f.temp.path().join("history.json");let bytes=br#"{"kind":"native_transcript","origin":"codex","text":"same bytes but a different selected file"}"#;
    fs::write(&path,bytes).unwrap();let imports=ImportService::new(f.objects.clone()).unwrap();let preview=imports.preview(ImportSource::document(path.clone())).await.unwrap();
    let replacement=f.temp.path().join("replacement");fs::write(&replacement,bytes).unwrap();fs::rename(replacement,path).unwrap();
    assert_eq!(imports.apply(preview.digest).await.unwrap_err().code,"ImportChanged");
}

#[tokio::test]
async fn default_attachment_cap_cannot_be_raised_by_caller_limit() {
    let f=Fixture::new().await;
    assert_eq!(f.objects.put(std::io::repeat(1).take(50*1024*1024+1),u64::MAX).await.unwrap_err().code,"ItemTooLarge");
    let count=f.store.with_connection(|c|Ok(c.query_row("SELECT count(*) FROM artifacts",[],|r|r.get::<_,i64>(0))?)).await.unwrap();assert_eq!(count,0);
}

#[tokio::test]
async fn connection_import_is_additive_and_preserves_original_identity() {
    let f=Fixture::new().await;let path=f.temp.path().join("connection.json");
    f.store.with_transaction(|tx|{tx.execute("INSERT INTO provider_references VALUES('same','{\"marker\":\"keep-existing-config\"}')",[])?;Ok(())}).await.unwrap();
    fs::write(&path,br#"{"kind":"connection","config":{"id":"same","protocol":"openai_completions","endpoint":"https://example.test/v1","models":[],"allowed_projects":[]}}"#).unwrap();
    let imports=ImportService::new(f.objects.clone()).unwrap();let preview=imports.preview(ImportSource::document(path)).await.unwrap();let receipt=imports.apply(preview.digest).await.unwrap();
    assert_eq!(receipt.origin,"same");assert_ne!(receipt.connection_id.as_deref(),Some("same"));
    let imported=receipt.connection_id.unwrap();let project=f.project;
    f.store.with_connection(move |c|{
        let old:String=c.query_row("SELECT config FROM provider_references WHERE id='same'",[],|r|r.get(0))?;assert_eq!(old,"{\"marker\":\"keep-existing-config\"}");
        let new:String=c.query_row("SELECT config FROM provider_references WHERE id=?1",[imported],|r|r.get(0))?;let new:ConnectionConfig=serde_json::from_str(&new)?;assert_eq!(new.allowed_projects,vec![project]);Ok(())
    }).await.unwrap();
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn ingestion_concurrency_is_shared_across_project_bound_handles() {
    use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
    struct ObservedReader {active:Arc<AtomicUsize>,peak:Arc<AtomicUsize>,entered:bool,done:bool,byte:u8}
    impl Read for ObservedReader {
        fn read(&mut self,buffer:&mut [u8])->std::io::Result<usize> {
            if self.done {return Ok(0);}
            if !self.entered {self.entered=true;let now=self.active.fetch_add(1,Ordering::SeqCst)+1;self.peak.fetch_max(now,Ordering::SeqCst);}
            std::thread::sleep(std::time::Duration::from_millis(30));buffer[0]=self.byte;self.done=true;Ok(1)
        }
    }
    impl Drop for ObservedReader {fn drop(&mut self){if self.entered{self.active.fetch_sub(1,Ordering::SeqCst);}}}
    let f=Fixture::new().await;let active=Arc::new(AtomicUsize::new(0));let peak=Arc::new(AtomicUsize::new(0));let mut tasks=Vec::new();
    for byte in 0..6 {
        let objects=ObjectStore::new(f.store.clone(),f.project,f.temp.path().join("objects"),ObjectLimits::default()).unwrap();
        let reader=ObservedReader{active:active.clone(),peak:peak.clone(),entered:false,done:false,byte};
        tasks.push(tokio::spawn(async move {objects.put(reader,10).await}));
    }
    for task in tasks {task.await.unwrap().unwrap();}
    assert!(peak.load(Ordering::SeqCst)<=2);assert_eq!(active.load(Ordering::SeqCst),0);
}

#[tokio::test]
async fn retention_reclaims_abandoned_staging_but_not_active_ingestion() {
    use std::os::fd::AsRawFd;
    let f=Fixture::new().await;let staging=f.temp.path().join("objects/.staging");let stale=staging.join("abandoned");let active=staging.join("active");
    fs::write(&stale,b"abandoned temporary bytes").unwrap();let file=fs::File::create(&active).unwrap();
    assert_eq!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
    let retention=RetentionService::new(f.store.clone(),f.temp.path().join("objects")).unwrap();let report=retention.sweep().await.unwrap();
    assert_eq!(report.removed_staging_files,1);assert!(!stale.exists());assert!(active.exists());
    drop(file);assert_eq!(retention.sweep().await.unwrap().removed_staging_files,1);assert!(!active.exists());
}

#[tokio::test]
async fn restored_partial_turn_retains_content_but_cannot_redispatch() {
    let f=Fixture::new().await;let artifact=f.objects.put(Cursor::new(b"partial checkpoint".to_vec()),100).await.unwrap();let run=f.references(artifact).await;
    let project=f.project;let conversation=ConversationId::new();let branch=BranchId::new();let message=MessageId::new();
    f.store.with_transaction(move |tx|{
        tx.execute("INSERT INTO conversations(id,project_id,created_at) VALUES(?1,?2,0)",params![conversation.to_string(),project.to_string()])?;
        tx.execute("INSERT INTO branches(id,conversation_id) VALUES(?1,?2)",params![branch.to_string(),conversation.to_string()])?;
        let content=serde_json::to_string(&vec![ContentPart::Text{text:"real partial response".into()}])?;
        let snapshot=serde_json::to_string(&stored_snapshot(project))?;
        tx.execute("INSERT INTO messages(id,branch_id,role,content,created_at,status) VALUES(?1,?2,'assistant',?3,0,'streaming')",params![message.to_string(),branch.to_string(),content])?;
        tx.execute("INSERT INTO conversation_turns(run_id,conversation_id,branch_id,request_id,request_digest,assistant_message_id,snapshot) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![run.to_string(),conversation.to_string(),branch.to_string(),uuid::Uuid::new_v4().to_string(),"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",message.to_string(),snapshot])?;
        Ok(())
    }).await.unwrap();
    let backup=f.temp.path().join("backup");f.backup.create(&backup).await.unwrap();let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    let conn=rusqlite::Connection::open(target.database).unwrap();
    let (content,status):(String,String)=conn.query_row("SELECT content,status FROM messages WHERE id=?1",[message.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(serde_json::from_str::<Vec<ContentPart>>(&content).unwrap(),vec![ContentPart::Text{text:"real partial response".into()}]);assert_eq!(status,"interrupted");
    let claimed:i64=conn.query_row("SELECT dispatch_claimed FROM conversation_turns WHERE run_id=?1",[run.to_string()],|r|r.get(0)).unwrap();assert_eq!(claimed,1);
}

/// A store that has ingested knowledge must still back up and restore, and the
/// restored store must rank the same cited hit. The FTS5 mirror (and the shadow
/// tables FTS5 creates for it) is derived content: exported, not refused.
#[tokio::test]
async fn backup_after_knowledge_ingest_restores_and_retrieval_still_ranks() {
    let f=Fixture::new().await;
    commit_policy(&f).await;
    let service=KnowledgeService::new(f.store.clone(),f.temp.path().join("objects"));
    let file=f.temp.path().join("notes.txt");
    let bytes=b"the quick brown knowledge fox".to_vec();
    fs::write(&file,&bytes).unwrap();
    // The revision digest is the content digest, so the ingest path itself must
    // publish the same bytes into the CAS: a source revision is always a
    // retention root (StorageSchema::roots), and nothing here writes it.
    // Ingesting creates the FTS5 mirror lazily; this is the trigger for the gap.
    let revision=service.ingest(f.project,SourceInput::File{path:file.to_string_lossy().into_owned()},DisclosurePolicy::ExplicitPublic).await.unwrap();
    let before=service.retrieve(f.project,"knowledge",10).await.unwrap();
    assert_eq!(before.len(),1,"pre-backup retrieval returns the cited hit");
    let citation=before[0].citation.clone();

    let backup=f.temp.path().join("backup");
    let manifest=f.backup.create(&backup).await.unwrap();
    assert_eq!(manifest.objects.len(),1,"the ingested revision's object closure is exported");
    let target=f.target();
    BackupService::restore(&backup,&target).await.unwrap();

    let restored=Store::open(&target.database).await.unwrap();
    let restored_service=KnowledgeService::new(restored.clone(),target.data_dir.join("objects"));
    let after=restored_service.retrieve(f.project,"knowledge",10).await.unwrap();
    assert_eq!(after.len(),1,"the restored store still ranks the same hit");
    assert_eq!(after[0].citation,citation);
    assert_eq!(after[0].citation.revision_digest.as_str(),revision.digest.as_str());
    assert!(after[0].text.contains("knowledge"));
    // The exported index text is exactly the exported base-row text: no
    // half-restored or divergent mirror.
    let divergent=restored.with_connection(|conn|Ok(conn.query_row("SELECT count(*) FROM knowledge_fts f JOIN chunks c ON c.id=f.chunk_id WHERE f.content != c.content",[],|r|r.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(divergent,0);
}

/// The derived-FTS exemption is an exact allowlist, not a name prefix, and the
/// fail-closed blob rule still holds for reviewed tables. Both must refuse.
#[tokio::test]
async fn derived_fts_exemption_does_not_widen_the_allowlist() {
    for mutation in ["lookalike_fts","blob_column"] {
        let f=Fixture::new().await;
        f.store.with_connection(move |conn|{
            if mutation=="lookalike_fts" {
                conn.execute_batch("CREATE TABLE knowledge_fts_extra(material TEXT); INSERT INTO knowledge_fts_extra VALUES('never-export');")?;
            } else {
                conn.execute_batch("ALTER TABLE provider_references ADD COLUMN sneak BLOB; INSERT INTO provider_references(id,config,sneak) VALUES('b','{}',x'00ff00ff');")?;
            }
            Ok(())
        }).await.unwrap();
        let backup=f.temp.path().join("unsafe-backup");
        assert_eq!(f.backup.create(&backup).await.unwrap_err().code,"UnknownBackupSchema");
        assert!(!backup.exists());
    }
}

/// The derived index must not retain a token that redaction removed from the
/// base row: the exported FTS mirror is reconciled with `chunks` after redaction.
#[tokio::test]
async fn exported_fts_index_cannot_retain_a_redacted_credential_token() {
    let f=Fixture::new().await;
    let service=KnowledgeService::new(f.store.clone(),f.temp.path().join("objects"));
    let file=f.temp.path().join("creds.txt");
    let bytes=b"release notes api_key: zzsecretzz42 tail".to_vec();
    fs::write(&file,&bytes).unwrap();
    f.objects.put(Cursor::new(bytes),100).await.unwrap();
    service.ingest(f.project,SourceInput::File{path:file.to_string_lossy().into_owned()},DisclosurePolicy::ExplicitPublic).await.unwrap();

    let backup=f.temp.path().join("backup");
    f.backup.create(&backup).await.unwrap();
    let conn=rusqlite::Connection::open(backup.join("core.sqlite3")).unwrap();
    let redacted:i64=conn.query_row("SELECT count(*) FROM chunks WHERE content LIKE '%[REDACTED]%'",[],|r|r.get(0)).unwrap();
    assert_eq!(redacted,1,"the base row was redacted");
    let indexed:i64=conn.query_row("SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zzsecretzz42'",[],|r|r.get(0)).unwrap();
    assert_eq!(indexed,0,"a redacted credential token must not survive as a searchable index term");
    let residual:i64=conn.query_row("SELECT count(*) FROM knowledge_fts WHERE content LIKE '%zzsecretzz42%'",[],|r|r.get(0)).unwrap();
    assert_eq!(residual,0,"the derived index text is reconciled with the redacted base row");
}

/// Commit a minimal policy so retrieval is permitted; no product command
/// commits one yet, so the fixture inserts it exactly as other knowledge tests do.
async fn commit_policy(f:&Fixture) {
    let project=f.project;
    let policy=serde_json::to_string(&ProjectPolicy{version:1,project_id:project,allowed_connection_ids:vec!["local".into()],allowed_tools:vec![],read_roots:vec![],write_roots:vec![],network_destinations:vec![],local_only:false,max_workers:2,max_requests:10,max_cost_microusd:None,spend_policy:SpendPolicy::FreeOnly,allow_unknown_cost:false}).unwrap();
    f.store.with_transaction(move |tx|{tx.execute("INSERT INTO policies(project_id,version,body) VALUES(?1,1,?2)",params![project.to_string(),policy])?;Ok(())}).await.unwrap();
}

fn stored_snapshot(project:ProjectId)->ironbark_core::conversations::TurnSnapshot {
    let digest=Digest::parse(&"b".repeat(64)).unwrap();
    ironbark_core::conversations::TurnSnapshot{
        route:RoutePin{connection_id:"local".into(),model_id:"model".into(),harness:None},
        context:ContextPackage{digest:digest.clone(),brief:String::new(),constraints:vec![],excerpts:vec![],artifacts:vec![],unresolved_questions:vec![],originating_sessions:vec![]},
        disclosure_digest:digest,
        policy:ProjectPolicy{version:1,project_id:project,allowed_connection_ids:vec!["local".into()],allowed_tools:vec![],read_roots:vec![],write_roots:vec![],network_destinations:vec![],local_only:true,max_workers:1,max_requests:1,max_cost_microusd:None,spend_policy:SpendPolicy::FreeOnly,allow_unknown_cost:false},
        artifact_receipts:vec![],
    }
}

#[tokio::test]
async fn typed_citation_and_attachment_are_roots_without_artifact_rows() {
    let f=Fixture::new().await;
    let cited=f.objects.put(Cursor::new(b"snapshot-only original".to_vec()),100).await.unwrap();
    let attachment=f.objects.put(Cursor::new(b"attachment-only original".to_vec()),100).await.unwrap();
    let run=ironbark_core::scheduler::service::SchedulerService::new(f.store.clone(),f.project).unwrap().create_run("preserve citations").await.unwrap().run_id;
    let project=f.project;
    let (citation_digest,attachment_digest)=f.store.with_transaction(move |tx|{
        let citation_digest:String=tx.query_row("SELECT digest FROM artifacts WHERE id=?1",[cited.to_string()],|r|r.get(0))?;
        let attachment_digest:String=tx.query_row("SELECT digest FROM artifacts WHERE id=?1",[attachment.to_string()],|r|r.get(0))?;
        let conversation=ConversationId::new();let branch=BranchId::new();let message=MessageId::new();
        tx.execute("INSERT INTO conversations(id,project_id,created_at) VALUES(?1,?2,0)",params![conversation.to_string(),project.to_string()])?;
        tx.execute("INSERT INTO branches(id,conversation_id) VALUES(?1,?2)",params![branch.to_string(),conversation.to_string()])?;
        let parts=serde_json::to_string(&vec![ContentPart::Text{text:r#"{"artifact_id":"literal-only"}"#.into()}])?;
        tx.execute("INSERT INTO messages(id,branch_id,role,content,created_at) VALUES(?1,?2,'user',?3,0)",params![message.to_string(),branch.to_string(),parts])?;
        tx.execute("INSERT INTO attachments VALUES(?1,?2,?3,?3)",params![uuid::Uuid::new_v4().to_string(),message.to_string(),attachment_digest])?;
        let mut snapshot=stored_snapshot(project);
        snapshot.context.excerpts.push(CitedExcerpt{text:"snapshot-only original".into(),citation:Citation{source_id:"historical-source".into(),revision_digest:Digest::parse(&citation_digest)?,locator:CitationLocator{page:None,section:None,line_start:Some(1),line_end:Some(1),sheet:None,cell_range:None,bounding_box:None,time_start_ms:None,time_end_ms:None}},project_id:project,disclosure:DisclosurePolicy::LocalOnly});
        tx.execute("INSERT INTO conversation_turns(run_id,conversation_id,branch_id,request_id,request_digest,user_message_id,snapshot) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![run.to_string(),conversation.to_string(),branch.to_string(),uuid::Uuid::new_v4().to_string(),snapshot.context.digest.as_str(),message.to_string(),serde_json::to_string(&snapshot)?])?;
        tx.execute("DELETE FROM artifacts",[])?;
        Ok((citation_digest,attachment_digest))
    }).await.unwrap();
    let report=RetentionService::new(f.store.clone(),f.temp.path().join("objects")).unwrap().sweep().await.unwrap();
    assert_eq!(report.retained_objects,2);assert_eq!(report.removed_objects,0);
    let backup=f.temp.path().join("backup");assert_eq!(f.backup.create(&backup).await.unwrap().objects.len(),2);
    let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    assert_eq!(fs::read(target.data_dir.join("objects").join(citation_digest)).unwrap(),b"snapshot-only original");
    assert_eq!(fs::read(target.data_dir.join("objects").join(attachment_digest)).unwrap(),b"attachment-only original");
}

#[tokio::test]
async fn unknown_checkpoint_form_refuses_before_sweep_or_backup_installation() {
    let f=Fixture::new().await;
    let artifact=f.objects.put(Cursor::new(b"unsupported form must not unlink".to_vec()),100).await.unwrap();
    let run=f.references(artifact).await;
    let orphan=f.objects.put(Cursor::new(b"unlinked only after complete review".to_vec()),100).await.unwrap();
    let digest=f.store.with_transaction(move |tx|{
        let digest:String=tx.query_row("SELECT digest FROM artifacts WHERE id=?1",[orphan.to_string()],|r|r.get(0))?;
        tx.execute("DELETE FROM artifacts WHERE id=?1",[orphan.to_string()])?;
        tx.execute("UPDATE checkpoints SET body=json_set(body,'$.future_reference',?1) WHERE run_id=?2",params![digest,run.to_string()])?;
        Ok(digest)
    }).await.unwrap();
    assert!(RetentionService::new(f.store.clone(),f.temp.path().join("objects")).unwrap().sweep().await.is_err());
    assert_eq!(fs::read(f.temp.path().join("objects").join(digest)).unwrap(),b"unlinked only after complete review");
    let backup=f.temp.path().join("backup");assert!(f.backup.create(&backup).await.is_err());assert!(!backup.exists());
}

#[tokio::test]
async fn identity_hashes_external_write_hashes_and_nonsecret_score_tokens_are_not_cas_roots() {
    let f=Fixture::new().await;
    let run=ironbark_core::scheduler::service::SchedulerService::new(f.store.clone(),f.project).unwrap().create_run(r#"literal {"artifact_id":"not-authority"}"#).await.unwrap().run_id;
    let project=f.project;let identity=Digest::parse(&"a".repeat(64)).unwrap();
    let identity_copy=identity.clone();
    f.store.with_transaction(move |tx|{
        let action=ActionSpec{id:ActionId::new(),run_id:run,project_id:project,kind:"file_write".into(),canonical_args:serde_json::json!({"path":"/external/file","content_digest":identity}),scope:serde_json::json!({"tools":["file_write"],"paths":["/external/file"]}),destination:None,policy_version:1,expected_state_digest:identity.clone()};
        tx.execute("INSERT INTO actions VALUES(?1,?2,?3,?4,1,'approved',?5)",params![action.id.to_string(),run.to_string(),project.to_string(),identity.as_str(),serde_json::to_string(&action)?])?;
        let judgment=JudgmentRecord{decision_id:DecisionId::new(),domain:"task_intent".into(),domain_version:"1".into(),candidates:vec!["yes".into()],input_refs:vec!["text provenance".into()],input_digest:identity.clone(),criterion:"choose".into(),route_policy_digest:identity.clone(),model_identity:"bearer model-identity".into(),runtime_identity_digest:identity.clone(),prompt_digest:identity.clone(),candidate_codes:std::collections::BTreeMap::new(),choice:None,state:JudgmentState::Abstain,score:ScoreEvidence::RawTokenLogProb{value:-0.5,token:"ordinary nonsecret token".into(),token_position:0},latency_ms:0,qualification_digest:None,reason:None};
        tx.execute("INSERT INTO judgments VALUES(?1,?2,?3,'abstain',?4,?5,?4,?6,0)",params![judgment.decision_id.to_string(),project.to_string(),run.to_string(),identity.as_str(),judgment.model_identity,serde_json::to_string(&judgment)?])?;
        let config=ConnectionConfig{id:"bearer connection-identity".into(),protocol:ProviderProtocol::OpenaiCompletions,endpoint:"http://127.0.0.1:1".into(),models:vec![],auth_ref:None,allowed_projects:vec![project]};
        tx.execute("INSERT INTO provider_references VALUES(?1,?2)",params![config.id,serde_json::to_string(&config)?])?;
        Ok(())
    }).await.unwrap();
    let backup=f.temp.path().join("backup");assert!(f.backup.create(&backup).await.unwrap().objects.is_empty());
    let conn=rusqlite::Connection::open(backup.join("core.sqlite3")).unwrap();
    let text:String=conn.query_row("SELECT body FROM judgments",[],|r|r.get(0)).unwrap();
    let judgment:JudgmentRecord=serde_json::from_str(&text).unwrap();
    assert_eq!(judgment.score,ScoreEvidence::RawTokenLogProb{value:-0.5,token:"ordinary nonsecret token".into(),token_position:0});
    assert_eq!(judgment.model_identity,"bearer model-identity");assert_eq!(judgment.prompt_digest,identity_copy);
    let config:String=conn.query_row("SELECT config FROM provider_references",[],|r|r.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<ConnectionConfig>(&config).unwrap().id,"bearer connection-identity");
    drop(conn);
    let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    let conn=rusqlite::Connection::open(&target.database).unwrap();
    let text:String=conn.query_row("SELECT body FROM judgments",[],|r|r.get(0)).unwrap();
    let restored:JudgmentRecord=serde_json::from_str(&text).unwrap();
    assert_eq!(restored,judgment,"restore's second sanitation preserves genuine score and typed identities");
    let config:String=conn.query_row("SELECT config FROM provider_references",[],|r|r.get(0)).unwrap();
    let config:ConnectionConfig=serde_json::from_str(&config).unwrap();
    assert_eq!(config.id,"bearer connection-identity");
    assert!(config.allowed_projects.is_empty(),"restore removes enrollment, not identity");
    assert!(config.auth_ref.is_none());
    assert_eq!(RetentionService::new(f.store.clone(),f.temp.path().join("objects")).unwrap().sweep().await.unwrap().removed_objects,0);
}

#[tokio::test]
async fn restored_workspace_checks_and_legacy_checkpoint_assertions_are_not_live_authority() {
    use ironbark_core::workspaces::{WorkspaceService,CheckKind};
    let f=Fixture::new().await;
    let target_file=f.temp.path().join("document-target.txt");fs::write(&target_file,b"owner scoped document").unwrap();
    ironbark_core::automation::AutomationBroker::new(f.store.clone()).observe(ironbark_core::automation::ApprovedTarget{scope_id:"restore-document-scope".into(),role:ironbark_core::automation::ScopeRole::OwnerOperator,kind:ironbark_core::automation::TargetKind::DocumentFile,target_id:target_file.to_string_lossy().into_owned(),expires_at:i64::MAX}).await.unwrap();
    let run=ironbark_core::scheduler::SchedulerService::new(f.store.clone(),f.project).unwrap().create_run("legacy assertion, not a native proof").await.unwrap().run_id;
    let identity=Digest::parse(&"a".repeat(64)).unwrap();
    let check=CheckReceipt{name:"legacy assertion".into(),passed:true,evidence_digest:Some(identity.clone()),details:"historical receipt, not re-qualified".into()};
    WorkspaceService::new(f.store.clone()).record_check(run,CheckKind::Verification,check.clone()).await.unwrap();
    let artifact=ironbark_core::scheduler::CheckpointStore::new(f.store.clone(),f.project,f.objects.clone())
        .capture(run,CheckpointPhase::PostVerification,ResultManifest{run_id:run,native_session:None,artifacts:vec![],checks:vec![check],acceptance_satisfied:true,unresolved:vec![]}).await.unwrap();
    let mut original=Vec::new();f.objects.get(artifact).await.unwrap().read_to_end(&mut original).unwrap();
    let original_digest=f.store.with_connection(move |conn|Ok(conn.query_row("SELECT digest FROM artifacts WHERE id=?1",[artifact.to_string()],|r|r.get::<_,String>(0))?)).await.unwrap();
    let backup=f.temp.path().join("backup");f.backup.create(&backup).await.unwrap();
    let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    assert_eq!(fs::read(target.data_dir.join("objects").join(original_digest)).unwrap(),original);
    let restored=Store::open(&target.database).await.unwrap();
    let checkpoint=ironbark_core::scheduler::CheckpointStore::new(restored.clone(),f.project,ObjectStore::new(restored.clone(),f.project,target.data_dir.join("objects"),ObjectLimits::default()).unwrap()).latest(run).await.unwrap().unwrap();
    let manifest:ResultManifest=serde_json::from_value(checkpoint.manifest).unwrap();
    assert!(!manifest.acceptance_satisfied);assert!(!manifest.checks[0].passed);
    assert_eq!(manifest.checks[0].evidence_digest,Some(identity));
    let body=restored.with_connection(|conn|Ok(conn.query_row("SELECT body FROM workspace_checks",[],|r|r.get::<_,String>(0))?)).await.unwrap();
    let check:CheckReceipt=serde_json::from_str(&body).unwrap();assert!(!check.passed);assert_eq!(check.details,"historical receipt, not re-qualified");
    let scope=restored.with_connection(|conn|Ok(conn.query_row("SELECT json_extract(payload,'$.state') FROM events WHERE json_extract(payload,'$.entity_id')='automation-scope/restore-document-scope' ORDER BY seq DESC LIMIT 1",[],|r|r.get::<_,String>(0))?)).await.unwrap();
    assert_eq!(serde_json::from_str::<ironbark_core::automation::AutomationScopeRecord>(&scope).unwrap().phase,ironbark_core::automation::ScopePhase::Released);
    assert_eq!(fs::read(target_file).unwrap(),b"owner scoped document");
}

#[tokio::test]
async fn tampered_unknown_schema_backup_never_installs_a_namespace() {
    use sha2::{Digest as _,Sha256};
    let f=Fixture::new().await;
    f.objects.put(Cursor::new(b"preserved backup object".to_vec()),100).await.unwrap();
    let backup=f.temp.path().join("backup");let mut manifest=f.backup.create(&backup).await.unwrap();
    let database=backup.join("core.sqlite3");
    let conn=rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch("CREATE TABLE future_object_ownership(pointer TEXT)").unwrap();drop(conn);
    let bytes=fs::read(&database).unwrap();
    manifest.database_digest=Digest::parse(&format!("{:x}",Sha256::digest(&bytes))).unwrap();
    manifest.database_bytes=bytes.len() as u64;
    fs::write(backup.join("manifest.json"),serde_json::to_vec(&manifest).unwrap()).unwrap();
    let target=f.target();
    assert!(BackupService::restore(&backup,&target).await.is_err());
    assert!(!target.data_dir.exists());assert!(!target.state_dir.exists());assert!(!target.runtime_dir.exists());
    assert_eq!(fs::read(backup.join("objects").join(manifest.objects[0].digest.as_str())).unwrap(),b"preserved backup object");
}

// A secret in a human heading is an exact citation key, not disposable prose.
// Refuse export rather than leak it or rewrite only one side of the reference.
#[tokio::test]
async fn credential_bearing_citation_locator_refuses_export_without_changing_originals() {
    use ironbark_core::store::schema::StorageSchema;
    for boundary in ["column_only", "embedded_only"] {
        let f = Fixture::new().await;
        commit_policy(&f).await;
        let source = f.temp.path().join("heading.md");
        let bytes = b"# password=A0_LOCATOR_CANARY\nordinary searchable body\n";
        fs::write(&source, bytes).unwrap();
        let service = KnowledgeService::new(f.store.clone(), f.temp.path().join("objects"));
        let revision = service.ingest(f.project, SourceInput::File { path: source.to_string_lossy().into_owned() }, DisclosurePolicy::ExplicitPublic).await.unwrap();
        let hit = service.retrieve(f.project, "searchable", 10).await.unwrap().into_iter()
            .find(|hit| hit.citation.locator.section.as_deref() == Some("password=A0_LOCATOR_CANARY"))
            .expect("actual Markdown heading is the stored locator");
        let citation = hit.citation.clone();
        assert!(matches!(service.open_citation_in(f.project, &citation).await.unwrap(), CitationView::Content { text } if text.contains("ordinary searchable body")));
        if boundary == "embedded_only" {
            let run = ironbark_core::scheduler::SchedulerService::new(f.store.clone(), f.project).unwrap()
                .create_run("preserve historical citation identity").await.unwrap().run_id;
            let project = f.project;
            f.store.with_transaction(move |tx| {
                let conversation = ConversationId::new(); let branch = BranchId::new(); let message = MessageId::new();
                tx.execute("INSERT INTO conversations(id,project_id,created_at) VALUES(?1,?2,0)", params![conversation.to_string(), project.to_string()])?;
                tx.execute("INSERT INTO branches(id,conversation_id) VALUES(?1,?2)", params![branch.to_string(), conversation.to_string()])?;
                tx.execute("INSERT INTO messages(id,branch_id,role,content,created_at) VALUES(?1,?2,'user',?3,0)", params![message.to_string(), branch.to_string(), serde_json::to_string(&vec![ContentPart::Text { text: "retain citation".into() }])?])?;
                let mut snapshot = stored_snapshot(project);
                let locator = serde_json::to_string(&hit.citation.locator)?;
                snapshot.context.excerpts.push(hit);
                tx.execute("INSERT INTO conversation_turns(run_id,conversation_id,branch_id,request_id,request_digest,user_message_id,snapshot) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![run.to_string(), conversation.to_string(), branch.to_string(), uuid::Uuid::new_v4().to_string(), snapshot.context.digest.as_str(), message.to_string(), serde_json::to_string(&snapshot)?])?;
                // A historical snapshot remains valid when its indexed chunk is
                // gone. Retain source revision/CAS; only the embedded locator
                // can now cause SensitiveBackupIdentity.
                assert_eq!(tx.execute("DELETE FROM chunks WHERE locator=?1", [locator])?, 1);
                Ok(())
            }).await.unwrap();
        }
        let before = service.open_citation_in(f.project, &citation).await.unwrap();
        let original = f.store.with_connection(move |conn| {
            StorageSchema::inspect(conn)?.roots(conn)?;
            let unsafe_columns:i64=conn.query_row("SELECT count(*) FROM chunks WHERE locator LIKE '%A0_LOCATOR_CANARY%'", [], |r|r.get(0))?;
            let embedded:i64=conn.query_row("SELECT count(*) FROM conversation_turns", [], |r|r.get(0))?;
            assert_eq!((unsafe_columns,embedded), if boundary=="column_only" {(1,0)} else {(0,1)});
            let mut rows=Vec::new();
            for query in ["SELECT locator || content FROM chunks ORDER BY id", "SELECT snapshot FROM conversation_turns ORDER BY run_id", "SELECT id || source_id || digest FROM source_revisions ORDER BY id"] {
                let mut stmt=conn.prepare(query)?;
                rows.extend(stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?);
            }
            Ok(rows)
        }).await.unwrap();
        let destination = f.temp.path().join("backup");
        assert_eq!(f.backup.create(&destination).await.unwrap_err().code, "SensitiveBackupIdentity", "{boundary}");
        assert!(!destination.exists(), "a refused export installs no recovery namespace");
        let unchanged = f.store.with_connection(|conn| {
            let mut rows=Vec::new();
            for query in ["SELECT locator || content FROM chunks ORDER BY id", "SELECT snapshot FROM conversation_turns ORDER BY run_id", "SELECT id || source_id || digest FROM source_revisions ORDER BY id"] {
                let mut stmt=conn.prepare(query)?;
                rows.extend(stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?);
            }
            Ok(rows)
        }).await.unwrap();
        assert_eq!(unchanged, original, "{boundary}: original SQLite rows are unchanged");
        assert_eq!(service.open_citation_in(f.project, &citation).await.unwrap(), before);
        assert_eq!(fs::read(source).unwrap(), bytes);
        assert_eq!(fs::read(f.temp.path().join("objects").join(revision.digest.as_str())).unwrap(), bytes);
    }
}

// Historical rows bypass append_tx/redacted_json deliberately: otherwise the
// live persistence redactor could hide a broken export-context transition.
#[tokio::test]
async fn persisted_action_and_event_value_boundaries_remove_credentials_on_export_and_restore() {
    use ironbark_core::store::schema::StorageSchema;
    let f=Fixture::new().await;
    let run=ironbark_core::scheduler::SchedulerService::new(f.store.clone(),f.project).unwrap()
        .create_run("historical arbitrary payloads").await.unwrap().run_id;
    let project=f.project;
    let labels=["ARGS","SCOPE","START_ARGS","START_SCOPE","APPROVAL_ARGS","APPROVAL_SCOPE","CALL","CONTENT","SDK","INPUT_SCHEMA","RESULT","DETAILS"];
    let payload=|label:&str| serde_json::json!({
        "id":format!("Bearer A0_{label}_ID_CANARY"),
        "state":format!("password=A0_{label}_STATE_CANARY"),
        "path":format!("Bearer A0_{label}_PATH_CANARY"),
        "nested":[[{
            "native_id":format!("Bearer A0_{label}_NATIVE_CANARY"),
            "input_digest":format!("password=A0_{label}_DIGEST_CANARY"),
            "type":"raw_token_log_prob", "value":-0.5,
            "token":format!("A0_{label}_TOKEN_CANARY"), "token_position":0
        }]]
    });
    let action=|args:&str,scope:&str| ActionSpec {
        id:ActionId::new(),run_id:run,project_id:project,kind:"desktop".into(),
        canonical_args:serde_json::json!({"target_id":"historical-target","observation_digest":"a".repeat(64),"operation":"inspect","parameters":payload(args)}),
        scope:payload(scope),destination:None,policy_version:1,
        expected_state_digest:Digest::parse(&"a".repeat(64)).unwrap(),
    };
    let stored_action=action("ARGS","SCOPE");
    let action_id=stored_action.id;
    let message=MessageId::new();
    let provider=ProviderMessage {
        role:ProviderRole::Assistant,content:vec![ContentPart::Text{text:"password=A0_PROVIDER_TEXT_CANARY".into()}],
        tool_call_id:None,tool_calls:None,sdk_content:Some(payload("SDK")),sdk_route:None,tool_result_error:None,
    };
    let tool=ToolDescriptor{name:"historical_tool".into(),description:"ordinary schema".into(),input_schema:payload("INPUT_SCHEMA")};
    let events=vec![
        CoreEvent::ToolStarted{action:action("START_ARGS","START_SCOPE")},
        CoreEvent::ApprovalNeeded{action:action("APPROVAL_ARGS","APPROVAL_SCOPE")},
        CoreEvent::ProviderToolCall{message_id:message,call:ProviderToolCall{id:"call-1".into(),name:"historical_tool".into(),arguments:payload("CALL")}},
        CoreEvent::ProviderMessage{message_id:message,content:payload("CONTENT"),route:RoutePin{connection_id:"local".into(),model_id:"fixture".into(),harness:None}},
        CoreEvent::ProviderMessage{message_id:message,content:serde_json::json!({"messages":[provider],"tools":[tool]}),route:RoutePin{connection_id:"local".into(),model_id:"fixture".into(),harness:None}},
        CoreEvent::ToolFinished{action_id,result:payload("RESULT")},
        CoreEvent::Error{code:"historical_error".into(),message:"ordinary diagnostic".into(),details:payload("DETAILS")},
        CoreEvent::StateChanged{entity_id:"historical-observation".into(),state:"password=A0_EVENT_STATE_CANARY".into()},
    ];
    let original_action=serde_json::to_string(&stored_action).unwrap();
    let original_events:Vec<String>=events.iter().map(|event|serde_json::to_string(event).unwrap()).collect();
    let action_copy=original_action.clone();let event_copies=original_events.clone();
    f.store.with_transaction(move |tx| {
        tx.execute("INSERT INTO actions(id,run_id,project_id,digest,policy_version,state,body) VALUES(?1,?2,?3,?4,1,'approved',?5)",
            params![action_id.to_string(),run.to_string(),project.to_string(),"a".repeat(64),action_copy])?;
        for text in event_copies {
            let _:CoreEvent=serde_json::from_str(&text)?;
            tx.execute("INSERT INTO events(protocol,project_id,run_id,timestamp,payload) VALUES(1,?1,?2,1,?3)",params![project.to_string(),run.to_string(),text])?;
        }
        StorageSchema::inspect(tx)?.roots(tx)?;
        Ok(())
    }).await.unwrap();
    let backup=f.temp.path().join("backup");f.backup.create(&backup).await.unwrap();
    let target=f.target();BackupService::restore(&backup,&target).await.unwrap();
    for path in [backup.join("core.sqlite3"),target.database] {
        let bytes=fs::read(&path).unwrap();
        let conn=rusqlite::Connection::open(path).unwrap();
        let body:String=conn.query_row("SELECT body FROM actions WHERE id=?1",[action_id.to_string()],|r|r.get(0)).unwrap();
        let action:ActionSpec=serde_json::from_str(&body).unwrap();
        assert_eq!(action.id,action_id);assert_eq!(action.run_id,run);
        assert_eq!(action.canonical_args["parameters"]["id"],"Bearer [REDACTED]");
        assert_eq!(action.scope["nested"][0][0]["token"],"[REDACTED]");
        let mut stmt=conn.prepare("SELECT payload FROM events WHERE timestamp=1 ORDER BY seq").unwrap();
        let exported=stmt.query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap();
        assert_eq!(exported.len(),8,"historical events are sanitized, not discarded");
        for event in exported { let _:CoreEvent=serde_json::from_str(&event).unwrap(); }
        for label in labels {
            for field in ["ID","STATE","PATH","NATIVE","DIGEST","TOKEN"] {
                let canary=format!("A0_{label}_{field}_CANARY");
                assert!(original_action.contains(&canary)||original_events.iter().any(|event|event.contains(&canary)),"fixture must contain {canary} before export");
                assert!(!bytes.windows(canary.len()).any(|window|window==canary.as_bytes()),"physical SQLite leaked {canary}");
            }
        }
        for canary in ["A0_PROVIDER_TEXT_CANARY","A0_EVENT_STATE_CANARY"] {
            assert!(original_events.iter().any(|event|event.contains(canary)));
            assert!(!bytes.windows(canary.len()).any(|window|window==canary.as_bytes()),"physical SQLite leaked {canary}");
        }
    }
    let unchanged=f.store.with_connection(move |conn| {
        let body:String=conn.query_row("SELECT body FROM actions WHERE id=?1",[action_id.to_string()],|r|r.get(0))?;
        let mut stmt=conn.prepare("SELECT payload FROM events WHERE timestamp=1 ORDER BY seq")?;
        let events=stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        Ok((body,events))
    }).await.unwrap();
    assert_eq!(unchanged,(original_action,original_events),"only the backup projection is sanitized");
}

#[tokio::test]
async fn arbitrary_reply_data_cannot_claim_identity_or_raw_score_sanitizer_exemptions() {
    let f = Fixture::new().await;
    let payload = serde_json::json!({
        "type": "raw_token_log_prob", "token": "A0_TOKEN_CANARY",
        "id": "Bearer A0_ID_CANARY", "state": "password=A0_STATE_CANARY",
        "nested": { "native_id": "Bearer A0_NATIVE_CANARY", "input_digest": "password=A0_DIGEST_CANARY" }
    });
    let replies = [CoreReply::Ok { value: CommandResult::RunInspect { value: RecoveryReport {
        // The legacy Ok envelope allowed arbitrary JSON; the typed envelope
        // bounds it, so the canary payload rides the only bounded-JSON leaf
        // field (observed_workspace) while still exercising the redactor.
        run_id: RunId::new(), run_state: RunState::Failed, recorded_pid: None, recorded_start_time: None,
        checkpoint: None, observed_workspace: payload.clone(), pending_mutation: false,
        uncertain_actions: vec![], recommended: vec![],
    } } },
        CoreReply::Error { code: "diagnostic".into(), message: "historical failure".into(), details: payload }];
    for reply in replies {
        let request = uuid::Uuid::new_v4();
        f.store.accept_request(request, &CoreCommand::Status {}).await.unwrap();
        f.store.with_transaction(move |tx| {
            tx.execute("UPDATE requests SET state='completed',reply=?1,completed_at=1 WHERE id=?2",
                params![serde_json::to_string(&reply)?, request.to_string()])?;
            Ok(())
        }).await.unwrap();
    }
    let original: Vec<String> = f.store.with_connection(|conn| {
        let mut statement = conn.prepare("SELECT reply FROM requests WHERE reply IS NOT NULL ORDER BY id")?;
        let replies = statement.query_map([], |row| row.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(replies)
    }).await.unwrap();
    let backup = f.temp.path().join("backup");
    f.backup.create(&backup).await.unwrap();
    let target = f.target();
    BackupService::restore(&backup, &target).await.unwrap();
    for path in [backup.join("core.sqlite3"), target.database] {
        let bytes = fs::read(path).unwrap();
        for canary in ["A0_TOKEN_CANARY", "A0_ID_CANARY", "A0_STATE_CANARY", "A0_NATIVE_CANARY", "A0_DIGEST_CANARY"] {
            assert!(!bytes.windows(canary.len()).any(|window| window == canary.as_bytes()), "arbitrary data leaked {canary}");
        }
    }
    let unchanged: Vec<String> = f.store.with_connection(|conn| {
        let mut statement = conn.prepare("SELECT reply FROM requests WHERE reply IS NOT NULL ORDER BY id")?;
        let replies = statement.query_map([], |row| row.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(replies)
    }).await.unwrap();
    assert_eq!(unchanged, original, "export sanitizes only the snapshot");
}

#[tokio::test]
async fn unknown_nested_work_forms_refuse_before_object_or_staging_unlink() {
    use std::os::unix::fs::PermissionsExt;
    for mutation in ["task_object", "missing_version", "graph_input", "graph_scope"] {
        let f = Fixture::new().await;
        let run = ironbark_core::scheduler::service::SchedulerService::new(f.store.clone(), f.project).unwrap()
            .create_run("real canonical parent work").await.unwrap();
        let artifact = f.objects.put(Cursor::new(b"unknown nested ownership must remain".to_vec()), 100).await.unwrap();
        let node = run.node_id; let run_id = run.run_id;
        let digest = f.store.with_transaction(move |tx| {
            let digest: String = tx.query_row("SELECT digest FROM artifacts WHERE id=?1", [artifact.to_string()], |row| row.get(0))?;
            tx.execute("DELETE FROM artifacts WHERE id=?1", [artifact.to_string()])?;
            let future = serde_json::json!({ "kind": "future_object_reference", "object_digest": digest });
            let (table, id, body) = match mutation {
                "task_object" => ("runs", run_id.to_string(), serde_json::json!({ "task": future, "version": 1 })),
                "missing_version" => ("runs", run_id.to_string(), serde_json::json!({ "task": "known outer form without its required version" })),
                "graph_input" => ("nodes", node.to_string(), serde_json::json!({
                    "version": 1, "position": 0, "role_id": "worker", "dependencies": [], "allowed_routes": [],
                    "inputs": [future], "acceptance": [], "workspace_mode": "read_only"
                })),
                _ => ("runs", run_id.to_string(), serde_json::json!({
                    "version": 1, "node_id": node.to_string(), "role_id": "worker", "workspace_mode": "read_only",
                    "allowed_routes": [], "route_cost_microusd": null,
                    "scope": { "tools": [future], "read_roots": [], "write_roots": [], "network_destinations": [] }
                })),
            };
            tx.execute(&format!("UPDATE {table} SET body=?1 WHERE id=?2"), params![serde_json::to_string(&body)?, id])?;
            Ok(digest)
        }).await.unwrap();
        let staging = f.temp.path().join("objects/.staging/unfinished");
        fs::write(&staging, b"staged evidence must remain").unwrap();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o600)).unwrap();
        let error = RetentionService::new(f.store.clone(), f.temp.path().join("objects")).unwrap().sweep().await.unwrap_err();
        assert_eq!(error.code, "UnknownBackupSchema", "{mutation}: unknown reference shape cannot authorize collection");
        assert_eq!(fs::read(f.temp.path().join("objects").join(&digest)).unwrap(), b"unknown nested ownership must remain");
        assert_eq!(fs::read(&staging).unwrap(), b"staged evidence must remain");
        let backup = f.temp.path().join("backup");
        assert_eq!(f.backup.create(&backup).await.unwrap_err().code, "UnknownBackupSchema", "{mutation}");
        assert!(!backup.exists());
        assert_eq!(fs::read(f.temp.path().join("objects").join(&digest)).unwrap(), b"unknown nested ownership must remain");
        assert_eq!(fs::read(staging).unwrap(), b"staged evidence must remain");
    }
}

// A closed decoder must accept bodies from the real producers, including a
// graph revision other than one and runtime metadata on every kind of run.
#[tokio::test]
async fn genuine_task_graph_turn_and_runtime_checkpoint_producers_remain_exportable() {
    use ironbark_core::{
        conversations::ConversationService,
        policy::{TaskAuthority, TaskScope},
        scheduler::{CheckpointStore, GraphValidator, RunControl, Scheduler, SchedulerService},
        store::schema::{ReferenceReason, StorageSchema},
    };

    let f = Fixture::new().await;
    commit_policy(&f).await;
    let policy = f.store.with_connection(|conn| {
        let body: String = conn.query_row("SELECT body FROM policies", [], |row| row.get(0))?;
        Ok(serde_json::from_str::<ProjectPolicy>(&body)?)
    }).await.unwrap();
    let parent = SchedulerService::new(f.store.clone(), f.project).unwrap()
        .create_run("literal task with ordinary inputs").await.unwrap();
    let graph = JobGraph {
        id: JobId::new(), project_id: f.project, version: 7,
        nodes: vec![JobNode {
            id: NodeId::new(), role_id: "researcher".into(), dependencies: vec![], route_pin: None,
            allowed_routes: vec!["local".into()], inputs: vec!["ordinary input".into()],
            acceptance: vec!["ordinary acceptance".into()], workspace_mode: WorkspaceMode::ReadOnly,
        }],
    };
    let validated = GraphValidator::builtin().validate(&graph, &policy).unwrap();
    let authority = TaskAuthority {
        run_id: parent.run_id, project_id: f.project, owner_approval_id: GrantId::new(),
        scope: TaskScope {
            tools: policy.allowed_tools.clone(), read_roots: policy.read_roots.clone(),
            write_roots: policy.write_roots.clone(), connection_ids: policy.allowed_connection_ids.clone(),
            network_destinations: policy.network_destinations.clone(),
        },
        policy_version: policy.version, expires_at: i64::MAX,
    };
    let job = Scheduler::submit(&f.store, &validated, &authority).await.unwrap();
    let graph_run = f.store.with_connection(move |conn| {
        let id: String = conn.query_row(
            "SELECT r.id FROM runs r JOIN nodes n ON n.id=r.node_id WHERE n.job_id=?1",
            [job.to_string()], |row| row.get(0),
        )?;
        Ok(RunId(uuid::Uuid::parse_str(&id).unwrap()))
    }).await.unwrap();
    let config = ConnectionConfig {
        id: "local".into(), protocol: ProviderProtocol::LlamaCpp, endpoint: "http://127.0.0.1:8091".into(),
        models: vec![ModelDescriptor { id: "fixture".into(), context_limit: 4096, output_limit: 256, capabilities: vec![] }],
        auth_ref: None, allowed_projects: vec![f.project],
    };
    f.store.with_connection(move |conn| {
        conn.execute("INSERT INTO provider_references VALUES('local',?1)", [serde_json::to_string(&config)?])?;
        Ok(())
    }).await.unwrap();
    let conversations = ConversationService::new(f.store.clone());
    let conversation = conversations.create(f.project).await.unwrap();
    let branch = conversations.history(conversation).await.unwrap().branches[0].id;
    let pin = RoutePin { connection_id: "local".into(), model_id: "fixture".into(), harness: None };
    let source=f.temp.path().join("turn-evidence.md");
    let source_bytes=b"# Historical maintenance\ncitationneedle original source evidence\n";
    fs::write(&source,source_bytes).unwrap();
    let knowledge=KnowledgeService::new(f.store.clone(),f.temp.path().join("objects"));
    let revision=knowledge.ingest(f.project,SourceInput::File{path:source.to_string_lossy().into_owned()},DisclosurePolicy::ExplicitPublic).await.unwrap();
    let mut context=knowledge.build_context(f.project,"citationneedle",10).await.unwrap();
    assert_eq!(context.excerpts.len(),1);
    let excerpt=context.excerpts[0].clone();
    assert_eq!(excerpt.citation.revision_digest,revision.digest);
    assert_eq!(excerpt.citation.locator.section.as_deref(),Some("Historical maintenance"));
    let citation_view=knowledge.open_citation_in(f.project,&excerpt.citation).await.unwrap();
    assert_eq!(citation_view,CitationView::Content{text:excerpt.text.clone()});
    let context_artifact=f.objects.put(Cursor::new(b"context artifact evidence".to_vec()),100).await.unwrap();
    let message_artifact=f.objects.put(Cursor::new(b"message artifact evidence".to_vec()),100).await.unwrap();
    context.artifacts.push(context_artifact);
    let turn = conversations.queue_turn(
        conversation, branch, uuid::Uuid::new_v4(),
        vec![ContentPart::Text { text: "ordinary prompt".into() },ContentPart::Artifact{artifact_id:message_artifact}], pin.clone(), Some(context.clone()),
    ).await.unwrap();
    let turn_run=turn.run_id;
    let snapshot=f.store.with_connection(move |conn| {
        let (text,claimed):(String,i64)=conn.query_row("SELECT snapshot,dispatch_claimed FROM conversation_turns WHERE run_id=?1",[turn_run.to_string()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        assert_eq!(claimed,0,"restore must close a previously dispatchable turn");
        Ok(serde_json::from_str::<ironbark_core::conversations::TurnSnapshot>(&text)?)
    }).await.unwrap();
    assert_eq!(snapshot.context,context);
    assert_eq!(snapshot.artifact_receipts.len(),2);
    for id in [context_artifact,message_artifact] {
        let receipt=snapshot.artifact_receipts.iter().find(|receipt|receipt.artifact_id==id).unwrap();
        assert!(receipt.origin_run_id.is_none());
    }
    let artifact = f.objects.put(Cursor::new(b"runtime-owned artifact".to_vec()), 100).await.unwrap();
    let digest = f.store.with_connection(move |conn| {
        Ok(conn.query_row("SELECT digest FROM artifacts WHERE id=?1", [artifact.to_string()], |row| row.get::<_, String>(0))?)
    }).await.unwrap();
    let control = RunControl::new(f.store.clone(), f.project, f.temp.path().join("objects")).unwrap();
    let checkpoints = CheckpointStore::new(f.store.clone(), f.project, f.objects.clone());
    let mut historical=Vec::new();
    for run in [parent.run_id, graph_run, turn.run_id] {
        let session = SessionRef {
            harness: HarnessKind::Omp, native_id: format!("fixture-{run}"), native_version: "fixture".into(),
            project_id: f.project, cwd: f.temp.path().to_string_lossy().into_owned(), resolved_route: pin.clone(),
        };
        control.start_observed(run, Some(session.clone()), vec![artifact], Some(42), Some(123)).await.unwrap();
        let link=checkpoints.capture(run, CheckpointPhase::Planned, ResultManifest {
            run_id: run, native_session: Some(session), artifacts: vec![artifact], checks: vec![],
            acceptance_satisfied: false, unresolved: vec!["verification has not run".into()],
        }).await.unwrap();
        let (link_digest,id,state_digest,body)=f.store.with_connection(move |conn| {
            let digest:String=conn.query_row("SELECT digest FROM artifacts WHERE id=?1",[link.to_string()],|r|r.get(0))?;
            let (id,state,body):(String,String,String)=conn.query_row("SELECT id,state_digest,body FROM checkpoints WHERE run_id=?1",[run.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            Ok((digest,id,state,body))
        }).await.unwrap();
        let bytes=fs::read(f.temp.path().join("objects").join(&link_digest)).unwrap();
        historical.push((run,link_digest,bytes,id,state_digest,serde_json::from_str::<serde_json::Value>(&body).unwrap()));
    }
    // Artifact rows are retained for typed-ID resolution. Reason/digest
    // multiplicities, not incidental artifacts.run_id ownership, discriminate
    // work traversal from the table's independent Artifact roots.
    let roots=f.store.with_connection(|conn|StorageSchema::inspect(conn)?.roots(conn)).await.unwrap();
    let context_digest=snapshot.artifact_receipts.iter().find(|r|r.artifact_id==context_artifact).unwrap().digest.clone();
    let message_digest=snapshot.artifact_receipts.iter().find(|r|r.artifact_id==message_artifact).unwrap().digest.clone();
    for (reason,mut expected) in [
        (ReferenceReason::Citation,vec![revision.digest.as_str().to_owned();5]),
        (ReferenceReason::ContextArtifact,vec![context_digest.as_str().to_owned();5]),
        (ReferenceReason::TurnArtifactReceipt,[vec![context_digest.as_str().to_owned();5],vec![message_digest.as_str().to_owned();5]].concat()),
        (ReferenceReason::MessageArtifact,vec![message_digest.as_str().to_owned()]),
        (ReferenceReason::RunArtifact,vec![digest.clone();6]),
        (ReferenceReason::CheckpointArtifact,vec![digest.clone();3]),
    ] {
        // Turn snapshot: conversation row + jobs/nodes/runs + turn checkpoint.
        // Runtime artifacts: three live runs + three observed checkpoint bodies.
        let mut actual:Vec<String>=roots.iter().filter(|r|r.reason==reason).map(|r|r.digest.as_str().to_owned()).collect();
        actual.sort();expected.sort();assert_eq!(actual,expected,"{reason:?} ownership");
    }
    let mut expected_objects=std::collections::BTreeSet::from([
        revision.digest.as_str().to_owned(),context_digest.as_str().to_owned(),message_digest.as_str().to_owned(),digest.clone(),
    ]);
    for (_,link_digest,_,_,_,_) in &historical {assert!(expected_objects.insert(link_digest.clone()));}
    assert_eq!(expected_objects.len(),7,"source + context/message/runtime artifacts + three distinct checkpoint links");
    let retained=RetentionService::new(f.store.clone(),f.temp.path().join("objects")).unwrap().sweep().await.unwrap();
    assert_eq!(retained.retained_objects,7);assert_eq!(retained.removed_objects,0);
    let backup=f.temp.path().join("backup");
    let manifest=f.backup.create(&backup).await.unwrap();
    assert_eq!(manifest.objects.len(),7);
    assert_eq!(manifest.objects.iter().map(|o|o.digest.as_str().to_owned()).collect::<std::collections::BTreeSet<_>>(),expected_objects);
    let target=f.target();
    let report=BackupService::restore(&backup,&target).await.unwrap();
    assert_eq!(report.blocked_runs,3);assert_eq!(report.restored_objects,7);assert!(report.stopped);
    assert!(!target.state_dir.exists());assert!(!target.runtime_dir.exists());
    assert!(!target.socket.exists());assert!(!target.nonce.exists());assert!(!target.lock.exists());
    for (hash,bytes) in [
        (revision.digest.as_str(),source_bytes.as_slice()),
        (context_digest.as_str(),b"context artifact evidence".as_slice()),
        (message_digest.as_str(),b"message artifact evidence".as_slice()),
        (digest.as_str(),b"runtime-owned artifact".as_slice()),
    ] {assert_eq!(fs::read(target.data_dir.join("objects").join(hash)).unwrap(),bytes);}
    // Inspect the inactive SQLite projection before opening Store (whose
    // startup recovery must not be allowed to mask a missing restore reset).
    let conn=rusqlite::Connection::open(&target.database).unwrap();
    for (run,link_digest,bytes,id,original_state,original) in &historical {
        assert_eq!(fs::read(target.data_dir.join("objects").join(link_digest)).unwrap(),*bytes);
        let (state,body):(String,String)=conn.query_row("SELECT state,body FROM runs WHERE id=?1",[run.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(state,"blocked");
        let body:serde_json::Value=serde_json::from_str(&body).unwrap();
        for field in ["native_session","recorded_pid","recorded_start_time"] {assert!(body.get(field).is_none(),"{field} cannot own restored runtime");}
        assert_eq!(body["artifacts"],serde_json::json!([artifact]));
        let (state,text):(String,String)=conn.query_row("SELECT state_digest,body FROM checkpoints WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        let projected:serde_json::Value=serde_json::from_str(&text).unwrap();
        assert_ne!(&state,original_state,"inactive manifest receives a new state identity");
        assert_eq!(projected["state_digest"],state);
        assert_eq!(projected["observed_workspace"],original["observed_workspace"],"historical runtime body remains evidence");
        assert_eq!(projected["phase"],original["phase"]);assert_eq!(projected["created_at"],original["created_at"]);
        let old:ResultManifest=serde_json::from_value(original["manifest"].clone()).unwrap();
        assert!(old.native_session.is_some());assert!(!old.acceptance_satisfied);assert!(old.checks.is_empty());
        let mut expected=old;expected.native_session=None;
        assert_eq!(serde_json::from_value::<ResultManifest>(projected["manifest"].clone()).unwrap(),expected);
        if *run==turn.run_id {
            assert_eq!(original["observed_workspace"]["run_body"]["snapshot"],serde_json::to_value(&snapshot).unwrap());
        }
    }
    for table in ["jobs","nodes"] {
        let (count,blocked):(i64,i64)=conn.query_row(&format!("SELECT count(*),sum(state='blocked') FROM {table}"),[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((count,blocked),(3,3));
    }
    let (claimed,text):(i64,String)=conn.query_row("SELECT dispatch_claimed,snapshot FROM conversation_turns WHERE run_id=?1",[turn.run_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(claimed,1);
    assert_eq!(serde_json::from_str::<ironbark_core::conversations::TurnSnapshot>(&text).unwrap(),snapshot);
    let status:String=conn.query_row("SELECT status FROM messages WHERE id=?1",[turn.assistant_message_id.to_string()],|r|r.get(0)).unwrap();
    assert_eq!(status,"interrupted");
    let before:(i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM events),(SELECT count(*) FROM messages)",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    drop(conn);
    let restored=Store::open(&target.database).await.unwrap();
    let restored_knowledge=KnowledgeService::new(restored.clone(),target.data_dir.join("objects"));
    assert_eq!(restored_knowledge.open_citation_in(f.project,&excerpt.citation).await.unwrap(),citation_view);
    assert!(ConversationService::new(restored.clone()).claim_dispatch(turn.run_id).await.unwrap().is_none(),"historical turn cannot dispatch");
    let after=restored.with_connection(|conn|Ok(conn.query_row("SELECT (SELECT count(*) FROM events),(SELECT count(*) FROM messages)",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?)).await.unwrap();
    assert_eq!(after,before,"opening restored history/claiming emits no new work");
    assert_eq!(fs::read(source).unwrap(),source_bytes);
}

#[tokio::test]
async fn checkpoint_observed_work_is_closed_before_object_or_staging_unlink() {
    let f = Fixture::new().await;
    let artifact = f.objects.put(Cursor::new(b"checkpoint manifest evidence".to_vec()), 100).await.unwrap();
    let run = f.references(artifact).await;
    let orphan = f.objects.put(Cursor::new(b"checkpoint nested future ownership".to_vec()), 100).await.unwrap();
    let digest = f.store.with_transaction(move |tx| {
        let digest: String = tx.query_row("SELECT digest FROM artifacts WHERE id=?1", [orphan.to_string()], |row| row.get(0))?;
        tx.execute("DELETE FROM artifacts WHERE id=?1", [orphan.to_string()])?;
        let future = serde_json::json!({ "kind": "future_object_reference", "object_digest": digest });
        tx.execute(
            "UPDATE checkpoints SET body=json_set(body,'$.observed_workspace.run_body.task',json(?1)) WHERE run_id=?2",
            params![serde_json::to_string(&future)?, run.to_string()],
        )?;
        Ok(digest)
    }).await.unwrap();
    let staging = f.temp.path().join("objects/.staging/unfinished-checkpoint");
    fs::write(&staging, b"staged checkpoint evidence").unwrap();
    let error = RetentionService::new(f.store.clone(), f.temp.path().join("objects")).unwrap().sweep().await.unwrap_err();
    assert_eq!(error.code, "UnknownBackupSchema");
    assert_eq!(fs::read(f.temp.path().join("objects").join(&digest)).unwrap(), b"checkpoint nested future ownership");
    assert_eq!(fs::read(&staging).unwrap(), b"staged checkpoint evidence");
    let backup = f.temp.path().join("backup");
    assert_eq!(f.backup.create(&backup).await.unwrap_err().code, "UnknownBackupSchema");
    assert!(!backup.exists());
    assert_eq!(fs::read(f.temp.path().join("objects").join(digest)).unwrap(), b"checkpoint nested future ownership");
    assert_eq!(fs::read(staging).unwrap(), b"staged checkpoint evidence");
}

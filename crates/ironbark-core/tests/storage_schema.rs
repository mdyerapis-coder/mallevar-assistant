//! Retention and backup must share a closed, typed storage reference boundary.
use std::{fs, io::Cursor};

use ironbark_core::{
    contracts::{CoreCommand, CoreReply, ProjectId},
    store::{
        backup::BackupService,
        objects::{ObjectLimits, ObjectStore},
        retention::RetentionService,
        Store,
    },
};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    temp: TempDir,
    store: Store,
    objects: ObjectStore,
    backup: BackupService,
}

impl Fixture {
    async fn new(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("core.sqlite3")).await.unwrap();
        let request = Uuid::new_v4();
        let command = CoreCommand::ProjectCreate {
            name: name.to_owned(),
            root: temp.path().to_str().unwrap().to_owned(),
        };
        store.accept_request(request, &command).await.unwrap();
        assert!(matches!(store.execute_accepted(request, &command).await.unwrap(), CoreReply::Ok { .. }));
        let project = store.with_connection(|conn| {
            let id: String = conn.query_row("SELECT id FROM projects", [], |row| row.get(0))?;
            Ok(ProjectId(Uuid::parse_str(&id).unwrap()))
        }).await.unwrap();
        let root = temp.path().join("objects");
        let objects = ObjectStore::new(store.clone(), project, root.clone(), ObjectLimits::default()).unwrap();
        let backup = BackupService::new(store.clone(), root).unwrap();
        Self { temp, store, objects, backup }
    }
}

#[tokio::test]
async fn literal_reference_shaped_project_text_cannot_invent_a_cas_dependency() {
    let f = Fixture::new(r#"{"artifact_id":"notes-not-an-artifact"}"#).await;
    let artifact = f.objects.put(Cursor::new(b"real evidence".to_vec()), 100).await.unwrap();
    let retained = RetentionService::new(f.store.clone(), f.temp.path().join("objects"))
        .unwrap().sweep().await.unwrap();
    assert_eq!(retained.retained_objects, 1);
    assert_eq!(retained.removed_objects, 0);
    let backup = f.backup.create(&f.temp.path().join("backup")).await.unwrap();
    assert_eq!(backup.objects.len(), 1);
    assert!(f.objects.get(artifact).await.is_ok());
}

#[tokio::test]
async fn unknown_durable_table_blocks_collection_before_any_object_is_unlinked() {
    let f = Fixture::new("future-storage").await;
    let artifact = f.objects.put(Cursor::new(b"future-owned evidence".to_vec()), 100).await.unwrap();
    let digest = f.store.with_transaction(move |tx| {
        let digest: String = tx.query_row("SELECT digest FROM artifacts WHERE id=?1", [artifact.to_string()], |row| row.get(0))?;
        tx.execute_batch("CREATE TABLE future_references (object_pointer TEXT NOT NULL)")?;
        tx.execute("INSERT INTO future_references VALUES(?1)", [&digest])?;
        tx.execute("DELETE FROM artifacts WHERE id=?1", [artifact.to_string()])?;
        Ok(digest)
    }).await.unwrap();
    let result = RetentionService::new(f.store.clone(), f.temp.path().join("objects"))
        .unwrap().sweep().await;
    assert!(result.is_err(), "unknown storage cannot authorize garbage collection");
    assert_eq!(fs::read(f.temp.path().join("objects").join(digest)).unwrap(), b"future-owned evidence");
    assert!(f.backup.create(&f.temp.path().join("backup")).await.is_err());
}

#[tokio::test]
async fn unknown_columns_and_reference_tags_refuse_before_unlink() {
    for mutation in ["column", "reference_tag", "blob", "sqlite_prefix"] {
        let f = Fixture::new("closed-schema").await;
        let artifact = f.objects.put(Cursor::new(b"must remain".to_vec()), 100).await.unwrap();
        let digest = f.store.with_transaction(move |tx| {
            let digest: String = tx.query_row("SELECT digest FROM artifacts WHERE id=?1", [artifact.to_string()], |r| r.get(0))?;
            tx.execute("DELETE FROM artifacts", [])?;
            match mutation {
                "column" => tx.execute_batch("ALTER TABLE projects ADD COLUMN future_pointer TEXT")?,
                "reference_tag" => {
                    let job = ironbark_core::contracts::JobId::new();
                    let node = ironbark_core::contracts::NodeId::new();
                    let run = ironbark_core::contracts::RunId::new();
                    let body = r#"{"task":"canonical parent","version":1}"#;
                    tx.execute("INSERT INTO jobs SELECT ?1,id,?2,'queued' FROM projects", rusqlite::params![job.to_string(),body])?;
                    tx.execute("INSERT INTO nodes VALUES(?1,?2,?3,'queued')", rusqlite::params![node.to_string(),job.to_string(),body])?;
                    tx.execute("INSERT INTO runs VALUES(?1,?2,'queued',?3)", rusqlite::params![run.to_string(),node.to_string(),r#"{"kind":"future_object_reference","object_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#])?;
                }
                "blob" => { tx.execute("UPDATE projects SET name=x'00ff'", [])?; }
                _ => tx.execute_batch("PRAGMA writable_schema=ON; INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql) VALUES('table','sqlite_future','sqlite_future',0,'CREATE TABLE sqlite_future(pointer TEXT)'); PRAGMA writable_schema=OFF;")?,
            }
            Ok(digest)
        }).await.unwrap();
        assert!(RetentionService::new(f.store.clone(), f.temp.path().join("objects")).unwrap().sweep().await.is_err(), "{mutation}");
        assert_eq!(fs::read(f.temp.path().join("objects").join(digest)).unwrap(), b"must remain");
        assert!(f.backup.create(&f.temp.path().join("backup")).await.is_err(), "{mutation}");
    }
}

// Removing a producer field or smuggling a future reference through any typed
// slot must fail even when the surrounding shape still looks familiar.
#[tokio::test]
async fn work_decoders_require_complete_closed_producer_shapes() {
    use ironbark_core::{contracts::*, conversations::TurnSnapshot, store::schema::StorageSchema};
    use serde_json::{json, Value};

    let f = Fixture::new("typed-work-bodies").await;
    let project = f.store.with_connection(|conn| {
        let id: String = conn.query_row("SELECT id FROM projects", [], |row| row.get(0))?;
        Ok(ProjectId(Uuid::parse_str(&id).unwrap()))
    }).await.unwrap();
    let run = ironbark_core::scheduler::SchedulerService::new(f.store.clone(), project).unwrap()
        .create_run("canonical task body").await.unwrap();
    let snapshot = TurnSnapshot {
        route: RoutePin { connection_id: "local".into(), model_id: "fixture".into(), harness: None },
        context: ContextPackage {
            digest: Digest::parse(&"a".repeat(64)).unwrap(), brief: String::new(),
            constraints: vec![], excerpts: vec![], artifacts: vec![],
            unresolved_questions: vec![], originating_sessions: vec![SessionRef {
                harness:HarnessKind::Omp,native_id:"historical-session".into(),native_version:"fixture".into(),
                project_id:project,cwd:"/not-a-native-profile".into(),
                resolved_route:RoutePin{connection_id:"local".into(),model_id:"fixture".into(),harness:None},
            }],
        },
        disclosure_digest: Digest::parse(&"b".repeat(64)).unwrap(),
        policy: ProjectPolicy {
            version: 1, project_id: project, allowed_connection_ids: vec!["local".into()],
            allowed_tools: vec![], read_roots: vec![], write_roots: vec![],
            network_destinations: vec![], local_only: true, max_workers: 1,
            max_requests: 1, max_cost_microusd: None, spend_policy: SpendPolicy::FreeOnly,
            allow_unknown_cost: false,
        },
        artifact_receipts: vec![],
    };
    let task = json!({ "task": "canonical task body", "version": 1 });
    let mut forms = vec![
        ("jobs", task.clone()),
        ("nodes", task.clone()),
        ("runs", task.clone()),
        ("jobs", json!({ "version": 7, "node_count": 1, "order": [run.node_id], "roles": ["researcher"] })),
        ("nodes", json!({
            "version": 1, "position": 0, "role_id": "researcher", "dependencies": [run.node_id],
            "allowed_routes": ["local"], "inputs": ["ordinary input"], "acceptance": ["ordinary check"],
            "workspace_mode": "read_only"
        })),
        ("runs", json!({
            "version": 1, "node_id": run.node_id, "role_id": "researcher", "workspace_mode": "read_only",
            "allowed_routes": ["local"], "route_cost_microusd": null,
            "scope": { "tools": ["file_read"], "read_roots": ["/tmp"], "write_roots": [], "network_destinations": [] }
        })),
        ("runs", json!({
            "kind": "conversation_turn", "conversation_id": ConversationId::new(), "branch_id": BranchId::new(),
            "request_id": Uuid::new_v4(), "snapshot": snapshot, "user_message_id": MessageId::new(),
            "assistant_message_id": MessageId::new()
        })),
    ];
    let turn=forms.last().unwrap().1.clone();
    forms.extend([("jobs",turn.clone()),("nodes",turn)]);
    f.store.with_connection(move |conn| {
        let schema = StorageSchema::inspect(conn)?;
        for (table, body) in forms {
            for table in ["jobs", "nodes", "runs"] {
                conn.execute(&format!("UPDATE {table} SET body=?1"), [serde_json::to_string(&task)?])?;
            }
            let update = format!("UPDATE {table} SET body=?1");
            conn.execute(&update, [serde_json::to_string(&body)?])?;
            assert!(schema.roots(conn)?.is_empty(), "{table}: canonical body must remain valid");

            let future = json!({ "kind": "future_object_reference", "object_digest": "c".repeat(64) });
            let mut mutations: Vec<(String, Value)> = Vec::new();
            for field in body.as_object().unwrap().keys() {
                let mut missing = body.clone();
                missing.as_object_mut().unwrap().remove(field);
                mutations.push((format!("missing {field}"), missing));
                let mut wrong_type = body.clone();
                wrong_type[field] = future.clone();
                mutations.push((format!("object in {field}"), wrong_type));
                if body[field].is_array() {
                    let mut nested = body.clone();
                    nested[field] = json!([future]);
                    mutations.push((format!("object element in {field}"), nested));
                }
            }
            let mut unknown = body.clone();
            unknown["future_reference"] = future.clone();
            mutations.push(("unknown outer field".into(), unknown));
            if table != "jobs" && body.get("version").is_some() || body.get("task").is_some() {
                let mut version = body.clone();
                version["version"] = json!(2);
                mutations.push(("unsupported format version".into(), version));
            }
            if body.get("workspace_mode").is_some() {
                let mut mode = body.clone();
                mode["workspace_mode"] = json!("future_workspace");
                mutations.push(("unknown workspace mode".into(), mode));
            }
            if let Some(scope) = body.get("scope") {
                for field in scope.as_object().unwrap().keys() {
                    let mut missing = body.clone();
                    missing["scope"].as_object_mut().unwrap().remove(field);
                    mutations.push((format!("missing scope.{field}"), missing));
                    let mut nested = body.clone();
                    nested["scope"][field] = json!([future]);
                    mutations.push((format!("object element in scope.{field}"), nested));
                }
                let mut unknown = body.clone();
                unknown["scope"]["future_reference"] = future.clone();
                mutations.push(("unknown scope field".into(), unknown));
                let mut cost = body.clone();
                cost["route_cost_microusd"] = json!(-1);
                mutations.push(("negative route cost".into(), cost));
            }
            if body.get("kind").and_then(Value::as_str)==Some("conversation_turn") {
                // Each enclosing producer must decode the closed nested DTOs,
                // not merely recognize the conversation_turn outer tag.
                for path in [
                    "/snapshot/route/connection_id",
                    "/snapshot/context/digest",
                    "/snapshot/context/brief",
                    "/snapshot/context/constraints",
                    "/snapshot/context/excerpts",
                    "/snapshot/context/artifacts",
                    "/snapshot/context/unresolved_questions",
                    "/snapshot/context/originating_sessions/0/native_id",
                    "/snapshot/context/originating_sessions/0/project_id",
                    "/snapshot/context/originating_sessions/0/resolved_route/model_id",
                    "/snapshot/disclosure_digest",
                    "/snapshot/policy/project_id",
                    "/snapshot/artifact_receipts",
                ] {
                    let mut nested=body.clone();
                    *nested.pointer_mut(path).unwrap()=future.clone();
                    mutations.push((format!("future reference in {path}"),nested));
                }
                for path in [
                    "/snapshot","/snapshot/route","/snapshot/context","/snapshot/policy",
                    "/snapshot/context/originating_sessions/0",
                    "/snapshot/context/originating_sessions/0/resolved_route",
                ] {
                    let mut nested=body.clone();
                    nested.pointer_mut(path).unwrap().as_object_mut().unwrap()
                        .insert("future_reference".into(),future.clone());
                    mutations.push((format!("unknown nested member in {path}"),nested));
                }
                for field in ["harness","native_id","native_version","project_id","cwd","resolved_route"] {
                    let mut nested=body.clone();
                    nested.pointer_mut("/snapshot/context/originating_sessions/0").unwrap()
                        .as_object_mut().unwrap().remove(field);
                    mutations.push((format!("missing originating SessionRef.{field}"),nested));
                }
            }
            for (field, wrong) in [
                ("native_session", future.clone()),
                ("recorded_pid", json!(-1)),
                ("recorded_start_time", json!("future identity")),
                ("artifacts", json!([future])),
            ] {
                let mut runtime = body.clone();
                runtime[field] = wrong;
                mutations.push((format!("invalid runtime {field}"), runtime));
                let mut null = body.clone();
                null[field] = Value::Null;
                mutations.push((format!("null runtime {field}"), null));
            }
            if table != "runs" {
                let mut runtime = body.clone();
                runtime["artifacts"] = json!([]);
                mutations.push(("runtime fields outside runs".into(), runtime));
            }
            for (name, mutation) in mutations {
                conn.execute(&update, [serde_json::to_string(&mutation)?])?;
                assert_eq!(schema.roots(conn).unwrap_err().code, "UnknownBackupSchema", "{table}: {name}");
            }
        }
        Ok(())
    }).await.unwrap();
}

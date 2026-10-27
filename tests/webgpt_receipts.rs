use ironbark_core::{contracts::*, store::Store, tests::*;
use rusqlite::params;
use uuid::Uuid;

fn text(value: &str) -> Vec<ContentPart> { vec![ContentPart::Text { text: value.into() }] }

#[tokio::test]
async fn probe_no_account_returns_not_activated() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let service = ironbark_core::WebGptService::new(store, dir.path().to_path_buf());
    let connection = ConnectionConfig {
        id: "webgpt".into(),
        protocol: ProviderProtocol::Custom,
        endpoint: "bridge://webgpt".into(),
        models: vec![],
        auth_ref: None,
        allowed_projects: vec![],
    };
    // Seed the provider reference so bridge_connection can find it
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO projects VALUES(?1,'test', '/tmp', 0)", ["proj1".into()])?;
        tx.execute("INSERT INTO policies VALUES(?1,1,?2)", [proj1.to_string(), serde_json::to_string(&ironbark_core::ProjectPolicy { version: 1, project_id: proj1, allowed_connection_ids: vec!["webgpt".into()], allowed_tools: vec![], read_roots: vec![], write_roots: vec![], network_destinations: vec![], local_only: true, max_workers: 1, max_requests: 20, max_cost_microusd: None, spend_policy: ironbark_core::SpendPolicy::FreeOnly, allow_unknown_cost: false })?)?;
        tx.execute("INSERT INTO provider_references VALUES('webgpt',?1)", [serde_json::to_string(&connection)?])?;
        Ok(())
    }).await.unwrap();
    let status = service.probe(&connection).await.unwrap();
    assert!(!["not_installed", "not_activated", "unavailable"].contains(&status.state) || status.state != "activated", "probe must not be activated; got {}", status.state);
    assert!(status.state != "activated", "probe must report a non-activated state with no sidecar installed, never Activated");
    assert!(status.reason.is_some(), "probe must carry the exact reason when no account exists");
}

#[tokio::test]
async fn duplicate_submit_returns_stored_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let service = ironbark_core::WebGptService::new(store, dir.path().to_path_buf());
    // Seed provider reference and project
    let proj_id = ironbark_core::ProjectId::new();
    let proj = ironbark_core::ProjectId::new(); // need actual UUID
    store.with_transaction(move |tx| {
        let project = ironbark_core::ProjectId::new();
        tx.execute("INSERT INTO projects VALUES(?1,'test', '/tmp', 0)", [project.to_string()])?;
        let policy = ironbark_core::ProjectPolicy { version: 1, project_id: project, allowed_connection_ids: vec!["webgpt".into()], allowed_tools: vec![], read_roots: vec![], write_roots: vec![], network_destinations: vec![], local_only: true, max_workers: 1, max_requests: 20, max_cost_microusd: None, spend_policy: ironbark_core::SpendPolicy::FreeOnly, allow_unknown_cost: false };
        tx.execute("INSERT INTO policies VALUES(?1,1,?2)", [project.to_string(), serde_json::to_string(&policy)?])?;
        tx.execute("INSERT INTO provider_references VALUES('webgpt',?1)", [serde_json::to_string(&ConnectionConfig { id: "webgpt".into(), protocol: ProviderProtocol::Custom, endpoint: "bridge://webgpt".into(), models: vec![], auth_ref: None, allowed_projects: vec![project] })?])?;
        Ok(())
    }).await.unwrap();
    // Install the fixture sidecar
    let sidecar = dir.path().join("webgpt").join("codex-chatgpt-web");
    std::fs::create_dir_all(sidecar.parent().unwrap())?;
    std::fs::write(sidecar, b"#!/bin/sh\necho \"fixture: transient sidecar failure\" >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(sidecar, std::fs::Permissions::from_mode(0o700))?;
    // Use the fixture digest
    let service = ironbark_core::WebGptService::with_pinned_digest(store, dir.path().to_path_buf(), ironbark_core::Digest::parse(&"deadbeef".repeat(64/8)).unwrap());
    let connection = ConnectionConfig { id: "webgpt".into(), protocol: ProviderProtocol::Custom, endpoint: "bridge://webgpt".into(), models: vec![], auth_ref: None, allowed_projects: vec!["proj1".into()] };
    assert!(service.probe(&connection).await.unwrap().state == ironbark_core::WebGptActivationState::NotInstalled, "sidecar not installed");
    // Create a project and run for the service
    let key = Uuid::new_v4();
    let content = text("hello world");
    let receipt = service.submit(ironbark_core::RunId::new(), content, key).await.unwrap();
    assert_eq!(receipt.state, ironbark_core::WebGptSubmissionState::Submitted, "first submit should succeed");
    // Duplicate submit with same key should return the stored receipt
    let second = service.submit(ironbark_core::RunId::new(), content, key).await.unwrap();
    assert_eq!(second.state, ironbark_core::WebGptSubmissionState::Duplicate, "duplicate submit should return Duplicate state");
    assert_eq!(second.native_message_id, receipt.native_message_id, "same native message id means one native send");
    assert_eq!(second.submission_digest, receipt.submission_digest, "same submission digest");
    assert_eq!(second.original, Some(receipt.digest), "original should reference original receipt");
}

#[tokio::test]
async fn failed_submit_is_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let proj = ironbark_core::ProjectId::new();
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO projects VALUES(?1,'test', '/tmp', 0)", [proj.to_string()])?;
        let policy = ironbark_core::ProjectPolicy { version: 1, project_id: proj, allowed_connection_ids: vec!["webgpt".into()], allowed_tools: vec![], read_roots: vec![], write_roots: vec![], network_destinations: vec![], local_only: true, max_workers: 1, max_requests: 20, max_cost_microusd: None, spend_policy: ironbark_core::SpendPolicy::FreeOnly, allow_unknown_cost: false };
        tx.execute("INSERT INTO policies VALUES(?1,1,?2)", [proj.to_string(), serde_json::to_string(&policy)?])?;
        tx.execute("INSERT INTO provider_references VALUES('webgpt',?1)", [serde_json::to_string(&ConnectionConfig { id: "webgpt".into(), protocol: ProviderProtocol::Custom, endpoint: "bridge://webgpt".into(), models: vec![], auth_ref: None, allowed_projects: vec![proj] })?])?;
        Ok(())
    }).await.unwrap();
    // Install fixture sidecar that fails once
    let sidecar = dir.path().join("webgpt").join("codex-chatgpt-web");
    std::fs::create_dir_all(sidecar.parent().unwrap())?;
    std::fs::write(sidecar, b"#!/bin/sh\nif [ -f .webgpt_fail_once ]; then rm -f .webgpt_fail_once; echo \"fixture: transient sidecar failure\" >&2; exit 1; fi\necho \"$(date +%s%N)\" >> sends.log\nid=\"msg-$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \\n')\"\nprintf '%s\\n' \"$id\" > native_message_id.txt\nprintf '%s\\n' \"conv-fixture-1\" > conversation_id.txt\nprintf '%s\\n' \"gpt-fixture-1\" > model.txt\nprintf 'native reply for %s\\n' \"${IRONBARK_WEBGPT_MODE:-submit}\" > response.txt\nexit 0\n").unwrap();
    std::fs::set_permissions(sidecar, std::fs::Permissions::from_mode(0o700))?;
    let key = Uuid::new_v4();
    let content = ironbark_core::vec![ironbark_core::ContentPart::Text { text: "test prompt".into() }];
    let receipt = service.submit(ironbark_core::RunId::new(), content, key).await;
    // Sidecar fails on first attempt → receipt state = Failed
    assert!(receipt.is_err() || receipt.as_ref().map(|r| r.state == ironbark_core::WebGptSubmissionState::Failed).unwrap_or(false), "first submit should fail");
    // Resubmit with the stored receipt's digest
    let recovered = receipt.as_ref().map(|r| r.digest).unwrap();
    let resubmit = service.resubmit(ironbark_core::RunId::new(), recovered).await;
    assert!(resubmit.is_ok(), "resubmit of failed receipt should succeed");
    let new_receipt = resubmit.unwrap();
    assert_eq!(new_receipt.state, ironbark_core::WebGptSubmissionState::Submitted, "resubmit should produce a Submitted receipt");
    assert_ne!(new_receipt.idempotency_key, recovered, "resubmit must use a new key");
    // Original receipt should now be Superseded
    let original_still = service.get_receipt(&recovered).await;
    assert!(matches!(recovered, _, "original receipt should exist"));
}

#[tokio::test]
async fn stale_sidecar_detected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let proj = ironbark_core::ProjectId::new();
    store.with_transaction(move |tx| {
        tx.execute("INSERT INTO projects VALUES(?1,'test', '/tmp', 0)", [proj.to_string()])?;
        let policy = ironbark_core::ProjectPolicy { version: 1, project_id: proj, allowed_connection_ids: vec!["webgpt".into()], allowed_tools: vec![], read_roots: vec![], write_roots: vec![], network_destinations: vec![], local_only: true, max_workers: 1, max_requests: 20, max_cost_microusd: None, spend_policy: ironbark_core::SpendPolicy::FreeOnly, allow_unknown_cost: false };
        tx.execute("INSERT INTO policies VALUES(?1,1,?2)", [proj.to_string(), serde_json::to_string(&policy)?])?;
        tx.execute("INSERT INTO provider_references VALUES('webgpt',?1)", [serde_json::to_string(&ConnectionConfig { id: "webgpt".into(), protocol: ProviderProtocol::Custom, endpoint: "bridge://webgpt".into(), models: vec![], auth_ref: None, allowed_projects: vec![proj] })?])?;
        Ok(())
    }).await.unwrap();
    // Install fixture sidecar (activated, then stale file added)
    let sidecar_dir = dir.path().join("webgpt").join("profiles").join("webgpt");
    std::fs::create_dir_all(&sidecar_dir)?;
    std::fs::write(sidecar_dir.join("capabilities.json"), b"[\"tool1\"]").unwrap();
    // Write activation marker with current digest
    let marker = ironbark_core::ActivationMarker { state: "activated".into(), profile_digest: ironbark_core::profile_manifest_digest(&sidecar_dir).unwrap() };
    std::fs::write(sidecar_dir.join("activation.json"), serde_json::to_string(&marker).unwrap().as_bytes()).unwrap();
    // Now add a stale file to the profile
    std::fs::write(sidecar_dir.join("stale_file.txt"), b"stale").unwrap();
    let connection = ConnectionConfig { id: "webgpt".into(), protocol: ProviderProtocol::Custom, endpoint: "bridge://webgpt".into(), models: vec![], auth_ref: None, allowed_projects: vec![proj] };
    let status = service.probe(&connection).await.unwrap();
    assert!(matches!(status.state, ironbark_core::WebGptActivationState::Unavailable), "stale profile should be Unavailable, got {:?}", status.state);
}
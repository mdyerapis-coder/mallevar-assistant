//! W1–W3 WebGPT connection invariants, exercised offline against a real `Store`
//! and the real `WebGptService`.
//!
//! Every case here runs with no network, no browser and no provider configured.
//! The only external process is a small local fixture script that stands in for
//! the pinned sidecar at the real process boundary (its digest is pinned via the
//! `with_pinned_digest` test seam the service already exposes); a confirming and
//! a non-confirming variant let both the duplicate and the ambiguous paths run.
//!
//! Covered:
//! - `not_installed_and_not_activated_are_distinct_states`
//! - `activation_records_the_pinned_digest_and_a_drift_is_detected`
//! - `a_duplicate_submission_returns_the_stored_receipt_without_a_second_send`
//! - `an_ambiguous_send_is_reconciled_never_auto_resent`
//! - `the_webgpt_route_never_falls_back_to_another_provider`
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ironbark_core::connections::webgpt::{
    profile_manifest_digest, WebGptActivationState, WebGptService, WebGptSubmissionState,
    WebGptTurnInput, PINNED_SIDECAR_NAME,
};
use ironbark_core::contracts::{
    ConnectionConfig, ContentPart, Digest, JobId, NodeId, ProjectId, ProviderProtocol, RunId,
};
use ironbark_core::store::Store;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

// ── Fixtures ────────────────────────────────────────────────────────────────

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    data_dir: PathBuf,
    store: Store,
    run: RunId,
}

fn bridge_config() -> ConnectionConfig {
    ConnectionConfig {
        id: "webgpt".into(),
        protocol: ProviderProtocol::Custom,
        endpoint: "bridge://webgpt".into(),
        models: vec![],
        auth_ref: None,
        allowed_projects: vec![],
    }
}

fn generic_config() -> ConnectionConfig {
    ConnectionConfig {
        id: "openai".into(),
        protocol: ProviderProtocol::OpenaiCompletions,
        endpoint: "https://api.openai.com/v1".into(),
        models: vec![],
        auth_ref: None,
        allowed_projects: vec![],
    }
}

/// The run body carries a resolved route; the WebGPT failure path must never
/// rewrite it (that is the offline proof that no fallback was selected).
fn route_run_body() -> String {
    serde_json::json!({
        "resolved_route": { "connection_id": "webgpt", "model_id": "chatgpt-web" }
    })
    .to_string()
}

async fn setup() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let store = Store::open(&root.join("core.db")).await.unwrap();
    let project = ProjectId::new();
    let run = RunId::new();
    let job = JobId::new();
    let node = NodeId::new();
    let root_str = root.to_string_lossy().into_owned();
    let policy = serde_json::json!({
        "version": "1",
        "project_id": project,
        "allowed_connection_ids": ["webgpt"],
        "allowed_tools": [],
        "read_roots": [],
        "write_roots": [],
        "network_destinations": [],
        "local_only": true,
        "max_workers": 1,
        "max_requests": 10,
        "max_cost_microusd": null,
        "spend_policy": "free_only",
        "allow_unknown_cost": false,
    })
    .to_string();
    let body = route_run_body();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects(id,name,root,created_at) VALUES(?1,'fixture',?2,1)",
                rusqlite::params![project.to_string(), root_str],
            )?;
            tx.execute(
                "INSERT INTO policies(project_id,version,body) VALUES(?1,1,?2)",
                rusqlite::params![project.to_string(), policy],
            )?;
            tx.execute(
                "INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![job.to_string(), project.to_string()],
            )?;
            tx.execute(
                "INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,'{}','queued')",
                rusqlite::params![node.to_string(), job.to_string()],
            )?;
            tx.execute(
                "INSERT INTO runs(id,node_id,state,body) VALUES(?1,?2,'ready',?3)",
                rusqlite::params![run.to_string(), node.to_string(), body],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        root,
        data_dir,
        store,
        run,
    }
}

async fn seed_connection(store: &Store, id: &str, config: &ConnectionConfig) {
    let json = serde_json::to_string(config).unwrap();
    let id = id.to_string();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO provider_references(id,config) VALUES(?1,?2)",
                rusqlite::params![id, json],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest::parse(&sha256_hex(bytes)).unwrap()
}

/// Write an executable fixture "sidecar" bundle and return its digest so the
/// test can pin the service to exactly these bytes.
fn write_sidecar(data_dir: &Path, body: &str) -> Digest {
    let root = data_dir.join("webgpt");
    fs::create_dir_all(&root).unwrap();
    let path = root.join(PINNED_SIDECAR_NAME);
    fs::write(&path, body).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    digest_of(&fs::read(&path).unwrap())
}

/// A confirming sidecar: appends one line to the invocation counter and writes
/// the native confirmation the service reads back.
fn confirm_sidecar(count_file: &Path) -> String {
    format!(
        "#!/bin/sh\nprintf 's\\n' >> '{count}'\nprintf '%s' 'native-message-1' > \"$IRONBARK_WEBGPT_REQUEST/native_message_id.txt\"\nexit 0\n",
        count = count_file.display()
    )
}

/// A non-confirming sidecar: it counts the invocation but never writes the
/// native confirmation (an ambiguous send).
fn silent_sidecar(count_file: &Path) -> String {
    format!(
        "#!/bin/sh\nprintf 's\\n' >> '{count}'\nexit 0\n",
        count = count_file.display()
    )
}

/// Record a genuine owner activation for `connection_id`: create an isolated
/// profile, bind its live manifest digest into an activation marker, and return
/// that digest. Mirrors what an owner login in the isolated profile records.
fn activate_profile(data_dir: &Path, connection_id: &str) -> Digest {
    let profile = data_dir.join("webgpt").join("profiles").join(connection_id);
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join("cookies.txt"), b"session-cookie-v1").unwrap();
    let digest = profile_manifest_digest(&profile).unwrap();
    fs::write(
        profile.join("activation.json"),
        serde_json::json!({ "state": "activated", "profile_digest": digest.as_str() }).to_string(),
    )
    .unwrap();
    digest
}

fn service(fx: &Fixture, pinned: Digest) -> WebGptService {
    WebGptService::with_pinned_digest(fx.store.clone(), fx.data_dir.clone(), pinned)
}

fn text_turn(text: &str) -> WebGptTurnInput {
    WebGptTurnInput {
        content: vec![ContentPart::Text { text: text.into() }],
        conversation_id: None,
    }
}

fn text_content(text: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text { text: text.into() }]
}

/// The service's canonical content bytes, reproduced here so the test can seed a
/// durable request dir whose content digest matches the live `prepare_content`
/// path exactly.
fn canonical_content(text: &str, image_digests: &[Digest]) -> String {
    serde_json::json!({
        "prompt_text": text,
        "image_digests": image_digests.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
    })
    .to_string()
}

fn send_count(count_file: &Path) -> usize {
    fs::read_to_string(count_file)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

async fn scalar_i64(store: &Store, sql: &str) -> i64 {
    let sql = sql.to_owned();
    store
        .with_connection(move |conn| {
            conn.query_row(&sql, [], |r| r.get::<_, i64>(0)).map_err(Into::into)
        })
        .await
        .unwrap()
}

async fn receipt_count(store: &Store) -> i64 {
    scalar_i64(store, "SELECT COUNT(*) FROM artifacts WHERE path LIKE 'webgpt_submission/%'").await
}

async fn observation_count(store: &Store) -> i64 {
    scalar_i64(store, "SELECT COUNT(*) FROM provider_observations").await
}

async fn provider_ids(store: &Store) -> Vec<String> {
    store
        .with_connection(move |conn| {
            let mut stmt = conn.prepare("SELECT id FROM provider_references ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
        .unwrap()
}

async fn run_body(store: &Store, run: RunId) -> String {
    store
        .with_connection(move |conn| {
            conn.query_row("SELECT body FROM runs WHERE id=?1", [run.to_string()], |r| r.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap()
}

// ── W1: activation states ────────────────────────────────────────────────────

#[tokio::test]
async fn not_installed_and_not_activated_are_distinct_states() {
    let fx = setup().await;
    let cfg = bridge_config();
    seed_connection(&fx.store, "webgpt", &cfg).await;

    // No sidecar at all: the status is exactly `NotInstalled`, with a reason.
    let svc = service(&fx, Digest::parse(&"1".repeat(64)).unwrap());
    let status = svc.probe(&cfg).await.unwrap();
    assert_eq!(status.state, WebGptActivationState::NotInstalled);
    assert!(status.reason.is_some());
    assert_eq!(status.sidecar_version, None);

    // A turn in a non-Activated state is refused, never silently attempted.
    let err = svc.turn(fx.run, text_turn("hello")).await.unwrap_err();
    assert_eq!(err.code, "webgpt_not_activated");
    assert_eq!(receipt_count(&fx.store).await, 0);

    // Install the pinned sidecar (digest matches): still no owner activation.
    let real = write_sidecar(&fx.data_dir, "#!/bin/sh\nexit 0\n");
    let svc = service(&fx, real);
    let status = svc.probe(&cfg).await.unwrap();
    assert_eq!(status.state, WebGptActivationState::NotActivated);
    assert_ne!(status.state, WebGptActivationState::NotInstalled);
    assert!(status.sidecar_version.is_some());

    // Still refused — `NotActivated` is a real, separate state.
    let err = svc.turn(fx.run, text_turn("hello")).await.unwrap_err();
    assert_eq!(err.code, "webgpt_not_activated");
    assert_eq!(receipt_count(&fx.store).await, 0);

    // A non-bridge connection is reported `Unavailable`, never treated as WebGPT.
    let other = generic_config();
    let status = svc.probe(&other).await.unwrap();
    assert_eq!(status.state, WebGptActivationState::Unavailable);
    assert!(status.reason.is_some());
}

#[tokio::test]
async fn activation_records_the_pinned_digest_and_a_drift_is_detected() {
    let fx = setup().await;
    let cfg = bridge_config();
    seed_connection(&fx.store, "webgpt", &cfg).await;
    let real = write_sidecar(&fx.data_dir, "#!/bin/sh\nexit 0\n");

    // A real owner activation binds the live profile digest into the marker.
    let bound = activate_profile(&fx.data_dir, &cfg.id);
    let svc = service(&fx, real);

    let status = svc.probe(&cfg).await.unwrap();
    assert_eq!(status.state, WebGptActivationState::Activated);
    assert_eq!(status.profile_digest.as_ref(), Some(&bound));
    assert_eq!(status.sidecar_version.as_deref(), Some(ironbark_core::connections::webgpt::PINNED_SIDECAR_VERSION));
    assert!(status.reason.is_none());

    // The profile drifts after activation: the marker no longer binds the live
    // digest, so the profile is refused, never silently reused.
    let profile = fx.data_dir.join("webgpt").join("profiles").join(&cfg.id);
    fs::write(profile.join("cookies.txt"), b"rotated-cookie").unwrap();

    let status = svc.probe(&cfg).await.unwrap();
    assert_eq!(status.state, WebGptActivationState::Unavailable);
    assert_ne!(status.state, WebGptActivationState::Activated);
    assert!(status.reason.as_deref().unwrap().contains("profile"));

    // And a turn under the drifted profile fails closed, recording nothing.
    let err = svc.turn(fx.run, text_turn("hello")).await.unwrap_err();
    assert_eq!(err.code, "webgpt_unavailable");
    assert_eq!(receipt_count(&fx.store).await, 0);
}

// ── W3: duplicate-send defense ───────────────────────────────────────────────

#[tokio::test]
async fn a_duplicate_submission_returns_the_stored_receipt_without_a_second_send() {
    let fx = setup().await;
    let cfg = bridge_config();
    seed_connection(&fx.store, "webgpt", &cfg).await;
    let count_file = fx.root.join("send-count");
    let real = write_sidecar(&fx.data_dir, &confirm_sidecar(&count_file));
    activate_profile(&fx.data_dir, &cfg.id);
    let svc = service(&fx, real);

    let key = uuid::Uuid::new_v4();
    let content = text_content("hello");

    // First send: exactly one native send and a durable `submitted` receipt.
    let first = svc.submit(fx.run, content.clone(), key).await.unwrap();
    assert_eq!(first.state, WebGptSubmissionState::Submitted);
    assert_eq!(first.native_message_id.as_deref(), Some("native-message-1"));
    assert_eq!(send_count(&count_file), 1);

    // Same key, different content: a typed conflict, never a silent overwrite.
    let err = svc.submit(fx.run, text_content("something else"), key).await.unwrap_err();
    assert_eq!(err.code, "idempotency_key_conflict");
    assert_eq!(send_count(&count_file), 1);

    // Same key, same content: the stored receipt is returned as `Duplicate`.
    let second = svc.submit(fx.run, content.clone(), key).await.unwrap();
    assert_eq!(second.state, WebGptSubmissionState::Duplicate);
    assert_eq!(second.original.as_ref(), Some(&first.digest));
    assert_eq!(second.native_message_id, first.native_message_id);

    // The recorded send count did not grow: exactly one native send ever happened.
    assert_eq!(send_count(&count_file), 1);

    // Durable rows exist: the original and the duplicate view are both indexed
    // under the idempotency key.
    assert!(receipt_count(&fx.store).await >= 2);
    assert_eq!(
        scalar_i64(
            &fx.store,
            &format!(
                "SELECT COUNT(*) FROM artifacts WHERE path LIKE 'webgpt_submission/{}%'",
                key
            ),
        )
        .await,
        2
    );
}

// ── W3: ambiguous send / recovery ────────────────────────────────────────────

#[tokio::test]
async fn an_ambiguous_send_is_reconciled_never_auto_resent() {
    let fx = setup().await;
    let cfg = bridge_config();
    seed_connection(&fx.store, "webgpt", &cfg).await;
    let count_file = fx.root.join("send-count");
    let real = write_sidecar(&fx.data_dir, &silent_sidecar(&count_file));
    activate_profile(&fx.data_dir, &cfg.id);
    let svc = service(&fx, real);

    // Simulate a crash between the native send and the receipt commit: a durable
    // per-key request dir exists with the content but no native confirmation.
    let key = uuid::Uuid::new_v4();
    let req_dir = fx.data_dir.join("webgpt").join("requests").join("webgpt").join(key.to_string());
    fs::create_dir_all(&req_dir).unwrap();
    fs::write(req_dir.join("content.json"), canonical_content("hello", &[])).unwrap();

    // Recovery resolves the unknown outcome to the explicit `failed` state.
    let receipt = svc.reconcile(fx.run, key).await.unwrap();
    assert_eq!(receipt.state, WebGptSubmissionState::Failed);
    assert!(receipt.native_message_id.is_none());
    assert!(receipt.reason.as_deref().unwrap().contains("unknown"));

    // reconcile never spawns the sidecar: no automatic second prompt was sent.
    assert_eq!(send_count(&count_file), 0);

    // A second reconcile is idempotent and still never sends.
    let again = svc.reconcile(fx.run, key).await.unwrap();
    assert_eq!(again, receipt);
    assert_eq!(send_count(&count_file), 0);

    // The normal submit path refuses to resend an ambiguous key, and still no send.
    let err = svc.submit(fx.run, text_content("hello"), key).await.unwrap_err();
    assert_eq!(err.code, "submission_failed");
    assert_eq!(send_count(&count_file), 0);

    // A resubmit requires the recorded receipt digest: an unknown digest cannot
    // silently trigger a second prompt.
    let unknown = Digest::parse(&"a".repeat(64)).unwrap();
    let err = svc.resubmit(fx.run, unknown).await.unwrap_err();
    assert_eq!(err.code, "receipt_not_found");
    assert_eq!(send_count(&count_file), 0);

    // The recorded digest is bound to its run: a receipt cannot be resubmitted
    // from another run.
    let other_run = RunId::new();
    let err = svc.resubmit(other_run, receipt.digest.clone()).await.unwrap_err();
    assert_eq!(err.code, "receipt_run_mismatch");
    assert_eq!(send_count(&count_file), 0);
}

// ── W3: no provider fallback ─────────────────────────────────────────────────

#[tokio::test]
async fn the_webgpt_route_never_falls_back_to_another_provider() {
    let fx = setup().await;
    let cfg = bridge_config();
    seed_connection(&fx.store, "webgpt", &cfg).await;
    // A plausible fallback provider exists in the store; the failure must not
    // select it.
    let other = generic_config();
    seed_connection(&fx.store, "openai", &other).await;

    let body_before = run_body(&fx.store, fx.run).await;
    let pinned = Digest::parse(&"2".repeat(64)).unwrap();
    let svc = service(&fx, pinned);

    // WebGPT fails closed (no sidecar installed) with a WebGPT-specific error,
    // not a route/provider error.
    let err = svc.turn(fx.run, text_turn("route me")).await.unwrap_err();
    assert_eq!(err.code, "webgpt_not_activated");

    // The resolved route is unchanged: the run body still holds the original
    // route, and nothing was written to switch providers.
    assert_eq!(run_body(&fx.store, fx.run).await, body_before);
    assert_eq!(provider_ids(&fx.store).await, vec!["openai".to_string(), "webgpt".to_string()]);
    assert_eq!(observation_count(&fx.store).await, 0);
    assert_eq!(receipt_count(&fx.store).await, 0);

    // A second attempt is the same refusal against the same unchanged state.
    let err = svc.turn(fx.run, text_turn("route me again")).await.unwrap_err();
    assert_eq!(err.code, "webgpt_not_activated");
    assert_eq!(run_body(&fx.store, fx.run).await, body_before);
    assert_eq!(observation_count(&fx.store).await, 0);
    assert_eq!(receipt_count(&fx.store).await, 0);
}

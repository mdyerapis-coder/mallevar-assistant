//! W1/W2/W3: the WebGPT (ChatGPT-Web) connection.
//!
//! WebGPT is a special connection, not a generic OpenAI-compatible provider.
//! The core supervises exactly one pinned sidecar bridge (the plan-pinned
//! `codex-chatgpt-web` bundle at its fixed commit), one isolated profile
//! directory and one isolated cookie store per connection, all under the core
//! data dir. The owner's personal browser is never shared, and the core never
//! reads or writes it.
//!
//! Invariants this module enforces:
//! - **Isolated ownership** — the sidecar binary and the per-connection profile
//!   live under `data_dir/webgpt`. `probe` re-derives the sidecar digest
//!   against the pinned value and the profile digest against a manifest of
//!   every file in the profile, so a stale, drifted or tampered sidecar is
//!   detected and refused — never silently used.
//! - **Owner-interactive activation** — activation is a real ChatGPT login that
//!   only the owner can perform. The core refuses to automate it: `activate`
//!   always returns the typed `interactive_required` error and the core never
//!   stores a password or bypasses a challenge. A profile is `Activated` only
//!   when it carries an activation marker whose recorded profile digest still
//!   matches the live profile.
//! - **Duplicate-send defense** — a submission's idempotency key (the frame
//!   request ID) is the durable identity. The same key with the same content
//!   returns the stored receipt (exactly one native send ever happens); the
//!   same key with different content is a typed conflict, never a silent
//!   overwrite. A failed submission is resubmitted under a NEW key and the old
//!   receipt is durably marked `superseded`.
//! - **Crash recovery** — the sidecar confirms a real send by writing
//!   `native_message_id.txt` into the durable per-key request dir. A core
//!   crash between that confirmation and the receipt commit leaves the dir
//!   behind; `reconcile` reconstructs the `submitted` receipt from the native
//!   confirmation (or records an explicit `failed`/unknown-outcome receipt
//!   when the confirmation is missing). No fresh chat is ever opened and no
//!   other account is ever substituted.
//!
//! Receipt durability: each receipt is a CAS object in the run's project
//! object store, indexed in the `artifacts` table under the virtual path
//! `webgpt_submission/<key>/<digest>` (state mirrored to the `events` table as
//! a `StateChanged` event), so a restart reconstructs the index from the store
//! alone.

use std::{
    fs,
    io::{Cursor, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use base64::Engine as _;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest as ShaDigest, Sha256};
use schemars::JsonSchema;
use ts_rs::TS;
use uuid::Uuid;

use crate::{
    contracts::{
        ArtifactId, ConnectionConfig, ContentPart, CoreEvent, Digest, ProjectId, ProviderProtocol,
        RunId,
    },
    error::{CoreError, Result},
    store::{
        events::now_ms,
        objects::{ObjectLimits, ObjectStore},
        Store,
    },
};

/// The plan-pinned ChatGPT-Web bridge (package 6.1.8 at the fixed commit). The
/// core refuses to run any other bytes: `probe` recomputes the sidecar digest
/// and compares it against the pinned value before anything else is accepted.
pub const PINNED_SIDECAR_NAME: &str = "codex-chatgpt-web";
pub const PINNED_SIDECAR_VERSION: &str = "6.1.8+0b6087c";
pub const PINNED_SIDECAR_DIGEST: &str = "0b6087c5307c7a58adda9c420358f3f70d2035f5";

const SIDECAR_TIMEOUT_MS: u64 = 120_000;

// ── Wire DTOs ────────────────────────────────────────────────────────────────

/// Activation state of a WebGPT connection as observed by the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum WebGptActivationState {
    /// The pinned sidecar bundle is not present under the core data dir.
    NotInstalled,
    /// The sidecar is present and pinned, but no owner activation exists yet.
    NotActivated,
    /// The owner has activated the isolated profile; a genuine turn can run.
    Activated,
    /// Present but unusable: non-bridge connection, pinned sidecar digest
    /// mismatch, or a profile that drifted after activation. Never used
    /// silently.
    Unavailable,
}

/// The observed WebGPT connection state. `reason` is the exact, non-secret
/// cause whenever the state is not `Activated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WebGptStatus {
    pub state: WebGptActivationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub sidecar_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub profile_digest: Option<Digest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reason: Option<String>,
}

/// Terminal state of one WebGPT submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum WebGptSubmissionState {
    /// The pinned sidecar confirmed the native send (native_message_id present).
    Submitted,
    /// The sidecar failed, or recovery found no native confirmation (unknown
    /// outcome — explicit, never implicitly retried).
    Failed,
    /// A repeat of the same idempotency key and content: the stored receipt is
    /// returned, no second native send happens.
    Duplicate,
    /// Superseded by a `resubmit`: retained for the audit trail; the original
    /// receipt's identity is unchanged, this view adds `superseded_by`.
    Superseded,
}

/// A durable WebGPT submission receipt. `idempotency_key` is the frame request
/// ID (the durable identity); `digest` is the receipt's own SHA-256 (over the
/// receipt content excluding the digest field) and its CAS path is
/// `webgpt_submission/<key>/<digest>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WebGptSubmissionReceipt {
    pub digest: Digest,
    pub run_id: RunId,
    pub idempotency_key: Uuid,
    pub state: WebGptSubmissionState,
    /// SHA-256 of the canonical content; a same-key/different-content submit is
    /// a conflict, never a silent overwrite.
    pub submission_digest: Digest,
    /// The native message id exactly as the sidecar wrote it. Absent when the
    /// send could not be confirmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub native_message_id: Option<String>,
    /// The pinned sidecar version that produced this receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub sidecar_version: Option<String>,
    /// The profile digest at send time, binding the receipt to the exact
    /// activation the turn ran under.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub profile_digest: Option<Digest>,
    /// Image parts the sidecar received, as CAS objects owned by the run's
    /// project; each digest is the image bytes' content digest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_digests: Vec<Digest>,
    /// The prompt text (all text parts, in order), kept for the audit trail.
    pub prompt_text: String,
    #[serde(with = "crate::contracts::wire::i64")] #[schemars(with = "crate::contracts::wire::WireI64Schema")] #[ts(type = "string")]
    pub timestamp: i64,
    /// Set on a superseded view: the digest of the resubmit receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub superseded_by: Option<Digest>,
    /// Set on a duplicate view: the digest of the original send's receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub original: Option<Digest>,
    /// Human-readable cause for non-submitted states (never a secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reason: Option<String>,
}

/// Input to a genuine WebGPT turn (W2). Image parts carry base64 inline data
/// (`data:<mime>;base64,<data>`); every image is stored in the run's project
/// CAS before the sidecar runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WebGptTurnInput {
    pub content: Vec<ContentPart>,
    /// Native conversation to continue, when the sidecar reports one. A turn
    /// never opens a fresh chat implicitly: continuation only follows a
    /// sidecar-confirmed conversation id, never a retry of a failed send.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub conversation_id: Option<String>,
}

/// The native WebGPT response with its provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WebGptTurnOutput {
    pub run_id: RunId,
    /// The native message id exactly as the sidecar wrote it (its
    /// confirmation file — not core-generated).
    pub native_message_id: String,
    /// Native conversation identifier as reported by the sidecar.
    pub conversation_id: String,
    /// The native model label as reported by the sidecar; the core never
    /// substitutes its own identity.
    pub model: String,
    /// The native reply text exactly as the sidecar returned it.
    pub response_text: String,
    pub sidecar_version: String,
    pub profile_digest: Digest,
    #[serde(with = "crate::contracts::wire::i64")] #[schemars(with = "crate::contracts::wire::WireI64Schema")] #[ts(type = "string")]
    pub timestamp: i64,
}

/// A tool the sidecar natively exposes. Only entries verified against the
/// sidecar's own capability manifest are ever advertised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WebGptToolDescriptor {
    pub name: String,
    pub verified: bool,
    pub description: String,
}

/// In-memory view: the stored receipt plus its run's project (needed to read
/// CAS objects for duplicate/resubmit/reconcile).
#[derive(Debug, Clone)]
pub struct ReceiptView {
    pub receipt: WebGptSubmissionReceipt,
    pub project_id: ProjectId,
}

// ── Service ──────────────────────────────────────────────────────────────────

/// W1/W2/W3 WebGPT connection service. Constructed on demand by dispatch;
/// clones share the single canonical Store. No in-process cache is ever
/// authoritative — every read goes to the Store or the core-owned sidecar root
/// under `data_dir/webgpt`.
#[derive(Clone)]
pub struct WebGptService {
    store: Store,
    data_dir: PathBuf,
    /// The sidecar digest the core will run. Defaults to the plan-pinned
    /// value; `with_pinned_digest` lets a test pin to its own fixture binary
    /// without weakening the production default.
    pinned_digest: Digest,
}

impl WebGptService {
    /// `data_dir` is the core data dir (`RuntimePaths::data_dir`); the sidecar
    /// root, isolated profiles and per-key request dirs all live under it. The
    /// sidecar must match `PINNED_SIDECAR_DIGEST`.
    pub fn new(store: Store, data_dir: PathBuf) -> Self {
        Self {
            store,
            data_dir,
            pinned_digest: Digest::parse(PINNED_SIDECAR_DIGEST)
                .expect("the pinned sidecar digest is a valid sha256 hex"),
        }
    }

    /// Test seam: pin the service to a specific sidecar digest (a test
    /// fixture binary). Production always uses `new` (the plan-pinned value).
    pub fn with_pinned_digest(store: Store, data_dir: PathBuf, pinned_digest: Digest) -> Self {
        Self {
            store,
            data_dir,
            pinned_digest,
        }
    }

    fn webgpt_root(&self) -> PathBuf {
        self.data_dir.join("webgpt")
    }

    fn sidecar_binary(&self) -> PathBuf {
        self.webgpt_root().join(PINNED_SIDECAR_NAME)
    }

    fn profile_dir(&self, connection_id: &str) -> PathBuf {
        self.webgpt_root().join("profiles").join(safe_segment(connection_id))
    }

    fn requests_root(&self) -> PathBuf {
        self.webgpt_root().join("requests")
    }

    /// The WebGPT bridge connection is the `custom`-protocol connection with
    /// the exact `bridge://webgpt` endpoint. Any other connection is refused —
    /// the core never treats a generic provider connection as a ChatGPT-Web
    /// account.
    fn is_bridge(connection: &ConnectionConfig) -> bool {
        connection.protocol == ProviderProtocol::Custom
            && connection.endpoint == "bridge://webgpt"
    }

    // ── W1: probe / activation ─────────────────────────────────────────────

    /// Detect whether the pinned, isolated, owner-activated sidecar for this
    /// connection exists. Never fabricates activation: with no owner account
    /// the state is `NotActivated` (sidecar present) or `NotInstalled`
    /// (absent), each with the exact reason; a drifted profile is
    /// `Unavailable`, not a silent reuse.
    pub async fn probe(&self, connection: &ConnectionConfig) -> Result<WebGptStatus> {
        if !Self::is_bridge(connection) {
            return Ok(WebGptStatus {
                state: WebGptActivationState::Unavailable,
                sidecar_version: None,
                profile_digest: None,
                reason: Some(
                    "connection is not the bridge://webgpt WebGPT connection".into(),
                ),
            });
        }
        let profile = self.profile_dir(&connection.id);
        let profile_digest = if profile.is_dir() {
            Some(profile_manifest_digest(&profile)?)
        } else {
            None
        };
        let sidecar = self.sidecar_binary();
        if !sidecar.is_file() {
            return Ok(WebGptStatus {
                state: WebGptActivationState::NotInstalled,
                sidecar_version: None,
                profile_digest,
                reason: Some(format!(
                    "pinned sidecar {PINNED_SIDECAR_NAME} is not installed under the core data dir"
                )),
            });
        }
        let sidecar_digest = file_digest(&sidecar)?;
        if sidecar_digest.as_str() != self.pinned_digest.as_str() {
            return Ok(WebGptStatus {
                state: WebGptActivationState::Unavailable,
                sidecar_version: None,
                profile_digest,
                reason: Some(format!(
                    "sidecar digest {} differs from the pinned digest {}; a stale or tampered bridge is never run",
                    sidecar_digest.as_str(),
                    self.pinned_digest.as_str()
                )),
            });
        }
        match read_activation_marker(&profile) {
            Some(marker)
                if marker.state == "activated" && profile_digest.as_ref() == Some(&marker.profile_digest) =>
            {
                Ok(WebGptStatus {
                    state: WebGptActivationState::Activated,
                    sidecar_version: Some(PINNED_SIDECAR_VERSION.into()),
                    profile_digest,
                    reason: None,
                })
            }
            Some(_) => Ok(WebGptStatus {
                state: WebGptActivationState::Unavailable,
                sidecar_version: Some(PINNED_SIDECAR_VERSION.into()),
                profile_digest,
                reason: Some(
                    "activation marker does not bind the current profile digest; the profile changed after activation and is refused, not silently reused".into(),
                ),
            }),
            None => Ok(WebGptStatus {
                state: WebGptActivationState::NotActivated,
                sidecar_version: Some(PINNED_SIDECAR_VERSION.into()),
                profile_digest,
                reason: Some(
                    "no owner activation exists; the ChatGPT login is interactive and owner-performed only".into(),
                ),
            }),
        }
    }

    /// Activation is owner-interactive. The core NEVER logs in, stores a
    /// password or bypasses a challenge, so this command always returns the
    /// typed `interactive_required` error until a real interactive owner
    /// channel exists. (An owner login performed in the isolated profile
    /// records the activation marker itself; the core only verifies the
    /// marker's profile-digest binding via `probe`.)
    pub async fn activate(&self, connection_id: &str) -> Result<WebGptStatus> {
        Err(CoreError::new(
            "interactive_required",
            format!(
                "webgpt activation for connection {connection_id} requires an interactive owner ChatGPT login in the isolated profile {}; the core never automates login, stores credentials or bypasses a challenge",
                self.profile_dir(connection_id).display()
            ),
        ))
    }

    /// A genuine send requires an activated sidecar. Returns the pinned
    /// version and the live profile digest; typed errors otherwise. This is
    /// the live-adapter gate: without an owner account it ALWAYS fails closed
    /// — the service has no canned-response path.
    async fn require_activated(&self, connection: &ConnectionConfig) -> Result<(String, Digest)> {
        let status = self.probe(connection).await?;
        match status.state {
            WebGptActivationState::Activated => {
                let version = status.sidecar_version.ok_or_else(|| {
                    CoreError::new("webgpt_unavailable", "activated sidecar lacks a version identity")
                })?;
                let digest = status.profile_digest.ok_or_else(|| {
                    CoreError::new("webgpt_unavailable", "activated sidecar lacks a profile digest")
                })?;
                Ok((version, digest))
            }
            WebGptActivationState::Unavailable => Err(CoreError::new(
                "webgpt_unavailable",
                status.reason.unwrap_or_else(|| "webgpt sidecar is unavailable".into()),
            )),
            _ => Err(CoreError::new(
                "webgpt_not_activated",
                status
                    .reason
                    .unwrap_or_else(|| "webgpt sidecar is not activated".into()),
            )),
        }
    }

    // ── W2: genuine native turns + qualified tools ─────────────────────────

    /// Send a real turn to the activated sidecar (pinned binary, isolated
    /// profile) and return the native response with provenance. The turn is a
    /// receipt-gated send under a fresh per-turn idempotency key; the native
    /// response files the sidecar wrote are then read back. Requires an
    /// activated sidecar (owner account): without one this fails closed with
    /// `webgpt_not_activated` — never a canned model reply.
    pub async fn turn(&self, run: RunId, input: WebGptTurnInput) -> Result<WebGptTurnOutput> {
        let key = Uuid::new_v4();
        let receipt = self.submit(run, input.content, key).await?;
        if receipt.state != WebGptSubmissionState::Submitted || receipt.native_message_id.is_none() {
            return Err(CoreError::new(
                "webgpt_turn_failed",
                "the native send was not confirmed; no turn output is fabricated",
            ));
        }
        let dir = self.requests_root().join("webgpt").join(safe_segment(&key.to_string()));
        let read_sidecar_file = |name: &str| -> Result<String> {
            fs::read_to_string(dir.join(name)).map_err(|_| {
                CoreError::new("webgpt_response_missing", format!("sidecar {name} file is missing"))
            })
        };
        let response_text = read_sidecar_file("response.txt")?;
        let conversation_id = read_sidecar_file("conversation.txt")?;
        let model = read_sidecar_file("model.txt")?;
        Ok(WebGptTurnOutput {
            run_id: run,
            native_message_id: receipt.native_message_id.expect("submitted receipt has a native id"),
            conversation_id: conversation_id.trim().to_owned(),
            model: model.trim().to_owned(),
            response_text,
            sidecar_version: receipt.sidecar_version.unwrap_or_default(),
            profile_digest: receipt
                .profile_digest
                .expect("an activated submit always records a profile digest"),
            timestamp: receipt.timestamp,
        })
    }

    /// Only tools the sidecar natively exposes are advertised, from the
    /// sidecar's own capability manifest in the activated profile. An entry
    /// the manifest does not verify is never advertised.
    pub async fn advertised_tools(
        &self,
        connection: &ConnectionConfig,
    ) -> Result<Vec<WebGptToolDescriptor>> {
        self.require_activated(connection).await?;
        let manifest = self.profile_dir(&connection.id).join("capabilities.json");
        let raw = fs::read(&manifest).map_err(|_| {
            CoreError::new(
                "webgpt_capabilities_missing",
                "activated profile has no sidecar capability manifest; no tools are advertised until the sidecar verifies them",
            )
        })?;
        let names: Vec<String> = serde_json::from_slice(&raw).map_err(|_| {
            CoreError::new(
                "webgpt_capabilities_invalid",
                "sidecar capability manifest is not a JSON list of tool names",
            )
        })?;
        Ok(names
            .into_iter()
            .filter(|name| !name.is_empty())
            .map(|name| WebGptToolDescriptor {
                name,
                verified: true,
                description: "natively exposed and verified by the pinned sidecar".into(),
            })
            .collect())
    }

    // ── W3: receipts, duplicate-send defense, recovery ─────────────────────

    /// Submit content to the activated sidecar under a durable idempotency
    /// key (the frame request ID).
    ///
    /// - Same key + same content → the stored receipt is returned (`Duplicate`
    ///   view on first repeat, the stored view afterwards); exactly one native
    ///   send ever happens for the key.
    /// - Same key + different content → typed `idempotency_key_conflict`;
    ///   never a silent overwrite.
    /// - Key already `superseded` → typed `idempotency_key_superseded`.
    /// - Key already `failed` with the same content → typed
    ///   `submission_failed`; use `resubmit` to send it again under a new key.
    /// - Fresh key → one native send; the receipt is committed durably (CAS
    ///   object + artifacts row + `StateChanged` event).
    ///
    /// Requires an activated sidecar; without an owner account this fails
    /// closed with `webgpt_not_activated` and no receipt is recorded.
    pub async fn submit(
        &self,
        run: RunId,
        content: Vec<ContentPart>,
        idempotency_key: Uuid,
    ) -> Result<WebGptSubmissionReceipt> {
        let connection = self.bridge_connection().await?;
        let (version, profile_digest) = self.require_activated(&connection).await?;
        let project = self.project_of_run(run).await?;
        let objects = self.objects(project)?;
        let payload = self.prepare_content(&objects, &content).await?;

        // Duplicate-send defense BEFORE any process is spawned: the durable
        // artifacts index is the authority, not any in-process cache.
        if let Some(existing) = self.latest_receipt(idempotency_key).await? {
            match existing.receipt.state {
                WebGptSubmissionState::Duplicate => return Ok(existing.receipt),
                WebGptSubmissionState::Superseded => {
                    return Err(CoreError::new(
                        "idempotency_key_superseded",
                        "this idempotency key was superseded by a resubmit; use the resubmit's stored receipt",
                    ))
                }
                WebGptSubmissionState::Failed => {
                    if existing.receipt.submission_digest != payload.submission_digest {
                        return Err(CoreError::new(
                            "idempotency_key_conflict",
                            "idempotency key already used with different content; a new key is required (never a silent overwrite)",
                        ));
                    }
                    return Err(CoreError::new(
                        "submission_failed",
                        "this idempotency key already has a failed submission; resubmit the stored receipt to send it again under a new key",
                    ));
                }
                WebGptSubmissionState::Submitted => {
                    if existing.receipt.submission_digest != payload.submission_digest {
                        return Err(CoreError::new(
                            "idempotency_key_conflict",
                            "idempotency key already used with different content; a new key is required (never a silent overwrite)",
                        ));
                    }
                    let view = duplicate_view(&existing.receipt);
                    self.commit_receipt(&view, project, run).await?;
                    return Ok(view);
                }
            }
        }

        let request_dir = self.new_request_dir(idempotency_key, run, &payload, "submit")?;
        let exit = self.invoke_sidecar(&idempotency_key.to_string(), &request_dir).await;
        let confirmed = read_native_message_id(&request_dir);
        let (state, reason) = match (&exit, &confirmed) {
            (Ok(_), Some(_)) => (WebGptSubmissionState::Submitted, None),
            (Ok(_), None) => (
                WebGptSubmissionState::Failed,
                Some("sidecar exited 0 without writing a native message confirmation".into()),
            ),
            (Err(_), Some(_)) => (WebGptSubmissionState::Submitted, None),
            (Err(error), None) => (WebGptSubmissionState::Failed, Some(error.to_string())),
        };
        let receipt = WebGptSubmissionReceipt {
            digest: Digest::parse(&"0".repeat(64)).expect("zero digest parses"),
            run_id: run,
            idempotency_key,
            state,
            submission_digest: payload.submission_digest,
            native_message_id: confirmed,
            sidecar_version: Some(version),
            profile_digest: Some(profile_digest),
            image_digests: payload.image_digests,
            prompt_text: payload.text,
            timestamp: now_ms(),
            superseded_by: None,
            original: None,
            reason,
        };
        let receipt = finalize_receipt(receipt);
        self.commit_receipt(&receipt, project, run).await?;
        Ok(receipt)
    }

    /// Re-send a FAILED submission under a NEW idempotency key (the old key is
    /// durably marked `superseded` once the re-send confirms). Only the stored
    /// receipt's own content is re-sent: no new prompt is composed, no fresh
    /// chat is opened, no other account is substituted. A submitted (or
    /// already-superseded) receipt cannot be resubmitted.
    pub async fn resubmit(&self, run: RunId, receipt_digest: Digest) -> Result<WebGptSubmissionReceipt> {
        let existing = self.get_receipt(&receipt_digest).await?;
        if existing.receipt.run_id != run {
            return Err(CoreError::new("receipt_run_mismatch", "receipt does not belong to this run"));
        }
        if existing.receipt.state != WebGptSubmissionState::Failed {
            return Err(CoreError::new(
                "submission_not_failed",
                "only a failed submission can be resubmitted; a submitted receipt is never re-sent",
            ));
        }
        let connection = self.bridge_connection().await?;
        let (version, profile_digest) = self.require_activated(&connection).await?;
        let new_key = Uuid::new_v4();
        let payload = self.replay_payload(&existing.receipt)?;
        let request_dir = self.new_request_dir(new_key, run, &payload, "resubmit")?;
        let exit = self.invoke_sidecar(&new_key.to_string(), &request_dir).await;
        let confirmed = read_native_message_id(&request_dir);
        let (state, reason) = match (&exit, &confirmed) {
            (Ok(_), Some(_)) => (WebGptSubmissionState::Submitted, None),
            (Ok(_), None) => (
                WebGptSubmissionState::Failed,
                Some("sidecar exited 0 without writing a native message confirmation".into()),
            ),
            (Err(_), Some(_)) => (WebGptSubmissionState::Submitted, None),
            (Err(error), None) => (WebGptSubmissionState::Failed, Some(error.to_string())),
        };
        let receipt = finalize_receipt(WebGptSubmissionReceipt {
            digest: Digest::parse(&"0".repeat(64)).expect("zero digest parses"),
            run_id: run,
            idempotency_key: new_key,
            state,
            submission_digest: payload.submission_digest,
            native_message_id: confirmed,
            sidecar_version: Some(version),
            profile_digest: Some(profile_digest),
            image_digests: payload.image_digests,
            prompt_text: payload.text,
            timestamp: now_ms(),
            superseded_by: None,
            original: Some(receipt_digest),
            reason,
        });
        self.commit_receipt(&receipt, existing.project_id, run).await?;
        if state == WebGptSubmissionState::Submitted {
            self.mark_superseded(&existing.receipt, receipt.digest.clone(), existing.project_id, run)
                .await?;
        }
        Ok(receipt)
    }

    /// Crash recovery for an idempotency key: rebuild the receipt from the
    /// sidecar's durable per-key request dir. Returns the stored receipt when
    /// one already exists (a content mismatch is a typed
    /// `reconciliation_conflict`, never a second send). When no receipt
    /// exists, reconstructs `submitted` (native confirmation present) or
    /// `failed` (no confirmation: unknown outcome, explicit, never implicitly
    /// retried) and commits it durably.
    pub async fn reconcile(&self, run: RunId, key: Uuid) -> Result<WebGptSubmissionReceipt> {
        let dir = self.requests_root().join("webgpt").join(safe_segment(&key.to_string()));
        if let Some(existing) = self.latest_receipt(key).await? {
            let content_file = dir.join("content.json");
            if content_file.is_file() {
                let content = fs::read(&content_file).map_err(|_| {
                    CoreError::new("receipt_not_found", "sidecar request dir is unreadable")
                })?;
                if content_digest_of(&content) != existing.receipt.submission_digest {
                    return Err(CoreError::new(
                        "reconciliation_conflict",
                        "stored receipt content differs from the sidecar request dir; owner reconciliation required, never an implicit re-send",
                    ));
                }
            }
            return Ok(existing.receipt);
        }
        if !dir.is_dir() {
            return Err(CoreError::new(
                "receipt_not_found",
                "no sidecar request dir exists for this idempotency key; nothing was ever sent",
            ));
        }
        let content = fs::read(dir.join("content.json")).map_err(|_| {
            CoreError::new("receipt_not_found", "sidecar request dir lacks the stored content")
        })?;
        let (version, profile_digest) = self.activation_identity().await;
        let confirmed = read_native_message_id(&dir);
        let state = if confirmed.is_some() {
            WebGptSubmissionState::Submitted
        } else {
            WebGptSubmissionState::Failed
        };
        let reason = if state == WebGptSubmissionState::Failed {
            Some(
                "no native confirmation after restart: the send outcome is unknown and must not be retried implicitly".into(),
            )
        } else {
            None
        };
        let project = self.project_of_run(run).await?;
        let receipt = finalize_receipt(WebGptSubmissionReceipt {
            digest: Digest::parse(&"0".repeat(64)).expect("zero digest parses"),
            run_id: run,
            idempotency_key: key,
            state,
            submission_digest: content_digest_of(&content),
            native_message_id: confirmed,
            sidecar_version: Some(version),
            profile_digest,
            image_digests: content_image_digests(&content),
            prompt_text: content_text(&content),
            timestamp: now_ms(),
            superseded_by: None,
            original: None,
            reason,
        });
        self.commit_receipt(&receipt, project, run).await?;
        Ok(receipt)
    }

    /// Read a stored receipt by its digest (artifacts row + CAS object).
    pub async fn get_receipt(&self, digest: &Digest) -> Result<ReceiptView> {
        let project_id = self
            .store
            .with_connection({
                let digest = digest.clone();
                move |conn| {
                    let project: Option<String> = conn
                        .query_row(
                            "SELECT project_id FROM artifacts WHERE path LIKE ?1",
                            params![format!("webgpt_submission/%/{}", digest.as_str())],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let project = project.ok_or_else(|| {
                        CoreError::new("receipt_not_found", "no webgpt submission receipt has this digest")
                    })?;
                    Uuid::parse_str(&project).map(ProjectId).map_err(|_| {
                        CoreError::new("storage_corrupt", "artifact project id is not a UUID")
                    })
                }
            })
            .await?;
        let id = self
            .store
            .with_connection({
                let digest = digest.clone();
                let project_id = project_id.clone();
                move |conn| {
                    let value: Option<String> = conn
                        .query_row(
                            "SELECT id FROM artifacts WHERE path LIKE ?1 AND project_id = ?2",
                            params![
                                format!("webgpt_submission/%/{}", digest.as_str()),
                                project_id.to_string()
                            ],
                            |r| r.get::<_, String>(0),
                        )
                        .optional()?;
                    let value = value.ok_or_else(|| {
                        CoreError::new("receipt_not_found", "no webgpt submission receipt has this digest")
                    })?;
                    Uuid::parse_str(&value).map(ArtifactId).map_err(|_| {
                        CoreError::new("storage_corrupt", "artifact id is not a UUID")
                    })
                }
            })
            .await?;
        let objects = self.objects(project_id)?;
        let mut reader = objects.get(id).await?;
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|_| CoreError::new("storage_error", "receipt object is unreadable"))?;
        let receipt: WebGptSubmissionReceipt = serde_json::from_slice(&bytes).map_err(|_| {
            CoreError::new("receipt_invalid", "stored receipt object is not a valid receipt")
        })?;
        if receipt.digest != *digest {
            return Err(CoreError::new(
                "receipt_digest_mismatch",
                "stored receipt object does not hash to its claimed digest",
            ));
        }
        Ok(ReceiptView {
            project_id,
            receipt,
        })
    }

    // ── Private helpers ────────────────────────────────────────────────────

    /// Load the core-owned bridge connection from provider_references.
    async fn bridge_connection(&self) -> Result<ConnectionConfig> {
        let config = self
            .store
            .with_connection(move |conn| {
                let raw: Option<String> = conn
                    .query_row(
                        "SELECT config FROM provider_references WHERE id = ?1",
                        params!["webgpt"],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(serde_json::from_str::<ConnectionConfig>(&raw.ok_or_else(|| {
                    CoreError::new(
                        "webgpt_connection_missing",
                        "no bridge://webgpt provider reference is seeded in this store",
                    )
                })?)?)
            })
            .await?;
        if !Self::is_bridge(&config) {
            return Err(CoreError::new(
                "webgpt_connection_missing",
                "seeded webgpt connection is not a bridge://webgpt custom-protocol connection",
            ));
        }
        Ok(config)
    }

    fn objects(&self, project: ProjectId) -> Result<ObjectStore> {
        let root = self.data_dir.join("objects").join(project.to_string());
        Ok(ObjectStore::new(self.store.clone(), project, root, ObjectLimits::default())?)
    }

    async fn project_of_run(&self, run: RunId) -> Result<ProjectId> {
        let project = self
            .store
            .with_connection(move |conn| {
                let value: Option<String> = conn
                    .query_row(
                        "SELECT j.project_id FROM runs r \
                         JOIN nodes n ON n.id = r.node_id \
                         JOIN jobs j ON j.id = n.job_id \
                         WHERE r.id = ?1",
                        params![run.to_string()],
                        |r| r.get(0),
                    )
                    .optional()?;
                let project = value.ok_or_else(|| CoreError::new("run_not_found", "the run does not exist in this store"))?;
                Uuid::parse_str(&project).map(ProjectId).map_err(|_| CoreError::new("storage_corrupt", "project id is not a UUID"))
            })
            .await?;
        Ok(project)
    }

    /// Find the latest receipt for an idempotency key (by timestamp, then digest).
    async fn latest_receipt(&self, key: Uuid) -> Result<Option<ReceiptView>> {
        let key_str = key.to_string();
        let rows = self
            .store
            .with_connection({
                let key = key_str;
                move |conn| {
                    let mut stmt = conn.prepare(
                        "SELECT id, project_id, digest FROM artifacts WHERE path LIKE ?1",
                    )?;
                    let rows = stmt.query_map(params![format!("webgpt_submission/{}/%", key)], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                    })?;
                    let mut out = Vec::new();
                    for row in rows { out.push(row?); }
                    Ok(out)
                }
            })
            .await?;
        if rows.is_empty() { return Ok(None); }
        // Read each candidate and pick the one with the highest timestamp.
        let mut best: Option<ReceiptView> = None;
        for (id_str, project_id_str, _file_digest) in rows {
            let project = ProjectId(Uuid::parse_str(&project_id_str).map_err(|_| CoreError::new("storage_corrupt", "project id is not a UUID"))?);
            let artifact_id = ArtifactId(Uuid::parse_str(&id_str).map_err(|_| CoreError::new("storage_corrupt", "artifact id is not a UUID"))?);
            let objects = self.objects(project)?;
            let mut reader = objects.get(artifact_id).await?;
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).map_err(|_| CoreError::new("storage_error", "receipt object is unreadable"))?;
            let receipt: WebGptSubmissionReceipt = serde_json::from_slice(&bytes).map_err(|_| CoreError::new("receipt_invalid", "stored receipt is not valid"))?;
            if best.is_none() || receipt.timestamp > best.as_ref().unwrap().receipt.timestamp ||
               (receipt.timestamp == best.as_ref().unwrap().receipt.timestamp && receipt.digest.as_str() > best.as_ref().unwrap().receipt.digest.as_str()) {
                best = Some(ReceiptView { project_id: project, receipt });
            }
        }
        Ok(best)
    }

    /// Commit a receipt durably: CAS put, then artifacts index row, then event.
    async fn commit_receipt(
        &self,
        receipt: &WebGptSubmissionReceipt,
        project: ProjectId,
        run: RunId,
    ) -> Result<()> {
        let path = format!("webgpt_submission/{}/{}", receipt.idempotency_key.to_string(), receipt.digest.as_str());
        let bytes = serde_json::to_vec(receipt).map_err(|_| CoreError::new("receipt_invalid", "receipt serialization failed"))?;
        let objects = self.objects(project)?;
        let limit = bytes.len() as u64;
        // Put into CAS (creates its own artifacts row with path=digest, run_id=NULL).
        let cas_id = objects.put(Cursor::new(bytes.clone()), limit).await?;
        // Update the CAS row to our virtual path and run_id.
        let path_clone = path.clone();
        let run_str = run.to_string();
        self.store.with_connection(move |conn| {
            conn.execute(
                "UPDATE artifacts SET path = ?1, run_id = ?2 WHERE id = ?3",
                params![path_clone, run_str, cas_id.to_string()],
            )?;
            Ok(())
        }).await?;
        // Record the state change event.
        self.store_event(run, receipt).await?;
        Ok(())
    }

    /// Mark a receipt as superseded by a new one (called after successful resubmit).
    async fn mark_superseded(
        &self,
        original: &WebGptSubmissionReceipt,
        new_digest: Digest,
        project: ProjectId,
        run: RunId,
    ) -> Result<()> {
        let mut variant = original.clone();
        variant.state = WebGptSubmissionState::Superseded;
        variant.superseded_by = Some(new_digest);
        variant.timestamp = now_ms();
        // Note: original.digest stays the same; variant has same fields but new state/ts/superseded_by → new digest via finalize_receipt
        let variant = finalize_receipt(variant);
        self.commit_receipt(&variant, project, run).await?;
        Ok(())
    }

    async fn store_event(&self, run: RunId, receipt: &WebGptSubmissionReceipt) -> Result<()> {
        let entity_id = format!("webgpt_submission/{}", receipt.idempotency_key.to_string());
        let state_str = match receipt.state {
            WebGptSubmissionState::Submitted => "submitted",
            WebGptSubmissionState::Failed => "failed",
            WebGptSubmissionState::Duplicate => "duplicate",
            WebGptSubmissionState::Superseded => "superseded",
        };
        self.store
            .append(Some(run), CoreEvent::StateChanged { entity_id, state: state_str.into() })
            .await
            .map(|_| ())
            .map_err(|_| CoreError::new("storage_error", "failed to record webgpt submission state event"))?;
        Ok(())
    }

    async fn activation_identity(&self) -> (String, Option<Digest>) {
        if let Ok(connection) = self.bridge_connection().await {
            if let Ok(status) = self.probe(&connection).await {
                if status.state == WebGptActivationState::Activated {
                    return (status.sidecar_version.unwrap_or_else(|| PINNED_SIDECAR_VERSION.to_owned()), status.profile_digest);
                }
            }
        }
        (PINNED_SIDECAR_VERSION.to_owned(), None)
    }

    /// Create the durable per-key request directory with content.json.
    fn new_request_dir(
        &self,
        key: Uuid,
        run: RunId,
        payload: &PreparedContent,
        verb: &str,
    ) -> Result<PathBuf> {
        let dir = self.requests_root().join("webgpt").join(safe_segment(&key.to_string()));
        if dir.exists() {
            return Err(CoreError::new(
                "request_dir_exists",
                "a sidecar request dir already exists for this key; reconcile it instead",
            ));
        }
        let parent = dir.parent().expect("request dir has a parent");
        fs::create_dir_all(parent)?;
        fs::create_dir(&dir)?;
        fs::write(
            dir.join("content.json"),
            serde_json::json!({
                "run_id": run.to_string(),
                "verb": verb,
                "submission_digest": payload.submission_digest.as_str(),
                "prompt_text": payload.text,
                "image_digests": payload.image_digests.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            })
            .to_string(),
        )?;
        fs::write(dir.join("payload.json"), payload.content_json.as_bytes())?;
        Ok(dir)
    }

    /// Spawn the pinned sidecar at the real process boundary with timeout.
    async fn invoke_sidecar(&self, key: &str, request_dir: &Path) -> Result<()> {
        let binary = self.sidecar_binary();
        if !binary.is_file() {
            return Err(CoreError::new("webgpt_sidecar_missing", "the pinned sidecar binary disappeared"));
        }
        let timeout = std::time::Duration::from_millis(SIDECAR_TIMEOUT_MS);
        let mut child = tokio::process::Command::new(&binary)
            .env("IRONBARK_WEBGPT_REQUEST", request_dir)
            .env("IRONBARK_WEBGPT_PROFILE", self.webgpt_root().join("profiles"))
            .env("IRONBARK_WEBGPT_VERSION", PINNED_SIDECAR_VERSION)
            .env("IRONBARK_WEBGPT_KEY", key)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|_| CoreError::new("webgpt_sidecar_failed", "failed to spawn the pinned sidecar"))?;
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        // Both select arms terminate: no loop is needed (clippy::never_loop).
        let outcome = tokio::select! {
            result = child.wait() => result.map_err(|e| CoreError::new("webgpt_sidecar_failed", format!("sidecar wait failed: {e}"))),
            _ = &mut deadline => {
                let _ = child.kill().await;
                child.wait().await.map_err(|_| CoreError::new("webgpt_sidecar_failed", "sidecar wait failed after timeout kill"))
            }
        };
        let status = outcome?;
        if status.success() {
            Ok(())
        } else {
            Err(CoreError::new("webgpt_sidecar_failed", format!("pinned sidecar exited with code {status}")))
        }
    }

    /// Prepare content parts: store images in CAS, build canonical JSON + digest.
    async fn prepare_content(&self, objects: &ObjectStore, content: &[ContentPart]) -> Result<PreparedContent> {
        let mut texts = Vec::new();
        let mut image_digests = Vec::new();
        for part in content {
            match part {
                ContentPart::Text { text } => texts.push(text.clone()),
                ContentPart::Image { uri, mime_type: _ } => {
                    let bytes = decode_image_uri(uri)?;
                    let digest = sha256_digest(&bytes)?;
                    let limit = bytes.len() as u64;
                    objects.put(Cursor::new(bytes), limit).await?;
                    image_digests.push(digest);
                }
                other => return Err(CoreError::new(
                    "webgpt_content_unsupported",
                    format!("webgpt submission only supports text and image parts, got {:?}", other),
                )),
            }
        }
        let text = texts.join("\n");
        let canonical = canonical_content(&text, &image_digests);
        let submission_digest = sha256_digest(canonical.as_bytes())?;
        Ok(PreparedContent { text, content_json: canonical, submission_digest, image_digests })
    }

    /// Reconstruct the payload from a stored receipt for resubmit.
    fn replay_payload(&self, receipt: &WebGptSubmissionReceipt) -> Result<PreparedContent> {
        let canonical = canonical_content(&receipt.prompt_text, &receipt.image_digests);
        let submission_digest = sha256_digest(canonical.as_bytes())?;
        if submission_digest != receipt.submission_digest {
            return Err(CoreError::new(
                "receipt_content_mismatch",
                "stored receipt fields do not reproduce the recorded canonical content",
            ));
        }
        Ok(PreparedContent {
            text: receipt.prompt_text.clone(),
            content_json: canonical,
            submission_digest,
            image_digests: receipt.image_digests.clone(),
        })
    }
}

fn decode_image_uri(uri: &str) -> Result<Vec<u8>> {
    if let Some(data) = uri.strip_prefix("data:") {
        if let Some((_, encoded)) = data.split_once(",") {
            base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|_| CoreError::new("webgpt_image_invalid", "failed to decode base64 image data"))
        } else {
            Err(CoreError::new("webgpt_image_invalid", "image URI is not a valid data URI"))
        }
    } else {
        Err(CoreError::new(
            "webgpt_image_invalid",
            format!("image URI must be a data URI (data:<mime>;base64,...), got: {uri}"),
        ))
    }
}

// ── Free helpers ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PreparedContent {
    text: String,
    content_json: String,
    submission_digest: Digest,
    image_digests: Vec<Digest>,
}

fn canonical_content(text: &str, image_digests: &[Digest]) -> String {
    serde_json::json!({
        "prompt_text": text,
        "image_digests": image_digests.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
    })
    .to_string()
}

fn content_digest_of(content: &[u8]) -> Digest {
    sha256_digest(content).expect("content digest always parses")
}

fn content_text(content: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(content).unwrap_or_default();
    v.get("prompt_text").and_then(|x| x.as_str()).unwrap_or_default().to_owned()
}

fn content_image_digests(content: &[u8]) -> Vec<Digest> {
    let v: serde_json::Value = serde_json::from_slice(content).unwrap_or_default();
    v.get("image_digests")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| Digest::parse(s).expect("stored digest is valid"))).collect())
        .unwrap_or_default()
}

fn finalize_receipt(mut receipt: WebGptSubmissionReceipt) -> WebGptSubmissionReceipt {
    receipt.digest = receipt_digest_of(&receipt);
    receipt
}

fn receipt_digest_of(receipt: &WebGptSubmissionReceipt) -> Digest {
    // Hash the receipt content EXCLUDING the digest field (canonical form).
    let canonical = serde_json::json!({
        "run_id": receipt.run_id.to_string(),
        "idempotency_key": receipt.idempotency_key.to_string(),
        "state": match receipt.state {
            WebGptSubmissionState::Submitted => "submitted",
            WebGptSubmissionState::Failed => "failed",
            WebGptSubmissionState::Duplicate => "duplicate",
            WebGptSubmissionState::Superseded => "superseded",
        },
        "submission_digest": receipt.submission_digest.as_str(),
        "native_message_id": receipt.native_message_id,
        "sidecar_version": receipt.sidecar_version,
        "profile_digest": receipt.profile_digest.as_ref().map(|d| d.as_str()),
        "image_digests": receipt.image_digests.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
        "prompt_text": receipt.prompt_text,
        "timestamp": receipt.timestamp,
        "superseded_by": receipt.superseded_by.as_ref().map(|d| d.as_str()),
        "original": receipt.original.as_ref().map(|d| d.as_str()),
        "reason": receipt.reason,
    });
    sha256_digest(canonical.to_string().as_bytes()).expect("canonical receipt digest parses")
}

fn duplicate_view(original: &WebGptSubmissionReceipt) -> WebGptSubmissionReceipt {
    finalize_receipt(WebGptSubmissionReceipt {
        digest: Digest::parse(&"0".repeat(64)).expect("zero digest parses"),
        run_id: original.run_id,
        idempotency_key: original.idempotency_key,
        state: WebGptSubmissionState::Duplicate,
        submission_digest: original.submission_digest.clone(),
        native_message_id: original.native_message_id.clone(),
        sidecar_version: original.sidecar_version.clone(),
        profile_digest: original.profile_digest.clone(),
        image_digests: original.image_digests.clone(),
        prompt_text: original.prompt_text.clone(),
        timestamp: now_ms(),
        superseded_by: None,
        original: Some(original.digest.clone()),
        reason: Some("duplicate submission — original receipt returned, no second native send".into()),
    })
}

fn read_native_message_id(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("native_message_id.txt")).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, Deserialize)]
struct ActivationMarker {
    state: String,
    profile_digest: Digest,
}

fn read_activation_marker(profile: &Path) -> Option<ActivationMarker> {
    let path = profile.join("activation.json");
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Compute the manifest digest of a profile directory (all files except dotfiles,
/// activation.json, capabilities.json), sorted by relative path.
pub fn profile_manifest_digest(profile: &Path) -> Result<Digest> {
    let mut entries: Vec<(String, String, u64)> = Vec::new();
    collect_manifest(profile, profile, &mut entries)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut payload = String::new();
    for (rel, digest, bytes) in entries {
        payload.push_str(&rel);
        payload.push(' ');
        payload.push_str(&digest);
        payload.push(' ');
        payload.push_str(&bytes.to_string());
        payload.push('\n');
    }
    sha256_digest(payload.as_bytes())
}

fn collect_manifest(root: &Path, dir: &Path, out: &mut Vec<(String, String, u64)>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') { continue; }
        let path = entry.path();
        if path.is_dir() {
            collect_manifest(root, &path, out)?;
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if rel == "activation.json" || rel == "capabilities.json" { continue; }
            let bytes = fs::read(&path)?;
            out.push((rel, sha256_hex(&bytes), bytes.len() as u64));
        }
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<Digest> {
    let bytes = fs::read(path).map_err(|_| CoreError::new("webgpt_sidecar_unreadable", "the pinned sidecar is unreadable"))?;
    sha256_digest(&bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_digest(bytes: &[u8]) -> Result<Digest> {
    Digest::parse(&sha256_hex(bytes))
}

fn safe_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() { out.push('_'); }
    out
}

//! Durable checkpoint links: the run's actual workspace/diff/commit/artifact
//! state at planning, before/after mutation, after verification/review and
//! before consequential action. Checkpoint rows are the reconciliation
//! boundary: after a crash the last durable checkpoint is the only place a
//! run may resume, and a checkpoint never replays an uncertain mutation.
use std::io::Cursor;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::contracts::{
    ArtifactId, CheckpointPhase, CheckpointRecord, CoreEvent, Digest, ProjectId, ResultManifest,
    RunId, RunState, SessionRef,
};
use crate::error::{CoreError, Result};
use crate::store::{
    Store,
    events::{append_tx, now_ms, redacted_json},
    objects::ObjectStore,
};

/// The serialized checkpoint link stored in `checkpoints.body`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointBody {
    run_id: RunId,
    phase: CheckpointPhase,
    state_digest: Digest,
    observed_workspace: Value,
    manifest: Value,
    created_at: i64,
}

#[derive(Clone)]
pub struct CheckpointStore {
    store: Store,
    project: ProjectId,
    objects: ObjectStore,
}

impl CheckpointStore {
    pub fn new(store: Store, project: ProjectId, objects: ObjectStore) -> Self {
        Self { store, project, objects }
    }

    /// Persist a checkpoint link after planning, before/after mutation, after
    /// verification/review, or before consequential action. The manifest's
    /// native session ref, artifact IDs, checks and unresolved items are the
    /// recorded workspace/diff/commit/artifact state; the link bytes are a CAS
    /// object so they survive core death, and the `checkpoints` row binds the
    /// run to the state digest.
    pub async fn capture(
        &self,
        run_id: RunId,
        phase: CheckpointPhase,
        manifest: ResultManifest,
    ) -> Result<ArtifactId> {
        if manifest.run_id != run_id {
            return Err(CoreError::new("run_mismatch", "Manifest belongs to another run"));
        }
        let project = self.project;
        let project_str = project.to_string();
        // Read the actual run state and stage the digest outside the write
        // transaction; no CAS or lock work happens inside it.
        let staged_manifest = manifest.clone();
        let staged = self.store.with_connection(move |conn| {
            let (body, state, run_project): (String, String, String) = conn
                .query_row(
                    "SELECT r.body,r.state,j.project_id FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE r.id=?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| CoreError::new("run_missing", "Run does not exist"))?;
            if run_project != project_str {
                return Err(CoreError::new("project_mismatch", "Run does not belong to this project"));
            }
            let state = map_state(&state)?;
            match state {
                RunState::Succeeded | RunState::Failed | RunState::Cancelled => {
                    return Err(CoreError::new("run_terminal", "Run already resolved; no new checkpoints"));
                }
                _ => {}
            }
            let observed = observe(&body, staged_manifest.native_session.as_ref());
            let state_digest = state_digest(&observed, &staged_manifest, phase)?;
            let payload = redacted_json(&json!({
                "format": 1,
                "phase": phase,
                "state_digest": state_digest.as_str(),
                "manifest": staged_manifest,
                "observed_workspace": observed,
                "created_at": now_ms(),
            }))?;
            let payload = payload;
            Ok((payload, observed, state_digest))
        })
        .await?;
        let (payload, observed, state_digest) = staged;

        // Persist the actual link bytes in the CAS, then bind the row + event.
        let payload_bytes = payload.into_bytes();
        let payload_len = payload_bytes.len() as u64;
        let artifact = self
            .objects
            .put(Cursor::new(payload_bytes), payload_len)
            .await?;
        let manifest_value = serde_json::to_value(&manifest)?;
        self.store
            .with_transaction(move |tx| {
                let created = now_ms();
                tx.execute(
                    "INSERT INTO checkpoints(id,run_id,state_digest,body) VALUES(?1,?2,?3,?4)",
                    params![
                        Uuid::new_v4().to_string(),
                        run_id.to_string(),
                        state_digest.as_str(),
                        redacted_json(&CheckpointBody {
                            run_id,
                            phase,
                            state_digest: state_digest.clone(),
                            observed_workspace: observed,
                            manifest: manifest_value,
                            created_at: created,
                        })?,
                    ],
                )?;
                tx.execute(
                    "UPDATE artifacts SET run_id=?1 WHERE id=?2 AND run_id IS NULL",
                    params![run_id.to_string(), artifact.to_string()],
                )?;
                append_tx(
                    tx,
                    None,
                    Some(project),
                    Some(run_id),
                    &CoreEvent::ArtifactAdded {
                        artifact_id: artifact,
                        digest: state_digest,
                    },
                )?;
                Ok(())
            })
            .await?;
        self.store
            .append(
                Some(run_id),
                CoreEvent::StateChanged {
                    entity_id: run_id.to_string(),
                    state: format!("checkpoint:{}", phase_as_str(phase)),
                },
            )
            .await?;
        Ok(artifact)
    }

    /// The latest durable checkpoint link for a run, if any.
    pub async fn latest(&self, run_id: RunId) -> Result<Option<CheckpointRecord>> {
        let row: Option<(String, String)> = self
            .store
            .with_connection(move |conn| {
                conn.query_row(
                    "SELECT state_digest,body FROM checkpoints WHERE run_id=?1 ORDER BY rowid DESC LIMIT 1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| CoreError::new("storage_error", &e.to_string()))
            })
            .await?;
        let Some((state_digest, body)) = row else {
            return Ok(None);
        };
        let parsed: CheckpointBody =
            serde_json::from_str(&body).map_err(|_| CoreError::new("storage_corrupt", "Checkpoint body is corrupt"))?;
        if parsed.run_id != run_id {
            return Err(CoreError::new("storage_corrupt", "Checkpoint belongs to another run"));
        }
        Ok(Some(CheckpointRecord {
            run_id,
            phase: parsed.phase,
            state_digest: Digest::parse(&state_digest).map_err(|_| CoreError::new("storage_corrupt", "Checkpoint digest is corrupt"))?,
            manifest: parsed.manifest,
            created_at: parsed.created_at,
        }))
    }

    /// All checkpoint links in durable order (reconciliation replays none of
    /// them; it only reads the boundary they define).
    pub async fn records(&self, run_id: RunId) -> Result<Vec<CheckpointRecord>> {
        let rows: Vec<(String, String)> = self
            .store
            .with_connection(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT state_digest,body FROM checkpoints WHERE run_id=?1 ORDER BY rowid ASC",
                )?;
                let rows = stmt.query_map([run_id.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
                let mut result = Vec::new();
                for row in rows {
                    result.push(row?);
                }
                Ok(result)
            })
            .await?;
        let mut result = Vec::new();
        for (state_digest, body) in rows {
            let parsed: CheckpointBody =
                serde_json::from_str(&body).map_err(|_| CoreError::new("storage_corrupt", "Checkpoint body is corrupt"))?;
            result.push(CheckpointRecord {
                run_id,
                phase: parsed.phase,
                state_digest: Digest::parse(&state_digest).map_err(|_| CoreError::new("storage_corrupt", "Checkpoint digest is corrupt"))?,
                manifest: parsed.manifest,
                created_at: parsed.created_at,
            });
        }
        Ok(result)
    }
}

fn map_state(text: &str) -> Result<RunState> {
    serde_json::from_value::<RunState>(json!(text)).map_err(|_| CoreError::new("storage_corrupt", "Invalid persisted run state"))
}

fn phase_as_str(phase: CheckpointPhase) -> &'static str {
    match phase {
        CheckpointPhase::Planned => "planned",
        CheckpointPhase::PreMutation => "pre_mutation",
        CheckpointPhase::PostMutation => "post_mutation",
        CheckpointPhase::PostVerification => "post_verification",
        CheckpointPhase::PreConsequential => "pre_consequential",
    }
}

/// The observed workspace state recorded with the checkpoint: the native
/// session ref plus the durable run body the run record actually carries.
fn observe(run_body: &str, session: Option<&SessionRef>) -> Value {
    let run: Value = serde_json::from_str(run_body).unwrap_or_else(|_| json!({}));
    json!({
        "native_session": session,
        "run_body": run,
    })
}

fn state_digest(observed: &Value, manifest: &ResultManifest, phase: CheckpointPhase) -> Result<Digest> {
    let canonical = json!({ "observed_workspace": observed, "manifest": manifest, "phase": phase });
    Digest::parse(&format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).map_err(|_| CoreError::new("invalid_state", "Cannot encode checkpoint state"))?)
    ))
}

//! C4 scheduler entry: create the durable job/node/run chain for one task.
//!
//! `RunCreate` is the only way a run comes into existence, so it is also the
//! only place the scheduler mints the `jobs`/`nodes`/`runs` rows the rest of
//! the lifecycle (start, pause, checkpoint, reconcile) operates on. The run is
//! created in `queued` and moves to `ready` only through the transition table.
//!
//! Scoping rules enforced here:
//! - the project must exist (no implicit project creation),
//! - the task text is bounded and non-empty,
//! - the run is born `queued` with a durable `state_changed` event, so a crash
//!   between creation and dispatch leaves observable, reconcilable state.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::contracts::{CoreEvent, JobId, NodeId, ProjectId, RunId, RunState};
use crate::error::{CoreError, Result};
use crate::store::{events::append_tx, events::redact_text, Store};

/// Hard ceiling on a task description, mirroring the store's text bounds.
pub const MAX_TASK_BYTES: usize = 16 * 1024;

/// What `RunCreate` returns: the durable identities a caller needs to drive the
/// run, plus the state it was born in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, ts_rs::TS)]
#[serde(deny_unknown_fields)]
pub struct RunReceipt {
    pub run_id: RunId,
    pub node_id: NodeId,
    pub job_id: JobId,
    pub project_id: ProjectId,
    pub state: RunState,
}

#[derive(Clone)]
pub struct SchedulerService {
    store: Store,
    project: ProjectId,
}

impl SchedulerService {
    pub fn new(store: Store, project: ProjectId) -> Result<Self> {
        Ok(Self { store, project })
    }

    /// Create one run for one task. Idempotent per call identity is the caller's
    /// responsibility (the IPC layer dedupes request ids); this method always
    /// creates a distinct run.
    pub async fn create_run(&self, task: &str) -> Result<RunReceipt> {
        let trimmed = task.trim();
        if trimmed.is_empty() {
            return Err(CoreError::new("invalid_task", "a run requires a non-empty task description"));
        }
        if trimmed.len() > MAX_TASK_BYTES {
            return Err(CoreError::new("invalid_task", "task description exceeds the 16 KiB bound"));
        }
        let project = self.project;
        let project_str = project.to_string();
        let task_text = redact_text(trimmed);
        let task_body = serde_json::to_string(&serde_json::json!({ "task": task_text, "version": 1 }))?;
        let job_id = JobId::new();
        let node_id = NodeId::new();
        let run_id = RunId::new();
        let receipt = RunReceipt { run_id, node_id, job_id, project_id: project, state: RunState::Queued };
        let body = task_body.clone();
        self.store
            .with_transaction(move |tx| {
                let exists: Option<String> = tx
                    .query_row("SELECT id FROM projects WHERE id=?1", [project_str.clone()], |row| row.get(0))
                    .ok();
                if exists.is_none() {
                    return Err(CoreError::new("project_missing", "Project does not exist; create it before scheduling work"));
                }
                tx.execute(
                    "INSERT INTO jobs (id,project_id,body,state) VALUES (?1,?2,?3,'queued')",
                    params![job_id.to_string(), project_str, body],
                )?;
                tx.execute(
                    "INSERT INTO nodes (id,job_id,body,state) VALUES (?1,?2,?3,'queued')",
                    params![node_id.to_string(), job_id.to_string(), task_body],
                )?;
                tx.execute(
                    "INSERT INTO runs (id,node_id,state,body) VALUES (?1,?2,'queued',?3)",
                    params![run_id.to_string(), node_id.to_string(), task_body],
                )?;
                append_tx(
                    tx,
                    None,
                    Some(project),
                    Some(run_id),
                    &CoreEvent::StateChanged {
                        entity_id: run_id.to_string(),
                        state: "queued (run_created)".into(),
                    },
                )?;
                Ok(())
            })
            .await?;
        Ok(receipt)
    }
}

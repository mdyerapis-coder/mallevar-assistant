//! P5 explicit memory. Three states live in the `memory` table: `candidate`
//! (proposed, NOT retrievable), `approved` (retrievable), and `forgotten`
//! (explicitly removed). Decisions are immutable and recorded in the record's
//! body; forgetting marks, it does not destroy, so the audit trail survives.
//!
//! A retrieved memory is a text excerpt for context only: it is never
//! dispatched as a tool, a command, or a credential, and it cannot expand the
//! route's allowed tools.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::contracts::{MemoryCandidate, MemoryDecision, MemoryRecord, ProjectId};
use crate::error::{CoreError, Result};
use super::{MemoryService, now_ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryBody {
    candidate_id: String,
    state: String,
    text: String,
    provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_refs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision: Option<String>,
}

impl MemoryService {
    /// Propose a memory: creates a `candidate` record tied to its originating
    /// project + evidence. A candidate is NOT retrievable.
    pub async fn propose(&self, candidate: MemoryCandidate) -> Result<String> {
        let project_str = candidate.project_id.to_string();
        if candidate.text.trim().is_empty() {
            return Err(CoreError::new("invalid_memory", "memory candidate text must not be empty"));
        }
        let candidate_id = Uuid::new_v4().to_string();
        let record_id = Uuid::new_v4().to_string();
        let created = now_ms();
        let body = MemoryBody {
            candidate_id: candidate_id.clone(),
            state: "candidate".into(),
            text: candidate.text.clone(),
            provenance: candidate.provenance.clone(),
            input_refs: candidate.input_refs.clone(),
            decision: None,
        };
        let body_json = serde_json::to_string(&body)?;
        self.store.with_transaction(move |tx| {
            super::ensure_project_tx(tx, candidate.project_id)?;
            tx.execute(
                "INSERT INTO memory (id, project_id, provenance, body, created_at) VALUES (?1,?2,?3,?4,?5)",
                params![record_id, project_str, candidate.provenance, body_json, created],
            )?;
            Ok(())
        }).await?;
        Ok(candidate_id)
    }

    /// Decide on a proposed memory. `Approve` keeps the text; `EditAndApprove`
    /// uses `edited_text`; `Reject` closes the candidate. Returns the updated
    /// record. Only `candidate` records can be decided.
    pub async fn decide(&self, candidate_id: &str, edited_text: Option<String>, decision: MemoryDecision) -> Result<MemoryRecord> {
        let candidate_id = candidate_id.to_string();
        let (new_state, decision_label) = match decision {
            MemoryDecision::Approve => ("approved", "approve"),
            MemoryDecision::EditAndApprove => ("approved", "edit_and_approve"),
            MemoryDecision::Reject => ("rejected", "reject"),
        };
        self.store.with_transaction(move |tx| {
            let row: Option<(String, String, String, i64)> = tx.query_row(
                "SELECT id, project_id, provenance, created_at FROM memory WHERE json_extract(body,'$.candidate_id')=?1 AND json_extract(body,'$.state')='candidate'",
                [candidate_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            ).optional()?;
            let Some((record_id, project_str, _provenance, created)) = row else {
                return Err(CoreError::new("memory_candidate_not_found", "candidate is not in a decidable state"));
            };
            let body_json: String = tx.query_row(
                "SELECT body FROM memory WHERE id=?1",
                [record_id.clone()],
                |r| r.get(0),
            )?;
            let mut body: MemoryBody = serde_json::from_str(&body_json)
                .map_err(|_| CoreError::new("storage_corrupt", "memory body is not valid JSON"))?;
            body.state = new_state.to_string();
            body.decision = Some(decision_label.to_string());
            if decision == MemoryDecision::EditAndApprove {
                let text = edited_text.ok_or_else(|| CoreError::new("invalid_memory", "EditAndApprove requires the edited text"))?;
                if text.trim().is_empty() {
                    return Err(CoreError::new("invalid_memory", "edited memory text must not be empty"));
                }
                body.text = text;
            }
            let updated = serde_json::to_string(&body)?;
            tx.execute("UPDATE memory SET body=?1 WHERE id=?2", params![updated, record_id])?;
            Ok(MemoryRecord {
                id: record_id,
                project_id: ProjectId(uuid::Uuid::parse_str(&project_str)
                    .map_err(|_| CoreError::new("storage_corrupt", "memory project id is not a UUID"))?),
                text: body.text,
                state: body.state,
                provenance: body.provenance,
                created_at: created,
            })
        }).await
    }

    /// Forget an approved memory: marks it `forgotten`. It is no longer
    /// retrievable but the record survives as an audit trail.
    pub async fn forget(&self, memory_id: &str) -> Result<MemoryRecord> {
        let memory_id = memory_id.to_string();
        self.store.with_transaction(move |tx| {
            let row: Option<(String, String, i64, String)> = tx.query_row(
                "SELECT id, project_id, CAST(created_at AS INTEGER) as created_at, json_extract(body,'$.state') FROM memory WHERE id=?1",
                [memory_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            ).optional()?;
            let Some((record_id, project_str, created, state)) = row else {
                return Err(CoreError::new("memory_not_found", "memory is not present in this project"));
            };
            if state != "approved" {
                return Err(CoreError::new("invalid_memory_state", "only approved memories can be forgotten"));
            }
            let body_json: String = tx.query_row("SELECT body FROM memory WHERE id=?1", [record_id.clone()], |r| r.get(0))?;
            let mut body: MemoryBody = serde_json::from_str(&body_json)
                .map_err(|_| CoreError::new("storage_corrupt", "memory body is not valid JSON"))?;
            body.state = "forgotten".into();
            body.decision = Some("forget".into());
            let updated = serde_json::to_string(&body)?;
            tx.execute("UPDATE memory SET body=?1 WHERE id=?2", params![updated, record_id])?;
            Ok(MemoryRecord {
                id: record_id,
                project_id: ProjectId(uuid::Uuid::parse_str(&project_str)
                    .map_err(|_| CoreError::new("storage_corrupt", "memory project id is not a UUID"))?),
                text: body.text,
                state: body.state,
                provenance: body.provenance,
                created_at: created,
            })
        }).await
    }

    /// Retrieve approved memories for a project matching the query terms.
    /// Candidates, rejected and forgotten records are excluded. Returns text
    /// excerpts only — these are context, never instructions to dispatch.
    pub async fn retrieve_memories(&self, project: ProjectId, query: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        let project_str = project.to_string();
        let limit = limit.max(1).min(64);
        let terms = fts_terms(query);
        let store = self.store.clone();
        store.with_connection(move |conn| {
            let rows: Vec<(String, String, i64, String, String, String)> = if terms.is_empty() {
                // No indexable terms: return recent approved (bounded), no filter.
                let mut stmt = conn.prepare(
                    "SELECT id, project_id, created_at, json_extract(body,'$.text'), provenance, body FROM memory
                     WHERE project_id=?1 AND json_extract(body,'$.state')='approved'
                     ORDER BY created_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![project_str, limit as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?;
                let mut out = Vec::new();
                for r in rows { out.push(r?); }
                out
            } else {
                // LIKE match across the query terms against approved text.
                let like = format!("%{}%", terms.join("%"));
                let mut stmt = conn.prepare(
                    "SELECT id, project_id, created_at, json_extract(body,'$.text'), provenance, body FROM memory
                     WHERE project_id=?1 AND json_extract(body,'$.state')='approved'
                       AND (json_extract(body,'$.text') LIKE ?2 OR json_extract(body,'$.provenance') LIKE ?2)
                     ORDER BY created_at DESC LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![project_str, like, limit as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?;
                let mut out = Vec::new();
                for r in rows { out.push(r?); }
                out
            };
            Ok(rows)
        }).await.map(|rows| {
            rows.into_iter().filter_map(|(id, project_id, created, text, provenance, _body)| {
                let pid = uuid::Uuid::parse_str(&project_id).ok()?;
                Some(MemoryRecord {
                    id,
                    project_id: ProjectId(pid),
                    text,
                    state: "approved".into(),
                    provenance,
                    created_at: created,
                })
            }).collect()
        })
    }
}

/// Whitespace-split query terms for LIKE matching (no SQL injection: bound).
fn fts_terms(query: &str) -> Vec<String> {
    query.split_whitespace()
        .map(|t| t.to_lowercase())
        .take(16)
        .collect()
}

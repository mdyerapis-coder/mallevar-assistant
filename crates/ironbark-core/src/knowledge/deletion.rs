//! P4 deletion. Deleting a source removes its ACTIVE chunks + vectors +
//! revision content, scoped to the owning project. The source row is kept as a
//! tombstone so historical citations resolve an explicit `Deleted` (never a
//! silent current-revision substitution, never `Missing`).

use rusqlite::OptionalExtension;
use rusqlite::params;
use crate::contracts::{DeletionReceipt, ProjectId};
use crate::error::{CoreError, Result};
use super::{KnowledgeService, now_ms};

impl KnowledgeService {
    /// Delete a source's active knowledge under its owning project. The source
    /// row is kept as a tombstone so historical citations resolve an explicit
    /// `Deleted` rather than a silent current-revision substitution.
    pub async fn delete_source(&self, project: ProjectId, source_id: String) -> Result<DeletionReceipt> {
        let project_str = project.to_string();
        self.store.with_transaction(move |tx| {
            let owner: Option<String> = tx.query_row(
                "SELECT project_id FROM sources WHERE id=?1",
                [source_id.clone()],
                |row| row.get(0),
            ).optional()?;
            match owner {
                Some(o) if o == project_str => delete_one(tx, &source_id),
                Some(_) => Err(CoreError::new("forbidden_source", "source is not owned by this project")),
                None => Err(CoreError::new("source_not_found", "source is not present in this project")),
            }
        }).await
    }

    /// Delete every source in a project (cascades chunks, vectors, revisions).
    pub async fn delete_project(&self, project: ProjectId) -> Result<DeletionReceipt> {
        let project_str = project.to_string();
        self.store.with_transaction(move |tx| {
            let sources: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM sources WHERE project_id=?1")?;
                let rows = stmt.query_map([project_str.clone()], |r| r.get::<_, String>(0))?;
                let mut ids = Vec::new();
                for r in rows { ids.push(r?); }
                ids
            };
            let mut receipt = DeletionReceipt { source_id: project_str, chunks_removed: 0, vectors_removed: 0, revisions_removed: 0, applied_at: now_ms() };
            for id in sources {
                let r = delete_one(tx, &id)?;
                receipt.chunks_removed += r.chunks_removed;
                receipt.vectors_removed += r.vectors_removed;
                receipt.revisions_removed += r.revisions_removed;
            }
            Ok(receipt)
        }).await
    }
}

/// Remove one source's active knowledge inside the caller's transaction.
fn delete_one(tx: &rusqlite::Transaction<'_>, source_id: &str) -> Result<DeletionReceipt> {
    let revision_ids: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM source_revisions WHERE source_id=?1")?;
        let rows = stmt.query_map([source_id], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for r in rows { ids.push(r?); }
        ids
    };
    let mut chunk_ids: Vec<String> = Vec::new();
    for rev in &revision_ids {
        // Every chunk of the revision, including the metadata sentinel: the
        // revision row itself is deleted below, and `chunks.revision_id`
        // references it, so leaving the sentinel behind would break the
        // foreign key and the whole deletion would fail.
        let mut stmt = tx.prepare("SELECT id FROM chunks WHERE revision_id=?1")?;
        let rows = stmt.query_map(params![rev], |r| r.get::<_, String>(0))?;
        for r in rows { chunk_ids.push(r?); }
    }
    // Vectors first (they reference chunks), then chunks, then FTS, then the
    // revision rows. The source row itself is kept as a tombstone.
    let vectors_removed = if chunk_ids.is_empty() {
        0
    } else {
        let mut n = 0usize;
        for chunk in &chunk_ids {
            let d: i64 = tx.query_row("SELECT count(*) FROM embeddings WHERE chunk_id=?1", [chunk], |r| r.get(0))?;
            tx.execute("DELETE FROM embeddings WHERE chunk_id=?1", [chunk])?;
            n += d as usize;
        }
        n
    };
    let mut chunks_removed = 0usize;
    for chunk in &chunk_ids {
        super::chunks::delete_chunk(tx, chunk)?;
        chunks_removed += 1;
    }
    let revisions_removed = if revision_ids.is_empty() {
        0
    } else {
        let ids: Vec<&str> = revision_ids.iter().map(|s| s.as_str()).collect();
        // A bounded, explicit delete list.
        let mut n = 0usize;
        for id in &ids {
            tx.execute("DELETE FROM source_revisions WHERE id=?1", [*id])?;
            n += 1;
        }
        n
    };
    Ok(DeletionReceipt {
        source_id: source_id.to_string(),
        chunks_removed,
        vectors_removed,
        revisions_removed,
        applied_at: now_ms(),
    })
}

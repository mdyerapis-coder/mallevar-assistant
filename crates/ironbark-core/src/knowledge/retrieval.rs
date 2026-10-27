//! P4 retrieval + citation opening. `KnowledgeService::retrieve` applies
//! project + access + disclosure filtering in SQL BEFORE bm25 ranking, so a
//! LocalOnly chunk can never reach a cloud route or a foreign project's
//! context. `CitationResolver::open` maps a citation back to the exact stored
//! unit, or an explicit `Deleted`/`Missing` — never a silent current-revision
//! substitution.

use rusqlite::{params, OptionalExtension};
use crate::contracts::{Citation, CitationView, CitedExcerpt, ProjectId, RoutePin};
use crate::error::Result;
use super::chunks::{self, ensure_fts};
use super::locators;
use super::{CitationResolver, KnowledgeService, require_policy};

impl KnowledgeService {
    /// Lexical retrieval: disclosure + project + access filtering in SQL, then
    /// bm25 rank, then char-budget packing that retains every locator. The
    /// route is revalidated against the freshly-read policy immediately before
    /// ranking so a stale route cannot widen the result.
    pub async fn retrieve(&self, project: ProjectId, query: &str, limit: usize) -> Result<Vec<CitedExcerpt>> {
        let policy = require_policy(&self.store, project).await?;
        let policy = policy.clone();
        let query = query.to_string();
        let limit = limit.max(1).min(64);
        let ranked = self.store.with_connection(move |conn| {
            ensure_fts(conn)?;
            chunks::rank_lexical(conn, project, &RoutePin { connection_id: "local".into(), model_id: "local".into(), harness: None }, &policy, &query, limit)
        }).await?;
        chunks::cites_from_ranked(&ranked, project)
    }

    /// Project-scoped citation open: a source that does not belong to
    /// `project` resolves `Missing` (it is foreign), a belonging-but-gone
    /// revision resolves `Deleted`, an intact unit resolves `Content`.
    pub async fn open_citation_in(&self, project: ProjectId, citation: &Citation) -> Result<CitationView> {
        CitationResolver::new(self.store.clone()).open(project, citation).await
    }
}

impl CitationResolver {
    /// Open a citation to the exact stored unit, or an explicit
    /// `Deleted`/`Missing`. `Content` carries only text; the locator lives on
    /// the `Citation`. The source row survives deletion as a tombstone, so a
    /// gone revision resolves `Deleted`, a source belonging to another project
    /// resolves `Missing`, and a source that never existed resolves `Missing`
    /// — never a current-revision substitution.
    pub async fn open(&self, project: ProjectId, citation: &Citation) -> Result<CitationView> {
        let locator_key = locators::locator_key(&citation.locator);
        let source_id = citation.source_id.clone();
        let digest = citation.revision_digest.as_str().to_string();
        let project_str = project.to_string();
        let store = self.store.clone();
        store.with_connection(move |conn| {
            let source: Option<(String, String)> = conn.query_row(
                "SELECT id, project_id FROM sources WHERE id=?1",
                [source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            let Some((source_id, source_project)) = source else {
                return Ok(CitationView::Missing);
            };
            if source_project != project_str {
                return Ok(CitationView::Missing);
            }
            Ok(open_revision(conn, &source_id, &digest, &locator_key))
        }).await
    }
}

/// Shared revision lookup: revision by digest (tombstone => Deleted), then the
/// exact chunk by locator.
fn open_revision(conn: &rusqlite::Connection, source_id: &str, digest: &str, locator_key: &str) -> CitationView {
    let revision_id: Option<String> = conn.query_row(
        "SELECT id FROM source_revisions WHERE source_id=?1 AND digest=?2",
        params![source_id, digest],
        |row| row.get(0),
    ).optional().unwrap_or(None);
    let Some(revision_id) = revision_id else {
        return CitationView::Deleted;
    };
    let content: Option<String> = conn.query_row(
        "SELECT content FROM chunks WHERE revision_id=?1 AND locator=?2",
        params![revision_id, locator_key],
        |row| row.get(0),
    ).optional().unwrap_or(None);
    match content {
        Some(text) => CitationView::Content { text },
        None => CitationView::Missing,
    }
}
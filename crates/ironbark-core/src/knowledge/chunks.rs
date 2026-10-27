//! Chunk storage + the FTS5 mirror. The mirror is a standalone FTS5 table the
//! knowledge module owns (the base migration is immutable here): it stores the
//! content plus an UNINDEXED disclosure column so project + disclosure
//! filtering happens in SQL BEFORE the bm25 rank is applied. Deletion removes
//! from both the base table and the mirror in the same transaction, so a
//! retrieval never sees an unfilterable orphan.

use rusqlite::{params, Connection};
use crate::contracts::{Citation, CitationLocator, CitedExcerpt, DisclosurePolicy, Digest, ProjectId, ProjectPolicy, RoutePin};
use super::RetrievalExcerpt;
use crate::error::{CoreError, Result};
use super::locators;

/// The FTS5 mirror table. Created idempotently on first use.
pub const FTS_TABLE: &str = "knowledge_fts";

/// Idempotently create the standalone FTS5 mirror. Runs inside the caller's
/// transaction so a failed publish leaves no partial index rows.
pub fn ensure_fts(conn: &Connection) -> Result<()> {
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [FTS_TABLE],
        |row| row.get(0),
    )?;
    if exists == 0 {
        conn.execute_batch(
            "CREATE VIRTUAL TABLE knowledge_fts USING fts5(
                chunk_id UNINDEXED,
                project_id UNINDEXED,
                disclosure UNINDEXED,
                content,
                tokenize = 'unicode61'
            );",
        )?;
    }
    Ok(())
}

/// Deterministic chunk identity: a source revision + its locator never change,
/// so the chunk id is stable across re-ingestion and deletions. FNV-1a keeps
/// this allocation-free and reproducible.
pub fn chunk_id(revision_id: &str, locator_key: &str) -> String {
    let mut h = 0xcbf29ce484222325u64;
    for b in locator_key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{revision_id}-{h:016x}")
}

/// Insert one extracted unit as a chunk (and FTS row) for a revision. Returns
/// the chunk id. The caller has already run `ensure_fts` in this transaction.
/// `disclosure` is the full serde JSON of the policy for this source, so the
/// retrieval SQL can test membership without a second join.
pub fn insert_chunk(
    conn: &Connection,
    revision_id: &str,
    project_id: &str,
    disclosure: &str,
    unit: &super::extractors::ExtractedUnit,
) -> Result<String> {
    let id = chunk_id(revision_id, &unit.locator_key);
    conn.execute(
        "INSERT OR IGNORE INTO chunks (id, revision_id, locator, content) VALUES (?1, ?2, ?3, ?4)",
        params![id, revision_id, unit.locator_key, unit.content],
    )?;
    conn.execute(
        "INSERT INTO knowledge_fts (chunk_id, project_id, disclosure, content) VALUES (?1, ?2, ?3, ?4)",
        params![id, project_id, disclosure, unit.content],
    )?;
    Ok(id)
}

/// Serialise a disclosure policy into its stored JSON form (the exact shape
/// the FTS mirror and retrieval filter both use).
pub fn disclosure_json(disclosure: &DisclosurePolicy) -> String {
    serde_json::to_string(disclosure).unwrap_or_else(|_| "{\"type\":\"local_only\"}".into())
}

/// Remove a chunk from both the base table and the FTS mirror by id.
pub fn delete_chunk(conn: &Connection, chunk_id: &str) -> Result<()> {
    conn.execute("DELETE FROM knowledge_fts WHERE chunk_id = ?1", [chunk_id])?;
    conn.execute("DELETE FROM chunks WHERE id = ?1", [chunk_id])?;
    Ok(())
}

/// A stored chunk with its metadata, ready to rank.
pub struct RankedChunk {
    pub id: String,
    pub revision_id: String,
    pub source_id: String,
    pub digest: String,
    pub locator: String,
    pub content: String,
    pub disclosure: String,
    pub project_id: String,
    pub rank: f64,
}

impl RankedChunk {
    pub fn locator(&self) -> Result<CitationLocator> {
        serde_json::from_str(&self.locator)
            .map_err(|_| CoreError::new("storage_corrupt", "chunk locator is not valid JSON"))
    }
    /// Rebuild the disclosure policy from its stored JSON.
    pub fn disclosure_policy(&self) -> Result<DisclosurePolicy> {
        serde_json::from_str(&self.disclosure)
            .map_err(|_| CoreError::new("storage_corrupt", "chunk disclosure is not valid JSON"))
    }
}

/// Build the FTS5 match expression for a free-text query: whitespace-split
/// terms, each double-quoted so user punctuation cannot inject FTS syntax.
/// Bounded to a term cap so a giant query cannot explode the index scan.
pub fn fts_query(query: &str) -> Option<String> {
    let mut terms = Vec::new();
    for term in query.split_whitespace() {
        let quoted = term.replace('"', "\"\"");
        if quoted.is_empty() { continue; }
        terms.push(format!("\"{quoted}\""));
        if terms.len() >= 24 { break; }
    }
    if terms.is_empty() { return None; }
    Some(terms.join(" "))
}

/// Lexical retrieval: project + disclosure filter in SQL, bm25 rank AFTER.
/// The caller has already verified `route` against `policy`; the filter here
/// additionally applies each source's stored disclosure policy per route, so a
/// LocalOnly chunk is never handed to the ranker under a route that cannot see
/// it. `candidate_vector_ids` shares the same WHERE shape so the two rank
/// paths cannot drift on the security boundary.
pub fn rank_lexical(
    conn: &Connection,
    project: ProjectId,
    route: &RoutePin,
    policy: &ProjectPolicy,
    query: &str,
    limit: usize,
) -> Result<Vec<RankedChunk>> {
    let match_expr = fts_query(query).ok_or_else(|| CoreError::new("invalid_query", "query has no indexable terms"))?;
    let route_local = if route_is_local(route, policy) { 1 } else { 0 };
    let project_str = project.to_string();
    let conn_id = route.connection_id.clone();
    let limit = (limit.max(1).min(64)) as i64;
    let stmt_sql = format!(
        "SELECT f.chunk_id, c.revision_id, sr.source_id, sr.digest, c.locator, c.content, f.disclosure, f.project_id, bm25(knowledge_fts)
         FROM {FTS_TABLE} f
         JOIN chunks c ON c.id = f.chunk_id
         JOIN source_revisions sr ON sr.id = c.revision_id
         JOIN sources s ON s.id = sr.source_id
         WHERE f.project_id = ?1
           AND s.project_id = ?2
           AND c.locator != ?3
           AND (
                json_extract(f.disclosure, '$.type') = 'explicit_public'
             OR (json_extract(f.disclosure, '$.type') = 'local_only' AND ?4 = 1)
             OR (json_extract(f.disclosure, '$.type') = 'allowed_connections' AND EXISTS (
                     SELECT 1 FROM json_each(f.disclosure, '$.connection_ids') WHERE json_each.value = ?5
                 ))
           )
           AND knowledge_fts MATCH ?6
         ORDER BY bm25(knowledge_fts)
         LIMIT ?7",
    );
    let mut stmt = conn.prepare(&stmt_sql)?;
    let mut rows = stmt.query(params![
        project_str,
        project_str,
        locators::META_LOCATOR,
        route_local,
        conn_id,
        match_expr,
        limit,
    ])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(RankedChunk {
            id: row.get(0)?,
            revision_id: row.get(1)?,
            source_id: row.get(2)?,
            digest: row.get(3)?,
            locator: row.get(4)?,
            content: row.get(5)?,
            disclosure: row.get(6)?,
            project_id: row.get(7)?,
            rank: row.get(8)?,
        });
    }
    Ok(out)
}

/// Candidate chunk ids a route may consider (project + disclosure filter only,
/// no FTS match). Vector ranking scores these, then falls back to lexical if
/// the vector index is empty for the model.
pub fn candidate_vector_ids(
    conn: &Connection,
    project: ProjectId,
    route: &RoutePin,
    policy: &ProjectPolicy,
    limit: usize,
) -> Result<Vec<String>> {
    let route_local = if route_is_local(route, policy) { 1 } else { 0 };
    let project_str = project.to_string();
    let conn_id = route.connection_id.clone();
    let limit = (limit.max(1).min(512)) as i64;
    let stmt_sql = format!(
        "SELECT c.id FROM chunks c
         JOIN knowledge_fts f ON f.chunk_id = c.id
         JOIN source_revisions sr ON sr.id = c.revision_id
         JOIN sources s ON s.id = sr.source_id
         WHERE s.project_id = ?1
           AND f.project_id = ?2
           AND c.locator != ?3
           AND (
                json_extract(f.disclosure, '$.type') = 'explicit_public'
             OR (json_extract(f.disclosure, '$.type') = 'local_only' AND ?4 = 1)
             OR (json_extract(f.disclosure, '$.type') = 'allowed_connections' AND EXISTS (
                     SELECT 1 FROM json_each(f.disclosure, '$.connection_ids') WHERE json_each.value = ?5
                 ))
           )
         LIMIT ?6",
    );
    let mut stmt = conn.prepare(&stmt_sql)?;
    let rows = stmt.query_map(params![
        project_str,
        project_str,
        locators::META_LOCATOR,
        route_local,
        conn_id,
        limit,
    ], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows { out.push(r?); }
    Ok(out)
}

/// A route is local if the committed policy is local_only OR the pinned
/// connection is the canonical local one. Cloud routes (provider endpoints)
/// are the opposite and must never see local_only chunks.
pub fn route_is_local(route: &RoutePin, policy: &ProjectPolicy) -> bool {
    if policy.local_only { return true; }
    route.connection_id == "local"
}

/// Per-excerpt char cap; the locator is always retained, only the text trims.
pub const PER_EXCERPT_CHARS: usize = 1200;

/// Truncate to `budget` chars without breaking a UTF-8 boundary.
pub fn pack_excerpt(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget { return text.to_string(); }
    let mut out = String::with_capacity(budget);
    for (i, c) in text.chars().enumerate() {
        if i >= budget { break; }
        out.push(c);
    }
    out
}

/// Build `CitedExcerpt` values from ranked chunks, retaining the locator +
/// disclosure of every chunk (only the text is char-budgeted).
pub fn cites_from_ranked(ranked: &[RankedChunk], project: ProjectId) -> Result<Vec<CitedExcerpt>> {
    let mut out = Vec::with_capacity(ranked.len());
    for c in ranked {
        let loc = c.locator()?;
        let dg = Digest::parse(&c.digest)?;
        let disc = c.disclosure_policy()?;
        out.push(CitedExcerpt {
            text: pack_excerpt(&c.content, PER_EXCERPT_CHARS),
            citation: Citation { source_id: c.source_id.clone(), revision_digest: dg, locator: loc },
            project_id: project,
            disclosure: disc,
        });
    }
    Ok(out)
}

/// Wrap ranked chunks into `RetrievalExcerpt`s labelled as lexical
/// (`lexical = true`, `rank = chunk.rank`). Disclosure already filtered them,
/// so they are safe to surface under either route.
pub fn retrieval_excerpts_from_ranked(ranked: Vec<RankedChunk>, project: ProjectId) -> Result<Vec<RetrievalExcerpt>> {
    let mut out = Vec::with_capacity(ranked.len());
    for c in ranked {
        let loc = c.locator()?;
        let dg = Digest::parse(&c.digest)?;
        let disc = c.disclosure_policy()?;
        let excerpt = CitedExcerpt {
            text: pack_excerpt(&c.content, PER_EXCERPT_CHARS),
            citation: Citation { source_id: c.source_id.clone(), revision_digest: dg, locator: loc },
            project_id: project,
            disclosure: disc,
        };
        out.push(RetrievalExcerpt { rank: c.rank, lexical: true, excerpt });
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_terms_and_caps() {
        let q = fts_query("don't \"panic\"  a  b c d e f g h i j k l m n o p q r s t u v w x").unwrap();
        assert!(q.starts_with("\"don't\" \"\"\"panic\"\"\""));
        // term cap: at most 24 terms
        assert!(q.split_whitespace().count() <= 24);
        assert!(fts_query("   ").is_none());
        assert!(fts_query("").is_none());
    }

    #[tokio::test]
    async fn fts_filter_runs_before_rank_and_deletes_both_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("t.db")).await.unwrap();
        let project = crate::contracts::ProjectId::new();
        let route = RoutePin { connection_id: "local".into(), model_id: "m".into(), harness: None };
        let policy = ProjectPolicy {
            version: 1, project_id: project,
            allowed_connection_ids: vec!["local".into(), "cloud".into()],
            allowed_tools: vec![], read_roots: vec![], write_roots: vec![],
            network_destinations: vec![], local_only: false,
            max_workers: 1, max_requests: 1, max_cost_microusd: None,
            spend_policy: crate::contracts::SpendPolicy::FreeOnly, allow_unknown_cost: false,
        };
        let project_str = project.to_string();
        store.with_connection(move |conn| {
            ensure_fts(conn)?;
            let project_str = project.to_string();
            conn.execute("INSERT INTO projects (id, name, root, created_at) VALUES (?1, 'test', '/tmp', 0)", [&project_str])?;
            conn.execute("INSERT INTO sources (id, project_id, uri) VALUES ('s1', ?1, 'u1')", [&project_str])?;
            conn.execute("INSERT INTO source_revisions (id, source_id, digest, created_at) VALUES ('r1', 's1', 'd1', 0)", [])?;
            let local_only = DisclosurePolicy::LocalOnly;
            let public = DisclosurePolicy::ExplicitPublic;
            let unit = |k: &str, t: &str| super::super::extractors::ExtractedUnit { locator_key: k.into(), content: t.into() };
            let private_id = insert_chunk(conn, "r1", &project_str, &disclosure_json(&local_only), &unit("L1", "zeta sentinel"))?;
            let public_id = insert_chunk(conn, "r1", &project_str, &disclosure_json(&public), &unit("L2", "zeta public"))?;
            // A cloud route must NOT see the local_only chunk, even though it
            // matches the query; the public one is visible.
            let cloud = RoutePin { connection_id: "cloud".into(), model_id: "m".into(), harness: None };
            let hits = rank_lexical(conn, project, &cloud, &policy, "zeta", 10)?;
            assert_eq!(hits.len(), 1, "cloud route must exclude local_only content");
            assert_eq!(hits[0].id, public_id);
            // A local route sees both, ranked by bm25.
            let hits = rank_lexical(conn, project, &route, &policy, "zeta", 10)?;
            assert_eq!(hits.len(), 2);
            // Deletion removes base + FTS in one call.
            delete_chunk(conn, &private_id)?;
            let gone: i64 = conn.query_row("SELECT count(*) FROM chunks WHERE id=?1", [&private_id], |r| r.get(0))?;
            let gone_fts: i64 = conn.query_row("SELECT count(*) FROM knowledge_fts WHERE chunk_id=?1", [&private_id], |r| r.get(0))?;
            assert_eq!(gone, 0);
            assert_eq!(gone_fts, 0);
            Ok(())
        }).await.unwrap();
    }
}

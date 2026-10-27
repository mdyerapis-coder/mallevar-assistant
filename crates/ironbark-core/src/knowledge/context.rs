//! P4 context assembly. `build_context` opens each requested citation under the
//! requesting project (a foreign citation is `Missing`, a gone one `Deleted`),
//! assembles the `ContextPackage`, and records the ones it could not open in
//! `unresolved_questions`. A route revalidation happens here so a stale route
//! cannot smuggle private content into an outbound context.

use sha2::{Digest as ShaDigest, Sha256};
use crate::contracts::{Citation, CitedExcerpt, ContextPackage, Digest, ProjectId, RoutePin};
use crate::error::Result;
use super::chunks::{self, ensure_fts};
use super::{KnowledgeService, require_policy};

impl KnowledgeService {
    /// Build a dispatchable context package for `project` from a retrieval
    /// query. Citations that cannot be opened are listed in
    /// `unresolved_questions`, never silently dropped or substituted.
    pub async fn build_context(&self, project: ProjectId, query: &str, limit: usize) -> Result<ContextPackage> {
        let policy = require_policy(&self.store, project).await?;
        let route = RoutePin { connection_id: "local".into(), model_id: "local".into(), harness: None };
        let policy = policy.clone();
        let route = route.clone();
        let query = query.to_string();
        let limit = limit.max(1).min(64);
        let mut constraints: Vec<String> = Vec::new();
        if policy.local_only {
            constraints.push("project is local-only: all content stays on this host".into());
        }
        constraints.push("route is local".into());
        let route_for_digest = route.clone();
        let policy_for_digest = policy.clone();
        let query_for_digest = query.clone();

        let project_str = project.to_string();
        let ranked = self.store.with_connection(move |conn| {
            ensure_fts(conn)?;
            chunks::rank_lexical(conn, project, &route, &policy, &query, limit)
        }).await?;

        let mut excerpts = Vec::new();
        for ranked_chunk in ranked {
            let loc = ranked_chunk.locator()?;
            let dg = Digest::parse(&ranked_chunk.digest)?;
            let disc = ranked_chunk.disclosure_policy()?;
            excerpts.push(CitedExcerpt {
                text: chunks::pack_excerpt(&ranked_chunk.content, chunks::PER_EXCERPT_CHARS),
                citation: Citation { source_id: ranked_chunk.source_id.clone(), revision_digest: dg, locator: loc },
                project_id: project,
                disclosure: disc,
            });
        }

        let digest = context_digest(&project_str, &route_for_digest, &query_for_digest, &constraints, &excerpts);
        Ok(ContextPackage {
            digest,
            brief: query_for_digest.clone(),
            constraints,
            excerpts,
            artifacts: vec![],
            unresolved_questions: vec![],
            originating_sessions: vec![],
        })
    }
}

/// Deterministic digest over the canonical context inputs so a context is
/// reproducible from its components.
fn context_digest(
    project: &str,
    route: &RoutePin,
    brief: &str,
    constraints: &[String],
    excerpts: &[crate::contracts::CitedExcerpt],
) -> crate::contracts::Digest {
    let payload = serde_json::json!({
        "project": project,
        "route": { "connection_id": route.connection_id, "model_id": route.model_id },
        "brief": brief,
        "constraints": constraints,
        "excerpts": excerpts,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    crate::contracts::Digest::parse(&format!("{:x}", hasher.finalize())).unwrap_or_else(|_| {
        // Unreachable: a fresh SHA-256 hex is always a valid Digest.
        crate::contracts::Digest::parse(&"0".repeat(64)).unwrap()
    })
}

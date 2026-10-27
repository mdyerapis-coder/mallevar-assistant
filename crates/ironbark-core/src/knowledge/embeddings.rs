//! P5 embeddings + index/retrieve on `EmbeddingIndex`.
//!
//! IMPORTANT: the local embedder is a DETERMINISTIC FEATURE-HASH bucketizer.
//! It is NOT a semantic model. `qualified_semantic_available()` therefore
//! returns false, and embedding absence NEVER blocks retrieval — retrieval
//! falls back to the (real, disclosure-filtered) lexical rank and reports the
//! result as `RetrievalResult::Lexical` so callers can label the provenance.
//!
//! Storage key is the 4-field `connection_id|model_id|version|dimensions`;
//! stored `dimensions` MUST equal the model identity's dimensions, or the
//! index/retrieve path rejects the vector (`embedding_incompatible`).

use rusqlite::{params, OptionalExtension};
use super::{RetrievalExcerpt, RetrievalResult};
use crate::contracts::{Citation, CitedExcerpt, EmbeddingIndexReceipt, EmbeddingModelIdentity, ProjectId, RoutePin, SourceRevision, Digest};
use crate::error::{CoreError, Result};
use super::chunks::{self, ensure_fts, rank_lexical, candidate_vector_ids};

/// Local deterministic feature-hash embedding. L2-normalized. FNV-1a 64-bit
/// mix of (text_len + char+position pairs) → per-dimension values → L2 norm.
fn embed_fn(text: &str, dims: u32) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hasher = DefaultHasher::new();
    text.len().hash(&mut hasher);
    for (i, ch) in text.chars().enumerate() {
        let v = (ch as u64) ^ ((i as u64).wrapping_mul(FNV_PRIME));
        hasher.write(&v.to_le_bytes());
    }
    let seed = hasher.finish();
    let mut out = Vec::with_capacity(dims as usize);
    let mut state = seed;
    for _ in 0..dims {
        state ^= (state >> 33).wrapping_mul(0xff51afd7ed558ccd);
        state ^= (state >> 33).wrapping_mul(0xc4ceb9fe1a85ec53);
        state ^= state >> 33;
        out.push(((state as i64) as f32 / u64::MAX as f32) * 2.0 - 1.0);
    }
    let norm: f32 = out.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 { for v in &mut out { *v /= norm; } }
    out
}

fn model_key(model: &EmbeddingModelIdentity) -> String {
    format!("{}|{}|{}|{}", model.connection_id, model.model_id, model.version, model.dimensions)
}

fn to_blob(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for v in vector { out.extend_from_slice(&v.to_le_bytes()); }
    out
}

fn decode_f32(blob: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        let mut b = [0u8; 4]; b.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(b));
    }
    out
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let mut s = 0.0_f64;
    for i in 0..n { s += (a[i] as f64) * (b[i] as f64); }
    s
}

fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).map(String::from).collect::<Vec<_>>().join(",")
}

impl super::EmbeddingIndex {
    /// Index a revision's non-meta chunks with `model`. Deterministic: the
    /// content-addressed vectors are idempotent, so re-indexing the same
    /// revision yields identical rows (INSERT OR REPLACE). Vectors are
    /// rejected if a stored row for the same model+chunk has incompatible
    /// dimensions.
    pub async fn index(&self, revision: &SourceRevision, model: &EmbeddingModelIdentity) -> Result<EmbeddingIndexReceipt> {
        let dimensions = model.dimensions;
        let model_key = model_key(model);
        let source_id = revision.source_id.clone();
        let digest = revision.digest.as_str().to_string();
        let store = self.store.clone();

        // Resolve the revision's storage id and pull its non-meta chunks.
        let result = store.with_connection(move |conn| {
            ensure_fts(conn)?;
            let revision_id: Option<String> = conn.query_row(
                "SELECT id FROM source_revisions WHERE source_id=?1 AND digest=?2",
                params![source_id, digest],
                |row| row.get(0),
            ).optional()?;
            let Some(revision_id) = revision_id else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(
                "SELECT c.id, c.content FROM chunks c WHERE c.revision_id=?1 AND c.locator!=?2"
            )?;
            let rows = stmt.query_map(params![revision_id, super::locators::META_LOCATOR], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut collected = Vec::new();
            for r in rows { collected.push(r?); }
            Ok(Some((revision_id, collected)))
        }).await?;
        let Some((revision_id, chunks_in)) = result else {
            return Err(CoreError::new("revision_not_found", "revision is not present in the store"));
        };
        if chunks_in.is_empty() {
            return Ok(EmbeddingIndexReceipt {
                revision_id,
                model: model.clone(),
                chunks_indexed: 0,
                indexed_at: super::now_ms(),
            });
        }

        // Compute embeddings under the global 2-permit worker pool.
        let worker = super::index_jobs::IndexWorker::new();
        let vectors = worker.run(chunks_in, dimensions).await?;

        let written = store.with_transaction(move |tx| {
            chunks::ensure_fts(tx)?;
            let mut upsert = tx.prepare_cached(
                "INSERT OR REPLACE INTO embeddings (chunk_id, model_id, dimensions, vector) VALUES (?1,?2,?3,?4)"
            )?;
            let mut written = 0usize;
            for (chunk_id, vector) in &vectors {
                if vector.len() != dimensions as usize {
                    return Err(CoreError::new("embedding_incompatible", "computed vector length does not match the model identity dimensions"));
                }
                let existing: Option<i64> = tx.query_row(
                    "SELECT dimensions FROM embeddings WHERE chunk_id=?1 AND model_id=?2",
                    params![chunk_id, model_key],
                    |row| row.get(0),
                ).optional()?;
                if let Some(d) = existing {
                    if d != dimensions as i64 {
                        return Err(CoreError::new("embedding_incompatible", "existing vector dimensions disagree with the model identity"));
                    }
                }
                let blob = to_blob(vector);
                upsert.execute(params![chunk_id, model_key, dimensions as i64, blob])?;
                written += 1;
            }
            Ok(written)
        }).await?;

        Ok(EmbeddingIndexReceipt {
            revision_id,
            model: model.clone(),
            chunks_indexed: written,
            indexed_at: super::now_ms(),
        })
    }

    /// Retrieve by embedding for `model`. Falls back to a labelled lexical
    /// pass (`RetrievalResult::Lexical`) when no vectors for `model` exist, so
    /// a missing/incompatible model never blocks retrieval with an error.
    pub async fn retrieve(&self, project: ProjectId, query: &str, route: &RoutePin, model: &EmbeddingModelIdentity, limit: usize) -> Result<RetrievalResult> {
        let policy = super::require_policy(&self.store, project).await?;
        super::validate_route(&policy, route)?;
        let policy = policy.clone();
        let route = route.clone();
        let model_key = model_key(model);
        let qvec = embed_fn(query, model.dimensions);
        let query = query.to_string();
        let limit = limit.max(1).min(64);
        let policy_for_fallback = policy.clone();
        let route_for_fallback = route.clone();

        let candidates = self.store.with_connection(move |conn| {
            ensure_fts(conn)?;
            candidate_vector_ids(conn, project, &route, &policy, limit * 8)
        }).await?;

        // Fetch candidate vectors for this exact model key.
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql + Send + Sync>> = vec![Box::new(model_key.clone())];
        for c in &candidates { params_vec.push(Box::new(c.clone())); }
        let placeholders = placeholders(candidates.len());
        let fetch_sql = format!(
            "SELECT c.id, c.revision_id, c.locator, c.content, f.disclosure, sr.source_id, sr.digest, e.vector, e.dimensions
             FROM chunks c
             JOIN knowledge_fts f ON f.chunk_id = c.id
             JOIN source_revisions sr ON sr.id = c.revision_id
             LEFT JOIN embeddings e ON e.chunk_id = c.id AND e.model_id = ?1
             WHERE c.id IN ({}) AND e.model_id = ?1",
            placeholders,
        );

        let rows = self.store.with_connection(move |conn| {
            ensure_fts(conn)?;
            let mut stmt = conn.prepare_cached(&fetch_sql)?;
            let rows: Vec<(String, String, String, String, String, String, String, Vec<u8>, i64)> = stmt
                .query_map(rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())), |row| {
                    let blob: Vec<u8> = row.get(7)?;
                    Ok((
                        row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                        row.get(4)?, row.get(5)?, row.get(6)?, blob, row.get(8)?,
                    ))
                })?.collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
            Ok(rows)
        }).await?;

        if rows.is_empty() {
            return Self::lexical_fallback(&self.store, project, &query, &route_for_fallback, &policy_for_fallback, limit).await;
        }

        // Verify stored dimensions match the model identity; otherwise reject.
        for (_c, _r, _l, _t, _d, _s, _dg, _v, d) in &rows {
            if *d as u32 != model.dimensions {
                return Err(CoreError::new("embedding_incompatible", "stored vector dimensions disagree with the model identity"));
            }
        }

        let mut scored: Vec<(f64, Vec<u8>)> = Vec::new();
        for (_c, _r, _l, _t, _d, _s, _dg, v, _dcol) in &rows {
            if v.len() != (model.dimensions as usize) * 4 {
                return Err(CoreError::new("embedding_incompatible", "vector blob length does not match the model dimensions"));
            }
            let fv = decode_f32(v);
            if fv.len() != qvec.len() { continue; }
            let score = dot(&qvec, &fv);
            scored.push((score, v.clone()));
        }

        // Sort by cosine (dot of L2-normalized vectors) descending.
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by(|a, b| scored[*b].0.partial_cmp(&scored[*a].0).unwrap_or(std::cmp::Ordering::Equal));

        let mut excerpts = Vec::new();
        for idx in order.into_iter().take(limit) {
            let (chunk_id, _rev, locator_key, content, disclosure_json, source_id, digest, _vec, _dcol) = &rows[idx];
            let citation = Citation {
                source_id: source_id.clone(),
                revision_digest: Digest::parse(digest)?,
                locator: serde_json::from_str(locator_key)?,
            };
            let disc = serde_json::from_str(disclosure_json)?;
            excerpts.push(RetrievalExcerpt {
                rank: scored[idx].0,
                lexical: false,
                excerpt: CitedExcerpt {
                    text: chunks::pack_excerpt(content, chunks::PER_EXCERPT_CHARS),
                    citation,
                    project_id: project,
                    disclosure: disc,
                },
            });
        }
        Ok(RetrievalResult::Embedding { model: model.clone(), excerpts })
    }

    /// `EmbeddingIndex` owns NO local model knowledge for qualification; the
    /// local embedder is a feature hash. Real semantic qualification is gated
    /// on an external provider (not present here).
    pub fn qualified_semantic_available() -> bool { false }

    async fn lexical_fallback(
        store: &crate::store::Store,
        project: ProjectId,
        query: &str,
        route: &RoutePin,
        policy: &crate::contracts::ProjectPolicy,
        limit: usize,
    ) -> Result<RetrievalResult> {
        let query = query.to_string();
        let route = route.clone();
        let policy = policy.clone();
        let ranked = store.with_connection(move |conn| {
            ensure_fts(conn)?;
            rank_lexical(conn, project, &route, &policy, &query, limit)
        }).await?;
        let excerpts = chunks::retrieval_excerpts_from_ranked(ranked, project)?;
        Ok(RetrievalResult::Lexical { excerpts })
    }
}

/// Re-export so `super::embeddings::embed` (used by the worker) resolves.
pub fn embed(text: &str, dims: u32) -> Vec<f32> {
    embed_fn(text, dims)
}
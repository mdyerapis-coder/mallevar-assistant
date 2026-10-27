//! P3 immutable source ingestion.
//!
//! A revision is content-addressed and immutable: re-ingesting identical bytes
//! is idempotent (returns the existing revision), and new bytes publish a NEW
//! revision while the old one stays exactly as it was, so historical citations
//! keep resolving.
//!
//! Publish is atomic. Extraction + whole-extraction validation happen BEFORE
//! any durable write, and the revision row + every chunk row + every FTS mirror
//! row are committed in ONE transaction. An extraction failure (encrypted,
//! malformed, unavailable extractor, empty) returns before that transaction and
//! therefore publishes NO partial revision.
//!
//! A revision's digest is a retention root (`store::schema::StorageSchema::roots`),
//! so the bytes it names must be a resolvable content-addressed object for EVERY
//! input kind. Ingestion therefore publishes the resolved bytes into the project
//! CAS BEFORE the revision row is committed: a failed publish leaves no revision
//! referencing a missing object, and a missing object would make every backup
//! fail closed with `IntegrityMismatch`.

use std::io::{Cursor, Read as _};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest as ShaDigest, Sha256};
use uuid::Uuid;
use crate::contracts::{Digest, DisclosurePolicy, ProjectId, SourceInput, SourceRevision};
use crate::error::{CoreError, Result};
use crate::store::objects::{ObjectLimits, ObjectStore};
use super::chunks;
use super::extractors;
use super::{ensure_project, ensure_project_tx, latest_policy, now_ms, KnowledgeService};

impl KnowledgeService {
    /// The project-scoped content-addressed store rooted at the project CAS.
    fn object_store(&self, project: ProjectId) -> Result<ObjectStore> {
        ObjectStore::new(self.store.clone(), project, self.cas_root.clone(), ObjectLimits::default())
    }

    /// Publish the resolved source bytes into the project CAS. Every input kind
    /// (`File`, `Attachment`, `Fetched`) must leave the revision's digest behind
    /// as a content-addressed object, because that digest is a retention root.
    /// Publishing bytes the store already holds verifies the existing object
    /// instead of replacing it, so re-publishing identical bytes is idempotent.
    async fn publish_source_bytes(&self, project: ProjectId, bytes: Vec<u8>) -> Result<()> {
        // Publishing is a durable write for the named project: refuse an
        // unregistered project here — with the same typed error the revision
        // transaction raises — before staging any bytes.
        self.store.with_connection(move |conn| ensure_project(conn, project)).await?;
        self.object_store(project)?.put(Cursor::new(bytes), extractors::MAX_SOURCE_BYTES).await?;
        Ok(())
    }

    /// Resolve the source bytes + canonical identity for the input kind.
    /// `File` reads a local path; `Attachment` reads a project CAS artifact;
    /// `Fetched` performs a policy-authorized anonymous GET.
    async fn resolve_bytes(&self, project: ProjectId, input: &SourceInput) -> Result<(Vec<u8>, String)> {
        match input {
            SourceInput::File { path } => {
                let metadata = tokio::fs::metadata(path).await
                    .map_err(|_| CoreError::new("source_not_found", "source file is not readable on this host"))?;
                if !metadata.is_file() {
                    return Err(CoreError::new("invalid_source", "source path is not a regular file"));
                }
                if metadata.len() > extractors::MAX_SOURCE_BYTES {
                    return Err(CoreError::new(
                        "source_too_large",
                        format!("source is {} bytes; the hard limit is {} bytes", metadata.len(), extractors::MAX_SOURCE_BYTES),
                    ));
                }
                let bytes = tokio::fs::read(path).await?;
                Ok((bytes, path.clone()))
            }
            SourceInput::Attachment { artifact_id } => {
                let mut reader = self.object_store(project)?.get(*artifact_id).await?;
                if reader.bytes > extractors::MAX_SOURCE_BYTES {
                    return Err(CoreError::new("source_too_large", "attachment exceeds the ingestion size limit"));
                }
                let mut bytes = Vec::with_capacity(reader.bytes as usize);
                reader.read_to_end(&mut bytes)?;
                if bytes.len() as u64 != reader.bytes {
                    return Err(CoreError::new("integrity_mismatch", "attachment byte count differs from its CAS manifest"));
                }
                // The CAS digest is the content identity: stable and unambiguous.
                Ok((bytes, reader.digest.as_str().to_string()))
            }
            SourceInput::Fetched { url, receipt } => {
                let policy = latest_policy(&self.store, project).await?
                    .ok_or_else(|| CoreError::new("policy_missing", "project has no committed policy; the fetch is refused"))?;
                let bytes = authorized_fetch(url, &policy).await?;
                if bytes.len() as u64 > extractors::MAX_SOURCE_BYTES {
                    return Err(CoreError::new("source_too_large", "fetched source exceeds the ingestion size limit"));
                }
                // The receipt is the owner-attested digest of the fetch; the
                // fetched bytes must match it before we trust them.
                let actual = sha256_hex(&bytes);
                if actual != receipt.as_str() {
                    return Err(CoreError::new("integrity_mismatch", "fetched bytes do not match the attested fetch receipt"));
                }
                Ok((bytes, url.clone()))
            }
        }
    }

    /// Ingest a source under a project with a disclosure policy. Returns the
    /// (possibly pre-existing) immutable revision.
    pub async fn ingest(&self, project: ProjectId, input: SourceInput, disclosure: DisclosurePolicy) -> Result<SourceRevision> {
        // LocalOnly content may never ride a remote fetch: a `Fetched` source is
        // pulled over the network, so a LocalOnly disclosure is a policy
        // violation at ingest time, not something to defer to retrieval.
        if matches!(disclosure, DisclosurePolicy::LocalOnly) && matches!(&input, SourceInput::Fetched { .. }) {
            return Err(CoreError::new(
                "disclosure_conflict",
                "a LocalOnly source cannot be Fetched over the network; use File or Attachment for local-only content",
            ));
        }
        let (bytes, uri) = self.resolve_bytes(project, &input).await?;
        let name = filename_for(&uri);
        let digest = sha256_hex(&bytes);
        let digest_parsed = Digest::parse(&digest)?;
        // Extract + validate BEFORE any durable write. The worker hands the bytes
        // back so the publish below reuses the exact resolved buffer.
        let (extraction, bytes) = tokio::task::spawn_blocking(move || {
            Ok::<_, CoreError>((extractors::extract(&name, &bytes)?, bytes))
        })
        .await
        .map_err(|_| CoreError::new("extraction_worker_failed", "extraction worker panicked"))??;
        extractors::validate(&extraction)?;
        // The revision's digest must name a resolvable CAS object, so publish the
        // resolved bytes before committing the revision row. Ordering matters
        // both ways: a refused source never leaves an orphan object (extraction
        // ran first), and a failed publish never leaves a revision that
        // references a missing object.
        self.publish_source_bytes(project, bytes).await?;

        let store = self.store.clone();
        let project_str = project.to_string();
        let digest_str = digest.clone();
        let uri_str = uri.clone();
        let disclosure_json = chunks::disclosure_json(&disclosure);
        let revision = store.with_transaction(move |tx| {
            // Refuse operations on a project that is not registered.
            ensure_project_tx(tx, project)?;
            chunks::ensure_fts(tx)?;
            // Existing source row for this (project, uri)?
            let source_id: String = match tx.query_row(
                "SELECT id FROM sources WHERE project_id=?1 AND uri=?2",
                params![project_str, uri_str],
                |row| row.get(0),
            ).optional()? {
                Some(id) => id,
                None => {
                    let id = Uuid::new_v4().to_string();
                    tx.execute("INSERT INTO sources (id, project_id, uri) VALUES (?1,?2,?3)", params![id, project_str, uri_str])?;
                    id
                }
            };
            // Already-published revision with the same digest => idempotent.
            let existing_rev: Option<String> = tx.query_row(
                "SELECT id FROM source_revisions WHERE source_id=?1 AND digest=?2",
                params![source_id, digest_str],
                |row| row.get(0),
            ).optional()?;
            if let Some(rev_id) = existing_rev {
                return read_revision(tx, &rev_id, project);
            }
            // Publish a NEW revision atomically.
            let rev_id = Uuid::new_v4().to_string();
            let created = now_ms();
            tx.execute(
                "INSERT INTO source_revisions (id, source_id, digest, created_at) VALUES (?1,?2,?3,?4)",
                params![rev_id, source_id, digest_str, created],
            )?;
            for unit in &extraction.units {
                chunks::insert_chunk(tx, &rev_id, &project_str, &disclosure_json, unit)?;
            }
            let revision = SourceRevision {
                source_id: source_id.clone(),
                project_id: project,
                digest: digest_parsed,
                mime: extraction.mime.clone(),
                extractor_id: extraction.extractor_id.clone(),
                extractor_version: extraction.extractor_version.clone(),
                locators: extraction.units.len(),
                created_at: created,
            };
            // Revision extractor identity, recorded on a sentinel meta chunk so
            // the idempotent read path can return a faithful revision.
            let meta = RevisionMeta {
                mime: extraction.mime.clone(),
                extractor_id: extraction.extractor_id.clone(),
                extractor_version: extraction.extractor_version.clone(),
                locators: extraction.units.len(),
            };
            tx.execute(
                "INSERT OR REPLACE INTO chunks (id, revision_id, locator, content) VALUES (?1,?2,?3,?4)",
                params![chunks::chunk_id(&rev_id, super::locators::META_LOCATOR), rev_id, super::locators::META_LOCATOR, serde_json::to_string(&meta)?],
            )?;
            Ok(revision)
        }).await?;
        Ok(revision)
    }
}

/// Read a published revision's metadata back from the store (idempotent path).
fn read_revision(tx: &rusqlite::Transaction<'_>, rev_id: &str, project: ProjectId) -> Result<SourceRevision> {
    let (source_id, digest_str, created_at): (String, String, i64) = tx.query_row(
        "SELECT sr.source_id, sr.digest, sr.created_at
         FROM source_revisions sr WHERE sr.id=?1",
        [rev_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let meta: String = tx.query_row(
        "SELECT content FROM chunks WHERE revision_id=?1 AND locator=?2",
        params![rev_id, super::locators::META_LOCATOR],
        |row| row.get(0),
    ).optional()?
        .ok_or_else(|| CoreError::new("storage_corrupt", "revision metadata row is missing"))?;
    let meta: RevisionMeta = serde_json::from_str(&meta)
        .map_err(|_| CoreError::new("storage_corrupt", "revision metadata is not valid JSON"))?;
    Ok(SourceRevision {
        source_id,
        project_id: project,
        digest: Digest::parse(&digest_str)?,
        mime: meta.mime,
        extractor_id: meta.extractor_id,
        extractor_version: meta.extractor_version,
        locators: meta.locators,
        created_at,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RevisionMeta {
    mime: String,
    extractor_id: String,
    extractor_version: String,
    locators: usize,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn filename_for(uri: &str) -> String {
    uri.rsplit('/').next()
        .filter(|s| !s.is_empty())
        .map(|s| s.split('?').next().unwrap_or(s).to_string())
        .unwrap_or_else(|| "source".to_string())
}

/// Policy-authorized anonymous GET for a `Fetched` source. No credentials are
/// ever attached; the destination must be in the committed policy's
/// `network_destinations` and, for a local_only project, must be loopback.
async fn authorized_fetch(url: &str, policy: &crate::contracts::ProjectPolicy) -> Result<Vec<u8>> {
    if !crate::policy::routes::destination_allowed(url, &policy.network_destinations) {
        return Err(CoreError::new(
            "destination_not_approved",
            "fetched source URL is not in the committed policy's approved network destinations",
        ));
    }
    if policy.local_only && !crate::policy::routes::is_loopback_destination(url) {
        return Err(CoreError::new("local_only_route", "a local_only project may only fetch loopback destinations"));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| CoreError::new("fetch_client_failed", "cannot construct the bounded fetch client"))?;
    let response = client.get(url).send().await
        .map_err(|_| CoreError::new("fetch_failed", "fetched source could not be reached"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(CoreError::new("fetch_failed", format!("fetched source returned HTTP {status}")));
    }
    let bytes = response.bytes().await
        .map_err(|_| CoreError::new("fetch_failed", "fetched source body could not be read"))?;
    Ok(bytes.to_vec())
}

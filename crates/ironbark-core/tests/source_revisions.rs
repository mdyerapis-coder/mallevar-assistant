//! P3 invariants: source revisions are immutable and content-addressed.
//! - Re-ingesting identical bytes is idempotent (returns the existing revision).
//! - New bytes publish a NEW revision; an old citation still resolves to its
//!   exact historic text (never a silent current-revision substitution).
//! - A failed extraction publishes no revision and no chunk (no partial publish).
//! - An encrypted PDF is refused, not indexed as a blank success.

use ironbark_core::contracts::{
    ArtifactId, Citation, CitationLocator, CitationView, DisclosurePolicy, ProjectId, SourceInput,
};
use ironbark_core::error::CoreError;
use ironbark_core::knowledge::{locators, KnowledgeService};
use ironbark_core::store::objects::{ObjectLimits, ObjectStore};
use ironbark_core::store::Store;
use std::io::Read;
use tempfile::tempdir;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    project: ProjectId,
    service: KnowledgeService,
}

async fn setup() -> Fixture {
    let dir = tempdir().unwrap();
    let store = Store::open(&dir.path().join("core.db")).await.unwrap();
    let project = ProjectId::new();
    let root = dir.path().to_string_lossy().into_owned();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects (id,name,root,created_at) VALUES (?1,'p',?2,0)",
                rusqlite::params![project.to_string(), root],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let service = KnowledgeService::new(store.clone(), dir.path().join("objects"));
    Fixture { _dir: dir, store, project, service }
}

/// The locator of the first real content unit published for a revision.
async fn first_locator(store: &Store, source_id: &str, digest: &str) -> CitationLocator {
    let source = source_id.to_string();
    let digest = digest.to_string();
    let raw: String = store
        .with_connection(move |conn| {
            conn.query_row(
                "SELECT c.locator FROM chunks c
                 JOIN source_revisions sr ON sr.id=c.revision_id
                 WHERE sr.source_id=?1 AND sr.digest=?2 AND c.locator != ?3
                 ORDER BY c.rowid ASC LIMIT 1",
                rusqlite::params![source, digest, locators::META_LOCATOR],
                |row| row.get(0),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[tokio::test]
async fn ingest_idempotent_same_bytes_returns_same_revision() {
    let f = setup().await;
    let file = f._dir.path().join("source.txt");
    tokio::fs::write(&file, b"alpha beta gamma delta epsilon").await.unwrap();
    let input = || SourceInput::File { path: file.to_string_lossy().into_owned() };

    let rev1 = f.service.ingest(f.project, input(), DisclosurePolicy::ExplicitPublic).await.unwrap();
    let rev2 = f.service.ingest(f.project, input(), DisclosurePolicy::ExplicitPublic).await.unwrap();

    assert_eq!(rev1.digest, rev2.digest, "identical bytes must resolve to the same revision");
    assert_eq!(rev1.source_id, rev2.source_id);
    assert_eq!(rev1.locators, rev2.locators);
    let published: i64 = f
        .store
        .with_connection(move |conn| {
            conn.query_row("SELECT count(*) FROM source_revisions", [], |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(published, 1, "re-ingesting identical bytes must not publish a second revision");
}

#[tokio::test]
async fn new_bytes_publish_new_revision_and_old_citation_stays_exact() {
    let f = setup().await;
    let file = f._dir.path().join("source.txt");
    tokio::fs::write(&file, b"first version").await.unwrap();
    let input = || SourceInput::File { path: file.to_string_lossy().into_owned() };

    let rev1 = f.service.ingest(f.project, input(), DisclosurePolicy::ExplicitPublic).await.unwrap();
    let citation = Citation {
        source_id: rev1.source_id.clone(),
        revision_digest: rev1.digest.clone(),
        locator: first_locator(&f.store, &rev1.source_id, rev1.digest.as_str()).await,
    };

    tokio::fs::write(&file, b"second version with different content").await.unwrap();
    let rev2 = f.service.ingest(f.project, input(), DisclosurePolicy::ExplicitPublic).await.unwrap();
    assert_ne!(rev1.digest, rev2.digest, "new bytes must publish a new revision digest");
    assert_eq!(rev1.source_id, rev2.source_id, "same uri is the same source");

    let view = f.service.open_citation_in(f.project, &citation).await.unwrap();
    match view {
        CitationView::Content { text } => {
            assert!(text.contains("first version"), "old citation resolves to its exact historic text, got {text:?}")
        }
        other => panic!("old citation must resolve to Content, got {other:?}"),
    }
    // A foreign project cannot open another project's citation.
    let stranger = ProjectId::new();
    assert_eq!(
        f.service.open_citation_in(stranger, &citation).await.unwrap(),
        CitationView::Missing,
        "a citation from another project must not resolve here"
    );
}

#[tokio::test]
async fn failed_extraction_publishes_no_partial_revision() {
    let f = setup().await;
    let empty = f._dir.path().join("empty.txt");
    tokio::fs::write(&empty, b"").await.unwrap();

    let err = f
        .service
        .ingest(
            f.project,
            SourceInput::File { path: empty.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .expect_err("an empty source cannot validate");
    assert_eq!(err.code, "extraction_empty", "the failure names the real validation reason");

    let (revisions, chunks): (i64, i64) = f
        .store
        .with_connection(move |conn| {
            Ok((
                conn.query_row("SELECT count(*) FROM source_revisions", [], |r| r.get(0))?,
                conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(revisions, 0, "a failed extraction publishes no revision");
    assert_eq!(chunks, 0, "a failed extraction publishes no chunk");
}

#[tokio::test]
async fn encrypted_pdf_is_not_indexed_as_blank_success() {
    let f = setup().await;
    let pdf = f._dir.path().join("secret.pdf");
    // A %PDF header (so the sniff path engages) whose trailer names /Encrypt.
    let mut body = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
    body.extend_from_slice(b"trailer\n<< /Encrypt 7 0 R >>\n%%EOF\n");
    tokio::fs::write(&pdf, &body).await.unwrap();

    let err = f
        .service
        .ingest(
            f.project,
            SourceInput::File { path: pdf.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .expect_err("an encrypted PDF must be refused");
    assert_eq!(err.code, "source_encrypted");

    let revisions: i64 = f
        .store
        .with_connection(move |conn| {
            conn.query_row("SELECT count(*) FROM source_revisions", [], |r| r.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(revisions, 0, "an encrypted source must not be published");
}

#[tokio::test]
async fn local_only_source_cannot_be_fetched_over_the_network() {
    let f = setup().await;
    let err = f
        .service
        .ingest(
            f.project,
            SourceInput::Fetched {
                url: "https://example.invalid/doc".into(),
                receipt: ironbark_core::contracts::Digest::parse(&"a".repeat(64)).unwrap(),
            },
            DisclosurePolicy::LocalOnly,
        )
        .await
        .expect_err("LocalOnly content must never ride a network fetch");
    assert_eq!(err.code, "disclosure_conflict");
}

#[test]
fn extraction_error_codes_are_typed() {
    // Guards the error contract the ingest path depends on.
    let err = CoreError::new("extraction_empty", "no units");
    assert_eq!(format!("{err}"), "extraction_empty: no units");
}

/// Published CAS objects for a project: dot-entries are the lease and staging
/// dir, never objects.
fn cas_objects(f: &Fixture) -> usize {
    std::fs::read_dir(f._dir.path().join("objects"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .count()
}

/// The project artifact row the CAS publish created for a digest.
async fn cas_artifact(f: &Fixture, digest: &str) -> ArtifactId {
    let project = f.project;
    let digest = digest.to_string();
    let id: String = f
        .store
        .with_connection(move |conn| {
            Ok(conn.query_row(
                "SELECT id FROM artifacts WHERE project_id=?1 AND digest=?2 ORDER BY rowid LIMIT 1",
                rusqlite::params![project.to_string(), digest],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    ArtifactId(uuid::Uuid::parse_str(&id).unwrap())
}

/// A revision digest is a retention root, so the ingest path itself must write
/// the source bytes into the project CAS; a missing object fails every backup
/// closed with `IntegrityMismatch`. Nothing in this test publishes the object.
#[tokio::test]
async fn file_ingest_publishes_the_source_bytes_into_the_cas() {
    let f = setup().await;
    let file = f._dir.path().join("cited.txt");
    let bytes = b"cited bytes must live in the content-addressed store".to_vec();
    tokio::fs::write(&file, &bytes).await.unwrap();
    let input = || SourceInput::File { path: file.to_string_lossy().into_owned() };

    let revision = f.service.ingest(f.project, input(), DisclosurePolicy::ExplicitPublic).await.unwrap();

    let object = f._dir.path().join("objects").join(revision.digest.as_str());
    assert_eq!(
        std::fs::read(&object).expect("the ingest published the revision digest into the CAS"),
        bytes,
        "the CAS object named by the revision digest is byte-identical to the source file"
    );
    assert_eq!(cas_objects(&f), 1, "the ingest published exactly one content-addressed object");

    // Reach the object through the CAS itself: `get` re-hashes what it opens, so
    // this also proves the bytes are the object the digest names.
    let objects = ObjectStore::new(f.store.clone(), f.project, f._dir.path().join("objects"), ObjectLimits::default()).unwrap();
    let mut reader = objects.get(cas_artifact(&f, revision.digest.as_str()).await).await.unwrap();
    assert_eq!(reader.digest, revision.digest);
    let mut stored = Vec::new();
    reader.read_to_end(&mut stored).unwrap();
    assert_eq!(stored, bytes);

    // Re-publishing identical bytes is idempotent: one revision, one object.
    let again = f.service.ingest(f.project, input(), DisclosurePolicy::ExplicitPublic).await.unwrap();
    assert_eq!(again.digest, revision.digest, "identical bytes still resolve to one revision");
    assert_eq!(cas_objects(&f), 1, "an idempotent publish does not duplicate or replace the object");
    assert_eq!(std::fs::read(&object).unwrap(), bytes, "the published object is unchanged");
}

/// `Attachment` bytes already ARE the CAS artifact, so publishing them again
/// must be verified idempotent, not assumed: same object, same digest.
#[tokio::test]
async fn attachment_ingest_republishes_its_existing_object_idempotently() {
    let f = setup().await;
    let bytes = b"attachment bytes already in the content-addressed store".to_vec();
    let objects = ObjectStore::new(f.store.clone(), f.project, f._dir.path().join("objects"), ObjectLimits::default()).unwrap();
    let artifact = objects.put(std::io::Cursor::new(bytes.clone()), 1024).await.unwrap();
    let digest: String = f
        .store
        .with_connection(move |conn| Ok(conn.query_row("SELECT digest FROM artifacts WHERE id=?1", [artifact.to_string()], |row| row.get(0))?))
        .await
        .unwrap();

    let revision = f
        .service
        .ingest(f.project, SourceInput::Attachment { artifact_id: artifact }, DisclosurePolicy::ExplicitPublic)
        .await
        .unwrap();
    assert_eq!(revision.digest.as_str(), digest, "the attachment's CAS digest is the revision digest");
    assert_eq!(cas_objects(&f), 1, "re-publishing an existing object neither duplicates nor clobbers it");
    assert_eq!(std::fs::read(f._dir.path().join("objects").join(revision.digest.as_str())).unwrap(), bytes);
}

/// The publish runs after extraction + whole-source validation, so a refused
/// source leaves no orphan object behind.
#[tokio::test]
async fn refused_source_publishes_no_cas_object() {
    let f = setup().await;
    // Create the project CAS root first so this reads a real, empty store.
    ObjectStore::new(f.store.clone(), f.project, f._dir.path().join("objects"), ObjectLimits::default()).unwrap();
    let empty = f._dir.path().join("empty.txt");
    tokio::fs::write(&empty, b"").await.unwrap();

    let err = f
        .service
        .ingest(f.project, SourceInput::File { path: empty.to_string_lossy().into_owned() }, DisclosurePolicy::ExplicitPublic)
        .await
        .expect_err("an empty source cannot validate");
    assert_eq!(err.code, "extraction_empty");
    assert_eq!(cas_objects(&f), 0, "a refused source publishes no CAS object");
}

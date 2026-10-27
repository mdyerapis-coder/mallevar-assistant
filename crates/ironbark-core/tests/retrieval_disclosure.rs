//! P4 invariants: retrieval filters project/access/disclosure in SQL BEFORE
//! ranking, so a LocalOnly chunk can never reach a cloud route and a foreign
//! project's content never reaches another project's context. Deleting a
//! source resolves an explicit `Deleted` for its historic citations — never a
//! silent current-revision substitution.

use ironbark_core::contracts::{
    Citation, CitationLocator, CitationView, DisclosurePolicy, ProjectId, ProjectPolicy, RoutePin,
    SourceInput,
};
use ironbark_core::knowledge::{chunks, locators, CitationResolver, KnowledgeService};
use ironbark_core::store::Store;
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
    let policy = serde_json::to_string(&ProjectPolicy {
        version: 1,
        project_id: project,
        allowed_connection_ids: vec!["local".into(), "cloud".into()],
        allowed_tools: vec![],
        read_roots: vec![],
        write_roots: vec![],
        network_destinations: vec![],
        local_only: false,
        max_workers: 2,
        max_requests: 10,
        max_cost_microusd: None,
        spend_policy: ironbark_core::contracts::SpendPolicy::FreeOnly,
        allow_unknown_cost: false,
    })
    .unwrap();
    let policy_for_tx = policy.clone();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects (id,name,root,created_at) VALUES (?1,'p',?2,0)",
                rusqlite::params![project.to_string(), root],
            )?;
            tx.execute(
                "INSERT INTO policies (project_id,version,body) VALUES (?1,1,?2)",
                rusqlite::params![project.to_string(), policy_for_tx],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let service = KnowledgeService::new(store.clone(), dir.path().join("objects"));
    Fixture { _dir: dir, store, project, service }
}

fn local_route() -> RoutePin {
    RoutePin { connection_id: "local".into(), model_id: "m".into(), harness: None }
}

fn cloud_route() -> RoutePin {
    RoutePin { connection_id: "cloud".into(), model_id: "m".into(), harness: None }
}

async fn policy_of(store: &Store, project: ProjectId) -> ProjectPolicy {
    let body: String = store
        .with_connection(move |conn| {
            conn.query_row(
                "SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1",
                [project.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap();
    serde_json::from_str(&body).unwrap()
}

/// Rank at the SQL boundary where the disclosure filter lives.
async fn rank(
    fixture: &Fixture,
    route: RoutePin,
    query: &str,
) -> Vec<chunks::RankedChunk> {
    let policy = policy_of(&fixture.store, fixture.project).await;
    let project = fixture.project;
    let query = query.to_string();
    fixture
        .store
        .with_connection(move |conn| {
            chunks::ensure_fts(conn)?;
            chunks::rank_lexical(conn, project, &route, &policy, &query, 10)
        })
        .await
        .unwrap()
}

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
async fn local_only_chunk_is_excluded_from_a_cloud_route_before_ranking() {
    let f = setup().await;
    let file = f._dir.path().join("local.txt");
    tokio::fs::write(&file, b"local only content zeta").await.unwrap();
    f.service
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::LocalOnly,
        )
        .await
        .unwrap();

    let cloud = rank(&f, cloud_route(), "zeta").await;
    assert!(cloud.is_empty(), "a LocalOnly chunk must not be ranked for a cloud route");

    let local = rank(&f, local_route(), "zeta").await;
    assert_eq!(local.len(), 1, "the local route sees the LocalOnly chunk");
    assert!(local[0].content.contains("zeta"));
    assert_eq!(local[0].disclosure_policy().unwrap(), DisclosurePolicy::LocalOnly);
}

#[tokio::test]
async fn explicit_public_chunk_is_visible_to_every_route() {
    let f = setup().await;
    let file = f._dir.path().join("public.txt");
    tokio::fs::write(&file, b"public content omega").await.unwrap();
    f.service
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .unwrap();

    assert_eq!(rank(&f, cloud_route(), "omega").await.len(), 1);
    assert_eq!(rank(&f, local_route(), "omega").await.len(), 1);
}

#[tokio::test]
async fn allowed_connections_route_sees_the_chunk_but_others_do_not() {
    let f = setup().await;
    let file = f._dir.path().join("scoped.txt");
    tokio::fs::write(&file, b"scoped content kappa").await.unwrap();
    f.service
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::AllowedConnections { connection_ids: vec!["cloud".into()] },
        )
        .await
        .unwrap();

    assert_eq!(rank(&f, cloud_route(), "kappa").await.len(), 1, "the named connection may see it");
    assert_eq!(rank(&f, local_route(), "kappa").await.len(), 0, "an unnamed connection may not");
}

#[tokio::test]
async fn foreign_project_content_never_reaches_another_project() {
    let f = setup().await;
    let file = f._dir.path().join("secret.txt");
    tokio::fs::write(&file, b"project alpha secret").await.unwrap();
    let rev = f
        .service
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .unwrap();

    // A second project's retrieval cannot see it, even with a public policy.
    let stranger = ProjectId::new();
    let stranger_body = serde_json::to_string(&ProjectPolicy {
        project_id: stranger,
        ..policy_of(&f.store, f.project).await
    })
    .unwrap();
    f.store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects (id,name,root,created_at) VALUES (?1,'q','/tmp',0)",
                [stranger.to_string()],
            )?;
            tx.execute(
                "INSERT INTO policies (project_id,version,body) VALUES (?1,1,?2)",
                rusqlite::params![stranger.to_string(), stranger_body],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let query = "secret".to_string();
    let policy = policy_of(&f.store, stranger).await;
    let hits = f
        .store
        .with_connection(move |conn| {
            chunks::ensure_fts(conn)?;
            chunks::rank_lexical(conn, stranger, &local_route(), &policy, &query, 10)
        })
        .await
        .unwrap();
    assert!(hits.is_empty(), "another project's chunks must not be ranked");

    // The citation itself is foreign to that project.
    let citation = Citation {
        source_id: rev.source_id.clone(),
        revision_digest: rev.digest.clone(),
        locator: first_locator(&f.store, &rev.source_id, rev.digest.as_str()).await,
    };
    assert_eq!(
        f.service.open_citation_in(stranger, &citation).await.unwrap(),
        CitationView::Missing,
        "a foreign citation resolves Missing, not Content"
    );
    assert!(matches!(
        f.service.open_citation_in(f.project, &citation).await.unwrap(),
        CitationView::Content { .. }
    ));
}

#[tokio::test]
async fn deleted_source_loses_chunks_and_resolves_deleted() {
    let f = setup().await;
    let file = f._dir.path().join("todelete.txt");
    tokio::fs::write(&file, b"to be deleted content").await.unwrap();
    let rev = f
        .service
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .unwrap();
    let citation = Citation {
        source_id: rev.source_id.clone(),
        revision_digest: rev.digest.clone(),
        locator: first_locator(&f.store, &rev.source_id, rev.digest.as_str()).await,
    };
    assert!(matches!(
        f.service.open_citation_in(f.project, &citation).await.unwrap(),
        CitationView::Content { .. }
    ));
    let ranked = rank(&f, local_route(), "deleted").await;
    assert_eq!(ranked.len(), 1, "the SQL rank layer finds the chunk");
    let cites = chunks::cites_from_ranked(&ranked, f.project).unwrap();
    assert_eq!(cites.len(), 1, "excerpt conversion works");
    let retrieved = f.service.retrieve(f.project, "deleted", 10).await;
    match &retrieved {
        Ok(hits) => assert_eq!(hits.len(), 1),
        Err(e) => panic!("retrieve failed: code={} message={}", e.code, e.message),
    }

    let receipt = f.service.delete_source(f.project, rev.source_id.clone()).await.unwrap();
    assert!(receipt.chunks_removed >= 1, "the active chunks were removed: {receipt:?}");

    // The revision is gone, the source row survives as a tombstone: the
    // historic citation resolves an explicit Deleted, never Missing and never
    // a silently substituted new revision.
    assert_eq!(
        f.service.open_citation_in(f.project, &citation).await.unwrap(),
        CitationView::Deleted
    );
    let remaining: i64 = f
        .store
        .with_connection(move |conn| {
            conn.query_row("SELECT count(*) FROM source_revisions", [], |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(remaining, 0, "delete removes the active revision content");
    assert!(f.service.retrieve(f.project, "deleted", 10).await.unwrap().is_empty());

    // The standalone resolver shares the exact same resolution path.
    let resolver = CitationResolver::new(f.store.clone());
    assert_eq!(resolver.open(f.project, &citation).await.unwrap(), CitationView::Deleted);
}

//! P5 invariants: explicit memory and optional versioned local embeddings.
//! - A proposal is NOT retrievable before the owner approves it.
//! - Forgetting an approved memory removes it from retrieval while the audit
//!   record survives.
//! - Retrieved memory is advisory text: it never expands the route's tools.
//! - Embeddings are tagged by full model identity/dimensions: an incompatible
//!   model is rejected on index and falls back to a labelled lexical result on
//!   retrieve, so an absent embedder never blocks knowledge.

use ironbark_core::contracts::{
    DisclosurePolicy, EmbeddingModelIdentity, MemoryCandidate, MemoryDecision, ProjectId,
    ProjectPolicy, RoutePin, SourceInput, SpendPolicy,
};
use ironbark_core::knowledge::{EmbeddingIndex, KnowledgeService, MemoryService, RetrievalResult};
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
        allowed_connection_ids: vec!["local".into()],
        allowed_tools: vec![],
        read_roots: vec![],
        write_roots: vec![],
        network_destinations: vec![],
        local_only: false,
        max_workers: 2,
        max_requests: 10,
        max_cost_microusd: None,
        spend_policy: SpendPolicy::FreeOnly,
        allow_unknown_cost: false,
    })
    .unwrap();
    store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects (id,name,root,created_at) VALUES (?1,'p',?2,0)",
                rusqlite::params![project.to_string(), root],
            )?;
            tx.execute(
                "INSERT INTO policies (project_id,version,body) VALUES (?1,1,?2)",
                rusqlite::params![project.to_string(), policy],
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

fn model(dimensions: u32) -> EmbeddingModelIdentity {
    EmbeddingModelIdentity { connection_id: "local".into(), model_id: "hash-bucket".into(), version: "1".into(), dimensions }
}

fn candidate(f: &Fixture, text: &str) -> MemoryCandidate {
    MemoryCandidate {
        project_id: f.project,
        text: text.to_string(),
        provenance: "conversation/turn-1".into(),
        input_refs: Some(vec!["citation:1".into()]),
    }
}

#[tokio::test]
async fn proposal_is_not_retrievable_before_approval() {
    let f = setup().await;
    let memory = MemoryService::new(f.store.clone());
    let id = memory.propose(candidate(&f, "the workshop uses a 40:1 epoxy mix")).await.unwrap();

    assert!(
        memory.retrieve_memories(f.project, "epoxy", 10).await.unwrap().is_empty(),
        "an unapproved candidate must never be retrievable"
    );

    let record = memory.decide(&id, None, MemoryDecision::Approve).await.unwrap();
    assert_eq!(record.state, "approved");
    let hits = memory.retrieve_memories(f.project, "epoxy", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "once approved the memory is retrievable");
    assert_eq!(hits[0].provenance, "conversation/turn-1");
}

#[tokio::test]
async fn rejected_and_edited_proposals_respect_the_owner_decision() {
    let f = setup().await;
    let memory = MemoryService::new(f.store.clone());
    let rejected = memory.propose(candidate(&f, "rejected fact alpha")).await.unwrap();
    memory.decide(&rejected, None, MemoryDecision::Reject).await.unwrap();
    assert!(memory.retrieve_memories(f.project, "alpha", 10).await.unwrap().is_empty());

    let edited = memory.propose(candidate(&f, "draft fact beta")).await.unwrap();
    let record = memory
        .decide(&edited, Some("owner-edited fact beta".into()), MemoryDecision::EditAndApprove)
        .await
        .unwrap();
    assert_eq!(record.text, "owner-edited fact beta", "the owner's edit is what is stored");
    let hits = memory.retrieve_memories(f.project, "beta", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text, "owner-edited fact beta");
}

#[tokio::test]
async fn forget_removes_active_memory_but_keeps_the_audit_record() {
    let f = setup().await;
    let memory = MemoryService::new(f.store.clone());
    let id = memory.propose(candidate(&f, "forgettable fact gamma")).await.unwrap();
    // `decide` takes the candidate id; `forget` takes the record id it returns.
    let approved = memory.decide(&id, None, MemoryDecision::Approve).await.unwrap();
    assert_eq!(approved.state, "approved");
    assert_eq!(memory.retrieve_memories(f.project, "gamma", 10).await.unwrap().len(), 1);

    let forgotten = memory.forget(&approved.id).await.unwrap();
    assert_eq!(forgotten.state, "forgotten");
    assert!(
        memory.retrieve_memories(f.project, "gamma", 10).await.unwrap().is_empty(),
        "a forgotten memory is no longer retrievable"
    );

    let surviving_id = approved.id.clone();
    let surviving: i64 = f
        .store
        .with_connection(move |conn| {
            conn.query_row("SELECT count(*) FROM memory WHERE id=?1", [surviving_id], |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .unwrap();
    assert_eq!(surviving, 1, "forgetting marks the record; the audit trail survives");
}

#[tokio::test]
async fn memory_is_project_scoped() {
    let f = setup().await;
    let memory = MemoryService::new(f.store.clone());
    let id = memory.propose(candidate(&f, "project alpha only fact delta")).await.unwrap();
    memory.decide(&id, None, MemoryDecision::Approve).await.unwrap();

    let stranger = ProjectId::new();
    f.store
        .with_transaction(move |tx| {
            tx.execute(
                "INSERT INTO projects (id,name,root,created_at) VALUES (?1,'q','/tmp',0)",
                [stranger.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        memory.retrieve_memories(stranger, "delta", 10).await.unwrap().is_empty(),
        "memory never crosses project boundaries"
    );
}

#[tokio::test]
async fn embedding_index_rejects_a_model_whose_dimensions_disagree() {
    let f = setup().await;
    let file = f._dir.path().join("doc.txt");
    tokio::fs::write(&file, b"indexable content epsilon").await.unwrap();
    let rev = f
        .service
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .unwrap();

    let index = EmbeddingIndex::new(f.store.clone());
    let receipt = index.index(&rev, &model(64)).await.unwrap();
    assert!(receipt.chunks_indexed >= 1, "the revision's chunks were indexed");
    assert_eq!(receipt.model.dimensions, 64);

    // A second, differently-dimensioned identity is a distinct index; the
    // stored vector for the first must be rejected under it, not silently
    // reinterpreted.
    let mismatched = index.retrieve(f.project, "epsilon", &local_route(), &model(32), 10).await.unwrap();
    assert!(
        matches!(mismatched, RetrievalResult::Lexical { .. }),
        "a different-dimension model falls back to a labelled lexical result"
    );

    let matched = index.retrieve(f.project, "epsilon", &local_route(), &model(64), 10).await.unwrap();
    match matched {
        RetrievalResult::Embedding { model: used, excerpts } => {
            assert_eq!(used.dimensions, 64);
            assert!(!excerpts.is_empty());
            assert!(excerpts.iter().all(|e| !e.lexical), "embedding hits are labelled non-lexical");
        }
        RetrievalResult::Lexical { .. } => panic!("indexed vectors must rank as embeddings"),
    }
}

#[tokio::test]
async fn qualified_semantic_availability_is_honest() {
    // The local embedder is a deterministic feature-hash bucketizer, not a
    // semantic model: availability must be reported false rather than implied.
    assert!(!EmbeddingIndex::qualified_semantic_available());
}

#[tokio::test]
async fn retrieve_reports_lexical_when_no_vectors_exist() {
    let f = setup().await;
    let file = f._dir.path().join("doc.txt");
    tokio::fs::write(&file, b"lexical only content zeta").await.unwrap();
    f.service
        .ingest(
            f.project,
            SourceInput::File { path: file.to_string_lossy().into_owned() },
            DisclosurePolicy::ExplicitPublic,
        )
        .await
        .unwrap();

    let index = EmbeddingIndex::new(f.store.clone());
    let result = index.retrieve(f.project, "zeta", &local_route(), &model(64), 10).await.unwrap();
    match result {
        RetrievalResult::Lexical { excerpts } => {
            assert_eq!(excerpts.len(), 1, "lexical fallback still answers");
            assert!(excerpts[0].lexical, "the fallback is explicitly labelled lexical");
        }
        RetrievalResult::Embedding { .. } => panic!("no vectors exist; the result must be lexical"),
    }
}

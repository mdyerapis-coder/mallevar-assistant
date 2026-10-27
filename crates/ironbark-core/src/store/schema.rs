//! Schema-1 storage ownership. A hash is not a CAS reference unless its typed
//! producer owns object bytes. Inspection is read-only and precedes every sweep.
use std::collections::{BTreeMap, BTreeSet};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use crate::{contracts::*, conversations::TurnSnapshot, error::{CoreError, Result}};
use super::events::{now_ms, redact_text};

macro_rules! tables {
    ($($variant:ident => $name:literal, $columns:literal;)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub enum StorageTable { $($variant,)+ }
        impl StorageTable {
            pub fn name(self) -> &'static str { match self { $(Self::$variant => $name,)+ } }
            fn columns(self) -> &'static str { match self { $(Self::$variant => $columns,)+ } }
            fn parse(name: &str) -> Result<Self> { match name { $($name => Ok(Self::$variant),)+ _ => Err(unknown("unreviewed storage table")) } }
        }
    }
}
// name:type; T=TEXT, I=INTEGER, B=BLOB, _=SQLite's undeclared internal affinity.
// Leaf tables are optional, not eagerly created by this registry.
tables! {
    Projects => "projects", "id:T name:T root:T created_at:I";
    Policies => "policies", "project_id:T version:I body:T";
    ProviderReferences => "provider_references", "id:T config:T";
    ProviderObservations => "provider_observations", "id:I connection_id:T name:T state:T body:T observed_at:I";
    Conversations => "conversations", "id:T project_id:T created_at:I title:T archived_at:I deleted_at:I";
    Branches => "branches", "id:T conversation_id:T parent_id:T fork_message_id:T provenance:T";
    Messages => "messages", "id:T branch_id:T role:T content:T created_at:I status:T";
    BranchMessages => "branch_messages", "branch_id:T position:I message_id:T";
    Attachments => "attachments", "id:T message_id:T digest:T path:T";
    Jobs => "jobs", "id:T project_id:T body:T state:T";
    Nodes => "nodes", "id:T job_id:T body:T state:T";
    Runs => "runs", "id:T node_id:T state:T body:T";
    ConversationTurns => "conversation_turns", "run_id:T conversation_id:T branch_id:T request_id:T request_digest:T user_message_id:T assistant_message_id:T snapshot:T dispatch_claimed:I last_worker_seq:I";
    Actions => "actions", "id:T run_id:T project_id:T digest:T policy_version:I state:T body:T";
    Grants => "grants", "id:T action_id:T action_digest:T expires_at:I revoked_at:I consumed_at:I body:T";
    ActionOutcomes => "action_outcomes", "action_id:T grant_id:T outcome:T receipt_digest:T observed_state_digest:T recorded_at:I";
    Requests => "requests", "id:T command_digest:T state:T reply:T accepted_at:I completed_at:I";
    Events => "events", "seq:I protocol:I request_id:T project_id:T run_id:T timestamp:I payload:T";
    Artifacts => "artifacts", "id:T project_id:T run_id:T digest:T path:T";
    Checkpoints => "checkpoints", "id:T run_id:T state_digest:T body:T";
    Sources => "sources", "id:T project_id:T uri:T";
    SourceRevisions => "source_revisions", "id:T source_id:T digest:T created_at:I";
    Chunks => "chunks", "id:T revision_id:T locator:T content:T";
    Embeddings => "embeddings", "chunk_id:T model_id:T dimensions:I vector:B";
    Memory => "memory", "id:T project_id:T provenance:T body:T created_at:I";
    Judgments => "judgments", "decision_id:T project_id:T run_id:T state:T input_digest:T model_identity:T prompt_digest:T body:T created_at:I";
    ImportPreviews => "import_previews", "digest:T project_id:T body:T created_at:I";
    ImportReceipts => "import_receipts", "digest:T project_id:T body:T applied_at:I";
    HarnessObservations => "harness_observations", "harness:T name:T state:T body:T observed_at:I";
    WorkspaceChoices => "workspace_choices", "project_id:T base_ref:T base_commit:T chosen_at:I";
    WorktreeLeases => "worktree_leases", "lease_id:T project_id:T run_id:T path:T base_commit:T mode:T state:T created_at:I";
    IntegrationReceipts => "integration_receipts", "receipt_digest:T project_id:T run_id:T lease_id:T body:T created_at:I";
    WorkspaceChecks => "workspace_checks", "id:T run_id:T kind:T body:T created_at:I";
    CapacityLimits => "capacity_limits", "id:T emergency_reserve_bytes:I headroom_bytes:I total_available:I used_bytes:I last_updated:I created_at:I";
    Reservations => "reservations", "id:T service_id:T bytes_allocated:I priority:T status:T reservation_ts:I expiry_ts:I";
    CapacityHistory => "capacity_history", "id:I timestamp:I event_type:T event_details:T";
    SqliteSequence => "sqlite_sequence", "name:_ seq:_";
    KnowledgeFts => "knowledge_fts", "chunk_id:_ project_id:_ disclosure:_ content:_";
    KnowledgeFtsData => "knowledge_fts_data", "id:I block:B";
    KnowledgeFtsIdx => "knowledge_fts_idx", "segid:_ term:_ pgno:_";
    KnowledgeFtsContent => "knowledge_fts_content", "id:I c0:_ c1:_ c2:_ c3:_";
    KnowledgeFtsDocsize => "knowledge_fts_docsize", "id:I sz:B";
    KnowledgeFtsConfig => "knowledge_fts_config", "k:_ v:_";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceVisibility { PrivateOriginal, ProjectEvidence }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceReason { Artifact, Attachment, SourceRevision, Citation, ContextArtifact, TurnArtifactReceipt, MessageArtifact, CheckpointArtifact, RunArtifact, ImportArtifact, ActionArtifact }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectReference { pub digest: Digest, pub reason: ReferenceReason, pub visibility: ReferenceVisibility }
#[derive(Debug, Clone)]
pub struct StorageSchema { tables: BTreeSet<StorageTable> }
fn unknown(message: &str) -> CoreError { CoreError::new("UnknownBackupSchema", message) }
pub(crate) fn quote(name: &str) -> String { format!("\"{}\"", name.replace('"', "\"\"")) }
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|_| unknown("unsupported persisted reference form"))
}
fn keys(value: &Value, allowed: &[&str]) -> Result<()> {
    let map = value.as_object().ok_or_else(|| unknown("reference body must be an object"))?;
    if map.keys().any(|key| !allowed.contains(&key.as_str())) { return Err(unknown("unreviewed reference field")); }
    Ok(())
}
fn fts_shadow(table: StorageTable) -> bool {
    matches!(table, StorageTable::KnowledgeFtsData | StorageTable::KnowledgeFtsIdx | StorageTable::KnowledgeFtsContent | StorageTable::KnowledgeFtsDocsize | StorageTable::KnowledgeFtsConfig)
}
fn same_sql(left: &str, right: &str) -> bool {
    // Whitespace outside literals is formatting; literal bytes are data.
    fn normalized(sql: &str) -> impl Iterator<Item=char> + '_ {
        let mut quote = None;
        sql.chars().filter_map(move |c| {
            if matches!(c, '\'' | '"') { if quote == Some(c) { quote=None; } else if quote.is_none() { quote=Some(c); } }
            if quote.is_none() && c.is_whitespace() { None } else { Some(c) }
        })
    }
    normalized(left).eq(normalized(right))
}
const FTS_SQL: &str = "CREATE VIRTUAL TABLE knowledge_fts USING fts5(chunk_id UNINDEXED, project_id UNINDEXED, disclosure UNINDEXED, content, tokenize = 'unicode61')";
fn table_sql(table: StorageTable) -> &'static str {
    match table {
        StorageTable::Projects => "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, root TEXT NOT NULL, created_at INTEGER NOT NULL)",
        StorageTable::Policies => "CREATE TABLE policies (project_id TEXT NOT NULL REFERENCES projects(id), version INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY(project_id, version))",
        StorageTable::ProviderReferences => "CREATE TABLE provider_references (id TEXT PRIMARY KEY, config TEXT NOT NULL)",
        StorageTable::ProviderObservations => "CREATE TABLE provider_observations (id INTEGER PRIMARY KEY, connection_id TEXT NOT NULL REFERENCES provider_references(id), name TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('unknown','unavailable','unqualified','qualified')), body TEXT NOT NULL, observed_at INTEGER NOT NULL)",
        StorageTable::Conversations => "CREATE TABLE conversations (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), created_at INTEGER NOT NULL, title TEXT NOT NULL DEFAULT 'Untitled', archived_at INTEGER, deleted_at INTEGER)",
        StorageTable::Branches => "CREATE TABLE branches (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id), parent_id TEXT REFERENCES branches(id), fork_message_id TEXT REFERENCES messages(id), provenance TEXT NOT NULL DEFAULT '{}')",
        StorageTable::Messages => "CREATE TABLE messages (id TEXT PRIMARY KEY, branch_id TEXT NOT NULL REFERENCES branches(id), role TEXT NOT NULL, content TEXT NOT NULL, created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete')",
        StorageTable::BranchMessages => "CREATE TABLE branch_messages (branch_id TEXT NOT NULL REFERENCES branches(id), position INTEGER NOT NULL, message_id TEXT NOT NULL REFERENCES messages(id), PRIMARY KEY(branch_id,position), UNIQUE(branch_id,message_id))",
        StorageTable::Attachments => "CREATE TABLE attachments (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id), digest TEXT NOT NULL, path TEXT NOT NULL)",
        StorageTable::Jobs => "CREATE TABLE jobs (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), body TEXT NOT NULL, state TEXT NOT NULL)",
        StorageTable::Nodes => "CREATE TABLE nodes (id TEXT PRIMARY KEY, job_id TEXT NOT NULL REFERENCES jobs(id), body TEXT NOT NULL, state TEXT NOT NULL)",
        StorageTable::Runs => "CREATE TABLE runs (id TEXT PRIMARY KEY, node_id TEXT NOT NULL REFERENCES nodes(id), state TEXT NOT NULL, body TEXT NOT NULL)",
        StorageTable::ConversationTurns => "CREATE TABLE conversation_turns (run_id TEXT PRIMARY KEY REFERENCES runs(id), conversation_id TEXT NOT NULL REFERENCES conversations(id), branch_id TEXT NOT NULL REFERENCES branches(id), request_id TEXT NOT NULL UNIQUE, request_digest TEXT NOT NULL, user_message_id TEXT REFERENCES messages(id), assistant_message_id TEXT REFERENCES messages(id), snapshot TEXT NOT NULL, dispatch_claimed INTEGER NOT NULL DEFAULT 0, last_worker_seq INTEGER NOT NULL DEFAULT 0)",
        StorageTable::Actions => "CREATE TABLE actions (id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id), project_id TEXT NOT NULL REFERENCES projects(id), digest TEXT NOT NULL, policy_version INTEGER NOT NULL, state TEXT NOT NULL, body TEXT NOT NULL)",
        StorageTable::Grants => "CREATE TABLE grants (id TEXT PRIMARY KEY, action_id TEXT NOT NULL REFERENCES actions(id), action_digest TEXT NOT NULL, expires_at INTEGER NOT NULL, revoked_at INTEGER, consumed_at INTEGER, body TEXT NOT NULL)",
        StorageTable::ActionOutcomes => "CREATE TABLE action_outcomes (action_id TEXT PRIMARY KEY REFERENCES actions(id), grant_id TEXT NOT NULL UNIQUE REFERENCES grants(id), outcome TEXT NOT NULL CHECK(outcome IN ('succeeded','failed','uncertain')), receipt_digest TEXT NOT NULL, observed_state_digest TEXT NOT NULL, recorded_at INTEGER NOT NULL)",
        StorageTable::Requests => "CREATE TABLE requests (id TEXT PRIMARY KEY, command_digest TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('in_flight','completed','abandoned')), reply TEXT, accepted_at INTEGER NOT NULL, completed_at INTEGER)",
        StorageTable::Events => "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, protocol INTEGER NOT NULL CHECK(protocol=1), request_id TEXT REFERENCES requests(id), project_id TEXT REFERENCES projects(id), run_id TEXT REFERENCES runs(id), timestamp INTEGER NOT NULL, payload TEXT NOT NULL)",
        StorageTable::Artifacts => "CREATE TABLE artifacts (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), run_id TEXT REFERENCES runs(id), digest TEXT NOT NULL, path TEXT NOT NULL)",
        StorageTable::Checkpoints => "CREATE TABLE checkpoints (id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id), state_digest TEXT NOT NULL, body TEXT NOT NULL)",
        StorageTable::Sources => "CREATE TABLE sources (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), uri TEXT NOT NULL)",
        StorageTable::SourceRevisions => "CREATE TABLE source_revisions (id TEXT PRIMARY KEY, source_id TEXT NOT NULL REFERENCES sources(id), digest TEXT NOT NULL, created_at INTEGER NOT NULL, UNIQUE(source_id,digest))",
        StorageTable::Chunks => "CREATE TABLE chunks (id TEXT PRIMARY KEY, revision_id TEXT NOT NULL REFERENCES source_revisions(id), locator TEXT NOT NULL, content TEXT NOT NULL)",
        StorageTable::Embeddings => "CREATE TABLE embeddings (chunk_id TEXT NOT NULL REFERENCES chunks(id), model_id TEXT NOT NULL, dimensions INTEGER NOT NULL, vector BLOB NOT NULL, PRIMARY KEY(chunk_id,model_id))",
        StorageTable::Memory => "CREATE TABLE memory (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), provenance TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL)",
        StorageTable::Judgments => "CREATE TABLE judgments (decision_id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects(id), run_id TEXT REFERENCES runs(id), state TEXT NOT NULL, input_digest TEXT NOT NULL, model_identity TEXT NOT NULL, prompt_digest TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL)",
        StorageTable::ImportPreviews => "CREATE TABLE import_previews (digest TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), body TEXT NOT NULL, created_at INTEGER NOT NULL)",
        StorageTable::ImportReceipts => "CREATE TABLE import_receipts (digest TEXT PRIMARY KEY REFERENCES import_previews(digest), project_id TEXT NOT NULL REFERENCES projects(id), body TEXT NOT NULL, applied_at INTEGER NOT NULL)",
        StorageTable::HarnessObservations => "CREATE TABLE harness_observations (harness TEXT NOT NULL, name TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('unknown','unavailable','unqualified','qualified')), body TEXT NOT NULL, observed_at INTEGER NOT NULL, PRIMARY KEY (harness, name))",
        StorageTable::WorkspaceChoices => "CREATE TABLE workspace_choices (project_id TEXT PRIMARY KEY, base_ref TEXT NOT NULL, base_commit TEXT NOT NULL, chosen_at INTEGER NOT NULL)",
        StorageTable::WorktreeLeases => "CREATE TABLE worktree_leases (lease_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, run_id TEXT NOT NULL, path TEXT NOT NULL, base_commit TEXT NOT NULL, mode TEXT NOT NULL CHECK(mode IN ('reader','writer')), state TEXT NOT NULL CHECK(state IN ('active','released','integrated')), created_at INTEGER NOT NULL)",
        StorageTable::IntegrationReceipts => "CREATE TABLE integration_receipts (receipt_digest TEXT PRIMARY KEY, project_id TEXT NOT NULL, run_id TEXT NOT NULL, lease_id TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL)",
        StorageTable::WorkspaceChecks => "CREATE TABLE workspace_checks (id TEXT PRIMARY KEY, run_id TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('review','verification')), body TEXT NOT NULL, created_at INTEGER NOT NULL)",
        StorageTable::KnowledgeFts => FTS_SQL,
        StorageTable::KnowledgeFtsData => "CREATE TABLE 'knowledge_fts_data'(id INTEGER PRIMARY KEY, block BLOB)",
        StorageTable::KnowledgeFtsIdx => "CREATE TABLE 'knowledge_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID",
        StorageTable::KnowledgeFtsContent => "CREATE TABLE 'knowledge_fts_content'(id INTEGER PRIMARY KEY, c0, c1, c2, c3)",
        StorageTable::KnowledgeFtsDocsize => "CREATE TABLE 'knowledge_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB)",
        StorageTable::KnowledgeFtsConfig => "CREATE TABLE 'knowledge_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID",
        StorageTable::CapacityLimits => "CREATE TABLE capacity_limits (id TEXT PRIMARY KEY, emergency_reserve_bytes INTEGER NOT NULL, headroom_bytes INTEGER NOT NULL, total_available INTEGER NOT NULL, used_bytes INTEGER NOT NULL, last_updated INTEGER NOT NULL, created_at INTEGER NOT NULL)",
        StorageTable::Reservations => "CREATE TABLE reservations (id TEXT PRIMARY KEY, service_id TEXT NOT NULL, bytes_allocated INTEGER NOT NULL, priority TEXT NOT NULL, status TEXT NOT NULL, reservation_ts INTEGER NOT NULL, expiry_ts INTEGER)",
        StorageTable::CapacityHistory => "CREATE TABLE capacity_history (id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, event_type TEXT NOT NULL, event_details TEXT)",
        StorageTable::SqliteSequence => "CREATE TABLE sqlite_sequence(name,seq)",
    }
}
fn index_sql(name: &str) -> Option<&'static str> {
    Some(match name {
        "provider_observations_route" => "CREATE INDEX provider_observations_route ON provider_observations(connection_id, name, observed_at)",
        "messages_branch" => "CREATE INDEX messages_branch ON messages(branch_id, created_at)",
        "runs_state" => "CREATE INDEX runs_state ON runs(state)",
        "conversation_turns_branch" => "CREATE INDEX conversation_turns_branch ON conversation_turns(conversation_id,branch_id)",
        "grants_action" => "CREATE INDEX grants_action ON grants(action_id)",
        "events_run" => "CREATE INDEX events_run ON events(run_id, seq)",
        "events_project" => "CREATE INDEX events_project ON events(project_id, seq)",
        "harness_observations_harness" => "CREATE INDEX harness_observations_harness ON harness_observations(harness)",
        "worktree_leases_one_active_writer" => "CREATE UNIQUE INDEX worktree_leases_one_active_writer ON worktree_leases(path) WHERE mode='writer' AND state='active'",
        "workspace_checks_run_kind" => "CREATE INDEX workspace_checks_run_kind ON workspace_checks(run_id, kind)",
        _ => return None,
    })
}

impl StorageSchema {
    pub fn for_version(version: u32) -> Result<Self> {
        if version != 1 { return Err(unknown("unsupported storage schema version")); }
        Ok(Self { tables: BTreeSet::new() })
    }
    pub fn inspect(conn: &Connection) -> Result<Self> {
        let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let mut schema = Self::for_version(version)?;
        let mut stmt = conn.prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY name")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))? {
            let (kind, name, owner, sql) = row?;
            match kind.as_str() {
                "table" => {
                    let table = StorageTable::parse(&name)?;
                    if owner != name { return Err(unknown("invalid table ownership")); }
                    if !sql.as_deref().is_some_and(|sql| same_sql(sql, table_sql(table))) { return Err(unknown("unreviewed table SQL definition")); }
                    schema.inspect_columns(conn, table)?;
                    schema.tables.insert(table);
                }
                "index" => {
                    StorageTable::parse(&owner)?;
                    if let Some(sql)=sql {
                        if !index_sql(&name).is_some_and(|expected| same_sql(&sql,expected)) { return Err(unknown("unreviewed index SQL definition")); }
                    } else if !name.starts_with(&format!("sqlite_autoindex_{owner}_")) { return Err(unknown("unreviewed internal index")); }
                }
                _ => return Err(unknown("executable SQL objects are not exportable")),
            }
        }
        for required in [StorageTable::Projects, StorageTable::Policies, StorageTable::ProviderReferences, StorageTable::ProviderObservations, StorageTable::Conversations, StorageTable::Branches, StorageTable::Messages, StorageTable::BranchMessages, StorageTable::Attachments, StorageTable::Jobs, StorageTable::Nodes, StorageTable::Runs, StorageTable::ConversationTurns, StorageTable::Actions, StorageTable::Grants, StorageTable::ActionOutcomes, StorageTable::Requests, StorageTable::Events, StorageTable::Artifacts, StorageTable::Checkpoints, StorageTable::Sources, StorageTable::SourceRevisions, StorageTable::Chunks, StorageTable::Embeddings, StorageTable::Memory, StorageTable::Judgments, StorageTable::ImportPreviews, StorageTable::ImportReceipts, StorageTable::SqliteSequence] {
            if !schema.tables.contains(&required) { return Err(unknown("missing schema-1 table")); }
        }
        let fts_count = schema.tables.iter().filter(|t| fts_shadow(**t) || **t == StorageTable::KnowledgeFts).count();
        if fts_count != 0 && fts_count != 6 { return Err(unknown("incomplete FTS mirror")); }
        let mut check = conn.prepare("PRAGMA integrity_check")?;
        let results = check.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        if results != ["ok"] { return Err(CoreError::new("IntegrityMismatch", "SQLite integrity check failed")); }
        if conn.prepare("PRAGMA foreign_key_check")?.query([])?.next()?.is_some() { return Err(CoreError::new("IntegrityMismatch", "SQLite foreign keys are invalid")); }
        Ok(schema)
    }
    fn inspect_columns(&self, conn: &Connection, table: StorageTable) -> Result<()> {
        let mut info = conn.prepare(&format!("PRAGMA table_xinfo({})", quote(table.name())))?;
        let columns = info.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(6)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
        let visible = columns.iter().filter(|(_, _, hidden)| *hidden == 0).collect::<Vec<_>>();
        let expected = table.columns().split_whitespace().collect::<Vec<_>>();
        if visible.len() != expected.len() { return Err(unknown("unreviewed storage columns")); }
        for ((name, kind, _), expected) in visible.into_iter().zip(expected) {
            let (expected_name, expected_kind) = expected.split_once(':').expect("registry column");
            let expected_kind = match expected_kind { "T" => "TEXT", "I" => "INTEGER", "B" => "BLOB", _ => "" };
            if name != expected_name || !kind.eq_ignore_ascii_case(expected_kind) { return Err(unknown("unreviewed column type or name")); }
            if !fts_shadow(table) {
                let blobs: i64 = conn.query_row(&format!("SELECT count(*) FROM {} WHERE typeof({})='blob'", quote(table.name()), quote(name)), [], |r| r.get(0))?;
                if blobs != 0 && !(table == StorageTable::Embeddings && name == "vector") { return Err(unknown("unreviewed binary storage")); }
            }
        }
        if columns.iter().any(|(name, _, hidden)| *hidden != 0 && !(table == StorageTable::KnowledgeFts && *hidden == 1 && matches!(name.as_str(), "knowledge_fts" | "rank"))) { return Err(unknown("unreviewed hidden/generated column")); }
        Ok(())
    }
    /// Re-inspect on every public operation; a registry value never licenses a
    /// subsequent ALTER or a different connection's schema.
    pub fn roots(&self, snapshot: &Connection) -> Result<Vec<ObjectReference>> {
        Self::inspect(snapshot)?.collect_roots(snapshot)
    }
    fn collect_roots(&self, snapshot: &Connection) -> Result<Vec<ObjectReference>> {
        let mut roots = Vec::new();
        let mut artifacts = BTreeMap::new();
        let mut stmt = snapshot.prepare("SELECT id,digest FROM artifacts")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, digest) = row?; let digest = Digest::parse(&digest)?;
            roots.push(ObjectReference { digest: digest.clone(), reason: ReferenceReason::Artifact, visibility: ReferenceVisibility::ProjectEvidence });
            artifacts.insert(id, digest);
        }
        for (table, reason) in [(StorageTable::Attachments, ReferenceReason::Attachment), (StorageTable::SourceRevisions, ReferenceReason::SourceRevision)] {
            let mut stmt = snapshot.prepare(&format!("SELECT digest FROM {}", table.name()))?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? { add_digest(&mut roots, Digest::parse(&row?)?, reason); }
        }
        for table in &self.tables {
            let column = match table {
                StorageTable::Messages => "content", StorageTable::ConversationTurns => "snapshot", StorageTable::Events => "payload", StorageTable::Requests => "reply",
                StorageTable::ProviderReferences => "config", StorageTable::KnowledgeFts => "disclosure",
                StorageTable::Checkpoints | StorageTable::Jobs | StorageTable::Nodes | StorageTable::Runs | StorageTable::Actions | StorageTable::Grants | StorageTable::ImportPreviews | StorageTable::ImportReceipts | StorageTable::Memory | StorageTable::Judgments | StorageTable::WorkspaceChecks | StorageTable::IntegrationReceipts | StorageTable::Policies | StorageTable::ProviderObservations | StorageTable::HarnessObservations => "body",
                _ => continue,
            };
            let mut stmt = snapshot.prepare(&format!("SELECT {} FROM {} WHERE {} IS NOT NULL", column, table.name(), column))?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let text = row?;
                match table {
                    StorageTable::ProviderReferences => { let _: ConnectionConfig=decode(&text)?; }
                    StorageTable::Policies => { let _: ProjectPolicy=decode(&text)?; }
                    StorageTable::Requests => { let _: CoreReply=decode(&text)?; }
                    StorageTable::ProviderObservations => { let _: CapabilityObservation=decode(&text)?; }
                    StorageTable::HarnessObservations => {
                        let value:Value=decode(&text)?;
                        keys(&value,&["qualified","evidence","version"])?;
                        if !value["qualified"].is_boolean() || !value["evidence"].is_string() || !value["version"].is_string() {return Err(unknown("invalid harness qualification record"));}
                    }
                    StorageTable::KnowledgeFts => { let _: DisclosurePolicy=decode(&text)?; }
                    StorageTable::Grants => { let _: ApprovalGrant=decode(&text)?; }
                    StorageTable::Judgments => {
                        if decode::<JudgmentRecord>(&text).is_err() { let _: QualificationRecord=decode(&text)?; }
                    }
                    StorageTable::WorkspaceChecks => { let _: CheckReceipt=decode(&text)?; }
                    StorageTable::IntegrationReceipts => { let _: crate::workspaces::worktrees::IntegrationReceipt=decode(&text)?; }
                    StorageTable::Memory => {
                        let value: Value=decode(&text)?;
                        keys(&value,&["candidate_id","state","text","provenance","input_refs","decision"])?;
                        // input_refs are descriptive provenance strings, not
                        // CAS addresses in schema 1.
                    }
                    StorageTable::ImportPreviews => {
                        let value: Value=decode(&text)?;
                        if value.get("source").is_some() {
                            keys(&value,&["source","preview"])?;
                            let _:super::imports::ImportSource=serde_json::from_value(value["source"].clone()).map_err(|_|unknown("invalid import source"))?;
                            let _:super::imports::ImportPreview=serde_json::from_value(value["preview"].clone()).map_err(|_|unknown("invalid import preview"))?;
                        } else { validate_role_record(&value,false)?; }
                    }
                    StorageTable::Messages => for part in decode::<Vec<ContentPart>>(&text)? { if let ContentPart::Artifact { artifact_id } = part { add_artifact(&mut roots, &artifacts, artifact_id, ReferenceReason::MessageArtifact)?; } },
                    StorageTable::ConversationTurns => visit_snapshot(&decode::<TurnSnapshot>(&text)?, &artifacts, &mut roots)?,
                    StorageTable::Checkpoints => {
                        let body: StoredCheckpoint = decode(&text)?;
                        let _ = (&body.run_id, &body.phase, &body.state_digest, body.created_at, &body.observed_workspace.native_session);
                        visit_work(&body.observed_workspace.run_body, StorageTable::Runs, &artifacts, &mut roots)?;
                        visit_manifest(&body.manifest, &artifacts, &mut roots)?;
                    }
                    StorageTable::Jobs | StorageTable::Nodes | StorageTable::Runs => visit_work(&decode::<Value>(&text)?, *table, &artifacts, &mut roots)?,
                    StorageTable::Actions => visit_action(&decode::<ActionSpec>(&text)?, &mut roots)?,
                    StorageTable::Events => match decode::<CoreEvent>(&text)? {
                        CoreEvent::ToolStarted { action } | CoreEvent::ApprovalNeeded { action } => visit_action(&action, &mut roots)?,
                        // ArtifactAdded.digest is a state digest for checkpoint
                        // events, not the artifact's CAS digest. Resolve its id.
                        CoreEvent::ArtifactAdded { artifact_id, .. } => add_artifact(&mut roots, &artifacts, artifact_id, ReferenceReason::Artifact)?,
                        CoreEvent::StateChanged{entity_id,state} if entity_id.starts_with("automation-scope/") => {let _:crate::automation::AutomationScopeRecord=decode(&state)?;}
                        CoreEvent::StateChanged{entity_id,state} if entity_id.starts_with("automation-consent/") => {let _:crate::automation::ConsentRecord=decode(&state)?;}
                        _ => {}
                    },
                    StorageTable::ImportReceipts => {
                        let value: Value = decode(&text)?;
                        if value.get("preview_digest").is_some() {
                            let receipt: super::imports::ImportReceipt = decode(&text)?;
                            if let Some(id) = receipt.artifact_id { add_artifact(&mut roots, &artifacts, id, ReferenceReason::ImportArtifact)?; }
                        } else { validate_role_record(&value,true)?; }
                    }
                    _ => unreachable!(),
                }
            }
        }
        Ok(roots)
    }
    /// Sanitize only declared text/data fields. Never parse a literal name,
    /// path, model id or human string as a JSON reference/control envelope.
    pub fn sanitize(&self, snapshot: &Connection) -> Result<()> {
        let schema = Self::inspect(snapshot)?;
        // Validate all reference forms before the first UPDATE, too.
        schema.collect_roots(snapshot)?;
        let tx = snapshot.unchecked_transaction()?;
        for table in &schema.tables {
            if fts_shadow(*table) || matches!(table, StorageTable::KnowledgeFts | StorageTable::SqliteSequence) { continue; }
            for column in table.columns().split_whitespace().filter_map(|c| c.strip_suffix(":T")) {
                let treatment = treatment(*table, column);
                if treatment == TextTreatment::Identity { continue; }
                let mut stmt = tx.prepare(&format!("SELECT rowid,{} FROM {} WHERE typeof({})='text'", quote(column), table.name(), quote(column)))?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
                for (id, text) in rows {
                    if treatment == TextTreatment::Locator {
                        if text == crate::knowledge::locators::META_LOCATOR { continue; }
                        let locator: CitationLocator = decode(&text)?;
                        require_exportable_locator(&locator)?;
                        continue;
                    }
                    let cleaned = if treatment == TextTreatment::Literal { redact_text(&text) } else {
                        let mut value: Value = decode(&text)?; let before = value.clone();
                        let context = match table {
                            StorageTable::Requests => JsonContext::Reply,
                            StorageTable::Events => JsonContext::Event,
                            StorageTable::Actions => JsonContext::Action,
                            StorageTable::Judgments if value.get("decision_id").is_some() => JsonContext::Judgment,
                            StorageTable::Memory | StorageTable::ImportPreviews | StorageTable::ImportReceipts | StorageTable::Branches => JsonContext::Data,
                            _ => JsonContext::Schema,
                        };
                        sanitize_json(&mut value, context)?;
                        if value == before { continue; }
                        serde_json::to_string(&value)?
                    };
                    if cleaned != text { tx.execute(&format!("UPDATE {} SET {}=?1 WHERE rowid=?2", table.name(), quote(column)), params![cleaned, id])?; }
                }
            }
        }
        if schema.tables.contains(&StorageTable::KnowledgeFts) {
            // Rebuild through FTS, preserving disclosure metadata but dropping
            // orphan mirror rows and old segment tokens after text sanitation.
            tx.execute_batch("CREATE TEMP TABLE ironbark_fts_export AS SELECT f.chunk_id,f.project_id,f.disclosure,c.content FROM knowledge_fts f JOIN chunks c ON c.id=f.chunk_id; DELETE FROM knowledge_fts; INSERT INTO knowledge_fts(chunk_id,project_id,disclosure,content) SELECT chunk_id,project_id,disclosure,content FROM ironbark_fts_export; DROP TABLE temp.ironbark_fts_export; INSERT INTO knowledge_fts(knowledge_fts) VALUES('optimize');")?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Imported receipts are evidence, not permission to resume native effects.
    pub fn restore_inactive(&self, snapshot: &Connection) -> Result<()> {
        let schema = Self::inspect(snapshot)?;
        schema.collect_roots(snapshot)?;
        let tx = snapshot.unchecked_transaction()?;
        for table in &schema.tables {
            match table {
                StorageTable::Runs => {
                    tx.execute("UPDATE runs SET state='blocked' WHERE state NOT IN ('succeeded','failed','cancelled')",[])?;
                    tx.execute("UPDATE runs SET body=json_remove(body,'$.native_session','$.recorded_pid','$.recorded_start_time')",[])?;
                }
                StorageTable::Checkpoints => {
                    let mut stmt=tx.prepare("SELECT rowid,body FROM checkpoints")?;
                    let rows=stmt.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
                    for (id,text) in rows {
                        let mut body:StoredCheckpoint=decode(&text)?;
                        body.manifest.acceptance_satisfied=false;
                        for check in &mut body.manifest.checks {check.passed=false;}
                        body.manifest.native_session=None;
                        // The original link object stays byte-exact in CAS;
                        // this inactive projection gets its own state identity.
                        let canonical=serde_json::json!({"observed_workspace":body.observed_workspace,"manifest":body.manifest,"phase":body.phase});
                        body.state_digest=Digest::parse(&format!("{:x}",Sha256::digest(serde_json::to_vec(&canonical)?)))?;
                        tx.execute("UPDATE checkpoints SET state_digest=?1,body=?2 WHERE rowid=?3",params![body.state_digest.as_str(),serde_json::to_string(&body)?,id])?;
                    }
                }
                StorageTable::Nodes | StorageTable::Jobs => { tx.execute(&format!("UPDATE {} SET state='blocked' WHERE state NOT IN ('succeeded','failed','cancelled')", table.name()), [])?; }
                StorageTable::Requests => { tx.execute("UPDATE requests SET state='abandoned',reply=NULL WHERE state='in_flight'", [])?; }
                StorageTable::ConversationTurns => { tx.execute("UPDATE conversation_turns SET dispatch_claimed=1", [])?; }
                StorageTable::Messages => { tx.execute("UPDATE messages SET status='interrupted' WHERE status IN ('queued','streaming','awaiting_acceptance')", [])?; }
                StorageTable::Grants => { tx.execute("UPDATE grants SET revoked_at=COALESCE(revoked_at,?1)", [now_ms()])?; }
                StorageTable::Actions => { tx.execute("UPDATE actions SET state='blocked' WHERE state IN ('executing','awaiting_approval','approved')", [])?; }
                StorageTable::WorktreeLeases => { tx.execute("UPDATE worktree_leases SET state='released' WHERE state='active'", [])?; }
                StorageTable::HarnessObservations => { tx.execute("UPDATE harness_observations SET state='unqualified',body=json_set(body,'$.qualified',json('false'))", [])?; }
                StorageTable::ProviderReferences => {
                    let mut stmt=tx.prepare("SELECT rowid,config FROM provider_references")?;
                    let rows=stmt.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
                    for (id,text) in rows {
                        let mut config:ConnectionConfig=decode(&text)?;
                        // Nonsecret vault identifiers can resolve real local
                        // credentials in the new namespace: do not enroll them.
                        config.auth_ref=None;
                        config.allowed_projects.clear();
                        for model in &mut config.models {for capability in &mut model.capabilities {capability.state=CapabilityState::Unqualified;}}
                        tx.execute("UPDATE provider_references SET config=?1 WHERE rowid=?2",params![serde_json::to_string(&config)?,id])?;
                    }
                }
                StorageTable::ProviderObservations => { tx.execute("UPDATE provider_observations SET state='unqualified',body=json_set(body,'$.state','unqualified')", [])?; }
                StorageTable::Judgments => { tx.execute("UPDATE judgments SET state='blocked',body=json_set(body,'$.state','blocked') WHERE json_type(body,'$.calibrator_digest') IS NOT NULL AND json_type(body,'$.holdout_size') IS NOT NULL", [])?; }
                StorageTable::WorkspaceChecks => { tx.execute("UPDATE workspace_checks SET body=json_set(body,'$.passed',json('false'))", [])?; }
                StorageTable::Events => {
                    let mut stmt=tx.prepare("SELECT seq,payload FROM events WHERE json_extract(payload,'$.type')='state_changed' AND json_extract(payload,'$.entity_id') LIKE 'automation-scope/%'")?;
                    let rows=stmt.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
                    for (seq,text) in rows {
                        let mut event:CoreEvent=decode(&text)?;
                        if let CoreEvent::StateChanged{state,..}=&mut event {
                            let mut scope:crate::automation::AutomationScopeRecord=decode(state)?;
                            scope.phase=crate::automation::ScopePhase::Released;
                            *state=serde_json::to_string(&scope)?;
                        }
                        tx.execute("UPDATE events SET payload=?1 WHERE seq=?2",params![serde_json::to_string(&event)?,seq])?;
                    }
                }
                _ => {}
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn validate_role_record(value:&Value,receipt:bool)->Result<()> {
    let timestamp=if receipt {"activated_at"}else{"created_at"};
    keys(value,&["role_id","project_id","definition_digest","definition",timestamp])?;
    for required in ["role_id","project_id","definition_digest","definition",timestamp] {
        if value.get(required).is_none() {return Err(unknown("incomplete role record"));}
    }
    let _:RoleDefinition=serde_json::from_value(value["definition"].clone()).map_err(|_|unknown("unreviewed role definition"))?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint { run_id: RunId, phase: CheckpointPhase, state_digest: Digest, observed_workspace: ObservedWorkspace, manifest: ResultManifest, created_at: i64 }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedWorkspace { native_session: Option<SessionRef>, run_body: Value }
fn add_digest(roots: &mut Vec<ObjectReference>, digest: Digest, reason: ReferenceReason) {
    roots.push(ObjectReference { digest, reason, visibility: ReferenceVisibility::ProjectEvidence });
}
fn add_artifact(roots: &mut Vec<ObjectReference>, artifacts: &BTreeMap<String, Digest>, id: ArtifactId, reason: ReferenceReason) -> Result<()> {
    let digest = artifacts.get(&id.to_string()).ok_or_else(|| CoreError::new("IntegrityMismatch", "typed reference names a missing artifact"))?;
    add_digest(roots, digest.clone(), reason); Ok(())
}
fn visit_context(context: &ContextPackage, artifacts: &BTreeMap<String, Digest>, roots: &mut Vec<ObjectReference>) -> Result<()> {
    for excerpt in &context.excerpts { add_digest(roots, excerpt.citation.revision_digest.clone(), ReferenceReason::Citation); }
    for id in &context.artifacts { add_artifact(roots, artifacts, *id, ReferenceReason::ContextArtifact)?; }
    Ok(())
}
fn visit_snapshot(snapshot: &TurnSnapshot, artifacts: &BTreeMap<String, Digest>, roots: &mut Vec<ObjectReference>) -> Result<()> {
    visit_context(&snapshot.context, artifacts, roots)?;
    for receipt in &snapshot.artifact_receipts { add_digest(roots, receipt.digest.clone(), ReferenceReason::TurnArtifactReceipt); }
    Ok(())
}
fn visit_manifest(manifest: &ResultManifest, artifacts: &BTreeMap<String, Digest>, roots: &mut Vec<ObjectReference>) -> Result<()> {
    for id in &manifest.artifacts { add_artifact(roots, artifacts, *id, ReferenceReason::CheckpointArtifact)?; }
    // CheckReceipt.evidence_digest is an attestation identity (workspace checks
    // do not publish a CAS object); it is deliberately not physical ownership.
    Ok(())
}
// These are the closed bodies written by scheduler/service, scheduler/graph
// and conversations/turns. Validate borrowed fields rather than cloning JSON or
// treating familiar outer keys as proof that nested reference forms are known.
fn work_field<'de, T: Deserialize<'de>>(value: &'de Value, field: &str) -> Result<T> {
    let field = value.get(field).ok_or_else(|| unknown("missing required work field"))?;
    T::deserialize(field).map_err(|_| unknown("unsupported work field form"))
}
fn work_array<'de, T: Deserialize<'de>>(value: &'de Value, field: &str) -> Result<()> {
    let items: &'de Vec<Value> = value.get(field).and_then(Value::as_array)
        .ok_or_else(|| unknown("missing or invalid work array"))?;
    for item in items {
        let _: T = T::deserialize(item).map_err(|_| unknown("unsupported work array element"))?;
    }
    Ok(())
}
fn work_keys(value: &Value, table: StorageTable, fields: &[&str]) -> Result<()> {
    let map = value.as_object().ok_or_else(|| unknown("work body must be an object"))?;
    for field in map.keys() {
        let runtime = matches!(field.as_str(), "native_session" | "recorded_pid" | "recorded_start_time" | "artifacts");
        if !fields.contains(&field.as_str()) && !(table == StorageTable::Runs && runtime) {
            return Err(unknown("unreviewed work field"));
        }
    }
    Ok(())
}
fn work_version(value: &Value) -> Result<()> {
    if work_field::<u64>(value, "version")? != 1 { return Err(unknown("unsupported work body version")); }
    Ok(())
}
fn decode_task_work(value: &Value, table: StorageTable) -> Result<()> {
    work_keys(value, table, &["task", "version"])?;
    work_version(value)?;
    let _: &str = work_field(value, "task")?;
    Ok(())
}
fn visit_turn_work(value: &Value, table: StorageTable, artifacts: &BTreeMap<String, Digest>, roots: &mut Vec<ObjectReference>) -> Result<()> {
    work_keys(value, table, &["kind", "conversation_id", "branch_id", "request_id", "snapshot", "user_message_id", "assistant_message_id"])?;
    if work_field::<&str>(value, "kind")? != "conversation_turn" { return Err(unknown("unknown work reference tag")); }
    let _: ConversationId = work_field(value, "conversation_id")?;
    let _: BranchId = work_field(value, "branch_id")?;
    let _: uuid::Uuid = work_field(value, "request_id")?;
    let _: MessageId = work_field(value, "user_message_id")?;
    let _: MessageId = work_field(value, "assistant_message_id")?;
    let snapshot: TurnSnapshot = work_field(value, "snapshot")?;
    visit_snapshot(&snapshot, artifacts, roots)
}
fn decode_graph_job(value: &Value) -> Result<()> {
    keys(value, &["version", "node_count", "order", "roles"])?;
    // This is the graph owner's revision, not a body-format discriminator.
    let _: u64 = work_field(value, "version")?;
    let _: usize = work_field(value, "node_count")?;
    work_array::<NodeId>(value, "order")?;
    work_array::<&str>(value, "roles")
}
fn decode_graph_node(value: &Value) -> Result<()> {
    keys(value, &["version", "position", "role_id", "dependencies", "allowed_routes", "inputs", "acceptance", "workspace_mode"])?;
    work_version(value)?;
    let _: usize = work_field(value, "position")?;
    let _: &str = work_field(value, "role_id")?;
    let _: WorkspaceMode = work_field(value, "workspace_mode")?;
    work_array::<NodeId>(value, "dependencies")?;
    for field in ["allowed_routes", "inputs", "acceptance"] { work_array::<&str>(value, field)?; }
    Ok(())
}
fn decode_graph_run(value: &Value) -> Result<()> {
    work_keys(value, StorageTable::Runs, &["version", "node_id", "role_id", "workspace_mode", "allowed_routes", "route_cost_microusd", "scope"])?;
    work_version(value)?;
    let _: NodeId = work_field(value, "node_id")?;
    let _: &str = work_field(value, "role_id")?;
    let _: WorkspaceMode = work_field(value, "workspace_mode")?;
    let _: Option<u64> = work_field(value, "route_cost_microusd")?;
    work_array::<&str>(value, "allowed_routes")?;
    let scope = value.get("scope").ok_or_else(|| unknown("missing graph run scope"))?;
    let fields = ["tools", "read_roots", "write_roots", "network_destinations"];
    keys(scope, &fields)?;
    for field in fields { work_array::<&str>(scope, field)?; }
    Ok(())
}
fn visit_work(value: &Value, table: StorageTable, artifacts: &BTreeMap<String, Digest>, roots: &mut Vec<ObjectReference>) -> Result<()> {
    if value.get("kind").is_some() {
        visit_turn_work(value, table, artifacts, roots)?;
    } else if value.get("task").is_some() {
        decode_task_work(value, table)?;
    } else {
        match table {
            StorageTable::Jobs => decode_graph_job(value)?,
            StorageTable::Nodes => decode_graph_node(value)?,
            StorageTable::Runs => decode_graph_run(value)?,
            _ => return Err(unknown("unsupported work table")),
        }
    }
    // recovery::RunControl::update_body writes runtime metadata only to runs.
    // Presence requires the producer's actual type; null is not an omitted field.
    if table == StorageTable::Runs {
        if value.get("native_session").is_some() { let _: SessionRef = work_field(value, "native_session")?; }
        if value.get("recorded_pid").is_some() { let _: u64 = work_field(value, "recorded_pid")?; }
        if value.get("recorded_start_time").is_some() { let _: i64 = work_field(value, "recorded_start_time")?; }
        if let Some(ids) = value.get("artifacts") {
            let ids = ids.as_array().ok_or_else(|| unknown("unsupported run artifact references"))?;
            for id in ids {
                let id = ArtifactId::deserialize(id).map_err(|_| unknown("unsupported run artifact reference"))?;
                add_artifact(roots, artifacts, id, ReferenceReason::RunArtifact)?;
            }
        }
    }
    Ok(())
}
fn visit_action(action: &ActionSpec, roots: &mut Vec<ObjectReference>) -> Result<()> {
    let required:&[&str]=match action.kind.as_str() {
        "file_read"=>&["path"],"file_write"=>&["path","content_digest"],
        "exec"=>&["argv","cwd"],"integration"=>&["worktree_digest","base_commit"],
        "publish"=>&["artifact_digest"],"disclosure"=>&["artifact_digest","connection_id"],
        "credential_change"|"connector_change"=>&["reference","operation"],
        "browser"|"desktop"=>&["target_id","observation_digest","operation","parameters"],
        _=>return Err(unknown("unreviewed action kind")),
    };
    keys(&action.canonical_args,required)?;
    if required.iter().any(|key|action.canonical_args.get(*key).is_none()) {return Err(unknown("incomplete action arguments"));}
    match action.kind.as_str() {
        "publish" | "disclosure" => {
            let digest = action.canonical_args.get("artifact_digest").and_then(Value::as_str).ok_or_else(|| unknown("invalid action artifact reference"))?;
            add_digest(roots, Digest::parse(digest)?, ReferenceReason::ActionArtifact);
        }
        // file_write.content_digest hashes external file bytes, not CAS bytes.
        "file_read" | "file_write" | "exec" | "integration" | "credential_change" | "connector_change" | "browser" | "desktop" => {}
        _ => return Err(unknown("unreviewed action kind")),
    }
    Ok(())
}
#[derive(PartialEq, Eq)]
enum TextTreatment { Identity, Literal, Json, Locator }
fn treatment(table: StorageTable, column: &str) -> TextTreatment {
    match (table, column) {
        (StorageTable::Chunks, "locator") => TextTreatment::Locator,
        (StorageTable::Projects, "name") | (StorageTable::Conversations, "title") | (StorageTable::Chunks, "content") | (StorageTable::Sources, "uri") | (StorageTable::Memory, "provenance") => TextTreatment::Literal,
        (_, "body" | "config" | "snapshot" | "payload" | "reply") | (StorageTable::Branches, "provenance") | (StorageTable::Messages, "content") => TextTreatment::Json,
        _ => TextTreatment::Identity,
    }
}
fn require_exportable_locator(locator: &CitationLocator) -> Result<()> {
    if [&locator.section, &locator.sheet, &locator.cell_range].into_iter().flatten()
        .any(|field| redact_text(field) != *field) {
        return Err(CoreError::new("SensitiveBackupIdentity", "citation locator requires redaction; export would change historical reference identity"));
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum JsonContext { Data, Schema, Reply, Event, Action, Judgment, Score, Locator }
fn sanitize_json(value: &mut Value, context: JsonContext) -> Result<()> {
    if context == JsonContext::Locator {
        let locator = CitationLocator::deserialize(&*value).map_err(|_| unknown("invalid citation locator"))?;
        return require_exportable_locator(&locator);
    }
    match value {
        Value::Object(map) => {
            // These are explicit encoded DTO fields, never JSON-looking human
            // text. The reference visitor already validates their exact types.
            let automation_state = context == JsonContext::Event && map.get("entity_id").and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("automation-scope/") || id.starts_with("automation-consent/"));
            if automation_state {
                let state = map.get_mut("state").ok_or_else(|| unknown("missing automation state"))?;
                let mut body: Value = decode(state.as_str().ok_or_else(|| unknown("invalid automation state"))?)?;
                sanitize_json(&mut body, JsonContext::Schema)?;
                *state = Value::String(serde_json::to_string(&body)?);
            }
            for (key, value) in map.iter_mut() {
                if automation_state && key == "state" { continue; }
                let child = match (context, key.as_str()) {
                    (JsonContext::Data, _) => JsonContext::Data,
                    (JsonContext::Reply, "value" | "details") => JsonContext::Data,
                    (JsonContext::Event, "content" | "result" | "details" | "state") => JsonContext::Data,
                    (JsonContext::Event, "action") => JsonContext::Action,
                    (JsonContext::Action, "canonical_args" | "scope") => JsonContext::Data,
                    (_, "arguments" | "sdk_content" | "input_schema") => JsonContext::Data,
                    (JsonContext::Judgment, "score") => JsonContext::Score,
                    (_, "locator") => JsonContext::Locator,
                    _ => JsonContext::Schema,
                };
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                // Only the decoded JudgmentRecord.score path gets token-text
                // treatment. An arbitrary object's own type tag grants nothing.
                if context == JsonContext::Score && key == "token" {
                    sanitize_json(value, JsonContext::Data)?;
                } else if matches!(normalized.as_str(), "authorization" | "proxy_authorization" | "cookie" | "set_cookie" | "api_key" | "apikey" | "access_token" | "refresh_token" | "token" | "password" | "client_secret" | "secret" | "credential" | "credentials" | "credential_lease_id" | "lease_material" | "private_key") || normalized.ends_with("_api_key") || normalized.ends_with("_token") || normalized.ends_with("_secret") || normalized.ends_with("_password") {
                    *value = Value::String("[REDACTED]".into());
                } else if child == JsonContext::Schema && (value.is_string() || value.is_number() || value.is_boolean() || value.is_null() || value.as_array().is_some_and(|items| items.iter().all(Value::is_string)))
                    && (matches!(key.as_str(), "type" | "kind" | "id" | "state" | "status" | "harness" | "model_identity" | "model_id" | "native_id" | "native_version" | "version" | "protocol" | "role" | "path" | "root" | "cwd" | "base_ref" | "base_commit" | "service" | "account" | "profile" | "field" | "allowed_connection_ids" | "allowed_tools" | "read_roots" | "write_roots" | "network_destinations" | "allowed_harnesses" | "allowed_routes" | "connection_ids" | "route_compatibility") || key.ends_with("_id") || key.ends_with("digest")) {
                    continue;
                } else {
                    sanitize_json(value, child)?;
                }
            }
        }
        Value::Array(items) => for item in items { sanitize_json(item, context)?; },
        Value::String(text) => *text = redact_text(text),
        _ => {}
    }
    Ok(())
}

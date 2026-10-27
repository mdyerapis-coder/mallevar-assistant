use rusqlite::Connection;
use crate::error::{CoreError, Result};

/// The implemented target, not a promised future migration.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

pub fn migrate(conn: &mut Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > 1 { return Err(CoreError::new("schema_too_new", "database requires a newer core")); }
    if version == 0 {
        let tx = conn.transaction()?;
        tx.execute_batch(r#"
CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, root TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE policies (project_id TEXT NOT NULL REFERENCES projects(id), version INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY(project_id, version));
CREATE TABLE provider_references (id TEXT PRIMARY KEY, config TEXT NOT NULL);
CREATE TABLE provider_observations (id INTEGER PRIMARY KEY, connection_id TEXT NOT NULL REFERENCES provider_references(id), name TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('unknown','unavailable','unqualified','qualified')), body TEXT NOT NULL, observed_at INTEGER NOT NULL);
CREATE INDEX provider_observations_route ON provider_observations(connection_id, name, observed_at);
CREATE TABLE conversations (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), created_at INTEGER NOT NULL, title TEXT NOT NULL DEFAULT 'Untitled', archived_at INTEGER, deleted_at INTEGER);
CREATE TABLE branches (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id), parent_id TEXT REFERENCES branches(id), fork_message_id TEXT REFERENCES messages(id), provenance TEXT NOT NULL DEFAULT '{}');
CREATE TABLE messages (id TEXT PRIMARY KEY, branch_id TEXT NOT NULL REFERENCES branches(id), role TEXT NOT NULL, content TEXT NOT NULL, created_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'complete');
CREATE INDEX messages_branch ON messages(branch_id, created_at);
CREATE TABLE branch_messages (branch_id TEXT NOT NULL REFERENCES branches(id), position INTEGER NOT NULL, message_id TEXT NOT NULL REFERENCES messages(id), PRIMARY KEY(branch_id,position), UNIQUE(branch_id,message_id));
CREATE TABLE attachments (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES messages(id), digest TEXT NOT NULL, path TEXT NOT NULL);
CREATE TABLE jobs (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), body TEXT NOT NULL, state TEXT NOT NULL);
CREATE TABLE nodes (id TEXT PRIMARY KEY, job_id TEXT NOT NULL REFERENCES jobs(id), body TEXT NOT NULL, state TEXT NOT NULL);
CREATE TABLE runs (id TEXT PRIMARY KEY, node_id TEXT NOT NULL REFERENCES nodes(id), state TEXT NOT NULL, body TEXT NOT NULL);
CREATE INDEX runs_state ON runs(state);
CREATE TABLE conversation_turns (run_id TEXT PRIMARY KEY REFERENCES runs(id), conversation_id TEXT NOT NULL REFERENCES conversations(id), branch_id TEXT NOT NULL REFERENCES branches(id), request_id TEXT NOT NULL UNIQUE, request_digest TEXT NOT NULL, user_message_id TEXT REFERENCES messages(id), assistant_message_id TEXT REFERENCES messages(id), snapshot TEXT NOT NULL, dispatch_claimed INTEGER NOT NULL DEFAULT 0, last_worker_seq INTEGER NOT NULL DEFAULT 0);
CREATE INDEX conversation_turns_branch ON conversation_turns(conversation_id,branch_id);
CREATE TABLE actions (id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id), project_id TEXT NOT NULL REFERENCES projects(id), digest TEXT NOT NULL, policy_version INTEGER NOT NULL, state TEXT NOT NULL, body TEXT NOT NULL);
CREATE TABLE grants (id TEXT PRIMARY KEY, action_id TEXT NOT NULL REFERENCES actions(id), action_digest TEXT NOT NULL, expires_at INTEGER NOT NULL, revoked_at INTEGER, consumed_at INTEGER, body TEXT NOT NULL);
CREATE INDEX grants_action ON grants(action_id);
CREATE TABLE action_outcomes (action_id TEXT PRIMARY KEY REFERENCES actions(id), grant_id TEXT NOT NULL UNIQUE REFERENCES grants(id), outcome TEXT NOT NULL CHECK(outcome IN ('succeeded','failed','uncertain')), receipt_digest TEXT NOT NULL, observed_state_digest TEXT NOT NULL, recorded_at INTEGER NOT NULL);
CREATE TABLE requests (id TEXT PRIMARY KEY, command_digest TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('in_flight','completed','abandoned')), reply TEXT, accepted_at INTEGER NOT NULL, completed_at INTEGER);
CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, protocol INTEGER NOT NULL CHECK(protocol=1), request_id TEXT REFERENCES requests(id), project_id TEXT REFERENCES projects(id), run_id TEXT REFERENCES runs(id), timestamp INTEGER NOT NULL, payload TEXT NOT NULL);
CREATE INDEX events_run ON events(run_id, seq);
CREATE INDEX events_project ON events(project_id, seq);
CREATE TABLE artifacts (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), run_id TEXT REFERENCES runs(id), digest TEXT NOT NULL, path TEXT NOT NULL);
CREATE TABLE checkpoints (id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id), state_digest TEXT NOT NULL, body TEXT NOT NULL);
CREATE TABLE sources (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), uri TEXT NOT NULL);
CREATE TABLE source_revisions (id TEXT PRIMARY KEY, source_id TEXT NOT NULL REFERENCES sources(id), digest TEXT NOT NULL, created_at INTEGER NOT NULL, UNIQUE(source_id,digest));
CREATE TABLE chunks (id TEXT PRIMARY KEY, revision_id TEXT NOT NULL REFERENCES source_revisions(id), locator TEXT NOT NULL, content TEXT NOT NULL);
CREATE TABLE embeddings (chunk_id TEXT NOT NULL REFERENCES chunks(id), model_id TEXT NOT NULL, dimensions INTEGER NOT NULL, vector BLOB NOT NULL, PRIMARY KEY(chunk_id,model_id));
CREATE TABLE memory (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), provenance TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE judgments (decision_id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects(id), run_id TEXT REFERENCES runs(id), state TEXT NOT NULL, input_digest TEXT NOT NULL, model_identity TEXT NOT NULL, prompt_digest TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE import_previews (digest TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), body TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE import_receipts (digest TEXT PRIMARY KEY REFERENCES import_previews(digest), project_id TEXT NOT NULL REFERENCES projects(id), body TEXT NOT NULL, applied_at INTEGER NOT NULL);
PRAGMA user_version=1;
"#)?;
        tx.commit()?;
    }
    // Accepted work from a previous process is never redispatched implicitly.
    conn.execute("UPDATE requests SET state='abandoned' WHERE state='in_flight'", [])?;
    conn.execute("UPDATE runs SET state='blocked' WHERE state IN ('running','cancelling','awaiting_approval')", [])?;
    Ok(())
}

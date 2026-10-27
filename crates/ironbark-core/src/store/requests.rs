use rusqlite::{params, OptionalExtension, Transaction};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use crate::{contracts::*, error::{CoreError, Result}};
use super::{events::{append_tx, now_ms, redacted_json, redact_text}, RequestDisposition};

pub fn command_digest(command: &CoreCommand) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(command)?)))
}
pub fn accept(conn: &mut rusqlite::Connection, id: Uuid, command: &CoreCommand) -> Result<RequestDisposition> {
    let digest = command_digest(command)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let row: Option<(String, String, Option<String>)> = tx.query_row("SELECT command_digest,state,reply FROM requests WHERE id=?1", [id.to_string()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let result = match row {
        Some((old_digest, _, _)) if old_digest != digest => return Err(CoreError::new("request_id_conflict", "request ID is bound to a different command")),
        Some((_, state, Some(reply))) if state == "completed" => RequestDisposition::Existing(serde_json::from_str(&reply)?),
        Some(_) => RequestDisposition::InFlight,
        None => { tx.execute("INSERT INTO requests(id,command_digest,state,accepted_at) VALUES(?1,?2,'in_flight',?3)", params![id.to_string(), digest, now_ms()])?; RequestDisposition::New }
    };
    tx.commit()?;
    Ok(result)
}

pub fn projects(conn: &rusqlite::Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare("SELECT id,name,root,created_at FROM projects ORDER BY created_at,id")?;
    let mut rows = stmt.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(Project { id: ProjectId(Uuid::parse_str(&row.get::<_, String>(0)?).map_err(|_| CoreError::new("storage_corrupt", "invalid project ID"))?), name: row.get(1)?, root: row.get(2)?, created_at: row.get(3)? });
    }
    Ok(result)
}

fn ok(value: CommandResult) -> CoreReply { CoreReply::Ok { value } }
fn invalid(message: &str) -> CoreReply { CoreReply::Error { code: "invalid_command".into(), message: message.into(), details: serde_json::json!({}) } }

pub fn execute(tx: &Transaction<'_>, id: Uuid, command: &CoreCommand) -> Result<CoreReply> {
    let (digest, state, reply): (String, String, Option<String>) = tx.query_row("SELECT command_digest,state,reply FROM requests WHERE id=?1", [id.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    if digest != command_digest(command)? { return Err(CoreError::new("request_id_conflict", "request ID is bound to a different command")); }
    if state == "completed" { return Ok(serde_json::from_str(&reply.ok_or_else(|| CoreError::new("storage_corrupt", "completed request lacks reply"))?)?); }
    if state != "in_flight" { return Err(CoreError::new("reconciliation_required", "abandoned request requires explicit reconciliation")); }
    let reply = match command {
        CoreCommand::ProjectCreate { name, root } => {
            if name.trim().is_empty() || name.len() > 256 || root.len() > 4096 || !std::path::Path::new(root).is_absolute() || root.contains('\0') {
                invalid("project requires a nonempty name and absolute root")
            } else {
                let project = Project { id: ProjectId::new(), name: redact_text(name), root: redact_text(root), created_at: now_ms() };
                tx.execute("INSERT INTO projects(id,name,root,created_at) VALUES(?1,?2,?3,?4)", params![project.id.to_string(), project.name, project.root, project.created_at])?;
                append_tx(tx, Some(id), Some(project.id), None, &CoreEvent::StateChanged { entity_id: project.id.to_string(), state: "project_created".into() })?;
                ok(CommandResult::ProjectCreate { value: project })
            }
        }
        CoreCommand::ProjectList {} => ok(CommandResult::ProjectList { value: projects(tx)? }),
        CoreCommand::Events { after } => {
            // A query returns a bounded page; stream subscriptions page indefinitely.
            if *after > i64::MAX as u64 {
                CoreReply::Error { code: "invalid_cursor".into(), message: "cursor exceeds SQLite range".into(), details: serde_json::json!({}) }
            } else { ok(CommandResult::Events { value: super::events::events_page(tx, *after, 256)? }) }
        }
        CoreCommand::Status {} => {
            let count: i64 = tx.query_row("SELECT count(*) FROM projects", [], |r| r.get(0))?;
            let seq: i64 = tx.query_row("SELECT coalesce(max(seq),0) FROM events", [], |r| r.get(0))?;
            let abandoned: i64 = tx.query_row("SELECT count(*) FROM requests WHERE state='abandoned'", [], |r| r.get(0))?;
            ok(CommandResult::Status { value: StatusReport {
                protocol: 1,
                projects: u64::try_from(count).unwrap_or(0),
                last_seq: u64::try_from(seq).unwrap_or(0),
                abandoned_requests: u64::try_from(abandoned).unwrap_or(0),
            } })
        }
        CoreCommand::Doctor {} => {
            let quick_check: String = tx.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
            let foreign_keys: i64 = tx.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
            let schema: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
            let journal: String = tx.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
            ok(CommandResult::Doctor { value: DoctorReport {
                protocol: 1, sqlite_integrity: quick_check, foreign_keys: foreign_keys == 1,
                schema_version: u64::try_from(schema).unwrap_or(0), journal_mode: journal,
            } })
        }
        CoreCommand::Shutdown {} => ok(CommandResult::Shutdown { value: ShutdownReceipt { shutting_down: true } }),
        _ => invalid("command is owned by the service dispatcher, not the transactional leaf path"),
    };
    // The reply and every mutation/event commit together, then and only then reach IPC.
    complete_tx(tx, id, command, &reply)
}

/// Leaf dispatchers call this inside the same transaction as their final durable
/// events/outcome. It cannot overwrite completed/abandoned work or a mismatched ID.
pub fn complete_tx(tx: &Transaction<'_>, id: Uuid, command: &CoreCommand, reply: &CoreReply) -> Result<CoreReply> {
    let serialized = redacted_json(reply)?;
    if serialized.len() > crate::ipc::MAX_FRAME - 1024 {
        return Err(CoreError::new("frame_too_large", "reply exceeds recoverable IPC frame limit"));
    }
    let changed = tx.execute("UPDATE requests SET state='completed',reply=?2,completed_at=?3 WHERE id=?1 AND state='in_flight' AND command_digest=?4", params![id.to_string(), serialized, now_ms(), command_digest(command)?])?;
    if changed != 1 { return Err(CoreError::new("request_not_in_flight", "request must be an accepted, matching dispatch")); }
    Ok(serde_json::from_str(&serialized)?)
}

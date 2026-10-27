pub mod backup;
pub mod events;
pub mod imports;
pub mod migrations;
pub mod objects;
pub mod preflight;
pub mod requests;
pub mod retention;
pub mod schema;

use std::{os::fd::AsRawFd, path::Path, sync::Arc};
use tokio::sync::{mpsc, oneshot, Notify};
use rusqlite::{Connection, Transaction};
use uuid::Uuid;
use crate::{contracts::*, error::{CoreError, Result}, paths::{directory_handle, private_file_at}};

#[derive(Debug, Clone, PartialEq)]
pub enum RequestDisposition { New, Existing(CoreReply), InFlight }
type Operation = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// Every clone talks to one bounded, dedicated SQLite thread. No SQLite work
/// runs on a Tokio worker, and no database lock is held across an async await.
#[derive(Clone)]
pub struct Store { sender: mpsc::Sender<Operation>, changed: Arc<Notify> }
impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| CoreError::new("unsafe_path", "database requires a parent directory"))?;
        let name = path.file_name().ok_or_else(|| CoreError::new("unsafe_path", "database requires a filename"))?;
        let directory = directory_handle(parent, false)?;
        let checked_file = private_file_at(&directory, name)?;
        let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
        let (sender, mut receiver) = mpsc::channel::<Operation>(64);
        let (ready_tx, ready_rx) = oneshot::channel();
        std::thread::Builder::new().name("ironbark-sqlite".into()).spawn(move || {
            // Keep the checked directory/file alive across SQLite and WAL operations.
            let _directory = directory;
            let _checked_file = checked_file;
            let initialized = Connection::open(path).map_err(CoreError::from).and_then(|mut conn| { migrations::migrate(&mut conn)?; Ok(conn) });
            match initialized {
                Ok(mut conn) => {
                    if ready_tx.send(Ok(())).is_err() { return; }
                    while let Some(operation) = receiver.blocking_recv() { operation(&mut conn); }
                }
                Err(error) => { let _ = ready_tx.send(Err(error)); }
            }
        })?;
        ready_rx.await.map_err(|_| CoreError::new("storage_closed", "database initialization stopped"))??;
        Ok(Self { sender, changed: Arc::new(Notify::new()) })
    }

    pub async fn with_connection<T, F>(&self, operation: F) -> Result<T>
    where T: Send + 'static, F: FnOnce(&mut Connection) -> Result<T> + Send + 'static {
        let (reply_tx, reply_rx) = oneshot::channel();
        // Never dispatch after callers have timed out while waiting for queue capacity.
        let permit = tokio::time::timeout(std::time::Duration::from_secs(5), self.sender.reserve()).await
            .map_err(|_| CoreError::new("storage_busy", "database queue is full"))?
            .map_err(|_| CoreError::new("storage_closed", "database worker stopped"))?;
        permit.send(Box::new(move |conn| { let _ = reply_tx.send(operation(conn)); }));
        // Once dispatched, wait for the actual transactional outcome; no fake timeout reply.
        reply_rx.await.map_err(|_| CoreError::new("storage_closed", "database worker stopped"))?
    }

    /// Leaf modules use this seam for transactional durable operations. Any
    /// credential-bearing body must go through events::redacted_json first.
    pub async fn with_transaction<T, F>(&self, operation: F) -> Result<T>
    where T: Send + 'static, F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static {
        let result = self.with_connection(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let result = operation(&tx)?;
            tx.commit()?;
            Ok(result)
        }).await?;
        self.changed.notify_waiters();
        Ok(result)
    }

    pub async fn accept_request(&self, id: Uuid, command: &CoreCommand) -> Result<RequestDisposition> {
        let command = command.clone();
        self.with_connection(move |conn| requests::accept(conn, id, &command)).await
    }
    pub async fn execute_accepted(&self, id: Uuid, command: &CoreCommand) -> Result<CoreReply> {
        let command = command.clone();
        self.with_transaction(move |tx| requests::execute(tx, id, &command)).await
    }
    /// Durably persist a service command's redacted reply under an already-accepted request
    /// ID. Asserts the request is still `in_flight` with a matching command digest, commits
    /// the reply in its own transaction, and marks it completed. Unlike `execute_accepted`
    /// this does not itself run the (non-transactional) service work — it only records the
    /// outcome the service produced. A replay of a completed request returns the stored reply.
    pub async fn complete_request(&self, id: Uuid, command: &CoreCommand, reply: &CoreReply) -> Result<CoreReply> {
        let command = command.clone();
        let reply = reply.clone();
        self.with_transaction(move |tx| requests::complete_tx(tx, id, &command, &reply)).await
    }
    pub async fn append(&self, run_id: Option<RunId>, event: CoreEvent) -> Result<EventEnvelope> {
        self.with_transaction(move |tx| events::append_tx(tx, None, None, run_id, &event)).await
    }
    pub async fn events_page(&self, seq: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
        self.with_connection(move |conn| events::events_page(conn, seq, limit.clamp(1, 256))).await
    }
    pub async fn events_after(&self, seq: u64) -> Result<Vec<EventEnvelope>> {
        let mut cursor = seq;
        let mut events = Vec::new();
        loop {
            let page = self.events_page(cursor, 256).await?;
            if page.is_empty() { return Ok(events); }
            cursor = page.last().expect("nonempty page").seq;
            events.extend(page);
        }
    }
    pub async fn projects(&self) -> Result<Vec<Project>> { self.with_connection(|conn| requests::projects(conn)).await }
    pub async fn abandoned_requests(&self) -> Result<Vec<Uuid>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM requests WHERE state='abandoned' ORDER BY accepted_at,id")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|row| Uuid::parse_str(&row?).map_err(|_| CoreError::new("storage_corrupt", "invalid request ID"))).collect()
        }).await
    }
    pub fn notification(&self) -> Arc<Notify> { self.changed.clone() }
}

impl Default for Store {
    fn default() -> Self {
        let (sender, mut receiver) = mpsc::channel::<Operation>(64);
        let changed = Arc::new(Notify::new());
        std::thread::Builder::new().name("ironbark-sqlite-default".into()).spawn(move || {
            let mut conn = Connection::open_in_memory().expect("in-memory SQLite");
            migrations::migrate(&mut conn).expect("migrations");
            let mut conn = conn;
            while let Some(operation) = receiver.blocking_recv() { operation(&mut conn); }
        }).expect("sqlite thread");
        Self { sender, changed }
    }
}

//! Native presentation client for the durable Ironbark core (plan D1).
//!
//! The core is the only scheduler/approval authority; this crate is transport +
//! typed projection glue. It authenticates over the user-scoped Unix socket — the
//! session nonce is read natively and never enters JS — dispatches typed commands
//! with caller-stable request IDs, and streams durable event envelopes to the
//! webview. No shell, SQL, file or worker-launch capability is exposed: the only
//! custom commands are the three typed core operations below.

use std::sync::atomic::{AtomicU64, Ordering};

use ironbark_core::{
    contracts::{CoreCommand, CoreReply},
    ipc::CoreClient,
    paths::RuntimePaths,
};
use tauri::{Emitter, State};
use uuid::Uuid;

/// Shared high-water mark for the durable event stream. A reconnect resumes from
/// the last delivered sequence, so native replay never re-delivers events the
/// webview already applied (the TS projection still does its own gap check).
#[derive(Default, Clone)]
struct EventCursor(std::sync::Arc<AtomicU64>);

fn runtime_paths() -> Result<RuntimePaths, String> {
    RuntimePaths::from_env().map_err(|e| format!("core runtime paths: {e}"))
}

/// Open a fresh authenticated command socket, dispatch one command under the
/// caller's stable request ID, return the core reply. The core de-duplicates by
/// request ID, so a retry after a transient failure is safe.
async fn dispatch(request_id: Uuid, command: CoreCommand) -> Result<CoreReply, String> {
    let paths = runtime_paths()?;
    let mut client = CoreClient::connect(&paths).await.map_err(|e| format!("connect: {e}"))?;
    client.request(request_id, command).await.map_err(|e| format!("dispatch: {e}"))
}

/// `connect_core()`: authenticate against the running core and return its status
/// snapshot. This is the webview's entry point; the nonce is consumed natively.
#[tauri::command]
async fn connect_core() -> Result<CoreReply, String> {
    dispatch(Uuid::new_v4(), CoreCommand::Status {}).await
}

/// `core_request(request_id, command)`: typed dispatch under a stable request ID.
/// The webview generates the ID once and reuses it on retry, so duplicate sends
/// collapse in the core instead of double-dispatching.
#[tauri::command]
async fn core_request(request_id: Uuid, command: CoreCommand) -> Result<CoreReply, String> {
    dispatch(request_id, command).await
}

/// `subscribe_events(after_seq)`: dedicated durable subscription socket with pull
/// replay from `after_seq` and automatic reconnect from the high-water mark.
/// Envelopes are emitted to the webview as `core-event`; the webview only ever
/// sees `EventEnvelope` JSON.
#[tauri::command]
async fn subscribe_events(app: tauri::AppHandle, after_seq: u64, state: State<'_, EventCursor>) -> Result<(), String> {
    let cursor = state.0.clone();
    cursor.fetch_max(after_seq, Ordering::Relaxed);
    let paths = runtime_paths()?;
    eprintln!("ironbark-desktop: subscribe_events invoked after_seq={after_seq} socket={}", paths.socket.display());
    let _ = tauri::async_runtime::spawn(async move {
        loop {
            let after = cursor.load(Ordering::Relaxed);
            let mut client = match CoreClient::subscribe(&paths, after).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ironbark-desktop: subscribe connect failed: {e}; retrying in 2s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };
            loop {
                match client.next_event().await {
                    Ok(event) => {
                        cursor.fetch_max(event.seq, Ordering::Relaxed);
                        if app.emit("core-event", &event).is_err() {
                            // Webview is gone; the subscription task is pointless.
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("ironbark-desktop: subscription ended: {e}; reconnecting");
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .manage(EventCursor::default())
        .invoke_handler(tauri::generate_handler![connect_core, core_request, subscribe_events])
        .run(tauri::generate_context!())
        .expect("error while running Ironbark Assistant");
}

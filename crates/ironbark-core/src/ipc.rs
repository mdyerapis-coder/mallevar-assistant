use std::{sync::Arc, time::Duration};
use tokio::{io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt}, net::{UnixListener, UnixStream}, sync::{watch, Semaphore}};
use crate::{contracts::*, dispatch::CoreServices, error::{CoreError, Result}, paths::current_uid, store::{RequestDisposition, Store}};

pub const MAX_FRAME: usize = 1024 * 1024;
const MAX_PEERS: usize = 64;

pub fn authorize_peer(expected_uid: u32, actual_uid: u32, protocol: u8, presented_nonce: &str, expected_nonce: &str) -> Result<()> {
    if expected_uid != actual_uid { return Err(CoreError::new("foreign_peer", "peer UID is not authorized")); }
    if protocol != 1 { return Err(CoreError::new("unsupported_protocol", "core requires protocol 1")); }
    // Nonce comparison does not exit early based on matching secret bytes.
    let mut difference = presented_nonce.len() ^ expected_nonce.len();
    for (a, b) in presented_nonce.bytes().zip(expected_nonce.bytes()) { difference |= (a ^ b) as usize; }
    if expected_nonce.is_empty() || difference != 0 { return Err(CoreError::new("bad_nonce", "session nonce is not authorized")); }
    Ok(())
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<ClientFrame>> {
    let line = read_line(reader).await?;
    line.map(|bytes| serde_json::from_slice(&bytes).map_err(CoreError::from)).transpose()
}
/// Reads one NDJSON frame without consuming bytes belonging to the next one.
/// Incremental buffered scanning bounds allocation even without a newline.
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() { return Ok(None); }
            return Err(CoreError::new("truncated_frame", "IPC frame ended before newline"));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if bytes.len().saturating_add(count) > MAX_FRAME {
            return Err(CoreError::new("frame_too_large", "IPC frame exceeds 1 MiB"));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() { return Ok(Some(bytes)); }
    }
}

pub async fn write_json<W: AsyncWrite + Unpin, T: serde::Serialize>(writer: &mut W, frame: &T) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    if bytes.len() > MAX_FRAME { return Err(CoreError::new("frame_too_large", "IPC reply exceeds 1 MiB")); }
    tokio::time::timeout(Duration::from_secs(30), async {
        writer.write_all(&bytes).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    }).await.map_err(|_| CoreError::new("slow_subscriber", "client stopped draining IPC; reconnect from last received sequence"))??;
    Ok(())
}
fn error_frame(error: CoreError, after_seq: u64) -> ServerFrame { ServerFrame::Error { code: error.code, message: error.message, after_seq } }

async fn serve_peer(stream: UnixStream, nonce: Arc<String>, store: Store, services: CoreServices, shutdown: watch::Sender<bool>, mut stop: watch::Receiver<bool>) -> Result<()> {
    let uid = stream.peer_cred()?.uid();
    if uid != current_uid() { return Err(CoreError::new("foreign_peer", "peer UID is not authorized")); }
    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let hello = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut reader)).await
        .map_err(|_| CoreError::new("hello_timeout", "client did not authenticate"))??;
    let authorization = match hello {
        Some(ClientFrame::Hello { protocol, session_nonce }) => authorize_peer(current_uid(), uid, protocol, &session_nonce, &nonce),
        _ => Err(CoreError::new("hello_required", "first frame must authenticate protocol and nonce")),
    };
    if let Err(error) = authorization { let _ = write_json(&mut writer, &error_frame(error, 0)).await; return Ok(()); }
    write_json(&mut writer, &ServerFrame::Hello { protocol: 1 }).await?;
    loop {
        let frame = tokio::select! {
            changed = stop.changed() => { let _ = changed; return Ok(()); }
            frame = read_frame(&mut reader) => frame,
        };
        let frame = match frame {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(error) => { let _ = write_json(&mut writer, &error_frame(error, 0)).await; return Ok(()); }
        };
        match frame {
            ClientFrame::Request { request_id, command } => {
                let is_shutdown = matches!(command, CoreCommand::Shutdown {});
                let mut freshly_executed = false;
                let reply = match store.accept_request(request_id, &command).await {
                    Ok(RequestDisposition::New) => {
                        freshly_executed = true;
                        if CoreServices::is_service_command(&command) {
                            Ok(services.dispatch(request_id, &command).await)
                        } else {
                            store.execute_accepted(request_id, &command).await
                        }
                    }
                    Ok(RequestDisposition::Existing(reply)) => Ok(reply),
                    Ok(RequestDisposition::InFlight) => Err(CoreError::new("request_in_flight", "request is accepted or abandoned; reconcile rather than redispatch")),
                    Err(error) => Err(error),
                };
                let reply = reply.unwrap_or_else(|error| CoreReply::Error { code: error.code, message: error.message, details: serde_json::json!({}) });
                let shutdown_ok = is_shutdown && freshly_executed && matches!(reply, CoreReply::Ok { .. });
                write_json(&mut writer, &ServerFrame::Reply { request_id, reply }).await?;
                if shutdown_ok { let _ = shutdown.send(true); return Ok(()); }
            }
            ClientFrame::Subscribe { after_seq } => {
                // Durable pull replay, bounded 256-event backlog; no lossy broadcast.
                // A slow writer stalls only its own subscriber, never SQLite or peers.
                let notification = store.notification();
                let mut cursor = after_seq;
                loop {
                    let notified = notification.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let page = store.events_page(cursor, 256).await?;
                    if !page.is_empty() {
                        for event in page {
                            let seq = event.seq;
                            if let Err(error) = write_json(&mut writer, &ServerFrame::Event { event: Box::new(event) }).await {
                                let _ = write_json(&mut writer, &error_frame(error.clone(), cursor)).await;
                                return Err(error);
                            }
                            cursor = seq;
                        }
                        continue;
                    }
                    tokio::select! {
                        _ = notified => {},
                        _ = stop.changed() => return Ok(()),
                        input = read_frame(&mut reader) => {
                            if input?.is_none() { return Ok(()); }
                            write_json(&mut writer, &error_frame(CoreError::new("subscription_only", "subscription connections cannot dispatch commands"), cursor)).await?;
                            return Ok(());
                        }
                    }
                }
            }
            ClientFrame::Hello { .. } => { write_json(&mut writer, &error_frame(CoreError::new("already_authenticated", "hello is only valid as first frame"), 0)).await?; return Ok(()); }
        }
    }
}

pub async fn serve(listener: UnixListener, nonce: String, store: Store, paths: crate::paths::RuntimePaths) -> Result<()> {
    let services = CoreServices::new(store.clone(), &paths);
    let nonce = Arc::new(nonce);
    let peers = Arc::new(Semaphore::new(MAX_PEERS));
    let (shutdown, mut stop) = watch::channel(false);
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = stop.changed() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = peers.clone().try_acquire_owned() else { drop(stream); continue; };
                let nonce = nonce.clone(); let store = store.clone(); let services = services.clone(); let shutdown = shutdown.clone(); let stop = stop.clone();
                tasks.spawn(async move { let _permit = permit; serve_peer(stream, nonce, store, services, shutdown, stop).await });
            }
        }
    }
    let _ = shutdown.send(true);
    // Stop subscribers and wait for already dispatched transactional replies.
    if tokio::time::timeout(Duration::from_secs(35), async { while tasks.join_next().await.is_some() {} }).await.is_err() { tasks.abort_all(); }
    Ok(())
}

pub struct CoreClient { reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>, writer: tokio::net::unix::OwnedWriteHalf }
impl CoreClient {
    /// Authenticate one socket. A connection is either a command dispatcher (request) or an
    /// event subscriber (subscribe), never both — the core rejects commands on a subscription
    /// connection. Desktop/CLI clients therefore open a dedicated socket for each role.
    async fn authenticate(paths: &crate::paths::RuntimePaths) -> Result<Self> {
        let stream = UnixStream::connect(&paths.socket).await?;
        if stream.peer_cred()?.uid() != current_uid() { return Err(CoreError::new("foreign_peer", "daemon UID is not authorized")); }
        let (reader, writer) = stream.into_split();
        let mut client = Self { reader: tokio::io::BufReader::new(reader), writer };
        write_json(&mut client.writer, &ClientFrame::Hello { protocol: 1, session_nonce: paths.read_nonce()? }).await?;
        match client.next_frame().await? { ServerFrame::Hello { protocol: 1 } => Ok(client), _ => Err(CoreError::new("hello_rejected", "core rejected authentication")) }
    }
    /// Open a dedicated event subscription socket and begin durable pull replay from `after_seq`.
    pub async fn subscribe(paths: &crate::paths::RuntimePaths, after_seq: u64) -> Result<Self> {
        let mut client = Self::authenticate(paths).await?;
        write_json(&mut client.writer, &ClientFrame::Subscribe { after_seq }).await?;
        Ok(client)
    }
    /// Read the next server frame; on a subscription socket this is the next `Event`
    /// envelope. Blocks until an event arrives or the core closes the connection — a
    /// silent core is the normal steady state, not an error, so there is no timeout here
    /// (unlike `next_frame`, where a command reply is due). A closed socket reads as
    /// `None`, which is the disconnect signal the caller reconnects on.
    pub async fn next_event(&mut self) -> Result<EventEnvelope> {
        let bytes = read_line(&mut self.reader).await?
            .ok_or_else(|| CoreError::new("core_disconnected", "core closed subscription"))?;
        match serde_json::from_slice::<ServerFrame>(&bytes)? { ServerFrame::Event { event } => Ok(*event), _ => Err(CoreError::new("unexpected_frame", "subscription socket returned a non-event frame")) }
    }
    pub async fn connect(paths: &crate::paths::RuntimePaths) -> Result<Self> {
        let stream = UnixStream::connect(&paths.socket).await?;
        if stream.peer_cred()?.uid() != current_uid() { return Err(CoreError::new("foreign_peer", "daemon UID is not authorized")); }
        let (reader, writer) = stream.into_split();
        let mut client = Self { reader: tokio::io::BufReader::new(reader), writer };
        write_json(&mut client.writer, &ClientFrame::Hello { protocol: 1, session_nonce: paths.read_nonce()? }).await?;
        match client.next_frame().await? { ServerFrame::Hello { protocol: 1 } => Ok(client), _ => Err(CoreError::new("hello_rejected", "core rejected authentication")) }
    }
    pub async fn next_frame(&mut self) -> Result<ServerFrame> {
        let bytes = tokio::time::timeout(Duration::from_secs(30), read_line(&mut self.reader)).await.map_err(|_| CoreError::new("response_timeout", "core did not answer"))??
            .ok_or_else(|| CoreError::new("core_disconnected", "core closed connection"))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub async fn request(&mut self, id: Uuid, command: CoreCommand) -> Result<CoreReply> {
        write_json(&mut self.writer, &ClientFrame::Request { request_id: id, command }).await?;
        match self.next_frame().await? { ServerFrame::Reply { request_id, reply } if request_id == id => Ok(reply), _ => Err(CoreError::new("unexpected_reply", "core reply correlation mismatch")) }
    }
}
use uuid::Uuid;

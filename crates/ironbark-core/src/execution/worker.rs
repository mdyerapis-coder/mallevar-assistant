use crate::{contracts::*, error::{CoreError, Result}, policy::{PolicyDecision, service::ApprovalService}, process::{LaunchSpec, NamespaceBootstrap, ProcessIdentity, ProcessLease, ProcessSupervisor, StopReceipt}, store::{Store, events::now_ms}};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{collections::{HashMap, HashSet}, fs::File, io::Read, os::{fd::{AsRawFd, FromRawFd}, unix::{ffi::OsStrExt, fs::MetadataExt, net::UnixStream as StdStream}}, path::{Path, PathBuf}, sync::{Arc, Mutex as SyncMutex}, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::UnixStream, sync::{mpsc, oneshot, Mutex}};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_FRAME: usize = 1_048_576;
const MAX_PENDING: usize = 128;
fn error(code: &str) -> CoreError { CoreError::new(code, "private worker operation refused") }
fn correlation() -> CoreError { error("worker_correlation") }
fn denied() -> CoreError { error("worker_permission_denied") }
fn hash(bytes: &[u8]) -> Digest { Digest::parse(&format!("{:x}", Sha256::digest(bytes))).expect("SHA256 hex") }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeMethod {
    Hello {}, Describe {}, Open { input: OpenSessionInput },
    Turn { session: SessionRef, input: TurnInput },
    AnswerPermission { session: SessionRef, answer: PermissionAnswer },
    Interrupt { session: SessionRef }, ExportContext { session: SessionRef }, Close { session: SessionRef },
}

impl NativeMethod {
    fn as_turn_input(&self) -> std::result::Result<TurnInput, CoreError> {
        match self {
            NativeMethod::Turn { input, .. } => Ok(input.clone()),
            _ => Err(correlation()),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeReply {
    Hello { protocol: u8 }, Describe { descriptor: HarnessDescriptor }, Open { session: SessionRef },
    Turn {}, AnswerPermission {}, Interrupt {}, ExportContext { context: ContextPackage }, Close {},
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError { code: String }
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Incoming {
    Reply {
        protocol: u8, id: Uuid, run_id: RunId,
        #[serde(default, deserialize_with = "present_non_null")] result: Option<NativeReply>,
        #[serde(default, deserialize_with = "present_non_null")] error: Option<WireError>,
    },
    Event { protocol: u8, id: Uuid, run_id: RunId, event: WorkerEvent },
    HostRequest { protocol: u8, id: Uuid, request_id: Uuid, run_id: RunId, session_id: String, method: HostMethod },
}
fn present_non_null<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}
#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativePermissionRequest { request_id: String, session_id: String, name: String, tool_call_id: String, input: Value, raw: Value }
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum HostMethod {
    State { input: OpenSessionInput, turn: Option<TurnInput> },
    PrepareAction { request: NativePermissionRequest, input: OpenSessionInput },
    VerifyAnswer { answer: PermissionAnswer, request: NativePermissionRequest },
    RecordNativeEvent { event: Value, input: OpenSessionInput },
}

/// Exact, read-only namespace runtime inputs, selected only by the native owner.
pub struct ManagedBunLaunch { pub bun: PathBuf, pub entry: PathBuf, pub config: PathBuf }
impl ManagedBunLaunch {
    pub fn apply(&self, mut spec: LaunchSpec) -> Result<LaunchSpec> {
        crate::process::sandbox::validate_env(&spec.env_allowlist)?;
        for path in [&self.bun, &self.entry, &self.config] {
            crate::process::sandbox::validate_destination(path)?;
            let source = spec.runtime.files.get(path).ok_or_else(|| error("containment_denied"))?;
            if source.canonicalize()? != *source || !source.is_file() || source.metadata()?.nlink() != 1 || spec.writable_mounts.iter().any(|root| path.starts_with(root) || source.starts_with(root)) {
                return Err(error("containment_denied"));
            }
        }
        let mut argv = vec!["--no-env-file".into(), "--no-install".into(), format!("--config={}", self.config.to_str().ok_or_else(|| error("containment_denied"))?), self.entry.to_str().ok_or_else(|| error("containment_denied"))?.into()];
        argv.append(&mut spec.argv);
        spec.executable = self.bun.clone(); spec.argv = argv; Ok(spec)
    }
}

#[derive(Clone)]
pub struct WorkerHost {
    store: Store, project: ProjectId, run: RunId, root: Option<(PathBuf, Arc<File>)>,
    #[cfg(test)]
    verify_snapshot_barrier: Option<Arc<VerifySnapshotBarrier>>,
}
struct Snapshot { policy: ProjectPolicy, root: PathBuf, root_fd: Option<Arc<File>>, state: Digest }
impl WorkerHost {
    pub fn new(store: Store, project: ProjectId, run: RunId) -> Self {
        Self { store, project, run, root: None,
            #[cfg(test)]
            verify_snapshot_barrier: None,
        }
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        let (project, run, pinned) = (self.project, self.run, self.root.clone());
        self.store.with_connection(move |db| {
            let row: Option<(String, String, String, String)> = db.query_row(
                "SELECT j.project_id,p.root,r.state,r.body FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id JOIN projects p ON p.id=j.project_id WHERE r.id=?1",
                [run.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
            let (owner, root, state, body) = row.ok_or_else(correlation)?;
            if owner != project.to_string() || state != "running" { return Err(correlation()); }
            let policy: String = db.query_row("SELECT body FROM policies WHERE project_id=?1 ORDER BY version DESC LIMIT 1", [project.to_string()], |row| row.get(0))?;
            let policy: ProjectPolicy = serde_json::from_str(&policy)?;
            if policy.project_id != project { return Err(correlation()); }
            let digest = hash(&serde_json::to_vec(&json!({"run":run,"project":project,"state":state,"body":body,"policy":policy}))?);
            if let Some((path,fd)) = &pinned {
                if path != Path::new(&root) || std::fs::read_link(format!("/proc/self/fd/{}",fd.as_raw_fd()))? != *path { return Err(correlation()); }
            }
            Ok(Snapshot { policy, root: PathBuf::from(root), root_fd:pinned.map(|(_,fd)|fd), state: digest })
        }).await
    }
    fn check_input(&self, input: &OpenSessionInput, snapshot: &Snapshot) -> Result<()> {
        if input.project_id != self.project || input.run_id != self.run || input.policy != snapshot.policy
            || !snapshot.policy.allowed_connection_ids.contains(&input.route.connection_id)
            || Path::new(&input.cwd).canonicalize()? != Path::new(&input.cwd)
            || !Path::new(&input.cwd).starts_with(&snapshot.root) { return Err(denied()); }
        Ok(())
    }
    async fn prepare(&self, request: &NativePermissionRequest, input: &OpenSessionInput) -> Result<ActionSpec> {
        let snapshot = self.snapshot().await?; self.check_input(input, &snapshot)?;
        // Native tool prose, hashes and arbitrary SDK argument shapes are not authority.
        // Add a mapping only when its canonical shape and native sink are understood.
        let canonical_args = match request.name.as_str() {
            "file_write" => &request.input,
            "acp.toolPermission" => {
                let wrapper=request.input.as_object().ok_or_else(denied)?;
                if wrapper.get("name").and_then(Value::as_str)!=Some("file_write")
                    || wrapper.keys().any(|key|!["name","title","kind","rawInput"].contains(&key.as_str())) {return Err(denied());}
                wrapper.get("rawInput").ok_or_else(denied)?
            },
            _ => return Err(denied()),
        };
        let args = canonical_args.as_object().ok_or_else(denied)?;
        if args.len() != 2 || !args.contains_key("path") || !args.contains_key("content_digest") { return Err(denied()); }
        let path = PathBuf::from(args["path"].as_str().ok_or_else(denied)?);
        Digest::parse(args["content_digest"].as_str().ok_or_else(denied)?)?;
        if !path.is_absolute() || !snapshot.policy.write_roots.iter().any(|root| path.starts_with(root)) { return Err(denied()); }
        let expected_state_digest = observe_resource(&snapshot, &path)?;
        let action = ActionSpec { id: ActionId::new(), project_id: self.project, run_id: self.run, kind: "file_write".into(), canonical_args: canonical_args.clone(), scope: json!({"tools":["file_write"],"paths":[path]}), destination: None, policy_version: snapshot.policy.version, expected_state_digest };
        if matches!(ApprovalService::new(self.store.clone()).record(action.clone(), now_ms()).await?, PolicyDecision::Deny(_)) { return Err(denied()); }
        Ok(action)
    }
    async fn verify(&self, permission: &PreparedPermission, answer: &PermissionAnswer) -> Result<bool> {
        if answer.native_request_id != permission.request.request_id || answer.action != permission.action { return Ok(false); }
        let snapshot = self.snapshot().await?;
        #[cfg(test)]
        if let Some(barrier) = &self.verify_snapshot_barrier {
            barrier.entered.notify_one();
            tokio::time::timeout(Duration::from_secs(8), barrier.release.notified()).await.map_err(|_| correlation())?;
        }
        if snapshot.policy.version != permission.action.policy_version { return Ok(false); }
        if answer.decision == PermissionDecision::Deny { return Ok(answer.grant.is_none()); }
        let Some(grant) = &answer.grant else { return Ok(false); };
        // Compare the complete supplied grant to the durable grant before consumption.
        let id = grant.id.to_string();
        let stored = self.store.with_connection(move |db| {
            let body: Option<String> = db.query_row("SELECT body FROM grants WHERE id=?1", [id], |row| row.get(0)).optional()?;
            body.map(|body| serde_json::from_str::<ApprovalGrant>(&body).map_err(Into::into)).transpose()
        }).await?;
        if stored.as_ref() != Some(grant) { return Ok(false); }
        let path = Path::new(permission.action.canonical_args["path"].as_str().ok_or_else(denied)?);
        let observed = observe_resource(&snapshot, path)?;
        Ok(ApprovalService::new(self.store.clone()).consume_while_running(permission.action.clone(), grant.id, observed, now_ms()).await.is_ok())
    }
    async fn verify_action_event(&self, action: &ActionSpec) -> Result<()> {
        if action.run_id!=self.run || action.project_id!=self.project {return Err(correlation());}
        let (id,run,project)=(action.id.to_string(),self.run.to_string(),self.project.to_string());
        let stored=self.store.with_connection(move |db| {
            let body:Option<String>=db.query_row(
                "SELECT a.body FROM actions a JOIN runs r ON r.id=a.run_id JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE a.id=?1 AND a.run_id=?2 AND a.project_id=?3 AND j.project_id=?3",
                rusqlite::params![id,run,project],|row|row.get(0)).optional()?;
            body.map(|body|serde_json::from_str::<ActionSpec>(&body).map_err(Into::into)).transpose()
        }).await?;
        if stored.as_ref()!=Some(action) {return Err(denied());}
        Ok(())
    }
    async fn audit(&self, request: Uuid, event: &Value) -> Result<()> {
        let event_digest = hash(&serde_json::to_vec(event)?);
        let event = CoreEvent::StateChanged { entity_id: self.run.to_string(), state: format!("native_observation:{}", event_digest.as_str()) };
        let (project,run) = (self.project,self.run);
        self.store.with_transaction(move |tx| { crate::store::events::append_tx(tx, Some(request), Some(project), Some(run), &event)?; Ok(()) }).await
    }
}

// Observe through a pinned root and openat2, rejecting all symlinks and submounts.
// The digest includes current stored run/policy and actual native inode/content state,
// never a client-provided context/state digest.
fn observe_resource(snapshot: &Snapshot, path: &Path) -> Result<Digest> {
    let relative = path.strip_prefix(&snapshot.root).map_err(|_| denied())?;
    if relative.as_os_str().is_empty() || relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) { return Err(denied()); }
    let root = snapshot.root_fd.as_ref().ok_or_else(denied)?;
    #[repr(C)] struct OpenHow { flags: u64, mode: u64, resolve: u64 }
    let open = |relative: &Path, flags: i32| -> Result<File> {
        let path = std::ffi::CString::new(relative.as_os_str().as_bytes()).map_err(|_| denied())?;
        let how = OpenHow { flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64, mode: 0, resolve: 0x01|0x02|0x04|0x08 };
        let fd = unsafe { libc::syscall(libc::SYS_openat2, root.as_raw_fd(), path.as_ptr(), &how, std::mem::size_of::<OpenHow>()) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    };
    let parent = relative.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent = open(parent, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let meta = parent.metadata()?;
    let mut hash_state = Sha256::new();
    hash_state.update(snapshot.state.as_str()); hash_state.update(path.as_os_str().as_bytes());
    hash_state.update(meta.dev().to_be_bytes()); hash_state.update(meta.ino().to_be_bytes());
    // Missing leaf is a real state, but any other open failure denies observation.
    let name = std::ffi::CString::new(relative.file_name().ok_or_else(denied)?.as_bytes()).map_err(|_| denied())?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY|libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK) };
    if fd < 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) { return Err(denied()); }
        hash_state.update(b"absent");
    } else {
        let mut file = unsafe { File::from_raw_fd(fd) }; let before = file.metadata()?;
        if !before.is_file() || before.dev() != meta.dev() || before.nlink()!=1 || before.len()>50*1024*1024 { return Err(denied()); }
        hash_state.update(before.dev().to_be_bytes()); hash_state.update(before.ino().to_be_bytes());
        let mut buffer = [0_u8;8192]; let mut total=0_u64;
        loop {
            let count = file.read(&mut buffer)?; if count == 0 { break; }
            total+=count as u64; if total>50*1024*1024 { return Err(denied()); }
            hash_state.update(&buffer[..count]);
        }
        let after = file.metadata()?;
        if (before.len(),before.mtime(),before.mtime_nsec(),before.ctime(),before.ctime_nsec()) != (after.len(),after.mtime(),after.mtime_nsec(),after.ctime(),after.ctime_nsec()) { return Err(denied()); }
    }
    Digest::parse(&format!("{:x}", hash_state.finalize()))
}

struct Pending { method: NativeMethod, reply: oneshot::Sender<Result<NativeReply>>, native_turn: Option<String>, finished: bool, poisoned: bool }
#[derive(Clone)]
struct Session { reference: SessionRef, input: OpenSessionInput }
#[derive(Clone)]
struct PreparedPermission { request: NativePermissionRequest, action: ActionSpec, verified: bool, allowed: bool, started: bool, finished: bool, control_id: Uuid }
#[derive(Default)]
struct State {
    pending: HashMap<Uuid, Pending>, sessions: HashMap<String, Session>,
    permissions: HashMap<String, PreparedPermission>, callbacks: HashMap<Uuid, HashSet<Uuid>>, provisional: HashMap<Uuid, String>,
    failure: Option<CoreError>,
}
struct Lifecycle { lease: SyncMutex<Option<ProcessLease>>, stopping: Mutex<Option<Result<StopReceipt>>>, supervisor: Arc<ProcessSupervisor> }
impl Lifecycle {
    async fn stop(&self) -> Result<StopReceipt> {
        let mut receipt = self.stopping.lock().await;
        if let Some(result) = &*receipt { return clone_receipt(result); }
        let lease = self.lease.lock().map_err(|_| correlation())?.take().ok_or_else(correlation)?;
        let supervisor = self.supervisor.clone();
        let result = tokio::task::spawn_blocking(move || supervisor.stop(lease, 2000)).await.map_err(|_| correlation())?;
        *receipt = Some(result); clone_receipt(receipt.as_ref().expect("stored stop result"))
    }
}
fn clone_receipt(receipt: &Result<StopReceipt>) -> Result<StopReceipt> {
    receipt.as_ref().map(|r| StopReceipt { owned_pids:r.owned_pids.clone(), all_terminated:r.all_terminated, preserved_artifacts:r.preserved_artifacts.clone(), exit_code:r.exit_code }).map_err(Clone::clone)
}
struct Shared { state: Mutex<State>, host: WorkerHost, lifecycle: Lifecycle, cancel: CancellationToken }
pub struct HarnessWorker { shared: Arc<Shared>, writer: mpsc::Sender<Vec<u8>>, events: Mutex<mpsc::Receiver<WorkerEvent>> }
impl Drop for HarnessWorker { fn drop(&mut self) { self.shared.cancel.cancel(); } }
impl HarnessWorker {
    pub async fn open(supervisor: Arc<ProcessSupervisor>, mut spec: LaunchSpec, mut host: WorkerHost) -> Result<Self> {
        if spec.run_id != host.run { return Err(correlation()); }
        let snapshot = host.snapshot().await?;
        if !spec.cwd.starts_with(&snapshot.root) { return Err(correlation()); }
        let root = supervisor.pin_worker_root(&snapshot.root,&snapshot.policy)?;
        host.root = Some((snapshot.root,Arc::new(root)));
        let (parent,child) = StdStream::pair()?;
        let credentials = match spec.bootstrap.take() { Some(NamespaceBootstrap::PrivateFd3(fd)) => Some(fd), None => None, _ => return Err(error("worker_method")) };
        spec.bootstrap = Some(NamespaceBootstrap::ManagedWorker { credentials, channel:child.into() });
        let owner = supervisor.clone();
        let lease = tokio::task::spawn_blocking(move || owner.spawn(spec)).await.map_err(|_| correlation())??;
        parent.set_nonblocking(true)?;
        let socket = UnixStream::from_std(parent)?;
        let shared = Arc::new(Shared { state:Mutex::new(State::default()), host, lifecycle:Lifecycle { lease:SyncMutex::new(Some(lease)), stopping:Mutex::new(None), supervisor }, cancel:CancellationToken::new() });
        let (writer, outgoing) = mpsc::channel(MAX_PENDING);
        let (events, receiver) = mpsc::channel(MAX_PENDING);
        let (reader, write) = socket.into_split();
        tokio::spawn(write_loop(shared.clone(), write, outgoing));
        tokio::spawn(read_loop(shared.clone(), reader, writer.clone(), events));
        Ok(Self { shared, writer, events:Mutex::new(receiver) })
    }
    pub async fn request(&self, id: Uuid, method: NativeMethod) -> Result<NativeReply> {
        let mut state = self.shared.state.lock().await;
        if let Some(failure) = &state.failure { return Err(failure.clone()); }
        if state.pending.contains_key(&id) { return Err(correlation()); }
        if state.pending.len() >= MAX_PENDING { return Err(error("worker_busy")); }
        validate_control(&self.shared.host, &state, &method).await?;
        let frame = encode(&json!({"protocol":2,"kind":"request","id":id,"run_id":self.shared.host.run,"method":method}))?;
        let (send, receive) = oneshot::channel();
        state.pending.insert(id, Pending { method, reply:send, native_turn:None, finished:false, poisoned:false });
        // Queue insertion cannot wait while holding the correlation lock.
        if self.writer.try_send(frame).is_err() { state.pending.remove(&id); return Err(error("worker_busy")); }
        drop(state);
        receive.await.map_err(|_| correlation())?
    }
    pub async fn next_event(&self) -> Result<WorkerEvent> {
        self.events.lock().await.recv().await.ok_or_else(correlation)
    }
    pub fn owned_processes(&self) -> Result<Vec<ProcessIdentity>> {
        let mut lease = self.shared.lifecycle.lease.lock().map_err(|_| correlation())?;
        lease.as_mut().ok_or_else(correlation)?.observe_owned_processes()
    }
    pub async fn stop(self) -> Result<StopReceipt> {
        self.shared.cancel.cancel(); fail_pending(&self.shared, correlation()).await;
        self.shared.lifecycle.stop().await
    }
}

async fn validate_control(host: &WorkerHost, state: &State, method: &NativeMethod) -> Result<()> {
    match method {
        NativeMethod::Hello {} | NativeMethod::Describe {} => {},
        NativeMethod::Open { input } => {
            host.check_input(input, &host.snapshot().await?)?;
            if !state.sessions.is_empty() || state.pending.values().any(|p| matches!(p.method, NativeMethod::Open { .. })) { return Err(error("worker_busy")); }
        },
        _ => {
            let session = match method {
                NativeMethod::Turn { session,.. } | NativeMethod::AnswerPermission { session,.. } | NativeMethod::Interrupt { session } | NativeMethod::ExportContext { session } | NativeMethod::Close { session } => session,
                _ => return Err(correlation()),
            };
            let bound = state.sessions.get(&session.native_id).ok_or_else(correlation)?;
            if &bound.reference != session { return Err(correlation()); }
            match method {
                NativeMethod::Turn { input,.. } => {
                    if state.pending.values().any(|p| matches!(p.method,NativeMethod::Turn { .. })) { return Err(error("worker_busy")); }
                    if input.budget != host.snapshot().await?.policy || input.allowed_tools.iter().any(|t| !input.budget.allowed_tools.contains(&t.name)) { return Err(denied()); }
                },
                NativeMethod::AnswerPermission { answer,.. } => {
                    let permission = state.permissions.get(&answer.native_request_id).ok_or_else(denied)?;
                    if permission.request.session_id != session.native_id || permission.action != answer.action || permission.verified { return Err(denied()); }
                    if state.pending.get(&permission.control_id).is_some_and(|p| p.finished) { return Err(denied()); }
                },
                _ => {},
            }
        },
    }
    Ok(())
}
fn encode(value: &Value) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(value)?;
    if body.is_empty() || body.len() > MAX_FRAME { return Err(error("worker_method")); }
    let mut frame = Vec::with_capacity(body.len()+4); frame.extend_from_slice(&(body.len() as u32).to_be_bytes()); frame.extend_from_slice(&body); Ok(frame)
}
async fn fail_pending(shared: &Shared, failure: CoreError) {
    let mut state = shared.state.lock().await;
    if state.failure.is_none() { state.failure = Some(failure.clone()); }
    for (_, pending) in state.pending.drain() { let _ = pending.reply.send(Err(failure.clone())); }
}
async fn write_loop(shared: Arc<Shared>, mut socket: tokio::net::unix::OwnedWriteHalf, mut outgoing: mpsc::Receiver<Vec<u8>>) {
    loop {
        let frame = tokio::select! { _ = shared.cancel.cancelled() => break, frame = outgoing.recv() => match frame { Some(frame) => frame, None => break } };
        let result = tokio::select! { _ = shared.cancel.cancelled() => break, result = tokio::time::timeout(Duration::from_secs(30),socket.write_all(&frame)) => result };
        if !matches!(result,Ok(Ok(()))) { fail_pending(&shared, correlation()).await; shared.cancel.cancel(); break; }
    }
    let _ = socket.shutdown().await;
}
async fn read_loop(shared: Arc<Shared>, mut socket: tokio::net::unix::OwnedReadHalf, writer: mpsc::Sender<Vec<u8>>, events: mpsc::Sender<WorkerEvent>) {
    let result: Result<()> = async {
        loop {
            let mut header = [0;4];
            tokio::select! { _ = shared.cancel.cancelled() => return Err(correlation()), result = socket.read_exact(&mut header) => { result.map_err(|_| correlation())?; } }
            let length = u32::from_be_bytes(header) as usize;
            if length == 0 || length > MAX_FRAME { return Err(correlation()); }
            let mut body = vec![0;length];
            tokio::select! { _ = shared.cancel.cancelled() => return Err(correlation()), result = socket.read_exact(&mut body) => { result.map_err(|_| correlation())?; } }
            let incoming: Incoming = serde_json::from_slice(&body).map_err(|_| correlation())?;
            dispatch(&shared, incoming, &writer, &events).await?;
        }
    }.await;
    fail_pending(&shared, result.err().unwrap_or_else(correlation)).await;
    shared.cancel.cancel(); drop(socket); drop(events);
    let _ = shared.lifecycle.stop().await;
}

async fn dispatch(shared: &Arc<Shared>, incoming: Incoming, writer: &mpsc::Sender<Vec<u8>>, events: &mpsc::Sender<WorkerEvent>) -> Result<()> {
    match incoming {
        Incoming::Reply { protocol,id,run_id,result,error:failure } => {
            let mut state = shared.state.lock().await;
            if protocol != 2 || run_id != shared.host.run || result.is_some() == failure.is_some() { return Err(correlation()); }
            if !state.pending.contains_key(&id) { return Err(correlation()); }
            // A control poisoned by a callback replay can never be resuscitated:
            // its terminal reply is a fatal correlation violation. This fires
            // fail_pending only after every event preceding the replay has been
            // enqueued, so the host observes the valid deltas first.
            if state.pending.get(&id).is_some_and(|p| p.poisoned) { return Err(correlation()); }
            if let Some(result) = &result { validate_reply(&mut state, id, result)?; }
            else if !matches!(failure.as_ref().map(|e|e.code.as_str()),Some("worker_correlation"|"worker_method"|"worker_busy"|"worker_permission_denied")) { return Err(correlation()); }
            // Only remove after validation, so malformed replies still fail this waiter.
            let pending = state.pending.remove(&id).ok_or_else(correlation)?;
            if matches!(pending.method,NativeMethod::Turn {..}) {
                state.permissions.retain(|_,permission|permission.control_id!=id);
            }
            state.callbacks.remove(&id);
            let response = match (result,failure) { (Some(result),None) => Ok(result), (None,Some(failure)) => Err(error(&failure.code)), _ => return Err(correlation()) };
            let _ = pending.reply.send(response);
        },
        Incoming::Event { protocol,id,run_id,event } => {
            let mut state = shared.state.lock().await;
            if protocol != 2 || run_id != shared.host.run { return Err(correlation()); }
            validate_event(&mut state,id,&event,shared.host.run)?;
            let action = match &event.payload {
                CoreEvent::ApprovalNeeded {action}|CoreEvent::ToolStarted {action}=>Some(action.clone()),
                CoreEvent::ToolFinished {action_id,..}=>Some(state.permissions.values().find(|p|p.action.id==*action_id && p.control_id==id).ok_or_else(denied)?.action.clone()),
                _=>None,
            };
            drop(state);
            if let Some(action)=action { shared.host.verify_action_event(&action).await?; }
            events.try_send(event).map_err(|_| error("worker_busy"))?;
        },
        Incoming::HostRequest { protocol,id,request_id,run_id,session_id,method } => {
            // A host_request whose callback UUID is already registered for the
            // pending control is a replay inside an active control. Reject it
            // with a typed error and poison the control, but keep the channel
            // healthy so the host can observe the rejection and so any events
            // that preceded the replay are delivered. The poisoned control's
            // terminal reply is then treated as a fatal correlation violation,
            // resolving the turn fail-closed without racing its events.
            let replay = {
                let mut state = shared.state.lock().await;
                let outstanding: usize = state.callbacks.values().map(|s| s.len()).sum();
                if protocol != 2 || run_id != shared.host.run || outstanding >= 4096 || !state.pending.contains_key(&request_id) { return Err(correlation()); }
                let poisoned = state.pending.get(&request_id).is_some_and(|p| p.poisoned);
                let replay = poisoned || state.callbacks.entry(request_id).or_default().contains(&id);
                if replay { if let Some(pending) = state.pending.get_mut(&request_id) { pending.poisoned = true; } }
                else { state.callbacks.entry(request_id).or_default().insert(id); }
                replay
            };
            if replay {
                let response = json!({"protocol":2,"kind":"host_reply","id":id,"request_id":request_id,"run_id":run_id,"session_id":session_id,"error":{"code":"worker_correlation"}});
                if let Err(failure) = encode(&response).and_then(|bytes| writer.try_send(bytes).map_err(|_| error("worker_busy"))) {
                    fail_pending(shared, failure).await;
                    shared.cancel.cancel();
                }
                return Ok(());
            }
            // Do not await Store I/O on the read loop. Interrupt replies must be
            // consumed while a host callback is blocked inside a transaction.
            let shared = shared.clone();
            let writer = writer.clone();
            tokio::spawn(async move {
                let result = host_callback(&shared, request_id, &session_id, method).await;
                let mut response = json!({"protocol":2,"kind":"host_reply","id":id,"request_id":request_id,"run_id":run_id,"session_id":session_id});
                match result {
                    Ok(result) => response["result"] = result,
                    Err(failure) if failure.code == "worker_correlation" => {
                        fail_pending(&shared, failure).await;
                        shared.cancel.cancel();
                        return;
                    }
                    Err(_) => response["error"] = json!({"code":"worker_permission_denied"}),
                }
                if let Err(failure) = encode(&response).and_then(|bytes| writer.try_send(bytes).map_err(|_| error("worker_busy"))) {
                    fail_pending(&shared, failure).await;
                    shared.cancel.cancel();
                }
            });
        },
    }
    Ok(())
}
fn validate_reply(state: &mut State, id: Uuid, reply: &NativeReply) -> Result<()> {
    let method = &state.pending.get(&id).ok_or_else(correlation)?.method;
    match (method,reply) {
        (NativeMethod::Hello {},NativeReply::Hello { protocol:2 }) | (NativeMethod::Describe {},NativeReply::Describe { .. }) |
        (NativeMethod::AnswerPermission { .. },NativeReply::AnswerPermission {}) |
        (NativeMethod::Interrupt { .. },NativeReply::Interrupt {}) | (NativeMethod::ExportContext { .. },NativeReply::ExportContext { .. }) => {},
        (NativeMethod::Turn { .. },NativeReply::Turn {}) => {
            if !state.pending.get(&id).ok_or_else(correlation)?.finished { return Err(correlation()); }
        },
        (NativeMethod::Open { input },NativeReply::Open { session }) => {
            if session.project_id != input.project_id || session.cwd != input.cwd || session.resolved_route != input.route || input.route.harness.is_some_and(|h| h != session.harness) || session.native_id.is_empty()
                || state.provisional.get(&id).is_some_and(|native| native != &session.native_id) { return Err(correlation()); }
            state.sessions.insert(session.native_id.clone(),Session { reference:session.clone(),input:input.clone() }); state.provisional.remove(&id);
        },
        (NativeMethod::Close { session },NativeReply::Close {}) => { state.sessions.remove(&session.native_id); },
        _ => return Err(correlation()),
    }
    Ok(())
}
fn validate_event(state: &mut State, id: Uuid, event: &WorkerEvent, run: RunId) -> Result<()> {
    let pending = state.pending.get(&id).ok_or_else(correlation)?;
    let NativeMethod::Turn { session,input } = &pending.method else { return Err(correlation()); };
    if pending.finished || event.run_id != run || event.session_id.as_deref() != Some(&session.native_id) || event.turn_id.is_empty() { return Err(correlation()); }
    if let Some(native_turn) = &pending.native_turn {
        if native_turn != &event.turn_id { return Err(correlation()); }
    } else if event.turn_id != input.message_id.to_string() && !matches!(&event.payload,CoreEvent::TurnStarted {message_id} if message_id==&input.message_id) {
        return Err(correlation());
    }
    match &event.payload {
        CoreEvent::TurnStarted {message_id} | CoreEvent::TextDelta {message_id,..} | CoreEvent::TextReplace {message_id,..} | CoreEvent::ReasoningDelta {message_id,..} | CoreEvent::ProviderToolCall {message_id,..} | CoreEvent::ProviderMessage {message_id,..} | CoreEvent::TurnFinished {message_id,..} if message_id != &input.message_id => return Err(correlation()),
        CoreEvent::ApprovalNeeded {action} | CoreEvent::ToolStarted {action} => {
            let mut matching=state.permissions.values_mut().filter(|p|p.action==*action && p.control_id==id && p.request.session_id==session.native_id
                && event.tool_id.as_ref().is_none_or(|tool|tool==&p.request.tool_call_id));
            let permission=matching.next().ok_or_else(denied)?;
            if matching.next().is_some() {return Err(denied());}
            if matches!(&event.payload,CoreEvent::ToolStarted {..}) {
                if !permission.verified || !permission.allowed || permission.started || permission.finished {return Err(denied());}
                permission.started=true;
            }
        },
        CoreEvent::ToolFinished {action_id,..} => {
            let permission=state.permissions.values_mut().find(|p|p.action.id==*action_id && p.control_id==id && p.request.session_id==session.native_id && p.verified && p.allowed
                && event.tool_id.as_ref().is_none_or(|tool|tool==&p.request.tool_call_id)).ok_or_else(denied)?;
            if !permission.started || permission.finished {return Err(denied());}
            permission.finished=true;
        },
        CoreEvent::SessionBound {..} | CoreEvent::StateChanged {..} | CoreEvent::ArtifactAdded {..} | CoreEvent::JudgmentRecorded {..} => return Err(denied()),
        _ => {},
    }
    let pending = state.pending.get_mut(&id).ok_or_else(correlation)?;
    pending.native_turn.get_or_insert_with(||event.turn_id.clone());
    if matches!(event.payload,CoreEvent::TurnFinished {..}) { 
        pending.finished=true; 
        state.permissions.retain(|_,permission|permission.control_id!=id);
        state.callbacks.remove(&id);
    }
    Ok(())
}
async fn host_callback(shared: &Shared, request_id: Uuid, session_id: &str, method: HostMethod) -> Result<Value> {
    // Phase 1: Read pending method and session under a short lock. Async I/O and
    // callback dispatch must NOT hold the shared State lock, so that Interrupt
    // admission and lease teardown remain responsive to callback I/O stalls.
    let pending_method = {
        let state = shared.state.lock().await;
        state.pending.get(&request_id).map(|p| p.method.clone()).ok_or_else(correlation)?
    };
    // Only the exact session-bound audit during an active open may establish a
    // provisional native ID. No action/state authority is inferred from that ID.
    if let NativeMethod::Open {input:bound} = &pending_method {
        let HostMethod::RecordNativeEvent {event,input} = method else { return Err(denied()); };
        if &input != bound || event["event"] != "session_bound" || event["native_id"].as_str() != Some(session_id) || session_id.is_empty() { return Err(correlation()); }
        shared.host.check_input(&input,&shared.host.snapshot().await?)?;
        let provisional_result = {
            let mut state = shared.state.lock().await;
            if state.provisional.get(&request_id).is_some_and(|id| id != session_id) { Err(correlation()) } else {
                state.provisional.insert(request_id,session_id.into());
                Ok(())
            }
        };
        provisional_result?;
        shared.host.audit(request_id,&event).await?;
        return Ok(json!({"type":"record_native_event"}));
    }
    // Copy the session out of the lock. Returning the map reference kept the
    // guard borrowed past this block (E0597) and across later awaits.
    let session = {
        let state = shared.state.lock().await;
        let session = state.sessions.get(session_id).cloned().ok_or_else(correlation)?;
        let control_session = match &pending_method {
            NativeMethod::Turn {session,..} | NativeMethod::AnswerPermission {session,..} | NativeMethod::Interrupt {session} | NativeMethod::ExportContext {session} | NativeMethod::Close {session} => session,
            _ => return Err(correlation()),
        };
        if control_session != &session.reference { return Err(correlation()); }
        session
    };
    match method {
        HostMethod::State {input,turn} => {
            if input.run_id != shared.host.run || input.project_id != shared.host.project || input.cwd != session.input.cwd || input.route != session.input.route { return Err(correlation()); }
            if let Some(turn) = turn {
                let pending_fresh = {
                    let state = shared.state.lock().await;
                    let p = state.pending.get(&request_id).ok_or_else(correlation)?;
                    if !matches!(&p.method, NativeMethod::Turn {..}) { return Err(correlation()); }
                    p.method.as_turn_input().ok().map(|t| t.clone())
                };
                if let Some(active) = pending_fresh {
                    if turn != active { return Err(correlation()); }
                } else {
                    return Err(correlation());
                }
            }
            let snapshot = shared.host.snapshot().await?;
            Ok(json!({"type":"state","state":{"policy_version":snapshot.policy.version.to_string(),"state_digest":snapshot.state}}))
        },
        HostMethod::PrepareAction {request,input} => {
            // R3: Require an unfinished originating turn at permission preparation.
            let pending_finished = {
                let state = shared.state.lock().await;
                let pending_entry = state.pending.get(&request_id).ok_or_else(correlation)?;
                if !matches!(pending_entry.method, NativeMethod::Turn {..}) || pending_entry.finished
                    || input != session.input || request.session_id != session_id
                    || request.request_id.is_empty() || request.tool_call_id.is_empty()
                    || state.permissions.contains_key(&request.request_id)
                    || state.permissions.len() >= MAX_PENDING { return Err(denied()); }
                pending_entry.finished
            };
            if pending_finished { return Err(denied()); }
            let action = shared.host.prepare(&request,&input).await?;
            {
                let mut state = shared.state.lock().await;
                // Re-check originating turn is still unfinished before recording.
                let pending_entry = state.pending.get(&request_id).ok_or_else(correlation)?;
                if pending_entry.finished || pending_entry.poisoned || state.permissions.contains_key(&request.request_id) { return Err(denied()); }
                state.permissions.insert(request.request_id.clone(),PreparedPermission {request,action:action.clone(),verified:false,allowed:false,started:false,finished:false,control_id:request_id});
            }
            Ok(json!({"type":"prepare_action","action":action}))
        },
        HostMethod::VerifyAnswer {answer,request} => {
            let (permission_snapshot, origin_finished) = {
                let state = shared.state.lock().await;
                let pending = state.pending.get(&request_id).ok_or_else(correlation)?;
                let NativeMethod::AnswerPermission {answer:control,..} = &pending.method else { return Err(denied()); };
                if &answer != control || request.session_id != session_id { return Err(denied()); }
                let permission = state.permissions.get(&request.request_id).ok_or_else(denied)?;
                let origin = permission.control_id;
                let origin_finished = state.pending.get(&origin).is_some_and(|p| p.finished);
                // R3: Require an unfinished originating turn at verification.
                if !state.pending.get(&origin).is_some_and(|p| matches!(&p.method,NativeMethod::Turn {session,..} if session.native_id == session_id && !p.finished)) { return Err(denied()); }
                if state.pending.get(&origin).is_some_and(|p| p.poisoned) { return Err(denied()); }
                if permission.request != request || permission.verified { return Err(denied()); }
                (permission.clone(), origin_finished)
            };
            if origin_finished { return Err(denied()); }
            let verified = shared.host.verify(&permission_snapshot,&answer).await?;
            {
                let mut state = shared.state.lock().await;
                let permission = state.permissions.get_mut(&request.request_id).ok_or_else(denied)?;
                if permission.verified { return Err(denied()); }
                permission.verified = verified;
                permission.allowed = verified && answer.decision == PermissionDecision::Allow;
            }
            Ok(json!({"type":"verify_answer","verified":verified}))
        },
        HostMethod::RecordNativeEvent {event,input} => {
            if input != session.input || !event.is_object() { return Err(correlation()); }
            // Store only a digest of untrusted observation data, never prose/secrets.
            shared.host.audit(request_id,&event).await?;
            Ok(json!({"type":"record_native_event"}))
        },
    }
}
#[cfg(test)]
pub(super) struct VerifySnapshotBarrier {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}
#[cfg(test)]
struct ReleaseOnDrop(Arc<VerifySnapshotBarrier>);
#[cfg(test)]
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) { self.0.release.notify_one(); }
}
#[cfg(test)]
mod race_tests {
    use super::*;
    use crate::policy::service::ApprovalDecision;
    use rusqlite::params;

    // Per-host, private and absent from release builds. It pauses only after the
    // real running-state snapshot, before the real approval consumption.
    // VerifySnapshotBarrier is defined at module level with cfg(test).
    // The barrier pauses only after the real running-state snapshot,
    // before the real approval consumption.

    async fn run_state_consume_case(terminal: Option<&'static str>) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let project = ProjectId::new();
        let run = RunId::new();
        let policy = ProjectPolicy {
            version: 1, project_id: project, allowed_connection_ids: vec!["fixture-local".into()],
            allowed_tools: vec!["file_write".into()],
            read_roots: vec![workspace.to_string_lossy().into_owned()],
            write_roots: vec![workspace.to_string_lossy().into_owned()],
            network_destinations: vec![], local_only: true, max_workers: 1, max_requests: 4,
            max_cost_microusd: None, spend_policy: SpendPolicy::FreeOnly, allow_unknown_cost: false,
        };
        let store = Store::open(&root.path().join("state.sqlite")).await.unwrap();
        let body = serde_json::to_string(&policy).unwrap();
        let path = workspace.to_string_lossy().into_owned();
        store.with_transaction(move |tx| {
            let job = Uuid::new_v4().to_string();
            let node = Uuid::new_v4().to_string();
            tx.execute("INSERT INTO projects(id,name,root,created_at) VALUES(?1,'consume-race',?2,0)", params![project.to_string(),path])?;
            tx.execute("INSERT INTO policies(project_id,version,body) VALUES(?1,1,?2)", params![project.to_string(),body])?;
            tx.execute("INSERT INTO jobs(id,project_id,body,state) VALUES(?1,?2,'{}','queued')", params![job,project.to_string()])?;
            tx.execute("INSERT INTO nodes(id,job_id,body,state) VALUES(?1,?2,'{}','queued')", params![node,job])?;
            tx.execute("INSERT INTO runs(id,node_id,body,state) VALUES(?1,?2,'{}','running')", params![run.to_string(),node])?;
            Ok(())
        }).await.unwrap();
        let mut host = WorkerHost::new(store.clone(), project, run);
        host.root = Some((workspace.clone(), Arc::new(File::open(&workspace).unwrap())));
        let input: OpenSessionInput = serde_json::from_value(json!({
            "run_id":run,"project_id":project,"cwd":workspace,
            "route":{"connection_id":"fixture-local","model_id":"fixture","harness":"opencode"},
            "context":{"digest":"a".repeat(64),"brief":"","constraints":[],"excerpts":[],"artifacts":[],
                "unresolved_questions":[],"originating_sessions":[]},
            "policy":policy
        })).unwrap();
        let request: NativePermissionRequest = serde_json::from_value(json!({
            "request_id":"permission-1","session_id":"bound-session","name":"file_write","tool_call_id":"tool-1",
            "input":{"path":workspace.join("tool-effect"),"content_digest":"c".repeat(64)},"raw":{}
        })).unwrap();
        let action = host.prepare(&request, &input).await.unwrap();
        let grant = ApprovalService::new(store.clone()).decide(action.id,
            ApprovalDecision::Approve { expires_at:now_ms()+60_000 },now_ms()).await.unwrap().unwrap();
        let answer: PermissionAnswer = serde_json::from_value(json!({
            "native_request_id":"permission-1","action":action,"grant":grant,"decision":"allow"
        })).unwrap();
        let permission = PreparedPermission { request, action, verified:false, allowed:false,
            started:false, finished:false, control_id:Uuid::new_v4() };
        let barrier = Arc::new(VerifySnapshotBarrier {
            entered:tokio::sync::Notify::new(), release:tokio::sync::Notify::new(),
        });
        host.verify_snapshot_barrier = Some(barrier.clone());
        let release = ReleaseOnDrop(barrier.clone());
        let verifying = tokio::spawn(async move { host.verify(&permission,&answer).await });
        tokio::time::timeout(Duration::from_secs(8),barrier.entered.notified()).await.unwrap();
        if let Some(state) = terminal {
            store.with_transaction(move |tx| {
                assert_eq!(tx.execute("UPDATE runs SET state=?1 WHERE id=?2 AND state='running'",
                    params![state,run.to_string()])?,1);
                Ok(())
            }).await.unwrap();
        }
        drop(release);
        let result = tokio::time::timeout(Duration::from_secs(8),verifying).await.unwrap().unwrap();
        let id = grant.id.to_string();
        let consumed: bool = store.with_connection(move |db| Ok(db.query_row(
            "SELECT consumed_at IS NOT NULL FROM grants WHERE id=?1",[id],|row|row.get(0))?)).await.unwrap();
        assert!(!workspace.join("tool-effect").exists(), "host verification itself must never execute the tool");
        if terminal.is_some() {
            assert!(!matches!(result,Ok(true)), "{terminal:?}: stale snapshot authorized a terminal bound run; consumed={consumed}");
            assert!(!consumed, "{terminal:?}: run-state update before consume must leave consumed_at NULL");
        } else {
            assert!(matches!(result,Ok(true)), "unchanged running run is the positive control");
            assert!(consumed, "positive control must actually commit grant consumption");
        }
    }

    #[tokio::test]
    async fn cancelled_bound_run_between_snapshot_and_consume_denies_grant() {
        run_state_consume_case(Some("cancelled")).await;
    }
    #[tokio::test]
    async fn succeeded_bound_run_between_snapshot_and_consume_denies_grant() {
        run_state_consume_case(Some("succeeded")).await;
    }
    #[tokio::test]
    async fn unchanged_running_bound_run_consumes_grant_after_snapshot_barrier() {
        run_state_consume_case(None).await;
    }
}

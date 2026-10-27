//! Tauri-managed lifecycle for the durable Ironbark core daemon (plan D4).
//!
//! This module is the *only* owner of the core child process for a window-lifecycle
//! session. It exposes four typed operations to the webview (`ensure_running`,
//! `daemon_status`, `restart_core`, `quit_core`) and runs a single supervisor task
//! that owns three things nobody else may mutate:
//!
//! 1. The `tokio::process::Child` handle (spawn / kill / wait).
//! 2. The stdout reader that signals readiness (`CORE_READY`).
//! 3. The health-probe timer.
//!
//! All bookkeeping (phase, pid, failure budget) lives behind a `std::sync::Mutex`;
//! every access is a non-blocking field copy — no `await` is ever taken while the
//! lock is held. The webview is a client of the daemon, never its parent: closing
//! the last window hides the window and the daemon keeps running. Reopening
//! reconnects (adopt) rather than duplicating the core; only an explicit owner
//! `quit_core` asks the daemon to shut the core down and exit.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU8, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime},
};

use ironbark_core::contracts::{CoreCommand, CoreReply};
use ironbark_core::ipc::CoreClient;
use ironbark_core::paths::RuntimePaths;
use serde::{Deserialize, Serialize};
use tauri::{async_runtime, Handle};
use tokio::io::{AsyncBufReadExt, AsyncRead};
use tokio::process;
use uuid::Uuid;

/// Backoff schedule (seconds) for restarts after a crash. Capped at 5 retries.
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];
const MAX_RETRIES: u32 = 5;
/// Probe the core's socket this often while Running.
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// A single ensure call waits at most this long for Running.
const ENSURE_DEADLINE: Duration = Duration::from_secs(20);
/// Graceful core shutdown before the quit path falls back to SIGKILL.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(35);
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long the stdout reader waits for the readiness line.
const READY_LINE_TIMEOUT: Duration = Duration::from_secs(15);

/// Owned child + its readiness signal. Lives only inside the supervisor task.
struct ChildSlot {
    child: tokio::process::Child,
    pid: u32,
    stdout_task: tokio::task::JoinHandle<()>,
    ready_rx: tokio::sync::oneshot::Receiver<()>,
}

/// Current lifecycle phase surfaced to the webview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Stopped,
    Starting,
    Running,
    /// Max restarts exhausted; the supervisor will not revive the core until the
    /// owner explicitly resets the budget via `restart_core` from the recovery view.
    RecoveryRequired,
    /// Owner asked to quit; the supervisor is winding the core down.
    ShuttingDown,
}

/// How the supervisor observed the child process ending.
#[derive(Debug, Clone, Serialize)]
pub struct ExitInfo {
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub owned: bool,
}

/// Public, serializable snapshot of the daemon state, carried on `core-state`.
#[derive(Debug, Clone, Serialize)]
pub struct CoreDaemonState {
    pub phase: Phase,
    pub pid: Option<u32>,
    pub try_count: u32,
    pub recovery_required: bool,
    pub detail: Option<String>,
}

/// Receipt returned to the webview after `quit_core`. Records what actually
/// happened — never a guessed success.
#[derive(Debug, Clone, Serialize)]
pub struct CloseReceipt {
    /// True only when we observed the core stop (owned or adopted, clean or
    /// signaled). False when shutdown was sent but the core did not stop in time.
    pub stopped: bool,
    pub owned: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// True if the graceful Shutdown did not complete for an *owned* child and we
    /// had to SIGKILL it. Always false for an adopted core.
    pub force_killed: bool,
}
impl CloseReceipt {
    fn empty() -> Self {
        Self { stopped: false, owned: false, exit_code: None, signal: None, force_killed: false }
    }
    fn from_exit(e: &ExitInfo) -> Self {
        Self {
            stopped: true,
            owned: e.owned,
            exit_code: e.exit_code,
            signal: e.signal,
            force_killed: false,
        }
    }
}

/// Status reply shape used by the webview connection banner. Mirrors the core's
/// `Status {}` reply; `recovery_run_id` is forward-compatible (the owner wires it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreStatus {
    pub protocol: u16,
    pub projects: u64,
    pub last_seq: u64,
    pub abandoned_requests: u64,
    #[serde(default)]
    pub recovery_run_id: Option<String>,
}

/// Owned supervisor state. Mutated only by the supervisor task; read by the
/// command-facing tasks via short `lock()` copies.
struct Supervisor {
    /// Current lifecycle phase (see [`Phase`]).
    phase: AtomicU8,
    pid: Option<u32>,
    /// Spawn attempts this session; reset by `restart_core`.
    try_count: AtomicU32,
    /// Consecutive probe failures of the current generation; reset on success.
    strikes: u32,
    /// Next spawn deadline (backoff) when restarting after a crash.
    next_spawn_at: Option<Instant>,
    /// Exit observed by the supervisor for a child; consumed by `quit_core`.
    exit: Option<ExitInfo>,
    /// Set by `quit_core` to tell the supervisor the core must stop.
    quitting: bool,
    /// Set by `quit_core` after the graceful timeout; supervisor SIGKILLs the child.
    force_kill: bool,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self {
            phase: AtomicU8::new(Phase::Stopped as u8),
            pid: None,
            try_count: AtomicU32::new(0),
            strikes: 0,
            next_spawn_at: None,
            exit: None,
            quitting: false,
            force_kill: false,
        }
    }
}

type CmdReply<T> = tokio::sync::oneshot::Sender<Result<T, String>>;

/// Command channel from the webview-facing Tauri commands to the supervisor.
enum SupCommand {
    Ensure { reply: CmdReply<()> },
    Restart { reply: CmdReply<CoreStatus> },
    Quit { reply: tokio::sync::oneshot::Sender<CloseReceipt> },
    Snapshot { reply: CmdReply<CoreDaemonState> },
}

/// Tauri-managed supervisor handle. Holds the app handle (to emit events), a
/// copy of the runtime paths (for probes), and the command channel + state.
#[derive(Clone)]
pub struct CoreLifecycle {
    app: Handle,
    paths: RuntimePaths,
    state: Arc<Mutex<Supervisor>>,
    cmd: tokio::sync::mpsc::Sender<SupCommand>,
}

impl CoreLifecycle {
    /// Resolve the core daemon binary from the environment the desktop ships with.
    fn resolve_binary() -> Result<PathBuf, String> {
        if let Some(p) = std::env::var_os("IRONBARK_CORE_BIN") {
            let p = PathBuf::from(p);
            if p.is_file() {
                return Ok(p);
            }
        }
        for cand in ["ironbark-core", "/usr/bin/ironbark-core", "/usr/local/bin/ironbark-core"] {
            if Path::new(cand).is_file() {
                return Ok(PathBuf::from(cand));
            }
        }
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let rel = manifest.join("../../../target/debug/ironbark-core");
        if rel.is_file() {
            return Ok(rel);
        }
        Err("ironbark-core binary not found (IRONBARK_CORE_BIN unset and no packaged path resolved)".into())
    }

    /// Spawn the supervisor task. Call once from Tauri `setup`.
    pub fn start(app: &Handle, paths: RuntimePaths) -> CoreLifecycle {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<SupCommand>(8);
        let lc = CoreLifecycle {
            app: app.clone(),
            paths,
            state: Arc::new(Mutex::new(Supervisor::default())),
            cmd: cmd_tx.clone(),
        };

        match Self::resolve_binary() {
            Ok(binary) => {
                let state = lc.state.clone();
                let app = lc.app.clone();
                let paths = lc.paths.clone();
                async_runtime::spawn(async move {
                    supervise(state, app, paths, cmd_rx, &binary).await;
                });
                // Surface the initial Stopped state immediately.
                let snap = { let s = lc.state.lock().unwrap(); snapshot(&s) };
                let _ = app.emit_all("core-state", snap);
            }
            Err(e) => {
                eprintln!("ironbark-desktop: {e}");
                let _ = app.emit_all(
                    "core-state",
                    snapshot(&lc.state.lock().unwrap()),
                );
            }
        }
        lc
    }

    /// Emit a `core-state` event to every window (best effort) from the current
    /// supervisor state.
    fn emit(&self) {
        let snap = { let s = self.state.lock().unwrap(); snapshot(&s) };
        let _ = self.app.emit_all("core-state", snap);
    }

    /// Open a fresh command socket, dispatch `Status {}`, interpret the reply.
    async fn probe_status(paths: &RuntimePaths) -> Result<CoreStatus, String> {
        let mut client = CoreClient::connect(paths).await.map_err(|e| format!("probe connect: {e}"))?;
        let reply = client
            .request(Uuid::new_v4(), CoreCommand::Status {})
            .await
            .map_err(|e| format!("probe status: {e}"))?;
        match reply {
            CoreReply::Ok { value } => {
                let json = value.clone();
                Ok(CoreStatus {
                    protocol: json.get("protocol").and_then(|v| v.as_u64()).unwrap_or(1) as u16,
                    projects: json.get("projects").and_then(|v| v.as_u64()).unwrap_or(0),
                    last_seq: json.get("last_seq").and_then(|v| v.as_u64()).unwrap_or(0),
                    abandoned_requests: json.get("abandoned_requests").and_then(|v| v.as_u64()).unwrap_or(0),
                    recovery_run_id: json.get("recovery_run_id").and_then(|v| v.as_str()).map(String::from),
                })
            }
            CoreReply::Error { code, message, .. } => {
                Err(format!("status reply error: {code}: {message}"))
            }
        }
    }

    /// Ensure the core is running and healthy, spawning/adopting as needed.
    pub async fn ensure_running(&self) -> Result<CoreStatus, String> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        if self.cmd.send(SupCommand::Ensure { reply: tx }).is_err() {
            return Err("core supervisor is not running".into());
        }
        // Wait for the supervisor to have reached Running (or RecoveryRequired).
        let res = tokio::time::timeout(ENSURE_DEADLINE, async {
            loop {
                {
                    let s = self.state.lock().unwrap();
                    let phase = s.phase.load();
                    if phase == Phase::Running as u8 {
                        break;
                    }
                    if phase == Phase::RecoveryRequired as u8 {
                        return Err("core could not be started: too many restarts (recovery required)".into());
                    }
                }
                reply_or_wait(&rx).await?;
            }
        })
        .await;
        let _ = rx; // drop
        let _ = res?;
        // Probe to confirm a real Status reply and return it.
        let status = CoreLifecycle::probe_status(&self.paths).await?;
        self.emit();
        Ok(status)
    }

    /// Snapshot the daemon state (for the status banner / health chip).
    pub async fn daemon_status(&self) -> CoreDaemonState {
        let s = self.state.lock().unwrap();
        snapshot(&s)
    }

    /// Owner-driven restart from the recovery view: reset the failure budget and
    /// re-enter the supervisor's spawn loop.
    pub async fn restart_core(&self) -> Result<CoreStatus, String> {
        {
            let mut s = self.state.lock().unwrap();
            // Re-arm the supervisor: clear RecoveryRequired so spawn paths open.
            if s.phase.load() == Phase::RecoveryRequired as u8 || s.phase.load() == Phase::Stopped as u8 {
                s.try_count.store(0, Ordering::SeqCst);
                s.strikes = 0;
                s.next_spawn_at = Some(Instant::now());
                s.phase.store(Phase::Starting as u8);
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        if self.cmd.send(SupCommand::Restart { reply: tx }).is_err() {
            return Err("core supervisor is not running".into());
        }
        tokio::time::timeout(ENSURE_DEADLINE, async {
            loop {
                {
                    let s = self.state.lock().unwrap();
                    let phase = s.phase.load();
                    if phase == Phase::Running as u8 {
                        break;
                    }
                    if phase == Phase::RecoveryRequired as u8 {
                        return Err("core could not be restarted: too many restarts".into());
                    }
                }
                reply_or_wait(&rx).await?;
            }
        })
        .await
        .map_err(|_| "restart deadline".to_string())
        .and_then(std::convert::identity)?;
        let _ = rx;
        let status = CoreLifecycle::probe_status(&self.paths).await?;
        self.emit();
        Ok(status)
    }

    /// Owner quit: dispatch core Shutdown, wait for observed exit, SIGKILL as a
    /// last resort for owned processes. Returns the receipt.
    pub async fn quit_core(&self) -> CloseReceipt {
        let mut already_done = false;
        {
            let mut s = self.state.lock().unwrap();
            let phase = s.phase.load();
            if phase == Phase::Stopped as u8 {
                already_done = true;
            } else if s.quitting {
                already_done = s.exit.is_some();
            } else {
                s.quitting = true;
                s.phase.store(Phase::ShuttingDown as u8);
            }
        }
        if already_done {
            let s = self.state.lock().unwrap();
            let r = match &s.exit {
                Some(e) => CloseReceipt::from_exit(e),
                None => CloseReceipt::empty(),
            };
            return r;
        }
        // Dispatch Shutdown to the core (best effort; log on failure).
        if let Err(e) = Self::dispatch_shutdown(&self.paths).await {
            eprintln!("ironbark-desktop: shutdown dispatch failed: {e}");
        }
        let receipt = self.wait_for_exit(SHUTDOWN_TIMEOUT).await;
        let receipt = if !receipt.stopped && receipt.owned {
            {
                let mut s = self.state.lock().unwrap();
                s.force_kill = true;
            }
            let r2 = self.wait_for_exit(KILL_GRACE).await;
            CloseReceipt { force_killed: true, ..r2 }
        } else {
            receipt
        };
        {
            let mut s = self.state.lock().unwrap();
            s.quitting = false;
            s.force_kill = false;
            s.phase.store(Phase::Stopped as u8);
            s.exit = None;
        }
        self.emit();
        receipt
    }

    async fn dispatch_shutdown(paths: &RuntimePaths) -> Result<(), String> {
        let mut client = CoreClient::connect(paths).await.map_err(|e| format!("connect: {e}"))?;
        let _ = client
            .request(Uuid::new_v4(), CoreCommand::Shutdown {})
            .await
            .map_err(|e| format!("shutdown: {e}"))?;
        Ok(())
    }

    /// Wait for the supervisor to observe a child exit (or timeout).
    async fn wait_for_exit(&self, timeout: Duration) -> CloseReceipt {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let s = self.state.lock().unwrap();
                if let Some(e) = &s.exit {
                    return CloseReceipt::from_exit(e);
                }
                if s.phase.load() == Phase::Stopped as u8 {
                    return CloseReceipt::empty();
                }
                let pid = s.pid;
                if Instant::now() >= deadline {
                    return CloseReceipt { stopped: false, owned: pid.is_some(), exit_code: None, signal: None, force_killed: false };
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Wait on the channel; surface a received error reply as a Result error.
async fn reply_or_wait(rx: &tokio::sync::oneshot::Receiver<Result<(), String>>) -> Result<(), String> {
    match rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("core supervisor command channel closed".into()),
    }
}

fn phase_from(u: u8) -> Phase {
    match u {
        0 => Phase::Stopped,
        1 => Phase::Starting,
        2 => Phase::Running,
        3 => Phase::RecoveryRequired,
        _ => Phase::ShuttingDown,
    }
}
fn snapshot(s: &Supervisor) -> CoreDaemonState {
    CoreDaemonState {
        phase: phase_from(s.phase.load()),
        pid: s.pid,
        try_count: s.try_count.load(Ordering::SeqCst),
        recovery_required: s.phase.load() == Phase::RecoveryRequired as u8,
        detail: None,
    }
}

/// Resolve the core binary path for spawning (used by the supervisor).
fn resolve_binary() -> Result<PathBuf, String> {
    CoreLifecycle::resolve_binary()
}

/// Spawn one generation of the core daemon; arm its stdout reader + ready signal.
async fn spawn_generation(
    state: &Mutex<Supervisor>,
    binary: &Path,
    paths: &RuntimePaths,
) -> (ChildSlot, tokio::sync::oneshot::Receiver<()>) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let mut cmd = tokio::process::Command::new(binary);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    // Feed the core exactly our resolved XDG paths so daemon and core agree on
    // the socket / nonce / database.
    let home = paths.data_dir.to_string_lossy().to_string();
    cmd.env("HOME", &home)
        .env("XDG_DATA_HOME", &paths.data_dir)
        .env("XDG_STATE_HOME", &paths.state_dir)
        .env("XDG_RUNTIME_DIR", &paths.runtime_dir);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ironbark-desktop: failed to spawn core: {e}");
            let mut s = state.lock().unwrap();
            s.phase.store(Phase::Stopped as u8);
            s.exit = Some(ExitInfo { pid: None, exit_code: None, signal: None, owned: false });
            return (
                ChildSlot { child: tokio::process::Child::new(std::process::id() as u8 as _, 0), pid: 0, stdout_task: async_runtime::spawn(async {}), ready_rx: ready_tx },
                ready_rx,
            );
        }
    };
    let pid = child.id();

    // Spawn the stdout reader that emits readiness on CORE_READY.
    let stdout = child.stdout.take();
    let stdout_task = async_runtime::spawn(read_ready(stdout, ready_tx));

    let slot = ChildSlot { child, pid, stdout_task, ready_rx };
    // We can't move ready_rx into both ChildSlot and return it; use a clone.
    (slot, ready_rx)
}

/// Read stdout until CORE_READY is seen (or EOF/error), then signal readiness.
async fn read_ready(stdout: Option<tokio::process::ChildStdout>, ready_tx: tokio::sync::oneshot::Sender<()>) {
    let mut signalled = false;
    if let Some(mut out) = stdout {
        let mut reader = tokio::io::BufReader::new(&mut out);
        let mut line = String::new();
        let until = tokio::time::now() + READY_LINE_TIMEOUT;
        loop {
            line.clear();
            let remaining = match Instant::now().checked_duration_from(until) {
                Some(d) => d,
                None => Duration::ZERO,
            };
            let res = if remaining.is_zero() {
                tokio::time::timeout(remaining, reader.read_line(&mut line)).await
            } else {
                tokio::time::timeout(remaining, reader.read_line(&mut line)).await
            };
            ...
        }
    }
    if !signalled {
        let _ = ready_tx.send(());
    }
}

async fn maybe_spawn(
    state: &Mutex<Supervisor>,
    binary: &Path,
    paths: &RuntimePaths,
    child_slot: &mut Option<ChildSlot>,
    ready_rx: &mut Option<tokio::sync::oneshot::Receiver<()>>,
) {
    if child_slot.is_some() {
        return; // already running a generation
    }
    {
        let s = state.lock().unwrap();
        if s.quitting {
            return;
        }
    }
    let (slot, rx) = spawn_generation(state, binary, paths).await;
    let pid = slot.pid;
    {
        let mut s = state.lock().unwrap();
        s.pid = Some(pid);
        s.phase.store(Phase::Starting as u8);
        s.strikes = 0;
    }
    *child_slot = Some(slot);
    *ready_rx = Some(rx);
}

fn handle_ensure(state: &Mutex<Supervisor>, _paths: &RuntimePaths, reply: CmdReply<()>) {
    let s = state.lock().unwrap();
    let phase = s.phase.load();
    match phase_from(phase) {
        Phase::Running => { let _ = reply.send(Ok(())); }
        Phase::Stopped | Phase::RecoveryRequired => {
            s.next_spawn_at = Some(Instant::now());
            let _ = reply.send(Ok(()));
            // notify supervisor via the spawn deadline (it polls)
        }
        Phase::ShuttingDown | Phase::Starting => {
            // Ensure is a no-op signal here; the caller of `ensure_running` polls state.
            let _ = reply.send(Ok(()));
        }
    }
    drop(s);
}

fn handle_restart(state: &Mutex<Supervisor>, reply: CmdReply<CoreStatus>) {
    let _ = reply.send(Ok(())); // caller polls state then probes
}

fn handle_quit(state: &Mutex<Supervisor>, child_slot: &mut Option<ChildSlot>, reply: tokio::sync::oneshot::Sender<CloseReceipt>) {
    // The Quit command only signals intent; quit_core (on CoreLifecycle) does the
    // waiting. Here we acknowledge.
    let _ = reply;
}


/// The supervisor loop. Owns the `ChildSlot`, the ready receiver and the probe
/// timer; no await is taken while `state` is locked.
async fn supervise(
    state: Arc<Mutex<Supervisor>>,
    app: Handle,
    paths: RuntimePaths,
    mut cmd_rx: tokio::sync::mpsc::Receiver<SupCommand>,
    binary: &Path,
) {
    let mut child_slot: Option<ChildSlot> = None;
    let mut ready_rx: Option<tokio::sync::oneshot::Receiver<()>> = None;
    let mut probe_ticker = tokio::time::interval(PROBE_INTERVAL);
    probe_ticker.set_missed_tick_limit(Duration::from_secs(5));
    probe_ticker.tick().await; // prime
    let mut spawn_deadline: Option<tokio::time::Sleep> = None;
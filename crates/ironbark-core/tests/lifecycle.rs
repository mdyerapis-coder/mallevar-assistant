//! D4 core lifecycle tests against the real `ironbark-core` binary.
//!
//! The daemon is spawned as a real child process with an explicit XDG
//! environment, driven through the real `ironbark-cli`, and killed/restarted to
//! exercise close, shutdown, signal and restart behaviour. Nothing here
//! mutates the test process's own environment: every spawned process receives
//! its own XDG_* values, so these tests are safe to run in parallel with each
//! other and with the rest of the suite.
//!
//! Binary locations come from `IRONBARK_CORE_BIN` / `IRONBARK_CLI_BIN` and
//! default to `target/debug/<name>` relative to the crate root.
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

fn core_bin() -> String {
    std::env::var("IRONBARK_CORE_BIN").unwrap_or_else(|_| "target/debug/ironbark-core".to_string())
}

fn cli_bin() -> String {
    std::env::var("IRONBARK_CLI_BIN").unwrap_or_else(|_| "target/debug/ironbark-cli".to_string())
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/ironbark-core
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").canonicalize().unwrap()
}

fn resolve(bin: &str) -> PathBuf {
    let path = Path::new(bin);
    if path.is_absolute() { path.to_path_buf() } else { workspace_root().join(path) }
}

/// A temporary XDG environment for one core instance.
struct Sandbox {
    _dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["data", "state", "runtime", "home"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        Self { _dir: dir }
    }

    fn var(&self, name: &str, sub: &str) -> (String, String) {
        (name.to_string(), self._dir.path().join(sub).to_string_lossy().into_owned())
    }

    fn envs(&self) -> Vec<(String, String)> {
        vec![
            self.var("HOME", "home"),
            self.var("XDG_DATA_HOME", "data"),
            self.var("XDG_STATE_HOME", "state"),
            self.var("XDG_RUNTIME_DIR", "runtime"),
        ]
    }

    fn path(&self, sub: &str) -> PathBuf {
        self._dir.path().join(sub)
    }

    /// Launch the real daemon with an explicit environment.
    fn spawn_core(&self) -> Child {
        let bin = resolve(&core_bin());
        assert!(bin.exists(), "core binary not built at {}", bin.display());
        Command::new(bin)
            .env_clear()
            .envs(self.envs())
            .env("PATH", "/usr/bin:/bin")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .expect("failed to spawn ironbark-core")
    }

    fn run_cli(&self, args: &[&str]) -> std::process::Output {
        let bin = resolve(&cli_bin());
        assert!(bin.exists(), "cli binary not built at {}", bin.display());
        Command::new(bin)
            .env_clear()
            .envs(self.envs())
            .env("PATH", "/usr/bin:/bin")
            .args(args)
            .output()
            .expect("failed to invoke ironbark-cli")
    }
}

/// Block until the daemon prints its readiness line, or panic with its stderr.
fn wait_for_ready(child: &mut Child) {
    let stdout = child.stdout.take().expect("core stdout is piped");
    let stderr = child.stderr.take();
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.starts_with("CORE_READY") => return,
            Ok(_) => continue,
            Err(error) => panic!("failed reading core stdout: {error}"),
        }
    }
    let mut detail = String::from_utf8_lossy(b"").into_owned();
    if let Some(mut err) = stderr {
        use std::io::Read;
        let mut buf = String::new();
        let _ = err.read_to_string(&mut buf);
        detail = buf;
    }
    panic!("core daemon did not emit CORE_READY; stderr: {detail}");
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn status_command_returns_expected_fields() {
    let sandbox = Sandbox::new();
    let mut core = sandbox.spawn_core();
    wait_for_ready(&mut core);

    let out = sandbox.run_cli(&["status"]);
    assert!(out.status.success(), "cli status failed: {}", String::from_utf8_lossy(&out.stderr));
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).expect("status prints one JSON DTO");
    assert_eq!(status.get("protocol").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(status.get("projects").and_then(|v| v.as_str()), Some("0"), "an empty store reports zero projects on the exact decimal wire");
    assert!(status.get("abandoned_requests").is_some(), "status reports abandoned-request accounting");

    // The client disconnecting (window close) must not stop the daemon: a second
    // client still gets a real answer.
    let again = sandbox.run_cli(&["status"]);
    assert!(again.status.success(), "core survived client disconnect");
    kill_and_reap(&mut core);
}

#[test]
fn shutdown_command_gracefully_stops_the_daemon() {
    let sandbox = Sandbox::new();
    let mut core = sandbox.spawn_core();
    wait_for_ready(&mut core);

    let out = sandbox.run_cli(&["shutdown"]);
    assert!(out.status.success(), "cli shutdown failed: {}", String::from_utf8_lossy(&out.stderr));
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).expect("shutdown prints one JSON DTO");
    assert_eq!(reply.get("shutting_down").and_then(|v| v.as_bool()), Some(true));

    // The daemon exits on its own after committing the shutdown reply.
    let status = core.wait().expect("wait failed");
    assert!(status.success(), "the daemon exits cleanly on shutdown: {status:?}");
}

#[test]
fn sigint_stops_core_and_preserves_durable_state() {
    let sandbox = Sandbox::new();
    let mut core = sandbox.spawn_core();
    wait_for_ready(&mut core);

    // A real client request commits durable state before the signal.
    let created = sandbox.run_cli(&["project", "create", "--root", sandbox.path("state").to_str().unwrap(), "lifecycle-fixture"]);
    assert!(created.status.success(), "project create failed: {}", String::from_utf8_lossy(&created.stderr));

    let pid = core.id().to_string();
    let killed = Command::new("kill").arg("-INT").arg(&pid).status().expect("deliver SIGINT");
    assert!(killed.success(), "SIGINT delivered");
    let _ = core.wait();

    // The store survives the signal: a fresh daemon reopens it and reports it.
    let mut restarted = sandbox.spawn_core();
    wait_for_ready(&mut restarted);
    let out = sandbox.run_cli(&["project", "list"]);
    assert!(out.status.success(), "restart status failed: {}", String::from_utf8_lossy(&out.stderr));
    let projects = String::from_utf8_lossy(&out.stdout);
    assert!(projects.contains("lifecycle-fixture"), "durable state survived the signal: {projects}");
    kill_and_reap(&mut restarted);
}

#[test]
fn restart_after_a_kill_reconciles_without_replaying() {
    let sandbox = Sandbox::new();
    let mut core = sandbox.spawn_core();
    wait_for_ready(&mut core);

    let created = sandbox.run_cli(&["project", "create", "--root", sandbox.path("state").to_str().unwrap(), "restart-fixture"]);
    assert!(created.status.success(), "project create failed: {}", String::from_utf8_lossy(&created.stderr));
    let first = sandbox.run_cli(&["project", "list"]);
    assert!(first.status.success());

    // Hard kill (no graceful shutdown): no request may be replayed on restart.
    kill_and_reap(&mut core);
    let mut restarted = sandbox.spawn_core();
    wait_for_ready(&mut restarted);

    let after = sandbox.run_cli(&["project", "list"]);
    assert!(after.status.success(), "status after restart failed");
    let list: serde_json::Value = serde_json::from_slice(&after.stdout).expect("project list prints JSON");
    let names: Vec<&str> = list
        .as_array()
        .map(|rows| rows.iter().filter_map(|row| row.get("name").and_then(|n| n.as_str())).collect())
        .unwrap_or_default();
    assert_eq!(
        names.iter().filter(|name| **name == "restart-fixture").count(),
        1,
        "exactly one project exists after the kill/restart, not a replayed duplicate: {names:?}"
    );
    kill_and_reap(&mut restarted);
}

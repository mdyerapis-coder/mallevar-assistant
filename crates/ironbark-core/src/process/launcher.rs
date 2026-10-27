use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::{fs::File, io::{Read, Write}, os::{fd::{AsRawFd, FromRawFd, OwnedFd, RawFd}, unix::{net::UnixStream, process::CommandExt}}, path::PathBuf, process::{Child, Command, Stdio}, sync::atomic::{AtomicI32, Ordering}, time::{Duration, Instant}};

pub struct SiblingCommand { pub executable: PathBuf, pub argv: Vec<String> }
/// Native-only capabilities: never serialise a descriptor or its contents into env/DTOs.
pub enum NamespaceBootstrap {
    /// Consumer MUST read fd3 then close it before starting any tool descendant.
    /// This is transport support, not qualification of that consumer's isolation.
    PrivateFd3(OwnedFd),
    /// Core-owned duplex transport. Credentials remain a distinct EOF source.
    ManagedWorker { credentials: Option<OwnedFd>, channel: OwnedFd },
    /// AlphaCode's managed daemon and ACP are direct siblings of the PID1 controller.
    /// Each receives its own bounded EOF-terminated identity pipe at fd4, not a shared stream.
    AlphaCodeSiblings { daemon: SiblingCommand, acp: SiblingCommand },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct RunnerConfig {
    pub executable: PathBuf, pub argv: Vec<String>, pub cwd: PathBuf,
    pub env: Vec<(String, String)>, pub private_fd3: bool, pub worker_channel: bool,
    pub siblings: Option<(PathBuf, Vec<String>, PathBuf, Vec<String>)>,
    pub bridge_port: Option<u16>,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Ready { pub namespace: u64, pub start_ticks: u64 }

pub(crate) fn send_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 65536 { return Err(CoreError::new("containment_protocol", "private control message exceeds limit")); }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?; writer.write_all(&bytes)?; Ok(())
}
pub(crate) fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut length = [0;4]; reader.read_exact(&mut length)?; let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > 65536 { return Err(CoreError::new("containment_protocol", "invalid private control message length")); }
    let mut bytes = vec![0;length]; reader.read_exact(&mut bytes)?; Ok(serde_json::from_slice(&bytes)?)
}

/// Call inside every managed native binary AFTER exec, before secrets/config/telemetry.
/// A pre_exec-only dumpable setting is insufficient: exec resets it.
pub fn harden_managed_process() -> Result<()> {
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 { return Err(std::io::Error::last_os_error().into()); }
        // Architecture check first; x32 syscall numbers cannot bypass native deny rules.
        #[cfg(target_arch = "x86_64")] let arch = 0xc000003e_u32;
        #[cfg(target_arch = "aarch64")] let arch = 0xc00000b7_u32;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))] return Err(CoreError::new("containment_unavailable", "seccomp architecture unsupported"));
        let statement = |code, k| libc::sock_filter { code, jt: 0, jf: 0, k };
        let jump = |k, jt, jf| libc::sock_filter { code: 0x15, jt, jf, k };
        let mut filter = vec![statement(0x20, 4), jump(arch, 1, 0), statement(0x06, 0x80000000), statement(0x20, 0)];
        #[cfg(target_arch = "x86_64")] { filter.push(libc::sock_filter { code: 0x35, jt: 0, jf: 1, k: 0x40000000 }); filter.push(statement(0x06, 0x80000000)); }
        for nr in [libc::SYS_ptrace, libc::SYS_process_vm_readv, libc::SYS_process_vm_writev, libc::SYS_pidfd_getfd] {
            filter.push(jump(nr as u32, 0, 1)); filter.push(statement(0x06, 0x00050000 | libc::EPERM as u32));
        }
        filter.push(statement(0x06, 0x7fff0000));
        let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_mut_ptr() };
        // TSYNC also protects existing Tokio/blocking threads; prctl alone
        // would filter only the caller and leave a multithreaded core bypass.
        if libc::syscall(libc::SYS_seccomp,1_u32,1_u32,&program as *const libc::sock_fprog) != 0 { return Err(CoreError::new("containment_unavailable","process-wide seccomp synchronization failed")); }
    }
    Ok(())
}

/// Main must dispatch this before constructing Tokio, database, IPC or auth services.
pub fn namespace_runner_entry() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--ironbark-namespace-runner")) { return None; }
    Some(runner().unwrap_or(125))
}
static SIGNAL: AtomicI32 = AtomicI32::new(0);
extern "C" fn remember_signal(signal: i32) { SIGNAL.store(signal, Ordering::Relaxed); }

fn child_command(executable: &PathBuf, argv: &[String], cwd: &PathBuf, env: &[(String,String)]) -> Command {
    let mut command = Command::new(executable); command.args(argv).current_dir(cwd).env_clear();
    command.env("PATH", "/usr/bin:/bin").env("HOME", "/home/worker").env("TMPDIR", "/tmp").env("XDG_CONFIG_HOME", "/home/worker/.config").env("XDG_CACHE_HOME", "/home/worker/.cache").env("XDG_DATA_HOME", "/home/worker/.local/share").env("XDG_RUNTIME_DIR", "/run/worker");
    for (key, value) in env { command.env(key, value); }
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()); command
}
fn close_child_fds(command: &mut Command, preserve: &[(RawFd, RawFd)]) -> Result<()> {
    // Allocate/duplicate in the parent, not in the post-fork signal-unsafe region.
    let mut temporaries = Vec::with_capacity(preserve.len());
    for &(source, target) in preserve {
        let fd = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 64) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
        temporaries.push((unsafe { OwnedFd::from_raw_fd(fd) }, target));
    }
    unsafe { command.pre_exec(move || {
        // Preserve Rust's exec-error pipe until exec; never close it prematurely.
        if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        for (source, target) in &temporaries {
            if libc::dup2(source.as_raw_fd(), *target) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }); }
    Ok(())
}
fn sibling(command: SiblingCommand, config: &RunnerConfig) -> Result<(Child, File)> {
    let mut pipe = [0;2]; if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 { return Err(std::io::Error::last_os_error().into()); }
    let input = unsafe { File::from_raw_fd(pipe[0]) }; let output = unsafe { File::from_raw_fd(pipe[1]) };
    if command.argv.iter().any(|arg|arg=="--ironbark-controller-fd" || arg.starts_with("--ironbark-controller-fd=")) {return Err(CoreError::new("containment_denied","controller fd is runner-owned"));}
    let mut argv=vec!["--ironbark-controller-fd".into(),"4".into()];argv.extend(command.argv.iter().cloned());
    let mut child = child_command(&command.executable, &argv, &config.cwd, &config.env);
    let socket=command.argv.windows(2).find(|v|v[0]=="--socket").map(|v|PathBuf::from(&v[1])).ok_or_else(||CoreError::new("containment_denied","managed sibling requires an explicit private socket"))?;
    let profile=socket.parent().ok_or_else(||CoreError::new("containment_denied","managed sibling socket has no profile"))?;
    if !profile.starts_with("/home/worker") || profile==std::path::Path::new("/home/worker") || profile.components().any(|c|matches!(c,std::path::Component::ParentDir)) {return Err(CoreError::new("containment_denied","managed sibling profile must be namespace-private"));}
    std::fs::create_dir_all(profile)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(profile,std::fs::Permissions::from_mode(0o700))?;
    child.env("ALPHACODE_HOME",profile).env("ALPHACODE_NO_TELEMETRY","1");
    close_child_fds(&mut child, &[(input.as_raw_fd(),4)])?;
    let child = child.spawn()?; drop(input); Ok((child,output))
}
fn signal_namespace(signal: i32) { unsafe { libc::kill(-1,signal); } }
fn reap() -> bool {
    loop { let result = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if result > 0 { continue; }
        if result == 0 { return false; }
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    }
}
fn runner() -> Result<i32> {
    if unsafe { libc::getpid() } != 1 { return Err(CoreError::new("containment_denied", "runner must be PID1 in owned namespace")); }
    harden_managed_process()?;
    unsafe { libc::signal(libc::SIGINT, remember_signal as *const () as libc::sighandler_t); libc::signal(libc::SIGTERM, remember_signal as *const () as libc::sighandler_t); libc::prctl(libc::PR_SET_CHILD_SUBREAPER,1,0,0,0); }
    let mut control = unsafe { UnixStream::from_raw_fd(3) };
    control.set_read_timeout(Some(Duration::from_secs(5)))?;
    let config: RunnerConfig = read_frame(&mut control)?;
    // bwrap opened host mount handles must not survive in the namespace controller.
    unsafe {
        libc::close(6);
        if libc::syscall(libc::SYS_close_range, if config.worker_channel {8_u32} else {7_u32},u32::MAX,0_u32) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let mut bridge: Option<Child> = None;
    if let Some(port) = config.bridge_port {
        let mut command = child_command(&PathBuf::from("/opt/ironbark/namespace-bridge"), &["--bootstrap-fd".into(),"3".into()], &config.cwd, &[]);
        close_child_fds(&mut command,&[(5,3)])?; bridge = Some(command.spawn()?);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().map_err(|_| CoreError::new("containment_protocol","invalid loopback port"))?, Duration::from_millis(50)).is_ok() { break; }
            if Instant::now() >= deadline || bridge.as_mut().unwrap().try_wait()?.is_some() { return Err(CoreError::new("containment_unavailable","namespace-local bridge did not bind")); }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    unsafe { libc::close(5); }
    let mut sibling_children = Vec::new();
    if let Some((daemon, daemon_argv, acp, acp_argv)) = &config.siblings {
        let (daemon, mut daemon_pipe) = sibling(SiblingCommand { executable:daemon.clone(),argv:daemon_argv.clone() },&config)?;
        let (acp, mut acp_pipe) = sibling(SiblingCommand { executable:acp.clone(),argv:acp_argv.clone() },&config)?;
        let identity = serde_json::to_vec(&serde_json::json!({"controller_pid":1,"daemon_pid":daemon.id(),"acp_pid":acp.id()}))?;
        if identity.len() > 4096 { return Err(CoreError::new("containment_protocol","sibling identity exceeds pipe limit")); }
        daemon_pipe.write_all(&identity)?; acp_pipe.write_all(&identity)?; drop(daemon_pipe); drop(acp_pipe);
        sibling_children.push(daemon); sibling_children.push(acp);
    }
    send_frame(&mut control,&Ready {namespace:super::sandbox::namespace_id(1)?,start_ticks:super::supervisor::read_stat(1)?.1})?;
    let mut ack=[0];control.read_exact(&mut ack)?;
    if ack!=[2] {return Err(CoreError::new("containment_protocol","controller did not acknowledge namespace ownership"));}
    let mut command = child_command(&config.executable,&config.argv,&config.cwd,&config.env);
    let mut preserved = Vec::new();
    if config.private_fd3 { preserved.push((4,3)); }
    if config.worker_channel { preserved.push((7,6)); }
    close_child_fds(&mut command, &preserved)?;
    let mut worker = command.spawn()?;
    // PID1 must not keep a duplicate socket endpoint alive after worker exit.
    drop(command);
    unsafe { libc::close(4); libc::close(7); }
    control.set_read_timeout(None)?; control.set_nonblocking(true)?;
    let mut stopping: Option<(Instant,u8)> = None; let mut code = 0;
    loop {
        let mut byte = [0]; match control.read(&mut byte) {
            Ok(0) => { signal_namespace(libc::SIGKILL); return Ok(125); },
            Ok(_) if stopping.is_none() => { signal_namespace(libc::SIGINT); stopping = Some((Instant::now(),0)); },
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {},
            Err(_) => { signal_namespace(libc::SIGKILL); return Ok(125); }, _ => {}
        }
        if SIGNAL.swap(0,Ordering::Relaxed) != 0 && stopping.is_none() { signal_namespace(libc::SIGINT); stopping=Some((Instant::now(),0)); }
        if stopping.is_none() {
            if let Some(status) = worker.try_wait()? { code=status.code().unwrap_or(128); signal_namespace(libc::SIGTERM); stopping=Some((Instant::now(),1)); }
            if bridge.as_mut().map(|c| c.try_wait()).transpose()?.flatten().is_some() || sibling_children.iter_mut().any(|c| !matches!(c.try_wait(),Ok(None))) { code=125;signal_namespace(libc::SIGINT);stopping=Some((Instant::now(),0)); }
        }
        if let Some((since,stage)) = stopping {
            if since.elapsed() >= Duration::from_secs(2) { let next=stage+1; signal_namespace(if next==1 {libc::SIGTERM} else {libc::SIGKILL}); stopping=Some((Instant::now(),next)); }
            if reap() { return Ok(code); }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

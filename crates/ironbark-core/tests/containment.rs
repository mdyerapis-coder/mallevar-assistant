//! Real Linux containment regressions. No skip is a qualification result.
//! Mutations caught: root/home mounts, following workspace symlinks, inherited
//! loader environment, host network sharing, and parent-only process cancellation.
use ironbark_core::contracts::{ProjectId, ProjectPolicy, RunId, SpendPolicy};
use ironbark_core::process::{LaunchSpec, ProcessSupervisor, RuntimeManifest};
use std::{fs, path::PathBuf, time::Duration};

struct Fixture { root: PathBuf, workspace: PathBuf, outside: PathBuf }
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ironbark-containment-{}", RunId::new()));
        let workspace = root.join("workspace"); let outside = root.join("outside");
        fs::create_dir_all(&workspace).unwrap(); fs::create_dir_all(&outside).unwrap();
        fs::create_dir(outside.join("missing")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root,fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&outside,fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(outside.join("sentinel"), "PRIVATE_SENTINEL").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("escape-link")).unwrap();
        Self { root, workspace, outside }
    }
    fn supervisor(&self) -> ProcessSupervisor {
        ProcessSupervisor::new(ProjectPolicy { version: 1, project_id: ProjectId::new(), allowed_connection_ids: vec![], allowed_tools: vec![], read_roots: vec![self.workspace.to_string_lossy().into_owned()], write_roots: vec![self.workspace.to_string_lossy().into_owned()], network_destinations: vec![], local_only: true, max_workers: 2, max_requests: 4, max_cost_microusd: None, spend_policy: SpendPolicy::FreeOnly, allow_unknown_cost: false }, PathBuf::from(env!("CARGO_BIN_EXE_ironbark-core"))).unwrap()
    }
    fn spec(&self, mode: &str) -> LaunchSpec {
        let runner = PathBuf::from(env!("CARGO_BIN_EXE_ironbark-core"));
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qa/fixtures/escape-worker.sh").canonicalize().unwrap();
        let mut runtime = RuntimeManifest::discover_elf(&[runner, PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/sleep"), PathBuf::from("/usr/bin/setsid")], &[PathBuf::from("/usr/lib"), PathBuf::from("/usr/lib/x86_64-linux-gnu"), PathBuf::from("/lib64")]).unwrap();
        runtime.add_file(fixture, PathBuf::from("/fixture.sh")).unwrap();
        LaunchSpec { run_id: RunId::new(), executable: PathBuf::from("/bin/bash"), argv: vec!["/fixture.sh".into(), mode.into(), self.outside.to_string_lossy().into_owned()], cwd: self.workspace.clone(), readonly_mounts: vec![], writable_mounts: vec![self.workspace.clone()], env_allowlist: vec![], network_lease: None, runtime, bootstrap: None }
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }

#[test]
fn symlink_and_nonexistent_tail_escape_is_denied() {
    let f = Fixture::new(); let supervisor = f.supervisor();
    let lease = supervisor.spawn(f.spec("escape")).unwrap();
    let receipt = supervisor.wait(lease, Duration::from_secs(10)).unwrap();
    assert!(receipt.all_terminated); assert!(f.workspace.join("allowed").exists());
    assert!(!f.outside.join("absolute-write").exists());
    assert!(!f.outside.join("symlink-write").exists());
    assert!(!f.outside.join("missing/tail-write").exists());
    assert_eq!(fs::read_to_string(f.workspace.join("sentinel-result")).unwrap(), "absent\n");
    evidence("symlink_and_nonexistent_tail_escape_is_denied",&receipt);
}
#[test]
fn secret_home_is_not_mounted() {
    let f = Fixture::new(); let s = f.supervisor(); let receipt = s.wait(s.spawn(f.spec("home")).unwrap(), Duration::from_secs(10)).unwrap();
    assert!(receipt.all_terminated);
    assert_eq!(fs::read_to_string(f.workspace.join("home-result")).unwrap(), "private\n");
    evidence("secret_home_is_not_mounted",&receipt);
}
#[test]
fn shell_cannot_bypass_network_allowlist() {
    let f = Fixture::new(); let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); listener.set_nonblocking(true).unwrap();
    let s = f.supervisor(); let mut spec = f.spec("network"); spec.argv.push(listener.local_addr().unwrap().port().to_string());
    let receipt = s.wait(s.spawn(spec).unwrap(), Duration::from_secs(10)).unwrap(); assert!(receipt.all_terminated);
    assert_eq!(fs::read_to_string(f.workspace.join("network-result")).unwrap(), "denied\n");
    assert!(listener.accept().is_err());
    evidence("shell_cannot_bypass_network_allowlist",&receipt);
}
#[test]
fn cancel_kills_grandchild_and_background_process() {
    let f = Fixture::new(); let s = f.supervisor(); let lease = s.spawn(f.spec("children")).unwrap();
    for _ in 0..200 { if f.workspace.join("children-ready").exists() { break; } std::thread::sleep(Duration::from_millis(10)); }
    assert!(f.workspace.join("children-ready").exists());
    let before = lease.owned_processes().unwrap(); assert!(before.len() >= 4, "must observe double-fork descendants, not just launcher");
    let receipt = s.stop(lease, 2000).unwrap(); assert!(receipt.all_terminated); assert!(receipt.owned_pids.len() >= 4);
    for identity in before { assert!(!identity.is_live().unwrap(), "owned PID/start tick survived cancellation"); }
    evidence("cancel_kills_grandchild_and_background_process",&receipt);
}
#[test]
fn symlink_mount_and_unapproved_loader_options_are_rejected() {
    let f = Fixture::new(); let s = f.supervisor(); let mut spec = f.spec("escape"); spec.writable_mounts.push(f.workspace.join("escape-link/missing"));
    assert!(s.spawn(spec).is_err());
    let mut spec = f.spec("escape"); spec.env_allowlist.push(("LD_PRELOAD".into(), "/outside.so".into())); assert!(s.spawn(spec).is_err());
}
#[test]
fn runtime_ancestor_rename_and_substitution_are_denied() {
    // Real rename/substitution regression, not argument inspection: the worker
    // attempts to write the runtime root, create a sibling of the private HOME,
    // and rename the private HOME boundary itself. All must fail while the
    // approved workspace bind stays writable.
    let f = Fixture::new(); let s = f.supervisor();
    let lease = s.spawn(f.spec("substitute")).unwrap();
    let receipt = s.wait(lease, Duration::from_secs(10)).unwrap();
    assert!(receipt.all_terminated);
    assert_eq!(fs::read_to_string(f.workspace.join("home-result")).unwrap(), "private\n");
    assert!(f.workspace.join("private-write").exists(), "approved workspace bind must stay writable");
    evidence("runtime_ancestor_rename_and_substitution_are_denied",&receipt);
}
fn evidence(name:&str,receipt:&ironbark_core::process::StopReceipt) {
    if let Some(directory)=std::env::var_os("IRONBARK_CONTAINMENT_EVIDENCE") {
        let directory=PathBuf::from(directory);fs::create_dir_all(&directory).unwrap();
        let identities:Vec<_>=receipt.owned_pids.iter().map(|p|serde_json::json!({"pid":p.pid,"start_ticks":p.start_ticks,"sandbox_identity":p.sandbox_identity,"live":p.is_live().unwrap()})).collect();
        fs::write(directory.join(format!("{name}.json")),serde_json::to_vec(&serde_json::json!({"test":name,"all_terminated":receipt.all_terminated,"exit_code":receipt.exit_code,"owned":identities})).unwrap()).unwrap();
    }
}

#[test]
fn worker_budget_is_enforced_for_live_namespaces() {
    let f=Fixture::new();let s=f.supervisor();
    let first=s.spawn(f.spec("children")).unwrap();let second=s.spawn(f.spec("children")).unwrap();
    let denied=s.spawn(f.spec("children")).err().expect("third concurrent namespace must exceed policy cap");
    assert_eq!(denied.code,"worker_budget_exhausted");
    let one=s.stop(first,2000).unwrap();let two=s.stop(second,2000).unwrap();
    assert!(one.all_terminated && two.all_terminated);
    let next=s.wait(s.spawn(f.spec("escape")).unwrap(),Duration::from_secs(10)).unwrap();
    assert!(next.all_terminated);
    evidence("worker_budget_is_enforced_for_live_namespaces",&next);
}

#[test]
fn ptrace_and_process_vm_are_denied() {
    // A real re-exec of this test under PID1 exercises inherited seccomp; no
    // mocked syscalls or mode-only assertions can pass this regression.
    if std::env::current_dir().unwrap().join("syscall-child-marker").exists() {
        let mut byte=42_u8;
        unsafe {
            assert_eq!(libc::ptrace(libc::PTRACE_TRACEME,0,0,0),-1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(libc::EPERM));
            let vector=libc::iovec {iov_base:(&mut byte as *mut u8).cast(),iov_len:1};
            assert_eq!(libc::process_vm_readv(libc::getpid(),&vector,1,&vector,1,0),-1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(libc::EPERM));
            assert_eq!(libc::process_vm_writev(libc::getpid(),&vector,1,&vector,1,0),-1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(libc::EPERM));
        }
        ironbark_core::process::harden_managed_process().unwrap();
        assert_eq!(unsafe {libc::prctl(libc::PR_GET_DUMPABLE)},0);
        assert!(!std::path::Path::new("/proc/self/fd/3").exists());
        assert!(!std::path::Path::new("/proc/self/fd/4").exists());
        assert!(!std::path::Path::new("/proc/self/fd/5").exists());
        fs::write("syscall-result","denied\n").unwrap();return;
    }
    let f=Fixture::new();let s=f.supervisor();fs::write(f.workspace.join("syscall-child-marker"),"probe").unwrap();
    let mut spec=f.spec("escape");let executable=std::env::current_exe().unwrap();
    let extra=RuntimeManifest::discover_elf(std::slice::from_ref(&executable),&[PathBuf::from("/usr/lib"),PathBuf::from("/usr/lib/x86_64-linux-gnu"),PathBuf::from("/lib64")]).unwrap();
    spec.runtime.merge(extra).unwrap();spec.executable=executable;spec.argv=vec!["--exact".into(),"ptrace_and_process_vm_are_denied".into(),"--test-threads=1".into()];
    let receipt=s.wait(s.spawn(spec).unwrap(),Duration::from_secs(10)).unwrap();
    assert!(receipt.all_terminated);assert_eq!(fs::read_to_string(f.workspace.join("syscall-result")).unwrap(),"denied\n");
    evidence("ptrace_and_process_vm_are_denied",&receipt);
}

#[test]
fn core_death_terminates_owned_namespace() {
    use std::io::Read;
    if std::env::var_os("IRONBARK_CONTROLLER_CHILD").is_some() {
        ironbark_core::process::harden_managed_process().unwrap();
        let workspace=std::env::current_dir().unwrap();let root=workspace.parent().unwrap().to_path_buf();
        let f=Fixture {outside:root.join("outside"),workspace,root};let s=f.supervisor();let lease=s.spawn(f.spec("children")).unwrap();
        for _ in 0..200 {if f.workspace.join("children-ready").exists(){break;}std::thread::sleep(Duration::from_millis(10));}
        assert!(f.workspace.join("children-ready").exists());
        let owned=lease.owned_processes().unwrap();assert!(owned.len()>=4);
        fs::write(f.workspace.join("controller-owned.tmp"),serde_json::to_vec(&owned.iter().map(|p|p.pid).collect::<Vec<_>>()).unwrap()).unwrap();
        fs::rename(f.workspace.join("controller-owned.tmp"),f.workspace.join("controller-owned.json")).unwrap();
        loop {std::thread::sleep(Duration::from_secs(1));}
    }
    let f=Fixture::new();
    let mut controller=ControllerGuard(std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","core_death_terminates_owned_namespace","--test-threads=1"]).env("IRONBARK_CONTROLLER_CHILD","1").current_dir(&f.workspace).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
    for _ in 0..1000 {if f.workspace.join("controller-owned.json").exists(){break;}assert!(controller.try_wait().unwrap().is_none());std::thread::sleep(Duration::from_millis(10));}
    let mut input=fs::File::open(f.workspace.join("controller-owned.json")).unwrap();let mut bytes=Vec::new();input.read_to_end(&mut bytes).unwrap();
    let pids:Vec<u32>=serde_json::from_slice(&bytes).unwrap();assert!(pids.len()>=4);
    use std::os::fd::{AsRawFd,FromRawFd,OwnedFd};
    let descriptors:Vec<OwnedFd>=pids.iter().map(|p|{let fd=unsafe {libc::syscall(libc::SYS_pidfd_open,*p,0)};assert!(fd>=0);unsafe {OwnedFd::from_raw_fd(fd as i32)}}).collect();
    controller.kill().unwrap();controller.wait().unwrap();
    for descriptor in descriptors {
        let mut poll=libc::pollfd {fd:descriptor.as_raw_fd(),events:libc::POLLIN,revents:0};
        assert!(unsafe {libc::poll(&mut poll,1,8000)}>0,"namespace member survived core death");
    }
    if let Some(directory)=std::env::var_os("IRONBARK_CONTAINMENT_EVIDENCE") {fs::write(PathBuf::from(directory).join("core_death_terminates_owned_namespace.json"),serde_json::to_vec(&serde_json::json!({"test":"core_death_terminates_owned_namespace","all_terminated":true,"observed_count":pids.len()})).unwrap()).unwrap();}
}

#[test]
fn private_fd3_is_consumed_then_absent_from_tool() {
    use std::os::fd::FromRawFd;
    use std::io::Read;
    if std::env::current_dir().unwrap().join("fd3-child-marker").exists() {
        ironbark_core::process::harden_managed_process().unwrap();
        let mut payload=String::new();
        let input=unsafe {fs::File::from_raw_fd(3)};
        input.take(4097).read_to_string(&mut payload).unwrap();
        assert_eq!(payload,"scoped-test-provider-lease");
        assert!(!std::path::Path::new("/proc/self/fd/3").exists());
        let status=std::process::Command::new("/bin/bash").args(["/fixture.sh","fds","/not-mounted"]).env_clear().status().unwrap();
        assert!(status.success());return;
    }
    let f=Fixture::new();let s=f.supervisor();fs::write(f.workspace.join("fd3-child-marker"),"probe").unwrap();
    let payload=f.outside.join("private-provider-fd");fs::write(&payload,"scoped-test-provider-lease").unwrap();
    let mut spec=f.spec("fds");let executable=std::env::current_exe().unwrap();
    let extra=RuntimeManifest::discover_elf(std::slice::from_ref(&executable),&[PathBuf::from("/usr/lib"),PathBuf::from("/usr/lib/x86_64-linux-gnu"),PathBuf::from("/lib64")]).unwrap();spec.runtime.merge(extra).unwrap();
    spec.executable=executable;spec.argv=vec!["--exact".into(),"private_fd3_is_consumed_then_absent_from_tool".into(),"--test-threads=1".into()];
    spec.bootstrap=Some(ironbark_core::process::NamespaceBootstrap::PrivateFd3(fs::File::open(payload).unwrap().into()));
    let receipt=s.wait(s.spawn(spec).unwrap(),Duration::from_secs(10)).unwrap();
    assert!(receipt.all_terminated);assert_eq!(fs::read_to_string(f.workspace.join("fd-result")).unwrap(),"closed\n");
    evidence("private_fd3_is_consumed_then_absent_from_tool",&receipt);
}

// Test-only RAII: failure to obtain a witness cannot leak the native controller.
struct ControllerGuard(std::process::Child);
impl std::ops::Deref for ControllerGuard {type Target=std::process::Child;fn deref(&self)->&Self::Target {&self.0}}
impl std::ops::DerefMut for ControllerGuard {fn deref_mut(&mut self)->&mut Self::Target {&mut self.0}}
impl Drop for ControllerGuard {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}

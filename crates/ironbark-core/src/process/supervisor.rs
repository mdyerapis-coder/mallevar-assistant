use super::{LaunchSpec, launcher::{self, NamespaceBootstrap, Ready, RunnerConfig}, sandbox};
use crate::{contracts::{ProjectPolicy, RunId}, error::{CoreError, Result}, net::NetworkLease};
use std::{collections::{BTreeMap,BTreeSet}, fs::{self,File}, io::{Read,Write}, os::{fd::{AsRawFd,FromRawFd,OwnedFd}, unix::{fs::MetadataExt, net::UnixStream,process::CommandExt}}, path::{Path,PathBuf}, process::{Child,Command,Stdio}, sync::{Arc,atomic::{AtomicUsize,Ordering}}, time::{Duration,Instant}};

fn denied(message: &str) -> CoreError { CoreError::new("containment_denied",message) }
/// A runtime file is exposed only when its path or another hardlink of the same
/// inode sits inside an approved writable root. nlink>1 alone is not exposure:
/// system libraries are often hardlinked under /lib and /usr/lib.
fn runtime_source_exposed(source: &Path, writable: &[PathBuf]) -> bool {
    if writable.iter().any(|root| source.starts_with(root)) { return true; }
    let Ok(meta) = fs::metadata(source) else { return true; };
    if !meta.is_file() { return true; }
    if meta.nlink() <= 1 { return false; }
    let (dev, ino) = (meta.dev(), meta.ino());
    writable.iter().any(|root| inode_in_tree(root, dev, ino))
}
fn inode_in_tree(root: &Path, dev: u64, ino: u64) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { return true; };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { return true; };
            if meta.dev() == dev && meta.ino() == ino { return true; }
            if meta.is_dir() && !meta.file_type().is_symlink() { stack.push(path); }
        }
    }
    false
}
fn pidfd(pid: u32) -> Result<OwnedFd> { let fd=unsafe {libc::syscall(libc::SYS_pidfd_open,pid,0_u32)}; if fd<0 {return Err(std::io::Error::last_os_error().into());} Ok(unsafe {OwnedFd::from_raw_fd(fd as i32)}) }
pub(crate) fn read_stat(pid:u32)->Result<(u32,u64)> {
    let stat=fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let tail=stat.rsplit_once(')').ok_or_else(||denied("invalid process stat"))?.1;
    let mut fields=tail.split_whitespace();
    let parent=fields.nth(1).and_then(|s|s.parse().ok()).ok_or_else(||denied("invalid parent identity"))?;
    let start=fields.nth(17).and_then(|s|s.parse().ok()).ok_or_else(||denied("invalid process start identity"))?;
    Ok((parent,start))
}
/// pidfd protects against PID reuse. sandbox_identity is the owning PID namespace,
/// anchored by bwrap child-pid plus runner's private namespace/start-tick handshake.
#[derive(Clone)]
pub struct ProcessIdentity { pub pid:u32, pub start_ticks:u64, pub sandbox_identity:u64, descriptor:Arc<OwnedFd> }
impl ProcessIdentity {
    fn capture(pid:u32,namespace:u64)->Result<Self> {
        let before=read_stat(pid)?.1; let descriptor=pidfd(pid)?;
        if read_stat(pid)?.1!=before {return Err(denied("process identity changed during observation"));}
        Ok(Self {pid,start_ticks:before,sandbox_identity:namespace,descriptor:Arc::new(descriptor)})
    }
    pub fn is_live(&self)->Result<bool> {
        let mut poll=libc::pollfd {fd:self.descriptor.as_raw_fd(),events:libc::POLLIN,revents:0};
        if unsafe {libc::poll(&mut poll,1,0)}<0 {return Err(std::io::Error::last_os_error().into());}
        Ok(poll.revents==0)
    }
    fn signal(&self,signal:i32)->Result<()> {
        if unsafe {libc::syscall(libc::SYS_pidfd_send_signal,self.descriptor.as_raw_fd(),signal,std::ptr::null::<libc::siginfo_t>(),0_u32)}<0 {
            let error=std::io::Error::last_os_error(); if error.raw_os_error()!=Some(libc::ESRCH) {return Err(error.into());}
        } Ok(())
    }
}
struct WorkerSlot(Arc<AtomicUsize>);
impl Drop for WorkerSlot {fn drop(&mut self){self.0.fetch_sub(1,Ordering::AcqRel);}}

pub struct ProcessLease {
    pub run_id:RunId, pub pid:u32, pub process_group_id:u32, pub sandbox_identity:u64,
    runner:ProcessIdentity, child:Child, control:UnixStream, network:Option<NetworkLease>,
    observed:BTreeMap<(u32,u64),ProcessIdentity>, preserved_artifacts:Vec<PathBuf>, _slot:WorkerSlot,
    finished:bool,
}
impl ProcessLease {
    pub fn owned_processes(&self)->Result<Vec<ProcessIdentity>> {
        if !self.runner.is_live()? {return Ok(Vec::new());}
        match read_stat(self.pid) {
            Ok((_,ticks)) if ticks==self.runner.start_ticks=>{},
            _ if !self.runner.is_live()?=>return Ok(Vec::new()),
            _=>return Err(denied("namespace controller identity changed")),
        }
        let mut stats=BTreeMap::new();
        for entry in fs::read_dir("/proc")? {
            let entry=entry?; if let Some(pid)=entry.file_name().to_str().and_then(|s|s.parse::<u32>().ok()) && let Ok(stat)=read_stat(pid) {stats.insert(pid,stat);}
        }
        // Double forks/setsid cannot escape PID namespace PID1 adoption. Host PPid
        // ancestry is readable even when /proc/PID/ns is hidden by nondumpable.
        let mut owned=BTreeSet::from([self.pid]);
        loop {let old=owned.len(); for (&pid,&(parent,_)) in &stats {if owned.contains(&parent){owned.insert(pid);}} if owned.len()==old {break;}}
        let mut identities=Vec::new();
        for pid in owned {
            // Revalidate every ancestor's start tick; a recycled intermediary
            // PID must never turn an unrelated host process into an owned child.
            let mut ancestor=pid;let mut valid=true;let mut seen=BTreeSet::new();
            while ancestor!=self.pid {
                if !seen.insert(ancestor) {valid=false;break;}
                let Some(expected)=stats.get(&ancestor) else {valid=false;break;};
                if read_stat(ancestor).ok().as_ref()!=Some(expected) {valid=false;break;}
                ancestor=expected.0;
            }
            if !valid {continue;}
            if pid==self.pid {identities.push(self.runner.clone());continue;}
            if let Ok(identity)=ProcessIdentity::capture(pid,self.sandbox_identity)
                && stats.get(&pid).is_some_and(|(_,tick)|*tick==identity.start_ticks) && identity.is_live()? {identities.push(identity);}
        }
        if !self.runner.is_live()? {return Ok(identities);}
        match read_stat(self.pid) {
            Ok((_,ticks)) if ticks==self.runner.start_ticks=>{},
            _ if !self.runner.is_live()?=>return Ok(identities),
            _=>return Err(denied("namespace identity changed while observing descendants")),
        }
        Ok(identities)
    }
    fn observe(&mut self)->Result<()> {for identity in self.owned_processes()? {self.observed.insert((identity.pid,identity.start_ticks),identity);} Ok(())}
    /// Persist every identity exposed to the controller, including short-lived
    /// descendants which may disappear before cancellation starts.
    pub fn observe_owned_processes(&mut self)->Result<Vec<ProcessIdentity>> {
        let identities=self.owned_processes()?;
        for identity in &identities {self.observed.insert((identity.pid,identity.start_ticks),identity.clone());}
        Ok(identities)
    }
}
impl Drop for ProcessLease {
    fn drop(&mut self) {
        if let Some(network)=&self.network {network.revoke();}
        if !self.finished {let _=self.runner.signal(libc::SIGKILL);bounded_child_teardown(&mut self.child);}
    }
}
pub struct StopReceipt {pub owned_pids:Vec<ProcessIdentity>,pub all_terminated:bool,pub preserved_artifacts:Vec<PathBuf>,pub exit_code:Option<i32>}
struct ApprovedRoot {path:PathBuf,descriptor:File}
impl ApprovedRoot {
    fn new(path:&str)->Result<Self> {
        let path=sandbox::approved_root(Path::new(path))?;let descriptor=sandbox::pin(&path,true)?;
        if fs::read_link(format!("/proc/self/fd/{}",descriptor.as_raw_fd()))?!=path {return Err(denied("approved workspace changed while pinning"));}
        Ok(Self {path,descriptor})
    }
}
pub struct ProcessSupervisor { read_roots:Vec<ApprovedRoot>,write_roots:Vec<ApprovedRoot>,runner:PathBuf,max_workers:usize,workers:Arc<AtomicUsize>,local_only:bool,policy:ProjectPolicy }
impl ProcessSupervisor {
    pub fn new(policy:ProjectPolicy,namespace_runner:PathBuf)->Result<Self> {
        if policy.max_workers==0 {return Err(denied("zero worker budget"));}
        let reads=policy.read_roots.iter().map(|p|ApprovedRoot::new(p)).collect::<Result<Vec<_>>>()?;
        let writes=policy.write_roots.iter().map(|p|ApprovedRoot::new(p)).collect::<Result<Vec<_>>>()?;
        let runner=namespace_runner.canonicalize()?;
        if !runner.is_file(){return Err(denied("missing namespace runner"));}
        Ok(Self {read_roots:reads,write_roots:writes,runner,max_workers:policy.max_workers as usize,workers:Arc::new(AtomicUsize::new(0)),local_only:policy.local_only,policy})
    }
    /// Reuse approved-root pinning and the supplied supervisor's policy/budget.
    pub(crate) fn pin_worker_root(&self,root:&Path,policy:&ProjectPolicy)->Result<File> {
        if policy!=&self.policy {return Err(denied("worker host and supervisor policy differ"));}
        self.mount(root,false).map(|(_,file)|file)
    }
    fn slot(&self)->Result<WorkerSlot> {
        self.workers.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|if n<self.max_workers {Some(n+1)}else{None}).map_err(|_|CoreError::new("worker_budget_exhausted","project worker limit reached"))?;
        Ok(WorkerSlot(self.workers.clone()))
    }
    fn mount(&self,path:&Path,writable:bool)->Result<(PathBuf,File)> {
        let canonical=sandbox::canonical_existing_ancestor(path)?;
        let roots=if writable {&self.write_roots}else{&self.read_roots};
        let root=roots.iter().filter(|root|canonical.starts_with(&root.path)).max_by_key(|root|root.path.components().count()).ok_or_else(||denied("mount outside approved workspace"))?;
        if canonical!=path || !canonical.is_dir() {return Err(denied("mount must use its canonical existing path, without symlink or missing tail"));}
        let relative=canonical.strip_prefix(&root.path).map_err(|_|denied("mount outside approved root"))?;
        let relative=if relative.as_os_str().is_empty(){Path::new(".")}else{relative};
        use std::os::unix::ffi::OsStrExt;
        let name=std::ffi::CString::new(relative.as_os_str().as_bytes()).map_err(|_|denied("invalid mount path"))?;
        #[repr(C)] struct OpenHow {flags:u64,mode:u64,resolve:u64}
        let how=OpenHow {flags:(libc::O_RDONLY|libc::O_DIRECTORY|libc::O_CLOEXEC) as u64,mode:0,resolve:0x01|0x02|0x04|0x08};
        let fd=unsafe {libc::syscall(libc::SYS_openat2,root.descriptor.as_raw_fd(),name.as_ptr(),&how,std::mem::size_of::<OpenHow>())};
        if fd<0 {return Err(CoreError::new("containment_denied","directory-relative mount open rejected; no symlink, submount or legacy fallback"));}
        let file=unsafe {File::from_raw_fd(fd as i32)};
        use std::os::unix::fs::MetadataExt;
        if file.metadata()?.uid()!=unsafe {libc::geteuid()} {return Err(denied("workspace mount owner changed"));}
        // Pinned directory fd makes the mount authoritative even if a path is renamed.
        let pinned=fs::read_link(format!("/proc/self/fd/{}",file.as_raw_fd()))?;
        if pinned!=canonical {return Err(denied("workspace moved while pinning"));}
        sandbox::reject_submounts(&canonical)?;
        Ok((canonical,file))
    }
    pub fn spawn(&self,spec:LaunchSpec)->Result<ProcessLease> {
        let mut network_guard=NetworkFailure(spec.network_lease.clone());
        if let Some(network)=&spec.network_lease {
            if self.local_only || network.run_id()!=spec.run_id {return Err(denied("network lease violates local-only or pinned run scope"));}
            network.assert_live()?;
        }
        let slot=self.slot()?; sandbox::validate_env(&spec.env_allowlist)?;
        if spec.argv.iter().any(|arg|arg.contains('\0')) {return Err(denied("invalid argv"));}
        if !spec.runtime.files.contains_key(&spec.executable) {return Err(denied("executable missing from exact runtime manifest"));}
        if !spec.runtime.files.values().any(|p|p==&self.runner) {return Err(denied("namespace controller missing from exact runtime manifest"));}
        let runner_destination=PathBuf::from("/opt/ironbark/namespace-runner");
        let cwd=sandbox::canonical_existing_ancestor(&spec.cwd)?;
        if cwd!=spec.cwd || !cwd.is_dir() || !spec.writable_mounts.iter().chain(&spec.readonly_mounts).any(|p|cwd.starts_with(p)) {return Err(denied("cwd must be inside a requested canonical workspace mount"));}
        let (mut control,parent_control)=UnixStream::pair()?; control.set_read_timeout(Some(Duration::from_secs(8)))?; control.set_write_timeout(Some(Duration::from_secs(5)))?;
        let (info,child_info)=UnixStream::pair()?; info.set_read_timeout(Some(Duration::from_secs(8)))?;
        let null=File::open("/dev/null")?;
        let mut descriptors:Vec<(i32,OwnedFd)>=vec![(3,parent_control.into()),(4,null.try_clone()?.into()),(5,null.try_clone()?.into()),(6,child_info.into()),(7,null.into())];
        let mut config=RunnerConfig {executable:spec.executable.clone(),argv:spec.argv,cwd,env:spec.env_allowlist,private_fd3:false,worker_channel:false,siblings:None,bridge_port:None};
        match spec.bootstrap {
            Some(NamespaceBootstrap::PrivateFd3(fd))=>{descriptors[1].1=fd;config.private_fd3=true;},
            Some(NamespaceBootstrap::ManagedWorker {credentials,channel})=>{
                descriptors[4].1=channel;config.worker_channel=true;config.private_fd3=true;
                // An unauthenticated worker still consumes an empty EOF capsule.
                if let Some(fd)=credentials {descriptors[1].1=fd;}
            },
            Some(NamespaceBootstrap::AlphaCodeSiblings {daemon,acp})=>{
                for executable in [&daemon.executable,&acp.executable] {if !spec.runtime.files.contains_key(executable) {return Err(denied("managed sibling missing from runtime manifest"));}}
                config.siblings=Some((daemon.executable,daemon.argv,acp.executable,acp.argv));
            },None=>{}
        }
        let mut command=Command::new("/usr/bin/bwrap"); command.env_clear().args(["--unshare-all","--die-with-parent","--new-session","--as-pid-1","--cap-drop","ALL","--clearenv","--proc","/proc","--dev","/dev","--tmpfs","/tmp","--tmpfs","/home","--tmpfs","/home/worker","--tmpfs","/run","--tmpfs","/run/worker"]);
        command.args(["--setenv","PATH","/usr/bin:/bin","--setenv","HOME","/home/worker","--setenv","XDG_RUNTIME_DIR","/run/worker","--info-fd","6"]);
        let mut next_fd=8;
        let mut add_mount=|command:&mut Command,source:OwnedFd,destination:&Path,readonly:bool| {
            let fd=next_fd;next_fd+=1; command.arg(if readonly {"--ro-bind"}else{"--bind"}).arg(format!("/proc/self/fd/{fd}")).arg(destination); descriptors.push((fd,source));
        };
        for (destination,source) in &spec.runtime.files {
            // The namespace controller must not be bind-mounted at its host path
            // (that would create /home/<user>/... prefix nodes inside the private
            // HOME tmpfs, leaking the operator's home layout to every worker).
            // Bind it at a fixed neutral namespace path instead.
            let destination = if source==&self.runner { runner_destination.clone() } else { destination.clone() };
            sandbox::validate_destination(&destination)?;
            if spec.readonly_mounts.iter().chain(&spec.writable_mounts).any(|m|destination.starts_with(m) || m.starts_with(&destination)) {return Err(denied("runtime and workspace mounts overlap"));}
            if runtime_source_exposed(source, &spec.writable_mounts) {return Err(denied("runtime source is exposed through a writable mount or multiply linked"));}
            add_mount(&mut command,sandbox::pin(source,false)?.into(),&destination,true);
        }
        for path in &spec.readonly_mounts {let (destination,file)=self.mount(path,false)?;add_mount(&mut command,file.into(),&destination,true);}
        for path in &spec.writable_mounts {let (destination,file)=self.mount(path,true)?;add_mount(&mut command,file.into(),&destination,false);}
        if let Some(network)=&spec.network_lease {
            network.assert_live()?;
            let bridge=PathBuf::from("/opt/ironbark/namespace-bridge"); if !spec.runtime.files.contains_key(&bridge) {return Err(denied("network bridge missing from exact runtime manifest"));}
            let mount=network.socket_mount_fd()?;
            add_mount(&mut command,mount,network.namespace_socket_path(),true);
            let endpoint=url::Url::parse(&network.transport_endpoint()).map_err(|_|denied("invalid namespace bridge endpoint"))?;
            if endpoint.host_str()!=Some("127.0.0.1") || endpoint.scheme()!="http" {return Err(denied("bridge must bind namespace-local loopback"));}
            config.bridge_port=Some(endpoint.port().ok_or_else(||denied("bridge port missing"))?);
        }
        // The add_mount closure (and its borrow of `command`) ends at block scope.
        // Runtime-ancestor immutability: after every bind, remount /, /home,
        // and /run read-only non-recursively. Only the private child tmpfs
        // (/home/worker, /run/worker) and the approved workspace bind mounts
        // stay writable; the runtime root and every unbound host path become
        // read-only, so a worker cannot rename or substitute its private HOME
        // boundary or the runtime root.
        command.args(["--remount-ro","/","--remount-ro","/home","--remount-ro","/run"]);
        if let Some(network)=&spec.network_lease {descriptors[2].1=network.bridge_bootstrap()?.into();}
        // bwrap 0.13.0 has no --preserve-fds; --as-pid-1 already preserves the
        // relocated fds 3..=next_fd-1 into the namespace controller at their exact
        // numbers (proven: the as-pid-1 path never runs close_extra_fds).
        command.arg("--").arg(runner_destination).arg("--ironbark-namespace-runner");
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // Move sources above target range before dup2; never collide with originals.
        let mut relocated=Vec::new();for (target,fd) in &descriptors {
            let high=unsafe {libc::fcntl(fd.as_raw_fd(),libc::F_DUPFD_CLOEXEC,next_fd+64)};
            if high<0 {return Err(std::io::Error::last_os_error().into());} relocated.push((*target,unsafe {OwnedFd::from_raw_fd(high)}));
        }
        let mapping:Vec<_>=relocated.iter().map(|(target,source)|(*target,source.as_raw_fd())).collect();
        unsafe {command.pre_exec(move || {
            for &(target,source) in &mapping {if libc::dup2(source,target)<0 {return Err(std::io::Error::last_os_error());}}
            if libc::syscall(libc::SYS_close_range,next_fd as u32,u32::MAX,4_u32)<0 {return Err(std::io::Error::last_os_error());}
            Ok(())
        });}
        let child=command.spawn().map_err(|_|CoreError::new("containment_unavailable","bubblewrap launch failed; no fallback is permitted"))?;
        drop(command);drop(relocated);drop(descriptors);
        let mut pending=Pending(Some(child));
        launcher::send_frame(&mut control,&config)?;
        let mut bytes=Vec::new();info.take(4097).read_to_end(&mut bytes)?;
        if bytes.len()>4096 {return Err(denied("invalid bubblewrap namespace information"));}
        let info:serde_json::Value=serde_json::from_slice(&bytes)?;
        let pid=info.get("child-pid").and_then(|p|p.as_u64()).and_then(|p|u32::try_from(p).ok()).ok_or_else(||denied("bubblewrap did not report namespace PID1"))?;
        let ready:Ready=launcher::read_frame(&mut control)?;
        let runner=ProcessIdentity::capture(pid,ready.namespace)?;
        if runner.start_ticks!=ready.start_ticks {return Err(denied("namespace handshake identity mismatch"));}
        if let Some(network)=&spec.network_lease {network.assert_live()?;}
        control.set_read_timeout(None)?;
        let group=unsafe {libc::getpgid(pid as i32)};
        if group<0 {return Err(std::io::Error::last_os_error().into());}
        let child=pending.take();
        let mut lease=ProcessLease {run_id:spec.run_id,pid,process_group_id:group as u32,sandbox_identity:ready.namespace,runner,child,control,network:spec.network_lease,observed:BTreeMap::new(),preserved_artifacts:spec.writable_mounts,_slot:slot,finished:false};
        lease.observe()?;lease.control.write_all(&[2])?;network_guard.0=None;Ok(lease)
    }
    pub fn stop(&self,mut lease:ProcessLease,grace_ms:u64)->Result<StopReceipt> {
        if grace_ms!=2000 {return Err(denied("containment cancellation grace must be exactly 2000ms"));}
        lease.observe()?;
        // The cancel byte may hit EPIPE when the runner already closed the control
        // channel while exiting: launcher.rs drops fd3 only after reap() reaped every
        // namespace child. pidfd liveness can lag that window, so termination must be
        // decided by finish()'s bounded reap/SIGKILL escalation, never by an
        // instantaneous EPIPE+is_live() snapshot. finish() still reports
        // termination_unknown truthfully if no termination is established.
        let _ = lease.control.write_all(&[1]);
        if let Some(network)=&lease.network {network.revoke();}
        self.finish(lease,Duration::from_secs(8),true)
    }
    pub fn wait(&self,lease:ProcessLease,timeout:Duration)->Result<StopReceipt> {self.finish(lease,timeout,false)}
    fn finish(&self,mut lease:ProcessLease,timeout:Duration,cancelling:bool)->Result<StopReceipt> {
        let deadline=Instant::now()+timeout;let mut forced=false; let mut force_deadline=None;
        loop {
            lease.observe()?;
            if let Some(status)=lease.child.try_wait()? {
                if let Some(network)=&lease.network {network.revoke();}
                let terminated=!lease.runner.is_live()? && lease.observed.values().all(|p|matches!(p.is_live(),Ok(false)));
                if !terminated {return Err(CoreError::new("termination_unknown","launcher exit did not establish descendant termination"));}
                lease.finished=true;
                return Ok(StopReceipt {owned_pids:lease.observed.values().cloned().collect(),all_terminated:true,preserved_artifacts:lease.preserved_artifacts.clone(),exit_code:status.code()});
            }
            if Instant::now()>=deadline && !forced {
                if let Some(network)=&lease.network {network.revoke();}
                lease.runner.signal(libc::SIGKILL)?;forced=true;force_deadline=Some(Instant::now()+Duration::from_secs(2));
            }
            if force_deadline.is_some_and(|d|Instant::now()>=d) {return Err(CoreError::new("termination_unknown","owned namespace did not exit after KILL"));}
            if forced && !cancelling {return Err(CoreError::new("worker_timeout","worker timeout; namespace teardown requested, not a successful result"));}
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
/// Protects failed handshakes. bwrap's die-with-parent also tears down its PID1.
struct Pending(Option<Child>);
impl Pending {fn take(&mut self)->Child {self.0.take().expect("owned pending child")}}
impl Drop for Pending {fn drop(&mut self){if let Some(child)=&mut self.0 {bounded_child_teardown(child);}}}
struct NetworkFailure(Option<NetworkLease>);
impl Drop for NetworkFailure {fn drop(&mut self){if let Some(lease)=&self.0 {lease.revoke();}}}

fn bounded_child_teardown(child:&mut Child) {
    let _=child.kill();let deadline=Instant::now()+Duration::from_secs(2);
    while Instant::now()<deadline {
        if matches!(child.try_wait(),Ok(Some(_))) {return;}
        std::thread::sleep(Duration::from_millis(10));
    }
    // Unknown termination stays unknown; Drop cannot fabricate a cancellation receipt.
}

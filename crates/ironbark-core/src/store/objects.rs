//! Project-bound immutable CAS. All publishers, collectors and snapshots take
//! the same filesystem lease, including across core processes.
use std::{collections::HashMap, fs::{self, File, OpenOptions}, io::{Read, Seek, SeekFrom, Write}, os::{fd::AsRawFd, unix::fs::{OpenOptionsExt, PermissionsExt}}, path::{Path, PathBuf}, sync::{Arc, LazyLock, Mutex, Weak}};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest as ShaDigest, Sha256};
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use ts_rs::TS;
use tokio::sync::Semaphore;
use crate::{contracts::{ArtifactId, Digest, ProjectId}, error::{CoreError, Result}, paths::{private_directory, private_file}};
use super::Store;

pub const DEFAULT_ITEM_BYTES: u64 = 50 * 1024 * 1024;
#[derive(Debug, Clone, Copy)]
pub struct ObjectLimits { pub item_bytes: u64, pub soft_bytes: u64, pub hard_bytes: u64 }
impl Default for ObjectLimits { fn default() -> Self { Self { item_bytes: DEFAULT_ITEM_BYTES, soft_bytes: 2*1024*1024*1024, hard_bytes: 10*1024*1024*1024 } } }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
pub struct CapacityReport { #[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub bytes: u64, #[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub soft_bytes: u64, #[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub hard_bytes: u64, pub soft_exceeded: bool }

static INGESTION: LazyLock<Mutex<HashMap<PathBuf, Weak<Semaphore>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
#[derive(Clone)]
pub struct ObjectStore { pub(crate) store: Store, pub(crate) project: ProjectId, pub(crate) root: PathBuf, limits: ObjectLimits, ingestion: Arc<Semaphore> }
pub struct ObjectReader { file: File, pub digest: Digest, pub bytes: u64 }
impl Read for ObjectReader { fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { self.file.read(buf) } }

pub(crate) struct CasLease(File);
impl Drop for CasLease { fn drop(&mut self) { unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN); } } }
pub(crate) fn lease(root: &Path) -> Result<CasLease> {
    let file=private_file(&root.join(".lease"))?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(),libc::LOCK_EX) } == 0 { return Ok(CasLease(file)); }
        let error=std::io::Error::last_os_error(); if error.kind()!=std::io::ErrorKind::Interrupted { return Err(error.into()); }
    }
}

/// No traversed ancestor may be a symlink. Call before creating owned children.
pub(crate) fn safe_ancestors(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.components().any(|c|matches!(c,std::path::Component::ParentDir)) { return Err(CoreError::new("unsafe_path","an absolute non-traversing path is required")); }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(m) if m.file_type().is_symlink() => return Err(CoreError::new("unsafe_path","symlink ancestors are forbidden")),
            Ok(_) => {}, Err(e) if e.kind()==std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub(crate) struct Staging { pub path: PathBuf }
impl Drop for Staging { fn drop(&mut self) { if self.path.is_dir() { let _=fs::remove_dir_all(&self.path); } else { let _=fs::remove_file(&self.path); } } }
pub(crate) fn new_file(path: &Path) -> Result<File> { Ok(OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?) }
pub(crate) fn read_file(path: &Path) -> Result<File> {
    let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() { return Err(CoreError::new("unsafe_path","object must be a regular file")); } Ok(file)
}
pub(crate) fn hash_reader(reader: &mut impl Read, limit:u64) -> Result<(Digest,u64)> {
    let mut hash=Sha256::new(); let mut bytes=0u64; let mut buffer=[0u8;64*1024];
    loop { let n=reader.read(&mut buffer)?; if n==0 { break; } bytes=bytes.checked_add(n as u64).ok_or_else(||CoreError::new("ItemTooLarge","item size overflow"))?;
        if bytes>limit { return Err(CoreError::new("ItemTooLarge","item exceeds streaming limit")); } hash.update(&buffer[..n]); }
    Ok((Digest::parse(&format!("{:x}",hash.finalize()))?,bytes))
}
pub(crate) fn verified_file(path: &Path, digest:&Digest, expected:Option<u64>) -> Result<(File,u64)> {
    let mut file=read_file(path).map_err(|_|CoreError::new("IntegrityMismatch","referenced object is missing or unsafe"))?;
    let (actual,bytes)=hash_reader(&mut file,expected.unwrap_or(u64::MAX)).map_err(|_|CoreError::new("IntegrityMismatch","object size or hash is invalid"))?;
    if actual!=*digest || expected.is_some_and(|e|e!=bytes) { return Err(CoreError::new("IntegrityMismatch","object size or SHA-256 differs")); }
    file.seek(SeekFrom::Start(0))?; Ok((file,bytes))
}
pub(crate) fn object_files(root:&Path)->Result<Vec<(Digest,u64)>> {
    let mut result=Vec::new();
    for entry in fs::read_dir(root)? { let entry=entry?; let name=entry.file_name(); let Some(name)=name.to_str() else { return Err(CoreError::new("storage_corrupt","invalid CAS name")); };
        if name.starts_with('.') { continue; }
        let digest=Digest::parse(name).map_err(|_|CoreError::new("storage_corrupt","unexpected CAS entry"))?;
        let meta=fs::symlink_metadata(entry.path())?; if !meta.is_file() || meta.file_type().is_symlink() { return Err(CoreError::new("unsafe_path","CAS contains a non-regular object")); }
        result.push((digest,meta.len()));
    } Ok(result)
}
pub(crate) fn sync_dir(path:&Path)->Result<()> { File::open(path)?.sync_all()?; Ok(()) }

impl ObjectStore {
    pub fn new(store:Store,project:ProjectId,root:PathBuf,limits:ObjectLimits)->Result<Self> {
        if limits.item_bytes==0 || limits.item_bytes>DEFAULT_ITEM_BYTES || limits.soft_bytes>limits.hard_bytes { return Err(CoreError::new("invalid_limits","invalid object capacity limits")); }
        safe_ancestors(&root)?; private_directory(&root)?; private_directory(&root.join(".staging"))?;
        let mut registry=INGESTION.lock().map_err(|_|CoreError::new("storage_closed","ingestion registry poisoned"))?;
        registry.retain(|_,v|v.strong_count()>0);
        let ingestion=registry.get(&root).and_then(Weak::upgrade).unwrap_or_else(||{let value=Arc::new(Semaphore::new(2));registry.insert(root.clone(),Arc::downgrade(&value));value});
        Ok(Self{store,project,root,limits,ingestion})
    }
    pub async fn put<R:Read+Send+'static>(&self,mut reader:R,limit_bytes:u64)->Result<ArtifactId> {
        let permit=self.ingestion.clone().acquire_owned().await.map_err(|_|CoreError::new("storage_closed","ingestion stopped"))?;
        let root=self.root.clone(); let limits=self.limits; let limit=limit_bytes.min(limits.item_bytes);
        let (digest,_bytes,guard)=tokio::task::spawn_blocking(move || -> Result<_> {
            let _permit=permit; let temp=Staging{path:root.join(".staging").join(uuid::Uuid::new_v4().to_string())};
            let mut file={
                let _creation_guard=lease(&root)?;let file=new_file(&temp.path)?;
                if unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {return Err(std::io::Error::last_os_error().into());}
                file
            };
            let mut hash=Sha256::new(); let mut bytes=0u64; let mut buffer=[0u8;64*1024];
            loop { let n=reader.read(&mut buffer)?; if n==0 {break;} bytes=bytes.checked_add(n as u64).ok_or_else(||CoreError::new("ItemTooLarge","item size overflow"))?;
                if bytes>limit {return Err(CoreError::new("ItemTooLarge","item exceeds streaming limit"));} file.write_all(&buffer[..n])?; hash.update(&buffer[..n]); }
            drop(reader);file.sync_all()?; let digest=Digest::parse(&format!("{:x}",hash.finalize()))?;
            let guard=lease(&root)?; let destination=root.join(digest.as_str());
            if fs::symlink_metadata(&destination).is_ok() { verified_file(&destination,&digest,Some(bytes))?; }
            else {
                let used=object_files(&root)?.into_iter().try_fold(0u64,|s,(_,n)|s.checked_add(n).ok_or_else(||CoreError::new("StoreFull","capacity overflow")))?;
                if used.saturating_add(bytes)>limits.hard_bytes {return Err(CoreError::new("StoreFull","hard CAS capacity exceeded"));}
                file.set_permissions(fs::Permissions::from_mode(0o400))?;
                // link is atomic and never replaces an existing digest path.
                fs::hard_link(&temp.path,&destination)?; sync_dir(&root)?;
            }
            Ok((digest,bytes,guard))
        }).await.map_err(|_|CoreError::new("storage_closed","ingestion worker stopped"))??;
        let project=self.project; let id=ArtifactId::new();
        let (id,_guard)=self.store.with_transaction(move |tx| {
            tx.execute("INSERT INTO artifacts(id,project_id,run_id,digest,path) VALUES(?1,?2,NULL,?3,?3)",params![id.to_string(),project.to_string(),digest.as_str()])?;
            Ok((id,guard))
        }).await?;
        Ok(id)
    }
    pub async fn get(&self,id:ArtifactId)->Result<ObjectReader> {
        let project=self.project;
        let digest=self.store.with_connection(move |conn| {
            let value:Option<String>=conn.query_row("SELECT digest FROM artifacts WHERE id=?1 AND project_id=?2",params![id.to_string(),project.to_string()],|r|r.get(0)).optional()?;
            Digest::parse(&value.ok_or_else(||CoreError::new("ArtifactNotFound","artifact is not owned by this project"))?)
        }).await?;
        let root=self.root.clone(); tokio::task::spawn_blocking(move || {let _guard=lease(&root)?;let (file,bytes)=verified_file(&root.join(digest.as_str()),&digest,None)?;Ok(ObjectReader{file,digest,bytes})}).await.map_err(|_|CoreError::new("storage_closed","object worker stopped"))?
    }
    pub async fn capacity(&self)->Result<CapacityReport> {
        let root=self.root.clone();let limits=self.limits;
        tokio::task::spawn_blocking(move || {let _guard=lease(&root)?;let bytes=object_files(&root)?.into_iter().try_fold(0u64,|s,(_,n)|s.checked_add(n).ok_or_else(||CoreError::new("StoreFull","capacity overflow")))?;Ok(CapacityReport{bytes,soft_bytes:limits.soft_bytes,hard_bytes:limits.hard_bytes,soft_exceeded:bytes>=limits.soft_bytes})}).await.map_err(|_|CoreError::new("storage_closed","capacity worker stopped"))?
    }
}

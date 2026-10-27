//! Offline, original-preserving inspection. SQLite only sees disposable private
//! copies. Recovery images are private raw evidence, NOT sanitized online backups
//! and NOT an authority to restore grants or resume work.
use std::{collections::BTreeSet, ffi::{CString, OsStr}, fs::File, io::{Read, Seek, SeekFrom, Write}, os::{fd::{AsRawFd, FromRawFd}, unix::{ffi::OsStrExt, fs::{MetadataExt, PermissionsExt}}}, path::{Path, PathBuf}, time::{Duration, Instant}};
use fs2::FileExt;
use rusqlite::{backup::{Backup, StepResult}, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use crate::{contracts::Digest, error::{CoreError, Result}, paths::{current_uid, directory_handle}};
pub use crate::paths::RuntimePaths;
use super::{migrations::CURRENT_SCHEMA_VERSION, objects, schema::{ReferenceReason, ReferenceVisibility, StorageSchema}};

const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;
const OVERHEAD: u64 = 16 * 1024 * 1024;
const ROLLBACK: &str = "Recovery image preserves exact stopped source DB/WAL/SHM and private CAS bytes. Rollback requires stopping every writer and a separately verified restore; it cannot preserve writes made after migration. This offline check does not gate writable startup until integrated by A1.";
const ORIGINALS: [&str; 3] = ["core.sqlite3", "core.sqlite3-wal", "core.sqlite3-shm"];
fn unhashed_digest() -> Digest {
    Digest::parse("0000000000000000000000000000000000000000000000000000000000000000").expect("64-zero sentinel")
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MigrationPreflightReport { Existing { source_schema: u32, target_schema: u32, source_digest: Digest, required_backup_bytes: u64, available_bytes: Option<u64>, checked_backup_digest: Option<Digest>, writable_allowed: bool, blockers: Vec<String>, rollback_limit: String, checkpoints: Vec<RecoveryReference> }, NewNamespace { target_schema: u32, available_bytes: Option<u64>, writable_allowed: bool, blockers: Vec<String>, checkpoints: Vec<RecoveryReference> } }
impl MigrationPreflightReport {
    pub fn writable_allowed(&self) -> bool {
        match self { Self::Existing { writable_allowed, .. } | Self::NewNamespace { writable_allowed, .. } => *writable_allowed }
    }
    /// Schema version of the source namespace; `0` when no source exists yet.
    pub fn source_schema(&self) -> u32 {
        match self { Self::Existing { source_schema, .. } => *source_schema, Self::NewNamespace { .. } => 0 }
    }
    /// Pinned CAS evidence roots the source snapshot depends on.
    pub fn checkpoints(&self) -> &[RecoveryReference] {
        match self { Self::Existing { checkpoints, .. } | Self::NewNamespace { checkpoints, .. } => checkpoints }
    }
}

// Deliberately private-protocol serde only: no generated IPC/TS contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity { pub device: u64, pub inode: u64, pub bytes: u64, pub modified_seconds: i64, pub modified_nanos: i64, pub changed_seconds: i64, pub changed_nanos: i64 }
impl FileIdentity {
    fn read(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        Ok(Self { device: m.dev(), inode: m.ino(), bytes: m.len(), modified_seconds: m.mtime(), modified_nanos: m.mtime_nsec(), changed_seconds: m.ctime(), changed_nanos: m.ctime_nsec() })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryFile { pub name: String, pub identity: FileIdentity, pub digest: Digest }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryObject { pub digest: Digest, pub identity: FileIdentity }
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReference { pub digest: String, pub reason: String, pub visibility: String }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryImageManifest {
    pub format: u32,
    pub source_database: PathBuf,
    pub source_directory_device: u64,
    pub source_directory_inode: u64,
    pub source_schema: u32,
    pub source_digest: Digest,
    pub originals: Vec<RecoveryFile>,
    pub snapshot_digest: Digest,
    pub snapshot_bytes: u64,
    pub references: Vec<RecoveryReference>,
    pub objects: Vec<RecoveryObject>,
}

impl RecoveryImageManifest {
    /// Number of evidence items carried by this image (originals, CAS
    /// objects and logical references).
    pub fn len(&self) -> usize {
        self.originals.len() + self.objects.len() + self.references.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn denied(code: &str) -> CoreError { CoreError::new(code, "offline recovery precondition failed; source evidence is unchanged") }
fn add(a: u64, b: u64) -> Result<u64> { a.checked_add(b).ok_or_else(|| denied("RecoverySizeOverflow")) }
fn multiply(a: u64, b: u64) -> Result<u64> { a.checked_mul(b).ok_or_else(|| denied("RecoverySizeOverflow")) }
fn fd_path(directory: &File) -> PathBuf { PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())) }
fn safe_file(file: &File) -> Result<()> {
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != current_uid() || m.nlink() != 1 || m.mode() & 0o022 != 0 { return Err(denied("unsafe_path")); }
    Ok(())
}
fn open_at(directory: &File, name: &OsStr, create_lock: bool) -> Result<Option<File>> {
    let name = CString::new(name.as_bytes()).map_err(|_| denied("unsafe_path"))?;
    let flags = if create_lock { libc::O_RDWR | libc::O_CREAT } else { libc::O_RDONLY };
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK, 0o600) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return if e.kind() == std::io::ErrorKind::NotFound && !create_lock { Ok(None) } else { Err(e.into()) };
    }
    let file = unsafe { File::from_raw_fd(fd) };
    safe_file(&file)?;
    Ok(Some(file))
}
fn required_at(directory: &File, name: &str) -> Result<File> { open_at(directory, OsStr::new(name), false)?.ok_or_else(|| denied("RecoveryMemberMissing")) }
fn hash(file: &mut File) -> Result<(Digest, u64)> {
    #[cfg(test)]
    race_tests::observe(race_tests::Observation::BeforeRead { file, name: None })?;
    file.seek(SeekFrom::Start(0))?;
    objects::hash_reader(file, u64::MAX)
}
fn capacity(directory: &File) -> Option<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 { return None; }
    let stat = unsafe { stat.assume_init() };
    (stat.f_bavail as u64).checked_mul(stat.f_frsize as u64)
}
fn require_capacity(directory: &File, bytes: u64) -> Result<u64> {
    let free = capacity(directory).ok_or_else(|| denied("CapacityUnavailable"))?;
    #[cfg(test)]
    race_tests::observe(race_tests::Observation::Capacity { required: bytes, available: free })?;
    if free < bytes { return Err(denied("InsufficientSpace")); }
    Ok(free)
}
fn nearest_existing(path: &Path) -> Result<File> {
    match directory_handle(path, false) {
        Ok(file) => Ok(file),
        Err(e) if e.code == "io_error" => {
            // Only ENOENT permits walking upward; other errors must not turn an
            // unsafe or unreadable namespace into a fresh one.
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => nearest_existing(path.parent().ok_or_else(|| denied("unsafe_path"))?),
                _ => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}
struct Pinned { evidence: RecoveryFile, file: File }
impl Pinned {
    fn open(directory: &File, name: &str) -> Result<Option<Self>> {
        Self::open_limited(directory, name, u64::MAX)
    }
    fn open_limited(directory: &File, name: &str, limit: u64) -> Result<Option<Self>> {
        let Some(mut file) = open_at(directory, OsStr::new(name), false)? else { return Ok(None); };
        let identity = FileIdentity::read(&file)?;
        if identity.bytes > limit { return Err(denied("RecoveryMemberTooLarge")); }
        #[cfg(test)]
        race_tests::observe(race_tests::Observation::BeforeRead { file: &file, name: Some(name) })?;
        let (digest, bytes) = objects::hash_reader(&mut file, limit)?;
        if identity != FileIdentity::read(&file)? || bytes != identity.bytes { return Err(denied("SourceChanged")); }
        Ok(Some(Self { evidence: RecoveryFile { name: name.into(), identity, digest }, file }))
    }
    /// R4: open and retain the descriptor WITHOUT reading content — the
    /// returned identity/size is the sole basis for capacity admission; the
    /// caller hashes the *same* descriptor with an admitted-size bound later.
    fn open_metadata(directory: &File, name: &str, byte_limit: u64) -> Result<Option<Self>> {
        let Some(file) = open_at(directory, OsStr::new(name), false)? else { return Ok(None); };
        let identity = FileIdentity::read(&file)?;
        if identity.bytes > byte_limit { return Err(denied("RecoveryMemberTooLarge")); }
        Ok(Some(Self { evidence: RecoveryFile { name: name.into(), identity, digest: unhashed_digest() }, file }))
    }
    /// R4: hash the retained descriptor with an admitted-size bound, rejecting
    /// any identity/size change between metadata capture and hashing.
    fn verify_and_hash(&mut self) -> Result<()> {
        #[cfg(test)]
        race_tests::observe(race_tests::Observation::BeforeRead { file: &self.file, name: Some(&self.evidence.name) })?;
        let (digest, bytes) = objects::hash_reader(&mut self.file, self.evidence.identity.bytes)?;
        if self.evidence.identity != FileIdentity::read(&self.file)? || bytes != self.evidence.identity.bytes { return Err(denied("SourceChanged")); }
        self.evidence.digest = digest;
        Ok(())
    }
    fn check(&mut self, directory: &File) -> Result<()> {
        let named = required_at(directory, &self.evidence.name)?;
        let (digest, bytes) = hash(&mut self.file)?;
        if FileIdentity::read(&named)? != self.evidence.identity || FileIdentity::read(&self.file)? != self.evidence.identity || digest != self.evidence.digest || bytes != self.evidence.identity.bytes { return Err(denied("SourceChanged")); }
        Ok(())
    }
    fn copy(&mut self, destination: &Path) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut output = objects::new_file(destination)?;
        let copied = std::io::copy(&mut (&mut self.file).take(add(self.evidence.identity.bytes, 1)?), &mut output)?;
        output.sync_all()?;
        let (digest, bytes) = hash(&mut output)?;
        if copied != self.evidence.identity.bytes || bytes != copied || digest != self.evidence.digest { return Err(denied("SourceChanged")); }
        Ok(())
    }
}
struct Source {
    path: PathBuf, directory: File, device: u64, inode: u64,
    _locks: Vec<File>, files: Vec<Pinned>, cas_directory: Option<File>, cas: Vec<Pinned>,
}
impl Source {
    fn open(paths: &RuntimePaths, staging_parent: &File) -> Result<Self> {
        let directory = directory_handle(&paths.data_dir, false)?;
        let m = directory.metadata()?;
        if m.uid() != current_uid() || m.mode() & 0o022 != 0 { return Err(denied("unsafe_path")); }
        let lock = open_at(&directory, OsStr::new("core.lock"), true)?.ok_or_else(|| denied("RecoveryLockMissing"))?;
        lock.try_lock_exclusive().map_err(|_| denied("core_already_running"))?;
        let mut locks = vec![lock];
        // Runtime lock is additional evidence; absent runtime paths stay absent.
        match std::fs::symlink_metadata(&paths.runtime_dir) {
            Ok(_) => {
                let runtime = directory_handle(&paths.runtime_dir, false)?;
                if let Some(lock) = open_at(&runtime, OsStr::new("core.lock"), false)? {
                    lock.try_lock_exclusive().map_err(|_| denied("core_already_running"))?;
                    locks.push(lock);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if open_at(&directory, OsStr::new("core.sqlite3-journal"), false)?.is_some() { return Err(denied("UnsupportedRollbackJournal")); }
        // R3: open and retain the original descriptors BEFORE capacity admission
        // so the budget is computed from the identities that are actually hashed.
        let mut files = Vec::new();
        let mut raw_bytes = 0;
        for name in ORIGINALS {
            if let Some(file) = Pinned::open_metadata(&directory, name, u64::MAX)? {
                raw_bytes = add(raw_bytes, file.evidence.identity.bytes)?;
                files.push(file);
            }
        }
        require_capacity(staging_parent, add(multiply(raw_bytes, 4)?, OVERHEAD)?)?;
        #[cfg(test)]
        race_tests::observe(race_tests::Observation::RawAdmitted)?;
        // R3: reject any post-admission substitution or growth of the named
        // originals before hashing the retained descriptors, using the metadata
        // captured at open time (no content read on the enlarged/hole region).
        for file in &mut files {
            let named = required_at(&directory, &file.evidence.name)?;
            let after = FileIdentity::read(&named)?;
            let held = FileIdentity::read(&file.file)?;
            if after != file.evidence.identity || held != file.evidence.identity { return Err(denied("SourceChanged")); }
        }
        for file in &mut files { file.verify_and_hash()?; }
        if !files.iter().any(|f| f.evidence.name == ORIGINALS[0]) { return Err(denied("RecoveryDatabaseMissing")); }
        let source = Self { path: paths.data_dir.clone(), directory, device: m.dev(), inode: m.ino(), _locks: locks, files, cas_directory: None, cas: Vec::new() };
        Ok(source)
    }
    fn raw_bytes(&self) -> Result<u64> { self.files.iter().try_fold(0, |n, f| add(n, f.evidence.identity.bytes)) }
    fn check(&mut self) -> Result<()> {
        let named = directory_handle(&self.path, false)?;
        let m = named.metadata()?;
        if m.dev() != self.device || m.ino() != self.inode { return Err(denied("SourceChanged")); }
        let named_lock = required_at(&self.directory, "core.lock")?;
        if named_lock.metadata()?.dev() != self._locks[0].metadata()?.dev() || named_lock.metadata()?.ino() != self._locks[0].metadata()?.ino() { return Err(denied("SourceChanged")); }
        for name in ORIGINALS {
            if !self.files.iter().any(|f| f.evidence.name == name) && open_at(&self.directory, OsStr::new(name), false)?.is_some() { return Err(denied("SourceChanged")); }
        }
        if open_at(&self.directory, OsStr::new("core.sqlite3-journal"), false)?.is_some() { return Err(denied("SourceChanged")); }
        for file in &mut self.files { file.check(&self.directory)?; }
        if let Some(cas) = &self.cas_directory {
            let current = child_directory(&self.directory, "objects")?;
            if current.metadata()?.dev() != cas.metadata()?.dev() || current.metadata()?.ino() != cas.metadata()?.ino() { return Err(denied("SourceChanged")); }
            for file in &mut self.cas { file.check(cas)?; }
        }
        Ok(())
    }
    fn bind_roots(&mut self, references: &[RecoveryReference]) -> Result<()> {
        let roots = references.iter().map(|r| r.digest.as_str()).collect::<BTreeSet<_>>();
        if roots.is_empty() { return Ok(()); }
        let directory = child_directory(&self.directory, "objects")?;
        for digest in roots {
            // R4: open and hold the descriptor WITHOUT hashing content. The
            // complete CAS budget is admitted by the caller before any CAS byte
            // is streamed via verify_and_hash.
            let file = Pinned::open_metadata(&directory, digest, objects::DEFAULT_ITEM_BYTES)?.ok_or_else(|| denied("RecoveryCasMissing"))?;
            if file.evidence.identity.bytes > objects::DEFAULT_ITEM_BYTES { return Err(denied("IntegrityMismatch")); }
            self.cas.push(file);
        }
        self.cas_directory = Some(directory);
        Ok(())
    }
}
fn child_directory(parent: &File, name: &str) -> Result<File> {
    let name = CString::new(name).map_err(|_| denied("unsafe_path"))?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    if m.uid() != current_uid() || m.mode() & 0o022 != 0 { return Err(denied("unsafe_path")); }
    Ok(file)
}
fn references(conn: &Connection) -> Result<Vec<RecoveryReference>> {
    let mut references = StorageSchema::inspect(conn)?.roots(conn)?.into_iter().map(|r| {
        let reason = match r.reason {
            ReferenceReason::Artifact => "artifact", ReferenceReason::Attachment => "attachment", ReferenceReason::SourceRevision => "source_revision", ReferenceReason::Citation => "citation", ReferenceReason::ContextArtifact => "context_artifact", ReferenceReason::TurnArtifactReceipt => "turn_artifact_receipt", ReferenceReason::MessageArtifact => "message_artifact", ReferenceReason::CheckpointArtifact => "checkpoint_artifact", ReferenceReason::RunArtifact => "run_artifact", ReferenceReason::ImportArtifact => "import_artifact", ReferenceReason::ActionArtifact => "action_artifact",
        };
        let visibility = match r.visibility { ReferenceVisibility::PrivateOriginal => "private_original", ReferenceVisibility::ProjectEvidence => "project_evidence" };
        RecoveryReference { digest: r.digest.as_str().into(), reason: reason.into(), visibility: visibility.into() }
    }).collect::<Vec<_>>();
    references.sort();
    Ok(references)
}
// SQLite deliberately ignores damaged/incomplete WAL tails. That is appropriate
// for crash recovery, but not proof that every selected original is recoverable.
// Fail closed on such evidence rather than certifying an older main-file view.
fn validate_wal(raw: &Path) -> Result<()> {
    let path = raw.join("core.sqlite3-wal");
    if !path.try_exists()? { return Ok(()); }
    let mut file = objects::read_file(&path)?;
    let length = file.metadata()?.len();
    if length == 0 { return Ok(()); }
    if length < 32 { return Err(denied("CorruptRecoveryWal")); }
    let mut header = [0u8; 32];
    file.read_exact(&mut header)?;
    let word = |bytes: &[u8]| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let magic = word(&header[..4]);
    if !matches!(magic, 0x377f0682 | 0x377f0683) || word(&header[4..8]) != 3_007_000 { return Err(denied("CorruptRecoveryWal")); }
    let page_bytes = word(&header[8..12]) as usize;
    if !(512..=65536).contains(&page_bytes) || !page_bytes.is_power_of_two() || (length - 32) % (24 + page_bytes as u64) != 0 { return Err(denied("CorruptRecoveryWal")); }
    fn checksum(bytes: &[u8], big_endian: bool, state: &mut (u32, u32)) {
        for pair in bytes.chunks_exact(8) {
            let decode = |v: &[u8]| {
                let value = [v[0], v[1], v[2], v[3]];
                if big_endian { u32::from_be_bytes(value) } else { u32::from_le_bytes(value) }
            };
            state.0 = state.0.wrapping_add(decode(&pair[..4])).wrapping_add(state.1);
            state.1 = state.1.wrapping_add(decode(&pair[4..])).wrapping_add(state.0);
        }
    }
    let big_endian = magic & 1 != 0;
    let mut state = (0, 0);
    checksum(&header[..24], big_endian, &mut state);
    if state != (word(&header[24..28]), word(&header[28..32])) { return Err(denied("CorruptRecoveryWal")); }
    let mut frame = [0u8; 24];
    let mut page = [0u8; 65536];
    for _ in 0..(length - 32) / (24 + page_bytes as u64) {
        file.read_exact(&mut frame)?;
        file.read_exact(&mut page[..page_bytes])?;
        if word(&frame[..4]) == 0 || frame[8..16] != header[16..24] { return Err(denied("CorruptRecoveryWal")); }
        checksum(&frame[..8], big_endian, &mut state);
        checksum(&page[..page_bytes], big_endian, &mut state);
        if state != (word(&frame[16..20]), word(&frame[20..24])) { return Err(denied("CorruptRecoveryWal")); }
    }
    Ok(())
}
fn replay(raw: &Path, working: &Path, snapshot: &Path) -> Result<(u32, Vec<RecoveryReference>)> {
    validate_wal(raw)?;
    std::fs::create_dir(working)?;
    std::fs::set_permissions(working, std::fs::Permissions::from_mode(0o700))?;
    for name in ORIGINALS {
        let path = raw.join(name);
        if path.try_exists()? { std::fs::copy(path, working.join(name))?; }
    }
    // READ_WRITE is intentional only on the disposable copy: real WAL replay
    // may rebuild SHM. immutable=1 would silently ignore uncheckpointed evidence.
    let conn = Connection::open_with_flags(working.join(ORIGINALS[0]), OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_secs(1))?;
    conn.pragma_update(None, "trusted_schema", "OFF")?;
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != CURRENT_SCHEMA_VERSION { return Err(denied("UnsupportedMigrationSchema")); }
    let roots = references(&conn)?;
    objects::new_file(snapshot)?;
    let mut destination = Connection::open(snapshot)?;
    {
        let backup = Backup::new(&conn, &mut destination)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() >= deadline { return Err(denied("BackupBusy")); }
            match backup.step(256)? {
                StepResult::Done => break, StepResult::More => {},
                StepResult::Busy | StepResult::Locked => std::thread::sleep(Duration::from_millis(10)),
                _ => return Err(denied("BackupBusy")),
            }
        }
    }
    destination.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
    if references(&destination)? != roots { return Err(denied("IntegrityMismatch")); }
    drop(destination);
    objects::read_file(snapshot)?.sync_all()?;
    Ok((version, roots))
}
// Removes only the directory tree reachable through the held staging descriptor,
// never by pathname. A substituted/replaced directory at an old pathname is
// therefore never traversed: only the owned descendants of `directory` are
// unlinked. Descriptor-relative recursion avoids following any racy renames.
fn cleanup_owned_staging(directory: &File) -> Result<()> {
    let entries: Vec<String> = std::fs::read_dir(fd_path(directory))?.filter_map(|entry| {
        entry.ok().and_then(|e| e.file_name().into_string().ok())
    }).collect();
    for name in entries {
        let cstr = CString::new(name).map_err(|_| denied("unsafe_path"))?;
        let fd = unsafe { libc::openat(directory.as_raw_fd(), cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd >= 0 {
            let child = unsafe { File::from_raw_fd(fd) };
            let m = child.metadata()?;
            if m.is_dir() && m.nlink() > 0 { let _ = cleanup_owned_staging(&child); }
            let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), cstr.as_ptr(), 0) };
        }
    }
    Ok(())
}
// Remove the owned staging tree through the held parent descriptor: first the
// descendants, then the stage root itself — but only if its pathname still names
// the owned inode. A substituted directory at the old stage name never matches
// the inode check, so it is left untouched (and never traversed).
fn drop_owned_staging(parent: &File, directory: &File, stage_name: &str) -> Result<()> {
    cleanup_owned_staging(directory)?;
    if let Ok(cstr) = CString::new(stage_name) {
        let fd = unsafe { libc::openat(parent.as_raw_fd(), cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd >= 0 {
            let probe = unsafe { File::from_raw_fd(fd) };
            let (held, named) = (directory.metadata(), probe.metadata());
            if let (Ok(held), Ok(named)) = (held, named) {
                if held.dev() == named.dev() && held.ino() == named.ino() {
                    let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), cstr.as_ptr(), libc::AT_REMOVEDIR) };
                }
            }
        }
    }
    Ok(())
}
struct Inspection { parent: File, directory: File, stage_name: String, manifest: RecoveryImageManifest, required: u64, cleanup: bool }
impl Drop for Inspection {
    fn drop(&mut self) {
        // Cleanup follows the held staging descriptor, never a pathname. A
        // directory substituted at the old stage name is therefore never
        // traversed, and a successfully published image (same inode) is never
        // removed because `cleanup` is disarmed right after publication.
        if self.cleanup { let _ = drop_owned_staging(&self.parent, &self.directory, &self.stage_name); }
    }
}
fn inspect_source(source: &mut Source, stage_parent: &File, database: &Path) -> Result<Inspection> {
    let raw_bytes = source.raw_bytes()?;
    let initial = add(multiply(raw_bytes, 4)?, OVERHEAD)?;
    require_capacity(stage_parent, initial)?;
    let stage = tempfile::Builder::new()
        .prefix(".ironbark-recovery-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(fd_path(stage_parent))?;
    let stage_name = stage.path().file_name().and_then(|name| name.to_str()).ok_or_else(|| denied("unsafe_path"))?.to_owned();
    // R1: disarm path-based TempDir cleanup; destruction follows the held
    // descriptor only, so a substituted dir at the old name is never traversed.
    let _stage_path = stage.keep();
    let directory = child_directory(stage_parent, &stage_name)?;
    // R1: RAII guard removes the owned staging tree on any early error; it
    // is disarmed when Inspection takes ownership at the end.
    struct StageGuard { parent: File, directory: Option<File>, stage_name: String }
    impl Drop for StageGuard {
        fn drop(&mut self) {
            if let Some(dir) = self.directory.take() {
                let _ = drop_owned_staging(&self.parent, &dir, &self.stage_name);
            }
        }
    }
    let mut guard = StageGuard { parent: stage_parent.try_clone()?, directory: Some(directory), stage_name: stage_name.clone() };
    let dir = guard.directory.as_mut().expect("stage directory");
    let stage_root = fd_path(dir);
    let raw = stage_root.join("originals");
    std::fs::create_dir(&raw)?;
    std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o700))?;
    source.check()?;
    for file in &mut source.files { file.copy(&raw.join(&file.evidence.name))?; }
    source.check()?;
    let snapshot = stage_root.join("snapshot.sqlite3");
    let (version, roots) = replay(&raw, &stage_root.join("replay"), &snapshot)?;
    // R4: collect CAS object identities/budget BEFORE any content hashing or
    // destination capacity admission. bind_roots now opens-and-holds the CAS
    // descriptors with their metadata; content is not streamed until after the
    // complete closure budget is admitted below.
    source.bind_roots(&roots)?;
    let object_bytes = source.cas.iter().try_fold(0, |n, f| add(n, f.evidence.identity.bytes))?;
    let required = add(initial, multiply(object_bytes, 2)?)?;
    // Existing private staging allocations are accounted for in the first
    // budget; the CAS copy still needs object bytes plus manifest overhead free.
    require_capacity(stage_parent, add(object_bytes, OVERHEAD)?)?;
    // R4: now that the complete CAS budget is admitted, hash the retained CAS
    // descriptors with their admitted-size bound.
    for file in &mut source.cas { file.verify_and_hash()?; }
    let snapshot_info = hash(&mut objects::read_file(&snapshot)?)?;
    let originals = source.files.iter().map(|f| f.evidence.clone()).collect::<Vec<_>>();
    let objects = source.cas.iter().map(|f| RecoveryObject { digest: f.evidence.digest.clone(), identity: f.evidence.identity.clone() }).collect::<Vec<_>>();
    let binding = serde_json::to_vec(&(database, source.device, source.inode, &originals, &objects, &roots))?;
    let (source_digest, _) = objects::hash_reader(&mut binding.as_slice(), MANIFEST_LIMIT)?;
    let manifest = RecoveryImageManifest { format: 1, source_database: database.to_owned(), source_directory_device: source.device, source_directory_inode: source.inode, source_schema: version, source_digest, originals, snapshot_digest: snapshot_info.0, snapshot_bytes: snapshot_info.1, references: roots, objects };
    source.check()?;
    // Disarm the guard; Inspection will own cleanup.
    let directory = guard.directory.take().expect("stage directory");
    Ok(Inspection { parent: stage_parent.try_clone()?, directory, stage_name, manifest, required, cleanup: true })
}
fn exact_members(directory: &File, expected: BTreeSet<String>) -> Result<()> {
    let actual = std::fs::read_dir(fd_path(directory))?.map(|entry| {
        entry?.file_name().into_string().map_err(|_| denied("RecoveryManifestInvalid"))
    }).collect::<Result<BTreeSet<_>>>()?;
    if actual != expected { return Err(denied("RecoveryClosureMismatch")); }
    Ok(())
}
fn verify_image(path: &Path, expected: &RecoveryImageManifest) -> Result<Digest> {
    let directory = directory_handle(path, false)?;
    let digest = verify_image_directory(&directory, expected)?;
    let named = directory_handle(path, false)?;
    if named.metadata()?.dev() != directory.metadata()?.dev() || named.metadata()?.ino() != directory.metadata()?.ino() { return Err(denied("SourceChanged")); }
    Ok(digest)
}
fn verify_image_directory(directory: &File, expected: &RecoveryImageManifest) -> Result<Digest> {
    if directory.metadata()?.uid() != current_uid() || directory.metadata()?.mode() & 0o077 != 0 { return Err(denied("unsafe_path")); }
    exact_members(directory, ["manifest.json", "originals", "snapshot.sqlite3", "objects"].into_iter().map(String::from).collect())?;
    let mut manifest_file = Pinned::open_limited(directory, "manifest.json", MANIFEST_LIMIT)?.ok_or_else(|| denied("RecoveryMemberMissing"))?;
    let mut bytes = Vec::new();
    manifest_file.file.seek(SeekFrom::Start(0))?;
    (&mut manifest_file.file).take(MANIFEST_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MANIFEST_LIMIT { return Err(denied("RecoveryManifestInvalid")); }
    let manifest: RecoveryImageManifest = serde_json::from_slice(&bytes).map_err(|_| denied("RecoveryManifestInvalid"))?;
    if manifest != *expected { return Err(denied("RecoveryImageStale")); }
    let originals = child_directory(directory, "originals")?;
    exact_members(&originals, manifest.originals.iter().map(|f| f.name.clone()).collect())?;
    let mut raw_files = Vec::new();
    for evidence in &manifest.originals {
        let pinned = Pinned::open_limited(&originals, &evidence.name, evidence.identity.bytes)?.ok_or_else(|| denied("RecoveryMemberMissing"))?;
        if pinned.evidence.digest != evidence.digest || pinned.evidence.identity.bytes != evidence.identity.bytes { return Err(denied("IntegrityMismatch")); }
        raw_files.push(pinned);
    }
    let mut snapshot = Pinned::open_limited(directory, "snapshot.sqlite3", manifest.snapshot_bytes)?.ok_or_else(|| denied("RecoveryMemberMissing"))?;
    if snapshot.evidence.digest != manifest.snapshot_digest || snapshot.evidence.identity.bytes != manifest.snapshot_bytes { return Err(denied("IntegrityMismatch")); }
    let cas = child_directory(directory, "objects")?;
    exact_members(&cas, manifest.objects.iter().map(|o| o.digest.as_str().to_owned()).collect())?;
    let mut object_files = Vec::new();
    for object in &manifest.objects {
        let pinned = Pinned::open_limited(&cas, object.digest.as_str(), object.identity.bytes)?.ok_or_else(|| denied("RecoveryMemberMissing"))?;
        if pinned.evidence.digest != object.digest || pinned.evidence.identity.bytes != object.identity.bytes { return Err(denied("IntegrityMismatch")); }
        object_files.push(pinned);
    }
    #[cfg(test)]
    race_tests::observe(race_tests::Observation::ImageChildrenHashed)?;
    // R2: rebind the named child directories to the image root and compare
    // identities with the descriptors held across hashing, then recheck exact
    // member sets. A child moved out/replaced mid-verification or an extra
    // member injected during hashing is rejected here without a checked digest.
    let rebind_originals = child_directory(directory, "originals")?;
    if rebind_originals.metadata()?.dev() != originals.metadata()?.dev() || rebind_originals.metadata()?.ino() != originals.metadata()?.ino() { return Err(denied("SourceChanged")); }
    let rebind_objects = child_directory(directory, "objects")?;
    if rebind_objects.metadata()?.dev() != cas.metadata()?.dev() || rebind_objects.metadata()?.ino() != cas.metadata()?.ino() { return Err(denied("SourceChanged")); }
    exact_members(&rebind_originals, manifest.originals.iter().map(|f| f.name.clone()).collect())?;
    exact_members(&rebind_objects, manifest.objects.iter().map(|o| o.digest.as_str().to_owned()).collect())?;
    exact_members(directory, ["manifest.json", "originals", "snapshot.sqlite3", "objects"].into_iter().map(String::from).collect())?;
    for file in &mut raw_files { file.check(&originals)?; }
    for file in &mut object_files { file.check(&cas)?; }
    snapshot.check(directory)?;
    manifest_file.check(directory)?;
    let (digest, _) = objects::hash_reader(&mut bytes.as_slice(), MANIFEST_LIMIT)?;
    if digest != manifest_file.evidence.digest { return Err(denied("SourceChanged")); }
    Ok(digest)
}

pub struct MigrationPreflight;
impl MigrationPreflight {
    pub fn inspect(paths: &RuntimePaths, backup: Option<&Path>) -> Result<MigrationPreflightReport> {
        validate_paths(paths)?;
        if !source_exists(paths)? {
            let mut available = None;
            let mut blockers = Vec::new();
            for path in [&paths.data_dir, &paths.state_dir, &paths.runtime_dir] {
                let free = capacity(&nearest_existing(path)?);
                if free.is_none() { blockers.push("CapacityUnavailable".into()); }
                if free.is_some_and(|n| n < OVERHEAD) { blockers.push("InsufficientSpace".into()); }
                available = match (available, free) { (Some(a), Some(b)) => Some(u64::min(a, b)), (None, b) => b, (_, None) => None };
            }
            // One unknown volume must not be hidden by a later known volume.
            if blockers.iter().any(|b| b == "CapacityUnavailable") { available = None; }
            if backup.is_some() { blockers.push("RecoveryImageHasNoMatchingSource".into()); }
            return Ok(MigrationPreflightReport::NewNamespace { target_schema: CURRENT_SCHEMA_VERSION, available_bytes: available, writable_allowed: blockers.is_empty(), blockers, checkpoints: Vec::new() });
        }
        let temp_parent = directory_handle(&std::env::temp_dir(), false)?;
        let mut source = Source::open(paths, &temp_parent)?;
        let inspected = inspect_source(&mut source, &temp_parent, &paths.database)?;
        let mut blockers = Vec::new();
        let checked = match backup {
            Some(path) => match verify_image(path, &inspected.manifest) { Ok(digest) => Some(digest), Err(e) => { blockers.push(e.code); None } },
            None => { blockers.push("RecoveryImageRequired".into()); None },
        };
        let available = capacity(&source.directory);
        if available.is_none() { blockers.push("CapacityUnavailable".into()); }
        if available.is_some_and(|n| n < inspected.required) { blockers.push("InsufficientSpace".into()); }
        source.check()?;
        Ok(MigrationPreflightReport::Existing { source_schema: inspected.manifest.source_schema, target_schema: CURRENT_SCHEMA_VERSION, source_digest: inspected.manifest.source_digest.clone(), required_backup_bytes: inspected.required, available_bytes: available, checked_backup_digest: checked, writable_allowed: blockers.is_empty(), blockers, rollback_limit: ROLLBACK.into(), checkpoints: inspected.manifest.references.clone() })
    }
    pub fn create_recovery_image(paths: &RuntimePaths, destination: &Path) -> Result<RecoveryImageManifest> {
        validate_paths(paths)?;
        if !source_exists(paths)? { return Err(denied("RecoveryDatabaseMissing")); }
        if !destination.is_absolute() || destination.starts_with(&paths.data_dir) || destination.starts_with(&paths.state_dir) || destination.starts_with(&paths.runtime_dir) { return Err(denied("unsafe_path")); }
        let parent_path = destination.parent().ok_or_else(|| denied("unsafe_path"))?;
        let parent = directory_handle(parent_path, false)?;
        let name = destination.file_name().ok_or_else(|| denied("unsafe_path"))?;
        if open_at(&parent, name, false)?.is_some() { return Err(denied("NamespaceExists")); }
        let mut source = Source::open(paths, &parent)?;
        let mut inspected = inspect_source(&mut source, &parent, &paths.database)?;
        let stage_root = fd_path(&inspected.directory);
        let objects_dir = stage_root.join("objects");
        std::fs::create_dir(&objects_dir)?;
        std::fs::set_permissions(&objects_dir, std::fs::Permissions::from_mode(0o700))?;
        for object in &mut source.cas { object.copy(&objects_dir.join(&object.evidence.name))?; }
        source.check()?;
        // Remove only our replay workspace, never raw originals or selected data.
        std::fs::remove_dir_all(stage_root.join("replay"))?;
        let mut file = objects::new_file(&stage_root.join("manifest.json"))?;
        let bytes = serde_json::to_vec(&inspected.manifest)?;
        if bytes.len() as u64 > MANIFEST_LIMIT { return Err(denied("RecoveryManifestInvalid")); }
        file.write_all(&bytes)?; file.sync_all()?;
        objects::sync_dir(&objects_dir)?;
        objects::sync_dir(&stage_root.join("originals"))?;
        inspected.directory.sync_all()?;
        verify_image_directory(&inspected.directory, &inspected.manifest)?;
        source.check()?;
        // Both paths remain anchored in the held parent; RENAME_NOREPLACE is
        // atomic even when another actor races the selected leaf into existence.
        let stage_name = inspected.stage_name.as_str();
        #[cfg(test)]
        race_tests::observe(race_tests::Observation::BeforePublish { stage: &parent_path.join(stage_name) })?;
        let from = CString::new(stage_name.as_bytes()).map_err(|_| denied("unsafe_path"))?;
        let to = CString::new(name.as_bytes()).map_err(|_| denied("unsafe_path"))?;
        let named_parent = directory_handle(parent_path, false)?;
        if named_parent.metadata()?.dev() != parent.metadata()?.dev() || named_parent.metadata()?.ino() != parent.metadata()?.ino() { return Err(denied("SourceChanged")); }
        let current_stage = child_directory(&parent, stage_name)?;
        if current_stage.metadata()?.dev() != inspected.directory.metadata()?.dev() || current_stage.metadata()?.ino() != inspected.directory.metadata()?.ino() { return Err(denied("SourceChanged")); }
        if unsafe { libc::renameat2(parent.as_raw_fd(), from.as_ptr(), parent.as_raw_fd(), to.as_ptr(), libc::RENAME_NOREPLACE) } != 0 { return Err(std::io::Error::last_os_error().into()); }
        // R1: publication consumed the owned staging directory (same inode, now
        // under its destination name). Disarm descriptor-based cleanup so the
        // published image is never removed — even if the parent fsync below fails.
        inspected.cleanup = false;
        #[cfg(test)]
        race_tests::observe(race_tests::Observation::AfterPublish { stage: &parent_path.join(stage_name) })?;
        parent.sync_all()?;
        Ok(inspected.manifest.clone())
    }
}
fn validate_paths(paths: &RuntimePaths) -> Result<()> {
    if paths.database != paths.data_dir.join(ORIGINALS[0]) || paths.lock != paths.runtime_dir.join("core.lock") || paths.socket != paths.runtime_dir.join("core.sock") || paths.nonce != paths.runtime_dir.join("session.nonce") { return Err(denied("unsafe_path")); }
    for path in [&paths.data_dir, &paths.state_dir, &paths.runtime_dir] { objects::safe_ancestors(path)?; }
    Ok(())
}
fn source_exists(paths: &RuntimePaths) -> Result<bool> {
    match std::fs::symlink_metadata(&paths.data_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => { nearest_existing(&paths.data_dir)?; Ok(false) },
        Err(e) => Err(e.into()),
        Ok(_) => {
            let directory = directory_handle(&paths.data_dir, false)?;
            let metadata = directory.metadata()?;
            if metadata.uid() != current_uid() || metadata.mode() & 0o022 != 0 { return Err(denied("unsafe_path")); }
            let database = open_at(&directory, OsStr::new(ORIGINALS[0]), false)?;
            if database.is_some() { return Ok(true); }
            for name in ["core.sqlite3-wal", "core.sqlite3-shm", "core.sqlite3-journal"] {
                if open_at(&directory, OsStr::new(name), false)?.is_some() { return Err(denied("RecoveryDatabaseMissing")); }
            }
            Ok(false)
        }
    }
}

// Synchronous, thread-local observation only: release builds have neither these
// callbacks nor a fixture mode. Read observers may abort before the first byte.
#[cfg(test)]
mod race_tests {
    use super::*;
    use std::{cell::{Cell, RefCell}, collections::BTreeMap, fs, rc::Rc};
    use crate::paths::private_directory;

    pub(super) enum Observation<'a> {
        Capacity { required: u64, available: u64 },
        RawAdmitted,
        BeforeRead { file: &'a File, name: Option<&'a str> },
        ImageChildrenHashed,
        BeforePublish { stage: &'a Path },
        AfterPublish { stage: &'a Path },
    }
    type Observer = dyn for<'a> FnMut(Observation<'a>) -> Result<()>;
    thread_local! {
        static OBSERVER: RefCell<Option<Box<Observer>>> = RefCell::new(None);
    }
    pub(super) fn observe(event: Observation<'_>) -> Result<()> {
        OBSERVER.with(|slot| match slot.borrow_mut().as_mut() {
            Some(observer) => observer(event),
            None => Ok(()),
        })
    }
    struct ObserverGuard;
    impl Drop for ObserverGuard {
        fn drop(&mut self) { OBSERVER.with(|slot| { slot.borrow_mut().take(); }); }
    }
    fn observing(observer: impl for<'a> FnMut(Observation<'a>) -> Result<()> + 'static) -> ObserverGuard {
        OBSERVER.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "observers cannot nest on one test thread");
            *slot = Some(Box::new(observer));
        });
        ObserverGuard
    }

    struct Fixture { root: tempfile::TempDir, paths: RuntimePaths }
    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let data_dir = root.path().join("data");
            let state_dir = root.path().join("state");
            let runtime_dir = root.path().join("runtime");
            let paths = RuntimePaths {
                database: data_dir.join("core.sqlite3"),
                nonce: runtime_dir.join("session.nonce"),
                socket: runtime_dir.join("core.sock"),
                lock: runtime_dir.join("core.lock"),
                data_dir, state_dir, runtime_dir,
            };
            private_directory(&paths.data_dir).unwrap();
            let mut conn = Connection::open(&paths.database).unwrap();
            super::super::migrations::migrate(&mut conn).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
            drop(conn);
            Self { root, paths }
        }
        fn add_objects(&self, payloads: &[&[u8]]) -> Vec<String> {
            use sha2::{Digest as _, Sha256};
            let conn = Connection::open(&self.paths.database).unwrap();
            conn.execute("INSERT INTO projects VALUES ('p','private','/private',1)", []).unwrap();
            let directory = self.paths.data_dir.join("objects");
            private_directory(&directory).unwrap();
            payloads.iter().enumerate().map(|(index, payload)| {
                let digest = format!("{:x}", Sha256::digest(payload));
                fs::write(directory.join(&digest), payload).unwrap();
                conn.execute("INSERT INTO artifacts VALUES (?1,'p',NULL,?2,?2)", [&format!("a{index}"), &digest]).unwrap();
                digest
            }).collect()
        }
        fn originals(&self) -> BTreeMap<String, Vec<u8>> {
            ORIGINALS.into_iter().filter_map(|name| {
                let path = self.paths.data_dir.join(name);
                match fs::read(path) {
                    Ok(bytes) => Some((name.to_owned(), bytes)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => panic!("cannot capture source evidence: {e}"),
                }
            }).collect()
        }
        fn object_bytes(&self) -> BTreeMap<String, Vec<u8>> {
            fs::read_dir(self.paths.data_dir.join("objects")).unwrap().map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name().into_string().unwrap(), fs::read(entry.path()).unwrap())
            }).collect()
        }
        fn image(&self) -> PathBuf {
            let image = self.root.path().join("image");
            MigrationPreflight::create_recovery_image(&self.paths, &image).unwrap();
            assert!(MigrationPreflight::inspect(&self.paths, Some(&image)).unwrap().writable_allowed(),
                "valid selected-image control must pass before any race");
            image
        }
    }

    fn stage_substitution(after_publish: bool, fail_after_rename: bool) {
        let fixture = Fixture::new();
        let before = fixture.originals();
        let destination = fixture.root.path().join("published");
        let detached = fixture.root.path().join("owned-stage-detached");
        let sentinel = Rc::new(RefCell::new(None::<PathBuf>));
        let recorded = sentinel.clone();
        let _observer = observing(move |event| {
            let stage = match event {
                Observation::BeforePublish { stage } if !after_publish => stage,
                Observation::AfterPublish { stage } if after_publish => stage,
                _ => return Ok(()),
            };
            if !after_publish { fs::rename(stage, &detached).unwrap(); }
            private_directory(stage).unwrap();
            let path = stage.join("unrelated-sentinel");
            fs::write(&path, b"substitute is not owned by recovery").unwrap();
            *recorded.borrow_mut() = Some(path);
            if fail_after_rename { return Err(std::io::Error::from_raw_os_error(libc::EIO).into()); }
            Ok(())
        });
        let result = MigrationPreflight::create_recovery_image(&fixture.paths, &destination);
        assert_eq!(fixture.originals(), before);
        if !after_publish {
            assert_eq!(result.unwrap_err().code, "SourceChanged");
            assert!(!destination.exists());
        } else {
            if fail_after_rename { assert_eq!(result.unwrap_err().code, "io_error"); }
            else { result.unwrap(); }
            assert!(destination.join("manifest.json").is_file(), "rename really published the owned image");
        }
        let path = sentinel.borrow().clone().expect("publication observer must run");
        assert_eq!(fs::read(path).expect("cleanup traversed an unrelated substitute"),
            b"substitute is not owned by recovery");
    }

    #[test]
    fn refused_publication_preserves_substituted_stage_sentinel() {
        stage_substitution(false, false);
    }

    #[test]
    fn published_image_preserves_old_stage_path_substitute() {
        stage_substitution(true, false);
    }

    #[test]
    fn post_rename_error_preserves_old_stage_path_substitute() {
        stage_substitution(true, true);
    }

    fn assert_selected_race_denied(child: &'static str, extra_member: bool) {
        let fixture = Fixture::new();
        let digests = fixture.add_objects(&[b"private CAS child binding evidence"]);
        let before = fixture.originals();
        let image = fixture.image();
        let moved = fixture.root.path().join(format!("detached-{child}"));
        let selected = image.clone();
        let read_target = fs::metadata(if child == "objects" {
            image.join("objects").join(&digests[0])
        } else {
            image.join("snapshot.sqlite3")
        }).unwrap();
        let changed = Rc::new(Cell::new(false));
        let did_change = changed.clone();
        let _observer = observing(move |event| {
            // Replacements happen after hashing via held child descriptors.
            // Extras are inserted at a selected-image read boundary after its
            // member check, not while inspecting the source/private replay.
            let trigger = match event {
                Observation::ImageChildrenHashed => !extra_member,
                Observation::BeforeRead { file, .. } if extra_member => {
                    let metadata = file.metadata().unwrap();
                    metadata.dev() == read_target.dev() && metadata.ino() == read_target.ino()
                }
                _ => false,
            };
            if trigger && !did_change.replace(true) {
                let directory = selected.join(child);
                if extra_member {
                    fs::write(directory.join("late-unlisted-member"), b"not in manifest").unwrap();
                } else {
                    fs::rename(&directory, &moved).unwrap();
                    private_directory(&directory).unwrap();
                }
            }
            Ok(())
        });
        let result = MigrationPreflight::inspect(&fixture.paths, Some(&image));
        assert!(changed.get(), "selected-image observation must be reached");
        assert_eq!(fixture.originals(), before);
        assert_eq!(fs::read(fixture.paths.data_dir.join("objects").join(&digests[0])).unwrap(),
            b"private CAS child binding evidence");
        if extra_member {
            assert_eq!(fs::read(image.join(child).join("late-unlisted-member")).unwrap(), b"not in manifest");
        } else {
            assert_eq!(fs::read_dir(image.join(child)).unwrap().count(), 0);
            assert!(fs::read_dir(fixture.root.path().join(format!("detached-{child}"))).unwrap().next().is_some());
        }
        match result.unwrap() {
            MigrationPreflightReport::Existing { writable_allowed, checked_backup_digest, blockers, .. } => {
                assert!(!writable_allowed, "detached/expanded selected closure was admitted");
                assert!(checked_backup_digest.is_none(), "invalid selected image acquired a checked digest");
                assert!(blockers.iter().any(|code| code == if extra_member { "RecoveryClosureMismatch" } else { "SourceChanged" }),
                    "must deny for the raced image binding, not a missing-image fallback: {blockers:?}");
            }
            _ => panic!("existing source misclassified"),
        }
    }

    #[test]
    fn selected_image_rebinds_originals_after_hashing() {
        assert_selected_race_denied("originals", false);
    }

    #[test]
    fn selected_image_rebinds_objects_after_hashing() {
        assert_selected_race_denied("objects", false);
    }

    #[test]
    fn selected_image_rechecks_original_members_after_hashing() {
        assert_selected_race_denied("originals", true);
    }

    #[test]
    fn selected_image_rechecks_object_members_after_hashing() {
        assert_selected_race_denied("objects", true);
    }

    #[test]
    fn selected_image_rechecks_top_level_members_after_hashing() {
        assert_selected_race_denied("", true);
    }

    fn assert_raw_race_rejected_before_read(substitute: bool) {
        let fixture = Fixture::new();
        let before = fs::read(&fixture.paths.database).unwrap();
        let database = fixture.paths.database.clone();
        let detached = fixture.root.path().join("original-before-substitution");
        let kept_original = detached.clone();
        let parent = directory_handle(fixture.root.path(), false).unwrap();
        // Logical-only extension beyond real fstatvfs availability; no disk fill.
        let logical_bytes = capacity(&parent).unwrap().checked_add(OVERHEAD).unwrap();
        let initial_free = capacity(&parent).unwrap();
        assert!(initial_free >= (before.len() as u64).checked_mul(4).unwrap() + OVERHEAD,
            "fixture requires sufficient real capacity before the adversarial race");
        let mutated = Rc::new(Cell::new(false));
        let read_attempted = Rc::new(Cell::new(false));
        let did_mutate = mutated.clone();
        let attempted = read_attempted.clone();
        let _observer = observing(move |event| {
            match event {
                Observation::RawAdmitted if !did_mutate.replace(true) => {
                    if substitute {
                        fs::rename(&database, &detached).unwrap();
                        fs::write(&database, b"replacement sparse logical source").unwrap();
                    }
                    fs::OpenOptions::new().write(true).open(&database).unwrap().set_len(logical_bytes).unwrap();
                }
                Observation::BeforeRead { file, .. } if file.metadata().unwrap().len() >= logical_bytes => {
                    attempted.set(true);
                    // Fail before even the first buffer. The broken implementation
                    // must never turn this RED regression into a giant scan.
                    return Err(denied("TestInadmissibleReadBoundary"));
                }
                _ => {}
            }
            Ok(())
        });
        let result = Source::open(&fixture.paths, &parent);
        assert!(mutated.get(), "raw admission barrier must run");
        if substitute { assert_eq!(fs::read(&kept_original).unwrap(), before); }
        else {
            let mut prefix = vec![0; before.len()];
            File::open(&fixture.paths.database).unwrap().read_exact(&mut prefix).unwrap();
            assert_eq!(prefix, before);
        }
        assert_eq!(fs::metadata(&fixture.paths.database).unwrap().len(), logical_bytes);
        // Bound cleanup before assertions/panics as well; never read the hole.
        fs::OpenOptions::new().write(true).open(&fixture.paths.database).unwrap().set_len(0).unwrap();
        assert!(!read_attempted.get(), "raw admission was bypassed; test observer stopped an inadmissible read");
        match result {
            Err(error) => assert!(matches!(error.code.as_str(), "SourceChanged" | "InsufficientSpace"),
                "must reject the size/identity race before content reads: {}", error.code),
            Ok(_) => panic!("raw source changed after admission but was accepted"),
        }
    }

    #[test]
    fn raw_growth_after_capacity_admission_is_rejected_before_content_read() {
        assert_raw_race_rejected_before_read(false);
    }

    #[test]
    fn raw_substitution_after_capacity_admission_is_rejected_before_content_read() {
        assert_raw_race_rejected_before_read(true);
    }

    #[test]
    fn complete_cas_capacity_admission_precedes_first_closure_read() {
        let fixture = Fixture::new();
        // Two distinct valid objects, each bounded well below DEFAULT_ITEM_BYTES.
        // The second object's size is essential: admitting just the first is not
        // complete-closure admission.
        let first = vec![b'a'; 2 * 1024 * 1024];
        let second = vec![b'b'; 3 * 1024 * 1024];
        let digests = fixture.add_objects(&[&first, &second]);
        let before = fixture.originals();
        let objects_before = fixture.object_bytes();
        let total = (first.len() + second.len()) as u64;
        let raw: u64 = before.values().map(|bytes| bytes.len() as u64).sum();
        assert!(total > raw * 4, "CAS budget must distinguish raw-only admission");
        let destination = fixture.root.path().join("capacity-ordered-image");
        let admitted = Rc::new(Cell::new(false));
        let early_read = Rc::new(Cell::new(false));
        let saw_admission = admitted.clone();
        let saw_read = early_read.clone();
        let _observer = observing(move |event| {
            match event {
                Observation::Capacity { required, available } if required >= OVERHEAD + total => {
                    if available >= required { saw_admission.set(true); }
                }
                Observation::BeforeRead { name: Some(name), .. } if digests.iter().any(|digest| digest == name) => {
                    if !saw_admission.get() {
                        saw_read.set(true);
                        return Err(denied("TestCasReadBeforeCompleteAdmission"));
                    }
                }
                _ => {}
            }
            Ok(())
        });
        let result = MigrationPreflight::create_recovery_image(&fixture.paths, &destination);
        assert_eq!(fixture.originals(), before);
        assert_eq!(fixture.object_bytes(), objects_before);
        assert!(!early_read.get(), "first CAS byte was attempted before complete closure capacity admission");
        assert!(admitted.get(), "complete closure needs an observed successful real capacity check");
        result.unwrap();
    }

    #[test]
    fn complete_cas_capacity_refusal_precedes_any_closure_content_read() {
        let fixture = Fixture::new();
        let first = vec![b'c'; 2 * 1024 * 1024];
        let second = vec![b'd'; 3 * 1024 * 1024];
        let digests = fixture.add_objects(&[&first, &second]);
        let before = fixture.originals();
        let objects_before = fixture.object_bytes();
        let total = (first.len() + second.len()) as u64;
        let raw: u64 = before.values().map(|bytes| bytes.len() as u64).sum();
        assert!(total > raw * 4, "raw-only admission must not trigger the CAS refusal");
        let destination = fixture.root.path().join("capacity-refused-image");
        let denied_at_admission = Rc::new(Cell::new(false));
        let content_read = Rc::new(Cell::new(false));
        let observed_denial = denied_at_admission.clone();
        let observed_read = content_read.clone();
        let _observer = observing(move |event| {
            match event {
                Observation::Capacity { required, .. } if required >= OVERHEAD + total => {
                    // Unit-only fault at the real admission boundary, NOT a
                    // fabricated fstatvfs result or constrained-filesystem proof.
                    observed_denial.set(true);
                    return Err(denied("InsufficientSpace"));
                }
                Observation::BeforeRead { name: Some(name), .. } if digests.iter().any(|digest| digest == name) => {
                    observed_read.set(true);
                    return Err(denied("TestCasReadBeforeCapacityRefusal"));
                }
                _ => {}
            }
            Ok(())
        });
        let result = MigrationPreflight::create_recovery_image(&fixture.paths, &destination);
        assert_eq!(fixture.originals(), before);
        assert_eq!(fixture.object_bytes(), objects_before);
        assert!(!destination.exists(), "an inadmissible closure must not be published");
        assert!(!content_read.get(), "capacity refusal came after an attempted CAS content read");
        assert!(denied_at_admission.get(), "must reach complete-closure admission before reading");
        assert_eq!(result.unwrap_err().code, "InsufficientSpace");
    }
}

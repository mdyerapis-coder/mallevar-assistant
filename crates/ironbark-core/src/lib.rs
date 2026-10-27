pub mod contracts;
pub mod conversations;
pub mod dispatch;
pub mod error;
pub mod ipc;
pub mod judgments;
pub mod knowledge;
pub mod net;
pub mod paths;
pub mod policy;
pub mod process;
pub mod roles;
pub mod tools;
pub mod connections;
pub mod automation;
pub mod execution;
pub mod store;
pub mod workspaces;
pub mod scheduler;

use std::{fs, io::Write, os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt}};
use fs2::FileExt;
use error::{CoreError, Result};
use paths::{private_file, RuntimePaths};
use store::Store;

pub struct Core;
struct EndpointGuard { paths: RuntimePaths, _lock: std::fs::File, _data_lock: std::fs::File }
impl Drop for EndpointGuard {
    fn drop(&mut self) { let _ = fs::remove_file(&self.paths.socket); let _ = fs::remove_file(&self.paths.nonce); }
}
impl Core {
    pub async fn serve(paths: RuntimePaths) -> Result<()> {
        paths.prepare()?;
        // Runtime endpoints can differ; the store still has exactly one live authority.
        // Acquire before Store::open, whose recovery migration changes active work.
        let data_lock = private_file(&paths.data_dir.join("core.lock"))?;
        data_lock.try_lock_exclusive().map_err(|_| CoreError::new("core_already_running", "another core holds the data authority lock"))?;
        let lock = private_file(&paths.lock)?;
        lock.try_lock_exclusive().map_err(|_| CoreError::new("core_already_running", "another core holds the runtime lock"))?;
        match fs::symlink_metadata(&paths.socket) {
            Ok(meta) if meta.file_type().is_socket() && meta.uid() == paths::current_uid() => fs::remove_file(&paths.socket)?,
            Ok(_) => return Err(CoreError::new("unsafe_socket", "stale endpoint is not an owned socket")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }
        let guard = EndpointGuard { paths: paths.clone(), _lock: lock, _data_lock: data_lock };
        let store = Store::open(&paths.database).await?;
        let nonce = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let mut file = private_file(&paths.nonce)?;
        file.set_len(0)?;
        file.write_all(nonce.as_bytes())?;
        file.sync_all()?;
        let listener = tokio::net::UnixListener::bind(&paths.socket)?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        println!("CORE_READY protocol=1");
        let result = ipc::serve(listener, nonce, store, paths).await;
        drop(guard);
        result
    }
}

//! Conservative mark/sweep: every durable artifact and every source revision
//! remains a root. Removing a conversation/revision is a separate authority-
//! checked operation; collection never discards a historical citation to save space.
use std::{collections::BTreeSet,os::fd::AsRawFd, path::PathBuf};
use serde::{Serialize,Deserialize};
use schemars::JsonSchema;
use ts_rs::TS;
use crate::{error::{CoreError,Result},paths::private_directory};
use super::{Store,objects::{lease,object_files,safe_ancestors,sync_dir},schema::StorageSchema};

#[derive(Debug,Clone,PartialEq,Serialize,Deserialize,JsonSchema,TS)]
pub struct RetentionReport { #[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub removed_objects:u64, #[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub removed_staging_files:u64, #[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub reclaimed_bytes:u64, #[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub retained_objects:u64 }
#[derive(Clone)]
pub struct RetentionService {store:Store,root:PathBuf}
impl RetentionService {
    pub fn new(store:Store,root:PathBuf)->Result<Self> {safe_ancestors(&root)?;private_directory(&root)?;Ok(Self{store,root})}
    pub async fn sweep(&self)->Result<RetentionReport> {
        let root=self.root.clone();
        let guard=tokio::task::spawn_blocking(move ||lease(&root)).await.map_err(|_|CoreError::new("storage_closed","retention lease worker stopped"))??;
        let root=self.root.clone();
        // Immediate transaction excludes reference changes until unlink completes;
        // the filesystem lease excludes publishers and all active backups.
        let (report,_guard)=self.store.with_transaction(move |tx| {
            let roots=StorageSchema::for_version(1)?.roots(tx)?.into_iter().map(|reference|reference.digest.as_str().to_owned()).collect::<BTreeSet<_>>();let mut report=RetentionReport{removed_objects:0,removed_staging_files:0,reclaimed_bytes:0,retained_objects:0};
            for (digest,bytes) in object_files(&root)? {
                if roots.contains(digest.as_str()) {report.retained_objects+=1;continue;}
                std::fs::remove_file(root.join(digest.as_str()))?;report.removed_objects+=1;report.reclaimed_bytes=report.reclaimed_bytes.saturating_add(bytes);
            }
            let staging=root.join(".staging");
            if staging.exists() {
                safe_ancestors(&staging)?;
                for entry in std::fs::read_dir(&staging)? {
                    let path=entry?.path();let file=super::objects::read_file(&path)?;
                    if unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {
                        let error=std::io::Error::last_os_error();
                        if error.raw_os_error()==Some(libc::EWOULDBLOCK) {continue;}
                        return Err(error.into());
                    }
                    let bytes=file.metadata()?.len();std::fs::remove_file(&path)?;
                    report.removed_staging_files+=1;report.reclaimed_bytes=report.reclaimed_bytes.saturating_add(bytes);
                }
                sync_dir(&staging)?;
            }
            sync_dir(&root)?;Ok((report,guard))
        }).await?; Ok(report)
    }
}

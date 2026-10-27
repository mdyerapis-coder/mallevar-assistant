//! Imports are reviewed, additive snapshots, not a native-session resume bridge.
//! A preview binds the project, source document, and every included sidecar hash.
use std::{fs,io::{Cursor,Read},os::unix::fs::MetadataExt,path::{Path,PathBuf}};
use rusqlite::{params,OptionalExtension};
use serde::{Serialize,Deserialize};
use schemars::JsonSchema;
use ts_rs::TS;
use crate::{contracts::{ArtifactId,ConnectionConfig,Digest,ProjectId},error::{CoreError,Result},paths::private_directory};
use super::{Store,events::{redacted_json,now_ms},objects::{self,ObjectStore,DEFAULT_ITEM_BYTES}};

#[derive(Debug,Clone,PartialEq,Serialize,Deserialize,JsonSchema,TS)]
#[serde(deny_unknown_fields)]
pub struct ImportSource {#[ts(type="string")] pub path:PathBuf,#[serde(default)] pub format:ImportFormat}
#[derive(Debug,Clone,Default,PartialEq,Serialize,Deserialize,JsonSchema,TS)]
#[serde(tag="type",rename_all="snake_case",deny_unknown_fields)]
pub enum ImportFormat {#[default] Document,NativeTranscript{origin:String}}
impl ImportSource {
    pub fn document(path:PathBuf)->Self {Self{path,format:ImportFormat::Document}}
    pub fn native_transcript(path:PathBuf,origin:String)->Self {Self{path,format:ImportFormat::NativeTranscript{origin}}}
}
#[derive(Debug,Clone,PartialEq,Serialize,Deserialize,JsonSchema,TS)]
#[serde(deny_unknown_fields)]
pub struct ImportFile {pub path:String,pub digest:Digest,#[serde(with="crate::contracts::wire::u64")] #[schemars(with="crate::contracts::wire::WireU64Schema")] #[ts(type="string")] pub bytes:u64,pub device:String,pub inode:String}
#[derive(Debug,Clone,PartialEq,Serialize,Deserialize,JsonSchema,TS)]
#[serde(deny_unknown_fields)]
pub struct ImportPreview {pub digest:Digest,pub project_id:ProjectId,pub kind:String,pub files:Vec<ImportFile>,pub additive:bool,pub archived_only:bool,pub warnings:Vec<String>}
#[derive(Debug,Clone,PartialEq,Serialize,Deserialize,JsonSchema,TS)]
#[serde(deny_unknown_fields)]
pub struct ImportReceipt {pub preview_digest:Digest,pub project_id:ProjectId,pub kind:String,pub origin:String,pub source_digest:Digest,#[serde(skip_serializing_if="Option::is_none")] #[ts(optional)] pub artifact_id:Option<ArtifactId>,#[serde(skip_serializing_if="Option::is_none")] #[ts(optional)] pub source_id:Option<String>,#[serde(skip_serializing_if="Option::is_none")] #[ts(optional)] pub connection_id:Option<String>,pub archived_only:bool,#[serde(with="crate::contracts::wire::i64")] #[schemars(with="crate::contracts::wire::WireI64Schema")] #[ts(type="string")] pub applied_at:i64}

#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(tag="kind",rename_all="snake_case",deny_unknown_fields)]
enum ImportDocument {
    Connection {config:ConnectionConfig},
    Source {uri:String,path:PathBuf},
    NativeTranscript {origin:String,text:String},
    History {origin:String,messages:Vec<ArchivedMessage>},
}
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivedMessage {role:String,text:String}
impl ImportDocument {
    fn kind(&self)->&'static str {match self {Self::Connection{..}=>"connection",Self::Source{..}=>"source",Self::NativeTranscript{..}=>"native_transcript",Self::History{..}=>"history"}}
    fn archived(&self)->bool {matches!(self,Self::NativeTranscript{..}|Self::History{..})}
    fn origin(&self)->&str {match self {Self::Connection{config}=>&config.id,Self::Source{uri,..}=>uri,Self::NativeTranscript{origin,..}|Self::History{origin,..}=>origin}}
}
#[derive(Serialize,Deserialize)]
struct PreviewRecord {source:ImportSource,preview:ImportPreview}
struct Loaded {document:ImportDocument,files:Vec<ImportFile>,object:Option<Vec<u8>>}

fn allowed_path(path:&Path)->Result<()> {
    objects::safe_ancestors(path)?;
    for part in path.components() {
        let part=part.as_os_str().to_string_lossy().to_ascii_lowercase();
        if matches!(part.as_str(),".ssh"|".gnupg"|".aws"|".azure"|".kube"|"keyrings"|"kwalletd"|"bitwarden"|"bitwarden cli"|"session.nonce"|"auth-token.txt"|"credentials"|"credentials.json"|".credentials.json"|"auth.json"|".env") {
            return Err(CoreError::new("SecretImportDenied","credential/native auth material cannot be imported"));
        }
    } Ok(())
}
fn read_bounded(path:&Path)->Result<(Vec<u8>,ImportFile)> {
    allowed_path(path)?;let mut file=objects::read_file(path)?;let meta=file.metadata()?;let mut bytes=Vec::new();(&mut file).take(DEFAULT_ITEM_BYTES+1).read_to_end(&mut bytes)?;
    if bytes.len() as u64>DEFAULT_ITEM_BYTES {return Err(CoreError::new("ItemTooLarge","import exceeds 50 MiB item cap"));}
    let current=fs::symlink_metadata(path)?;
    if current.file_type().is_symlink() || current.dev()!=meta.dev() || current.ino()!=meta.ino() {return Err(CoreError::new("ImportChanged","selected file identity changed while reading"));}
    let record=ImportFile{path:path.to_str().ok_or_else(||CoreError::new("unsafe_path","import path must be UTF-8"))?.into(),digest:digest(&bytes)?,bytes:bytes.len() as u64,device:meta.dev().to_string(),inode:meta.ino().to_string()};
    Ok((bytes,record))
}
fn digest(bytes:&[u8])->Result<Digest> {Ok(objects::hash_reader(&mut Cursor::new(bytes),DEFAULT_ITEM_BYTES)?.0)}
fn redact_native_text(text:&str)->Result<String> {
    if let Ok(mut value)=serde_json::from_str::<serde_json::Value>(text) {super::events::redact_value(&mut value);return Ok(serde_json::to_string(&value)?);}
    let mut output=String::new();
    for line in text.split_inclusive('\n') {
        if let Ok(mut value)=serde_json::from_str::<serde_json::Value>(line) {
            super::events::redact_value(&mut value);output.push_str(&serde_json::to_string(&value)?);
            if line.ends_with('\n') {output.push('\n');}
        }else{output.push_str(&super::events::redact_text(line));}
    } Ok(output)
}
fn load(source:&ImportSource)->Result<Loaded> {
    let (bytes,record)=read_bounded(&source.path)?;let mut files=vec![record];
    let document:ImportDocument=match &source.format {
        ImportFormat::Document=>serde_json::from_slice(&bytes)?,
        ImportFormat::NativeTranscript{origin}=>ImportDocument::NativeTranscript{
            origin:origin.clone(),
            text:redact_native_text(std::str::from_utf8(&bytes).map_err(|_|CoreError::new("UnsupportedImport","native transcript must be UTF-8 JSON, JSONL or text"))?)?,
        },
    };
    let object=match &document {
        ImportDocument::Connection{config}=>{
            if config.id.is_empty() {return Err(CoreError::new("invalid_import","connection origin ID is required"));}
            let parsed=url::Url::parse(&config.endpoint).map_err(|_|CoreError::new("invalid_import","invalid connection endpoint"))?;
            if !matches!(parsed.scheme(),"http"|"https") || !parsed.username().is_empty() || parsed.password().is_some() {return Err(CoreError::new("SecretImportDenied","connection endpoint cannot carry credentials"));}
            if serde_json::to_value(config)?!=serde_json::from_str::<serde_json::Value>(&redacted_json(config)?)? {return Err(CoreError::new("SecretImportDenied","connection must use a credential reference"));}
            None
        },
        ImportDocument::Source{uri,path}=>{
            if uri.is_empty() || super::events::redact_text(uri)!=*uri {return Err(CoreError::new("invalid_import","source provenance URI must be non-secret and nonempty"));}
            let included=if path.is_absolute(){path.clone()}else{source.path.parent().ok_or_else(||CoreError::new("unsafe_path","source requires a parent"))?.join(path)};
            let (content,record)=read_bounded(&included)?;files.push(record);Some(content)
        },
        ImportDocument::NativeTranscript{origin,text}=>{
            if origin.is_empty() || text.is_empty() {return Err(CoreError::new("invalid_import","transcript origin and content are required"));}
            Some(redacted_json(&document)?.into_bytes())
        },
        ImportDocument::History{origin,messages}=>{
            if origin.is_empty() || messages.is_empty() || messages.len()>10_000 || messages.iter().any(|m|!matches!(m.role.as_str(),"system"|"user"|"assistant"|"tool")) {return Err(CoreError::new("invalid_import","history must have an origin and 1..10000 bounded messages with known roles"));}
            Some(redacted_json(&document)?.into_bytes())
        },
    };
    if object.as_ref().is_some_and(|b|b.len() as u64>DEFAULT_ITEM_BYTES) {return Err(CoreError::new("ItemTooLarge","redacted archive exceeds item cap"));}
    Ok(Loaded{document,files,object})
}
fn preview_digest(project:ProjectId,source:&ImportSource,kind:&str,files:&[ImportFile])->Result<Digest> {
    let bound=serde_json::to_vec(&serde_json::json!({"format":1,"project_id":project,"source":source,"kind":kind,"files":files,"additive":true,"native_resume":false}))?;digest(&bound)
}
#[derive(Clone)]
pub struct ImportService {store:Store,project:ProjectId,objects:ObjectStore}
impl ImportService {
    pub fn new(objects:ObjectStore)->Result<Self> {
        let store=objects.store.clone();let project=objects.project;
        // Separate lease serializes imports without recursively taking the CAS lease.
        private_directory(&objects.root.join(".imports"))?;Ok(Self{store,project,objects})
    }
    pub async fn preview(&self,source:ImportSource)->Result<ImportPreview> {
        let source_copy=source.clone();let loaded=tokio::task::spawn_blocking(move ||load(&source_copy)).await.map_err(|_|CoreError::new("storage_closed","import reader stopped"))??;
        let kind=loaded.document.kind().to_owned();let archived_only=loaded.document.archived();
        let preview=ImportPreview{digest:preview_digest(self.project,&source,&kind,&loaded.files)?,project_id:self.project,kind,files:loaded.files,additive:true,archived_only,warnings:if archived_only {vec!["Archived evidence only; no native session, run, authority or resume capability is created".into()]}else{vec!["Creates new records only; existing configuration and source revisions are not changed".into()]}};
        let record=PreviewRecord{source,preview:preview.clone()};let project=self.project;let body=redacted_json(&record)?;let digest=preview.digest.clone();
        // Redaction may not silently change the file/hash binding.
        if serde_json::to_value(&record)?!=serde_json::from_str::<serde_json::Value>(&body)? {return Err(CoreError::new("SecretImportDenied","preview paths must not carry credential material"));}
        self.store.with_transaction(move |tx|{tx.execute("INSERT INTO import_previews(digest,project_id,body,created_at) VALUES(?1,?2,?3,?4) ON CONFLICT(digest) DO NOTHING",params![digest.as_str(),project.to_string(),body,now_ms()])?;Ok(())}).await?;Ok(preview)
    }
    pub async fn apply(&self,preview_digest:Digest)->Result<ImportReceipt> {
        let lock_root=self.objects.root.join(".imports");let guard=tokio::task::spawn_blocking(move ||objects::lease(&lock_root)).await.map_err(|_|CoreError::new("storage_closed","import lease worker stopped"))??;
        let project=self.project;let requested=preview_digest.clone();
        let (record,existing)=self.store.with_connection(move |conn|{
            let existing:Option<String>=conn.query_row("SELECT body FROM import_receipts WHERE digest=?1 AND project_id=?2",params![requested.as_str(),project.to_string()],|r|r.get(0)).optional()?;
            if let Some(body)=existing {return Ok((None,Some(serde_json::from_str::<ImportReceipt>(&body)?)));}
            let body:Option<String>=conn.query_row("SELECT body FROM import_previews WHERE digest=?1 AND project_id=?2",params![requested.as_str(),project.to_string()],|r|r.get(0)).optional()?;
            let record:PreviewRecord=serde_json::from_str(&body.ok_or_else(||CoreError::new("ImportPreviewMissing","review a preview in this project first"))?)?;Ok((Some(record),None))
        }).await?;
        if let Some(receipt)=existing {return Ok(receipt);}
        let record=record.expect("existing receipt handled");let source=record.source;let source_copy=source.clone();
        let loaded=tokio::task::spawn_blocking(move ||load(&source_copy)).await.map_err(|_|CoreError::new("storage_closed","import reader stopped"))?
            .map_err(|_|CoreError::new("ImportChanged","document or included file is missing, unsafe or changed; review a new preview"))?;
        if record.preview.project_id!=project || record.preview.digest!=preview_digest || preview_digest!=self::preview_digest(project,&source,loaded.document.kind(),&loaded.files)? {
            return Err(CoreError::new("ImportChanged","document or included file changed; a new preview requires review"));
        }
        // Apply exactly the validated bytes, not a third read from a mutable file.
        let artifact_id=if let Some(bytes)=loaded.object {Some(self.objects.put(Cursor::new(bytes),DEFAULT_ITEM_BYTES).await?)}else{None};
        let kind=loaded.document.kind().to_owned();let archived_only=loaded.document.archived();
        let source_id=if matches!(&loaded.document,ImportDocument::Source{..}) {Some(uuid::Uuid::new_v4().to_string())}else{None};
        let connection_id=if matches!(&loaded.document,ImportDocument::Connection{..}) {Some(format!("import-{}",uuid::Uuid::new_v4()))}else{None};
        let receipt=ImportReceipt{preview_digest:preview_digest.clone(),project_id:project,kind,origin:super::events::redact_text(loaded.document.origin()),source_digest:record.preview.files[0].digest.clone(),artifact_id,source_id,connection_id,archived_only,applied_at:now_ms()};
        let result=receipt.clone();
        let (receipt,_guard)=self.store.with_transaction(move |tx|{
            match loaded.document {
                ImportDocument::Connection{mut config}=>{
                    config.id=result.connection_id.clone().expect("connection import ID");config.allowed_projects=vec![project];
                    tx.execute("INSERT INTO provider_references(id,config) VALUES(?1,?2)",params![config.id,redacted_json(&config)?])?;
                },
                ImportDocument::Source{uri,..}=>{
                    let id=result.source_id.as_ref().expect("source import ID");let artifact=result.artifact_id.expect("source object");
                    let digest:String=tx.query_row("SELECT digest FROM artifacts WHERE id=?1 AND project_id=?2",params![artifact.to_string(),project.to_string()],|r|r.get(0))?;
                    tx.execute("INSERT INTO sources(id,project_id,uri) VALUES(?1,?2,?3)",params![id,project.to_string(),uri])?;
                    tx.execute("INSERT INTO source_revisions(id,source_id,digest,created_at) VALUES(?1,?2,?3,?4)",params![uuid::Uuid::new_v4().to_string(),id,digest,result.applied_at])?;
                },
                ImportDocument::NativeTranscript{..}|ImportDocument::History{..}=>{},
            }
            tx.execute("INSERT INTO import_receipts(digest,project_id,body,applied_at) VALUES(?1,?2,?3,?4)",params![preview_digest.as_str(),project.to_string(),redacted_json(&result)?,result.applied_at])?;Ok((result,guard))
        }).await?;Ok(receipt)
    }
}

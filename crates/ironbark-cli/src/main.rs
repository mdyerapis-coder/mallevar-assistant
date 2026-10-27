//! Same-user Ironbark core client. Structured commands print exactly one JSON DTO on stdout;
//! `artifact read` emits the raw artifact bytes. Nonzero errors carry the real `CoreError`
//! code. `backup restore` is a deliberately offline owner-selected operation: it never
//! authenticates against a running core and never calls `RuntimePaths::prepare` before the
//! associated restore API. All operation arguments are named flags (the canonical CLI shapes).
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use ironbark_core::{
    conversations::{ExportFormat, RunResolution},
    contracts::{
        ArtifactId, BranchId, CheckpointPhase, CommandResult, ContentPart, ConversationId, CoreCommand,
        CoreReply, Digest, HarnessKind, MessageId, ProjectId, ProjectPolicy, RecoveryDecision,
        ResultManifest, RunId, RoutePin,
    },
    error::CoreError,
    ipc::CoreClient,
    paths::RuntimePaths,
    store::{backup::BackupService, imports::{ImportFormat, ImportSource}, preflight::MigrationPreflight},
};
use std::{io::Write as _, str::FromStr};
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about = "Trusted same-user Ironbark core client")]
struct Args {
    /// Durable request identity for structured commands (replay-safe).
    #[arg(long, global = true)] request_id: Option<Uuid>,
    #[command(subcommand)] command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status,
    Events { #[arg(long, default_value_t = 0)] after: u64 },
    Doctor { #[arg(long)] json: bool },
    Shutdown,
    /// Project lifecycle.
    Project { #[command(subcommand)] command: ProjectCommand },
    /// Project policy: commit a new version, or read the latest committed one.
    Policy { #[command(subcommand)] command: PolicyCommand },
    /// Additive, reviewed import (document or native transcript).
    Import { #[command(subcommand)] command: ImportCommand },
    /// Read back a durable CAS artifact (raw bytes on stdout).
    Artifact { #[command(subcommand)] command: ArtifactCommand },
    /// Snapshot-derived backup and the offline stopped restore.
    Backup { #[command(subcommand)] command: BackupCommand },
    /// Offline original-preserving recovery image and migration inspection.
    Migration { #[command(subcommand)] command: MigrationCommand },
    /// Conservative reference-aware CAS retention sweep.
    Retention { #[command(subcommand)] command: RetentionCommand },
    /// Published CAS capacity report.
    ObjectCapacity { #[arg(long)] project: String },
    /// Conversation tree operations.
    Conversation { #[command(subcommand)] command: ConversationCommand },
    /// Run scheduler lifecycle (create, start, pause, resume, inspect, reconcile).
    Run { #[command(subcommand)] command: RunCommand },
    /// Run checkpoints (a CAS manifest link for one run phase).
    Checkpoint { #[command(subcommand)] command: CheckpointCommand },
    /// Generic core command: the full `CoreCommand` as one JSON argument.
    #[command(name = "command")]
    Raw { #[arg(value_name = "COMMAND_JSON")] json: String },
}

#[derive(Subcommand)]
enum ProjectCommand {
    Create { name: String, #[arg(long)] root: Option<std::path::PathBuf> },
    List,
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Commit a new policy version, parsed from a `ProjectPolicy` JSON file. The
    /// policy's own `project_id` must match `--project`.
    Set { #[arg(long)] project: String, #[arg(long)] file: std::path::PathBuf },
    /// Print the latest committed policy for a project.
    Show { #[arg(long)] project: String },
}

#[derive(Subcommand)]
enum ImportCommand {
    Preview {
        #[arg(long)] project: String,
        #[arg(long)] file: std::path::PathBuf,
        /// document (default) | native-transcript
        #[arg(long, default_value = "document")] kind: String,
        #[arg(long)] origin: Option<String>,
    },
    Apply { #[arg(long)] project: String, #[arg(long)] digest: String },
}

#[derive(Subcommand)]
enum ArtifactCommand {
    Read { #[arg(long)] project: String, #[arg(long)] id: String },
}

#[derive(Subcommand)]
enum BackupCommand {
    Create { #[arg(long)] destination: std::path::PathBuf },
    /// Offline restore into a freshly selected namespace (the current XDG env). Never
    /// authenticates and never overwrites an existing namespace.
    Restore { #[arg(long)] source: std::path::PathBuf, #[arg(long, default_value_t = true)] new_namespace: bool },
}

#[derive(Subcommand)]
enum MigrationCommand {
    /// Copy a stopped source into a new private raw recovery image.
    Backup { #[arg(long)] destination: std::path::PathBuf },
    /// Inspect without starting a core; a selected backup must already exist.
    Preflight { #[arg(long)] backup: Option<std::path::PathBuf> },
}

#[derive(Subcommand)]
enum RetentionCommand {
    Sweep,
}

#[derive(Subcommand)]
enum ConversationCommand {
    Create { #[arg(long)] project: String },
    History { #[arg(long)] conversation: String },
    Rename { #[arg(long)] conversation: String, title: String },
    Search { #[arg(long)] project: String, query: String, #[arg(long, default_value_t = 100)] limit: u32, #[arg(long)] include_archived: bool },
    Export { #[arg(long)] conversation: String, #[arg(long, default_value = "json")] format: String },
    Edit { #[arg(long)] message: String, parts: String },
    QueueTurn { #[arg(long)] conversation: String, #[arg(long)] branch: String, parts: String, #[arg(long)] pin: String },
    Regenerate { #[arg(long)] message: String, #[arg(long)] pin: String },
    Archive { #[arg(long)] conversation: String },
    Delete { #[arg(long)] conversation: String, #[arg(long)] run: Vec<String> },
}

#[derive(Subcommand)]
enum RunCommand {
    /// Create a queued run for a task. `--workspace` is folded into the task text as an
    /// explicit note (the protocol carries `project_id` + `task` only).
    Create {
        #[arg(long)] project: String,
        #[arg(long)] task: String,
        #[arg(long)] workspace: Option<std::path::PathBuf>,
    },
    /// Transition a queued run to running. The pin flags build the `RoutePin`.
    Start {
        #[arg(long)] project: String,
        #[arg(long)] run: String,
        #[arg(long, default_value = "")] connection: String,
        #[arg(long, default_value = "")] model: String,
        /// omp | codex | opencode | claude | hermes | alphacode
        #[arg(long)] harness: Option<String>,
    },
    /// Move a running run to paused.
    Pause { #[arg(long)] project: String, #[arg(long)] run: String },
    /// Move a paused run back to running.
    Resume { #[arg(long)] project: String, #[arg(long)] run: String },
    /// Request cancellation of a run.
    Cancel { #[arg(long)] project: String, #[arg(long)] run: String },
    /// Record an observed native process stop for a run.
    ObserveStop { #[arg(long)] project: String, #[arg(long)] run: String },
    /// Record that the native harness reported its own run complete.
    NativeDone {
        #[arg(long)] project: String,
        #[arg(long)] run: String,
        /// Native harness completion receipt text.
        #[arg(long, default_value = "")] receipt: String,
    },
    /// Print the recovery report for a run.
    Inspect { #[arg(long)] project: String, #[arg(long)] run: String },
    /// Apply a recovery decision to an interrupted run:
    /// accept-observed | resume-read-only | continue-from-checkpoint | abandon.
    Reconcile {
        #[arg(long)] project: String,
        #[arg(long)] run: String,
        #[arg(long)] decision: String,
    },
}

#[derive(Subcommand)]
enum CheckpointCommand {
    /// Capture a checkpoint for a run phase, parsing the manifest file as a
    /// `ResultManifest`; prints the returned artifact id DTO.
    Capture {
        #[arg(long)] project: String,
        #[arg(long)] run: String,
        /// planned | pre-mutation | post-mutation | post-verification | pre-consequential
        #[arg(long)] phase: String,
        #[arg(long)] manifest: std::path::PathBuf,
    },
}

// The core ID newtypes are transparent Uuid wrappers; parse from the CLI string form.
fn project_id(raw: &str) -> Result<ProjectId, String> {
    Ok(ProjectId(Uuid::from_str(raw).map_err(|e| e.to_string())?))
}
fn conversation_id(raw: &str) -> Result<ConversationId, String> {
    Ok(ConversationId(Uuid::from_str(raw).map_err(|e| e.to_string())?))
}
fn branch_id(raw: &str) -> Result<BranchId, String> {
    Ok(BranchId(Uuid::from_str(raw).map_err(|e| e.to_string())?))
}
fn message_id(raw: &str) -> Result<MessageId, String> {
    Ok(MessageId(Uuid::from_str(raw).map_err(|e| e.to_string())?))
}
fn run_id(raw: &str) -> Result<RunId, String> {
    Ok(RunId(Uuid::from_str(raw).map_err(|e| e.to_string())?))
}
fn artifact_id(raw: &str) -> Result<ArtifactId, String> {
    Ok(ArtifactId(Uuid::from_str(raw).map_err(|e| e.to_string())?))
}

async fn run() -> Result<(), String> {
    let args = Args::parse();
    let paths = RuntimePaths::from_env().map_err(display)?;

    // The offline owner-selected restore: no daemon, no auth, no prepare.
    if let Command::Backup { command: BackupCommand::Restore { source, .. } } = &args.command {
        let report = BackupService::restore(source, &paths).await.map_err(|e| e.code.clone())?;
        return print_json(&report);
    }

    // Offline preflight never connects, prepares paths, or starts a writable Store.
    if let Command::Migration { command } = &args.command {
        return match command {
            MigrationCommand::Backup { destination } => {
                let manifest = MigrationPreflight::create_recovery_image(&paths, destination).map_err(|e| e.code)?;
                print_json(&manifest)
            }
            MigrationCommand::Preflight { backup } => {
                let report = MigrationPreflight::inspect(&paths, backup.as_deref()).map_err(|e| e.code)?;
                print_json(&report)?;
                if report.writable_allowed() { Ok(()) } else { Err("migration_preflight_blocked".into()) }
            }
        };
    }

    let mut client = CoreClient::connect(&paths).await.map_err(display)?;
    let id = args.request_id.unwrap_or_else(Uuid::new_v4);

    // `events` streams bounded pages until the backlog drains; each envelope is one line.
    if let Command::Events { after } = &args.command {
        let mut after = *after;
        loop {
            let reply = client.request(Uuid::new_v4(), CoreCommand::Events { after }).await.map_err(display)?;
            let events = match outcome(reply).map_err(display)? {
                CommandResult::Events { value } => value,
                other => return Err(format!("expected events result, got {}", serde_json::to_string(&other).map_err(display)?)),
            };
            if events.is_empty() { break; }
            for event in events {
                after = event.seq;
                println!("{}", serde_json::to_string(&event).map_err(display)?);
            }
        }
        return Ok(());
    }

    let command = args.command.build().map_err(display)?;
    let reply = client.request(id, command.clone()).await.map_err(display)?;
    emit(&paths, reply, &command).map_err(display)
}

impl Command {
    fn build(&self) -> Result<CoreCommand, String> {
        use CoreCommand as C;
        Ok(match self {
            Command::Status => C::Status {},
            Command::Doctor { .. } => C::Doctor {},
            Command::Shutdown => C::Shutdown {},
            Command::Project { command: ProjectCommand::Create { name, root } } => {
                let root = match root.clone() { Some(path) => path, None => std::env::current_dir().map_err(display)? };
                let root = std::fs::canonicalize(&root).map_err(|e| format!("canonicalize {root:?}: {e}"))?;
                C::ProjectCreate { name: name.clone(), root: root.to_string_lossy().into_owned() }
            }
            Command::Project { command: ProjectCommand::List } => C::ProjectList {},
            Command::Policy { command } => command.build()?,
            Command::Import { command: ImportCommand::Preview { project, file, kind, origin } } => {
                let format = match kind.as_str() {
                    "document" => ImportFormat::Document,
                    "native-transcript" => ImportFormat::NativeTranscript { origin: origin.clone().ok_or("import preview --kind native-transcript requires --origin")? },
                    other => return Err(format!("import --kind must be document or native-transcript, got {other:?}")),
                };
                C::ImportPreview { project_id: project_id(project)?, source: ImportSource { path: file.clone(), format } }
            }
            Command::Import { command: ImportCommand::Apply { project, digest } } => {
                C::ImportApply { project_id: project_id(project)?, preview_digest: Digest::parse(digest).map_err(display)? }
            }
            Command::Artifact { command: ArtifactCommand::Read { project, id } } => {
                C::ArtifactRead { project_id: project_id(project)?, artifact_id: artifact_id(id)? }
            }
            Command::Backup { command: BackupCommand::Create { destination } } => {
                C::BackupCreate { destination: destination.to_string_lossy().into_owned() }
            }
            Command::Backup { command: BackupCommand::Restore { .. } } => {
                return Err("backup restore is offline and does not route through the daemon".into())
            }
            Command::Migration { .. } => return Err("migration commands are offline and do not route through the daemon".into()),
            Command::Retention { command: RetentionCommand::Sweep } => C::RetentionSweep {},
            Command::ObjectCapacity { project } => C::ObjectCapacity { project_id: project_id(project)? },
            Command::Conversation { command } => command.build()?,
            Command::Run { command } => command.build()?,
            Command::Checkpoint { command } => command.build()?,
            // Handled directly in run() before build is reached.
            Command::Events { .. } => return Err("events streams directly; use the events subcommand".into()),
            Command::Raw { json } => serde_json::from_str(json).map_err(display)?,
        })
    }
}

impl PolicyCommand {
    fn build(&self) -> Result<CoreCommand, String> {
        use CoreCommand as C;
        Ok(match self {
            PolicyCommand::Set { project, file } => {
                let raw = std::fs::read_to_string(file).map_err(|e| format!("read policy {file:?}: {e}"))?;
                let policy = serde_json::from_str::<ProjectPolicy>(&raw).map_err(|e| format!("policy: {e}"))?;
                C::ProjectPolicySet { project_id: project_id(project)?, policy }
            }
            PolicyCommand::Show { project } => C::ProjectPolicyShow { project_id: project_id(project)? },
        })
    }
}

impl ConversationCommand {
    fn build(&self) -> Result<CoreCommand, String> {
        use CoreCommand as C;
        let parse_pin = |raw: &str| serde_json::from_str::<RoutePin>(raw).map_err(|e| format!("pin: {e}"));
        let parse_parts = |raw: &str| serde_json::from_str::<Vec<ContentPart>>(raw).map_err(|e| format!("parts: {e}"));
        Ok(match self {
            ConversationCommand::Create { project } => C::ConversationCreate { project_id: project_id(project)? },
            ConversationCommand::History { conversation } => C::ConversationHistory { conversation_id: conversation_id(conversation)? },
            ConversationCommand::Rename { conversation, title } => C::ConversationRename { conversation_id: conversation_id(conversation)?, title: title.clone() },
            ConversationCommand::Search { project, query, limit, include_archived } => C::ConversationSearch { project_id: project_id(project)?, query: query.clone(), limit: *limit, include_archived: *include_archived },
            ConversationCommand::Export { conversation, format } => {
                let format = match format.as_str() {
                    "json" => ExportFormat::Json,
                    "markdown" => ExportFormat::Markdown,
                    other => return Err(format!("export --format must be json or markdown, got {other:?}")),
                };
                C::ConversationExport { conversation_id: conversation_id(conversation)?, format }
            }
            ConversationCommand::Edit { message, parts } => C::ConversationEdit { message_id: message_id(message)?, parts: parse_parts(parts)? },
            ConversationCommand::QueueTurn { conversation, branch, parts, pin } => C::ConversationQueueTurn { conversation_id: conversation_id(conversation)?, branch_id: branch_id(branch)?, parts: parse_parts(parts)?, pin: parse_pin(pin)?, context: None },
            ConversationCommand::Regenerate { message, pin } => C::ConversationRegenerate { message_id: message_id(message)?, pin: parse_pin(pin)? },
            ConversationCommand::Archive { conversation } => C::ConversationArchive { conversation_id: conversation_id(conversation)? },
            ConversationCommand::Delete { conversation, run } => {
                let run_ids = run.iter().map(|r| run_id(r)).collect::<Result<Vec<_>, _>>()?;
                C::ConversationDelete { conversation_id: conversation_id(conversation)?, resolution: RunResolution { run_ids } }
            }
        })
    }
}

impl RunCommand {
    fn build(&self) -> Result<CoreCommand, String> {
        use CoreCommand as C;
        Ok(match self {
            RunCommand::Create { project, task, workspace } => {
                // The protocol carries `project_id` + `task` only; a requested workspace is an
                // explicit, bounded note appended to the task text.
                let task = match workspace {
                    Some(dir) => format!("{task}\n\n[workspace] {}", dir.display()),
                    None => task.clone(),
                };
                C::RunCreate { project_id: project_id(project)?, task }
            }
            RunCommand::Start { project, run, connection, model, harness } => {
                let harness = match harness {
                    Some(kind) => Some(parse_harness(kind)?),
                    None => None,
                };
                C::RunStart {
                    project_id: project_id(project)?,
                    run_id: run_id(run)?,
                    pin: RoutePin { connection_id: connection.clone(), model_id: model.clone(), harness },
                    native_session: None,
                    artifacts: Vec::new(),
                    recorded_pid: None,
                    recorded_start_time: None,
                }
            }
            RunCommand::Pause { project, run } => C::RunPause { project_id: project_id(project)?, run_id: run_id(run)? },
            RunCommand::Resume { project, run } => C::RunResume { project_id: project_id(project)?, run_id: run_id(run)? },
            RunCommand::Cancel { project, run } => C::RunCancel { project_id: project_id(project)?, run_id: run_id(run)? },
            RunCommand::ObserveStop { project, run } => C::RunObserveStop { project_id: project_id(project)?, run_id: run_id(run)? },
            RunCommand::NativeDone { project, run, receipt } => C::RunNativeDone { project_id: project_id(project)?, run_id: run_id(run)?, receipt: receipt.clone() },
            RunCommand::Inspect { project, run } => C::RunInspect { project_id: project_id(project)?, run_id: run_id(run)? },
            RunCommand::Reconcile { project, run, decision } => C::RunReconcile {
                project_id: project_id(project)?, run_id: run_id(run)?, decision: parse_recovery_decision(decision)?,
            },
        })
    }
}

impl CheckpointCommand {
    fn build(&self) -> Result<CoreCommand, String> {
        use CoreCommand as C;
        Ok(match self {
            CheckpointCommand::Capture { project, run, phase, manifest } => {
                let raw = std::fs::read_to_string(manifest).map_err(|e| format!("read manifest {manifest:?}: {e}"))?;
                let manifest = serde_json::from_str::<ResultManifest>(&raw).map_err(|e| format!("manifest: {e}"))?;
                C::RunCheckpoint {
                    project_id: project_id(project)?, run_id: run_id(run)?, phase: parse_checkpoint_phase(phase)?, manifest,
                }
            }
        })
    }
}

fn parse_harness(raw: &str) -> Result<HarnessKind, String> {
    Ok(match raw {
        "omp" => HarnessKind::Omp,
        "codex" => HarnessKind::Codex,
        "opencode" => HarnessKind::Opencode,
        "claude" => HarnessKind::Claude,
        "hermes" => HarnessKind::Hermes,
        "alphacode" => HarnessKind::Alphacode,
        other => return Err(format!("run start --harness must be omp, codex, opencode, claude, hermes or alphacode, got {other:?}")),
    })
}

fn parse_checkpoint_phase(raw: &str) -> Result<CheckpointPhase, String> {
    Ok(match raw {
        "planned" => CheckpointPhase::Planned,
        "pre-mutation" => CheckpointPhase::PreMutation,
        "post-mutation" => CheckpointPhase::PostMutation,
        "post-verification" => CheckpointPhase::PostVerification,
        "pre-consequential" => CheckpointPhase::PreConsequential,
        other => return Err(format!("checkpoint capture --phase must be planned, pre-mutation, post-mutation, post-verification or pre-consequential, got {other:?}")),
    })
}

fn parse_recovery_decision(raw: &str) -> Result<RecoveryDecision, String> {
    Ok(match raw {
        "accept-observed" => RecoveryDecision::AcceptObserved,
        "resume-read-only" => RecoveryDecision::ResumeReadOnly,
        "continue-from-checkpoint" => RecoveryDecision::ContinueFromCheckpoint,
        "abandon" => RecoveryDecision::Abandon,
        other => return Err(format!("run reconcile --decision must be accept-observed, resume-read-only, continue-from-checkpoint or abandon, got {other:?}")),
    })
}

// `artifact read` returns base64 in the DTO; the raw bytes are what a caller wants on stdout.
// Every other outcome prints its typed leaf value (the command tag is the request's own).
fn emit(_paths: &RuntimePaths, reply: CoreReply, command: &CoreCommand) -> Result<(), String> {
    let result = outcome(reply).map_err(display)?;
    if let CommandResult::ArtifactRead { value: read } = &result {
        let bytes = STANDARD.decode(&read.data).map_err(|e| format!("artifact base64: {e}"))?;
        if bytes.len() as u64 != read.bytes {
            return Err(format!("artifact byte count differs from its manifest"));
        }
        let mut out = std::io::stdout().lock();
        out.write_all(&bytes).map_err(|e| format!("write artifact bytes: {e}"))?;
        out.flush().map_err(|e| format!("flush artifact bytes: {e}"))?;
        return Ok(());
    }
    if !matches!(command, CoreCommand::ArtifactRead { .. }) {
        println!("{}", serde_json::to_string_pretty(&result.leaf_json()).map_err(display)?);
    }
    Ok(())
}

/// Unwrap the typed `CommandResult` from a reply, surfacing the typed error.
fn outcome(reply: CoreReply) -> Result<CommandResult, CoreError> {
    match reply {
        CoreReply::Ok { value } => Ok(value),
        CoreReply::Error { code, message, .. } => Err(CoreError::new(&code, message)),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    println!("{}", serde_json::to_string_pretty(value).map_err(display)?);
    Ok(())
}

fn display<T: std::fmt::Display>(e: T) -> String { e.to_string() }

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

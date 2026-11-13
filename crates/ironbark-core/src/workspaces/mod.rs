// H1: real Git worktree ownership, reader/writer leases, integration receipts.
pub mod worktrees;
pub mod diffs;

pub use worktrees::{CheckKind, WorkspaceChoice, WorkspaceService, WorktreeLease, IntegrationReceipt};
// The canonical workspace mode lives in the contract module (H6 job nodes use it
// too); re-exported here so workspace callers keep one import path.
pub use crate::contracts::WorkspaceMode;
pub use diffs::{DiffCapture, DiffSummary};

use crate::error::CoreError;

/// One active writer lease per worktree path. The second writer gets a typed
/// `workspace_busy` error instead of a shared mutable baseline.
pub(crate) fn workspace_busy(path: &str) -> CoreError {
    CoreError::new("workspace_busy", format!("A writer already owns worktree {path}"))
}

/// A git command or plumbing failure that carries the real command and output.
pub(crate) fn git_failed(command: &str, code: std::process::ExitStatus, output: &str) -> CoreError {
    let detail = if output.trim().is_empty() {
        format!("git {command} exited {code}")
    } else {
        format!("git {command} exited {code}: {}", output.trim().lines().next().unwrap_or_default())
    };
    CoreError::new("git_failed", detail)
}


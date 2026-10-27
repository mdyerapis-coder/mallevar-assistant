// H1: real diff capture against the lease's base commit. Every digest an
// integration grant or receipt carries is computed here from `git diff`
// output — never supplied by a worker.
use std::path::Path;
use sha2::Digest as _;

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffSummary {
    pub base_commit: String,
    pub head_commit: String,
    pub worktree_digest: crate::contracts::Digest,
    pub changed_paths: Vec<String>,
}

/// Run git against `root` (or `cwd` when given). Command and arguments are
/// fixed by the caller; no shell is involved.
pub(crate) fn git(cwd: Option<&Path>, args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut command = std::process::Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
    command.args(args);
    command.output()
}

pub struct DiffCapture;
impl DiffCapture {
    fn require_commit(value: &str, what: &str) -> Result<()> {
        if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CoreError::new("invalid_base_commit", format!("{what} is not a 40-hex commit")));
        }
        Ok(())
    }

    pub fn diff_text(worktree: &Path, base: &str) -> Result<String> {
        Self::require_commit(base, "Base commit")?;
        let output = git(Some(worktree), &["diff", "--binary", base])
            .map_err(|e| CoreError::new("git_spawn_failed", format!("git diff could not run: {e}")))?;
        if !output.status.success() {
            return Err(crate::workspaces::git_failed(
                "diff",
                output.status,
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn head(worktree: &Path) -> Result<String> {
        let output = git(Some(worktree), &["rev-parse", "--verify", "HEAD^{commit}"])
            .map_err(|e| CoreError::new("git_spawn_failed", format!("git rev-parse could not run: {e}")))?;
        if !output.status.success() {
            return Err(crate::workspaces::git_failed(
                "rev-parse",
                output.status,
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Self::require_commit(&commit, "HEAD")?;
        Ok(commit)
    }

    pub fn changed_paths(worktree: &Path, base: &str) -> Result<Vec<String>> {
        let output = git(Some(worktree), &["diff", "--name-only", base])
            .map_err(|e| CoreError::new("git_spawn_failed", format!("git diff could not run: {e}")))?;
        if !output.status.success() {
            return Err(crate::workspaces::git_failed(
                "diff",
                output.status,
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// The canonical observation bound into integration actions and receipts:
    /// base, head and the digest of the full binary diff between them.
    pub fn capture(worktree: &Path, base: &str) -> Result<DiffSummary> {
        let diff = Self::diff_text(worktree, base)?;
        let head = Self::head(worktree)?;
        let changed_paths = Self::changed_paths(worktree, base)?;
        let digest = crate::contracts::Digest::parse(&format!(
            "{:x}",
            sha2::Sha256::digest(diff.as_bytes())
        ))?;
        Ok(DiffSummary { base_commit: base.to_owned(), head_commit: head, worktree_digest: digest, changed_paths })
    }
}

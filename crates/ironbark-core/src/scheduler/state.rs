//! C4 master run-state union and the legal transition table. This is the only
//! code path allowed to compute durable run state; worker events and frontend
//! state labels never write `runs.state` directly.
use crate::contracts::RunState;
use crate::error::{CoreError, Result};
use serde_json::json;
/// C4 lifecycle events. Private: adapters and clients may never name a
/// transition, only the scheduler's authenticated services apply them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEvent {
    /// The scheduler claims a queued turn and prepares it for dispatch.
    Ready,
    /// A native adapter/session has started producing the turn.
    Started,
    /// A consequential action needs owner approval before it may run.
    ApprovalNeeded,
    /// The owner approved the pending action; the run continues.
    Approved,
    /// The owner denied the pending action; the run is blocked.
    Denied,
    /// The run paused (owner command/UI) - native session + partial artifacts retained.
    Paused,
    /// A paused run is resumed.
    Resumed,
    /// A mutation attempt began and must complete before the run may proceed.
    Mutating,
    /// Verification of the produced work began.
    Verifying,
    /// Verification is complete: work is awaiting review/acceptance.
    Verified,
    /// Review/acceptance of the final result began.
    Reviewing,
    /// The owner issued a cancel/interrupt REQUEST to the native process.
    CancelRequested,
    /// The native process was observed stopped (actual stop receipt).
    StopObserved,
    /// Native reports completion WITHOUT evidence - never success by itself.
    NativeDone,
    /// A durable acceptance receipt for the produced work arrived.
    Accepted,
    /// The produced work was rejected by review.
    Rejected,
    /// A mutation was attempted without a valid consumption receipt.
    MutationBlocked,
    /// A reconciled terminal failure was committed.
    Failed,
    /// The run was abandoned during reconciliation.
    Abandoned,
    /// Reconciliation accepted the observed (crash) state as final.
    AcceptObserved,
    /// Reconciliation resumed a crashed run read-only.
    ResumeReadOnly,
    /// Reconciliation resumed a crashed run from its durable checkpoint.
    ContinueFromCheckpoint,
    /// A bound grant failed eligibility under the current policy; the run parks
    /// awaiting a fresh owner approval (the stale grant is not reused).
    StaleGrant,
}

fn illegal(from: RunState, event: RunEvent) -> Result<RunState> {
    Err(CoreError::new("illegal_transition", &format!(
        "event {:?} is not legal from run state {:?}; run state is owned by C4",
        event, from
    )))
}

/// The legal transition table. Terminal states accept nothing; `Succeeded` is
/// reachable ONLY through `Reviewing + Accepted` (an explicit acceptance
/// receipt), never from a native completion label, and `Cancelled` only from
/// an observed stop, never from a cancel request alone.
pub fn transition(from: RunState, event: RunEvent) -> Result<RunState> {
    use RunEvent as E;
    use RunState as S;
    Ok(match (from, event) {
        (S::Queued, E::Ready) => S::Ready,
        (S::Queued, E::CancelRequested) => S::Cancelling,
        (S::Queued, E::Failed) => S::Failed,

        (S::Ready, E::Started) => S::Running,
        (S::Ready, E::Paused) => S::Paused,
        (S::Ready, E::CancelRequested) => S::Cancelling,
        (S::Ready, E::Failed) => S::Failed,

        (S::Running, E::ApprovalNeeded) => S::AwaitingApproval,
        (S::Running, E::Mutating) => S::Running,
        (S::Running, E::Verifying) => S::Verifying,
        (S::Running, E::Reviewing) => S::Reviewing,
        (S::Running, E::NativeDone) => S::Reviewing,
        (S::Running, E::Paused) => S::Paused,
        (S::Running, E::CancelRequested) => S::Cancelling,
        (S::Running, E::MutationBlocked) => S::Blocked,
        (S::Running, E::Failed) => S::Failed,

        (S::AwaitingApproval, E::Approved) => S::Running,
        (S::AwaitingApproval, E::Denied) => S::Blocked,
        (S::AwaitingApproval, E::Paused) => S::Paused,
        (S::AwaitingApproval, E::CancelRequested) => S::Cancelling,
        (S::AwaitingApproval, E::Failed) => S::Failed,

        (S::Verifying, E::Verified) => S::Reviewing,
        (S::Verifying, E::Paused) => S::Paused,
        (S::Verifying, E::CancelRequested) => S::Cancelling,
        (S::Verifying, E::Failed) => S::Failed,

        (S::Reviewing, E::Accepted) => S::Succeeded,
        (S::Reviewing, E::Rejected) => S::Failed,
        (S::Reviewing, E::Failed) => S::Failed,
        (S::Reviewing, E::Paused) => S::Paused,
        (S::Reviewing, E::CancelRequested) => S::Cancelling,

        (S::Paused, E::Resumed) => S::Ready,
        (S::Paused, E::CancelRequested) => S::Cancelling,
        (S::Paused, E::Failed) => S::Failed,

        (S::Cancelling, E::StopObserved) => S::Cancelled,
        (S::Cancelling, E::Failed) => S::Failed,

        (S::Blocked, E::Abandoned) => S::Cancelled,
        (S::Blocked, E::StaleGrant) => S::AwaitingApproval,

        _ => return illegal(from, event),
    })
}

/// Shared: parse a persisted `runs.state` into a RunState.
pub(crate) fn map_state(text: &str) -> Result<RunState> {
    serde_json::from_value::<RunState>(json!(text)).map_err(|_| CoreError::new("storage_corrupt", "Invalid persisted run state"))
}

/// Shared: serialize a RunState to its persisted snake_case string.
pub(crate) fn state_str(state: RunState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}
// state.rs helpers

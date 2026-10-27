//! C4: run state transitions, durable checkpoints, crash reconciliation.
//! `transition` is the master state union; `CheckpointStore` persists the
//! reconciliation boundary (workspace/diff/commit/artifact state); `Recovery`
//! inspects a crashed run and applies an owner decision; `RunControl` owns the
//! lifecycle commands that feed the transition table.
pub mod state;
pub mod checkpoints;
pub mod recovery;
pub mod service;
pub mod graph;
pub mod budgets;
pub mod handoffs;

pub use state::{transition, RunEvent};
pub use checkpoints::CheckpointStore;
pub use recovery::{Recovery, RunControl, MutationReceipt};
pub use service::{RunReceipt, SchedulerService};
pub use MutationReceipt as mutation_receipt;
pub use graph::{GraphValidator, Scheduler, ValidatedGraph, BUILTIN_ROLE_IDS};
pub use budgets::{Admission, AdmissionBudget, BlockedRun, Budgets, RouteCost, Usage, DEFAULT_MAX_WORKERS};
pub use handoffs::{build_handoff, HandoffRequest};
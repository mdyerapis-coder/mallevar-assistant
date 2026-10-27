//! Action-scoped browser and Linux desktop automation.
//!
//! The [`AutomationBroker`] is the single front door for browser (`BrowserProfile`)
//! and desktop (`NativeWindow`) automation. It is also the owner of a core-owned,
//! byte-verified `document_file` backend that the offline tests drive against a
//! real temp-filesystem target.
//!
//! Authority model (H8 spec):
//! * A coding worker NEVER gets host-session or personal-browser access. It can
//!   only act on a target the owner explicitly scoped for it, and every act is a
//!   broker request — there is no path to the host session bus or the operator's
//!   personal browser from worker authority.
//! * The owner opens a scope with [`AutomationBroker::observe`], which records a
//!   durable `automation-scope/<scope_id>` state event and returns the target's
//!   current revision digest. `release` transitions that scope to "released"
//!   (idempotent). `execute` refuses any released / stale scope.
//! * Before authorising a single operation the broker checks, in order:
//!   (1) an active, unexpired scope exists for the target kind + destination and
//!       the caller's role is permitted (host-session is owner-only);
//!   (2) the target's live revision digest is FRESH — it is re-observed and
//!       compared to `action.expected_state_digest`; a stale snapshot cannot
//!       receive a click;
//!   (3) the exact grant matches the action digest + policy version and is
//!       unexpired / unrevoked (delegated to `ApprovalService::consume`);
//!   (4) for disclosures (screenshot, clipboard, cookie, upload), an owner-issued
//!       consent record is required — page-world cannot approve an upload.
//!   then the global-input capability gate, then the authorised operation runs
//!   (the core document model, or the pinned AlphaCode executor for browser/AT-SPI)
//!   and the receipt records the ACTUAL observed outcome, not the intended click.

pub mod broker;
pub mod consent;
pub mod targets;

pub use broker::{
    AutomationBroker, AutomationObservation, AutomationReceipt, Effect,
};
pub use consent::{ConsentKind, ConsentRecord, ConsentSource, validate_consent};
pub use targets::{
    ApprovedTarget, AutomationScopeRecord, ScopePhase, ScopeRole, TargetKind,
    HOST_SESSION_TARGET, check_target_in_scope, document_revision, scope_of_scope,
};

/// Entity id of a scope state event.
pub fn scope_entity(scope_id: &str) -> String { format!("automation-scope/{scope_id}") }
/// Entity id of a consent state event.
pub fn consent_entity(scope_id: &str, kind: &str) -> String { format!("automation-consent/{scope_id}/{kind}") }
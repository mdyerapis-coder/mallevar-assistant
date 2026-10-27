//! Consent for disclosure operations.
//!
//! A page (page-world script/remote content) can *request* a disclosure but can
//! never *approve* one: the owner is the only consent source the broker
//! accepts. Owner consent is recorded through the owner's adapter when the
//! operator explicitly approves a disclosure for a scope.

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use ts_rs::TS;
use super::targets::TargetKind;

/// Where an explicit disclosure consent came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ConsentSource {
    /// The authenticated owner's adapter recorded this consent.
    Owner,
    /// A page-world eval / remote content tried to supply consent.
    PageWorld,
}

/// The disclosure kinds that need explicit consent on top of the exact action
/// scope: leaving the target's state (screenshot / cookies / clipboard / file
/// upload). Page-world eval, clicks and typing ride the exact action scope and
/// need no separate consent record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ConsentKind {
    Screenshot,
    GetCookies,
    SetCookies,
    ClipboardRead,
    ClipboardWrite,
    FileUpload,
}

/// A consent record carried on an execute request. The broker validates the
/// source, matches the kind to the operation, stores it durably per
/// (scope, kind), and only then authorises the disclosure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ConsentRecord {
    pub source: ConsentSource,
    pub kind: ConsentKind,
    pub scope_id: String,
    /// SHA-256 of the disclosed subject (target identity / URL / file path).
    pub digest: crate::contracts::Digest,
}

/// The disclosure kind an operation carries, or `None` for non-disclosures.
/// `document_file` targets have no disclosures: the file's bytes are the target
/// state itself, read and verified by the core.
pub fn consent_kind_for_operation(kind: TargetKind, operation: &str) -> Option<ConsentKind> {
    use ConsentKind as C;
    match (kind, operation) {
        (TargetKind::BrowserProfile, "screenshot") => Some(C::Screenshot),
        (TargetKind::BrowserProfile, "get_cookies") => Some(C::GetCookies),
        (TargetKind::BrowserProfile, "set_cookies") => Some(C::SetCookies),
        (TargetKind::BrowserProfile, "upload") => Some(C::FileUpload),
        (TargetKind::BrowserProfile, "clipboard_read") => Some(C::ClipboardRead),
        (TargetKind::BrowserProfile, "clipboard_write") => Some(C::ClipboardWrite),
        (TargetKind::NativeWindow, "screenshot") => Some(C::Screenshot),
        (TargetKind::NativeWindow, "clipboard_read") => Some(C::ClipboardRead),
        (TargetKind::NativeWindow, "clipboard_write") => Some(C::ClipboardWrite),
        (TargetKind::NativeWindow, "file_upload") => Some(C::FileUpload),
        _ => None,
    }
}

pub fn validate_consent(record: &ConsentRecord, kind: TargetKind, operation: &str) -> Result<()> {
    let Some(expected) = consent_kind_for_operation(kind, operation) else {
        return Err(CoreError::new(
            "consent_mismatch",
            "A consent record was supplied for an operation that is not a disclosure",
        ));
    };
    if record.source != ConsentSource::Owner {
        return Err(CoreError::new(
            "consent_owner_required",
            "A page instruction cannot approve a disclosure; owner consent is required",
        ));
    }
    if record.kind != expected {
        return Err(CoreError::new(
            "consent_mismatch",
            "The consent record's kind does not match this operation",
        ));
    }
    Ok(())
}
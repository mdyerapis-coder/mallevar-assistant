//! Special owner-account connections (W1/W2/W3: WebGPT / ChatGPT-Web sidecar).
//!
//! A WebGPT connection is a special connection, not a generic OpenAI-compatible
//! provider: the core supervises one pinned sidecar bundle, one isolated profile
//! directory and one isolated cookie store under the core data dir, never the
//! owner's personal browser. Activation is owner-interactive; the core refuses
//! to log in, store a password or bypass a challenge.

pub mod webgpt;

pub use webgpt::{
    WebGptActivationState, WebGptService, WebGptStatus, WebGptSubmissionReceipt,
    WebGptSubmissionState, WebGptTurnInput, WebGptTurnOutput, WebGptToolDescriptor,
};

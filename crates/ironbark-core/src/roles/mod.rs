// H6: role definitions, per-harness activation with import/activation preview.
pub mod activation;
pub mod registry;

pub use activation::{resolve, ActiveRole, HarnessProfile};
pub use registry::{ensure_within_policy, ActiveDefinition, RoleRegistry, BUILTIN_ROLE_IDS};

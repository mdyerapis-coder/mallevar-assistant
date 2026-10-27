// H6: scoped tool registries + qualified local stdio MCP connectors.
pub mod mcp;
pub mod registry;

pub use mcp::{McpActivation, McpApprovalBehaviour, McpAuth, McpConnector, McpConnectorDescriptor, McpInvocation, McpToolDeclaration, McpTransport};
pub use registry::{CatalogEntry, ExcludedTool, ToolListing, ToolRegistry};

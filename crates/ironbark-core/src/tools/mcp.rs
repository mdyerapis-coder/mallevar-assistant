//! Qualified local stdio MCP connectors.
//!
//! A connector declares its transport, the tools it offers (each with a
//! namespace, JSON input schema, auth reference and approval behaviour), and it
//! must be explicitly activated before ANY tool may be invoked. An unknown
//! namespace fails closed. Preparation of an invocation never grants authority:
//! the returned action is still checked by the policy engine against the task
//! authority, so an MCP entry — like a skill — cannot widen policy.

use serde::{Deserialize, Serialize};

use crate::contracts::{ActionId, ActionSpec, Digest, ProjectId, RunId};
use crate::error::{CoreError, Result};

/// Where the local MCP server runs. Only local stdio is qualified: a remote
/// endpoint would need a separately approved connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransport {
    LocalStdio { command: String, args: Vec<String> },
}

/// How a connector authenticates. The auth reference is a pointer; this module
/// never resolves or reads credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpAuth {
    None,
    SecretService { service: String, account: String },
    NativeSession { harness: String, profile: String },
}

/// The declared approval behaviour for a tool. It is a declaration the caller
/// honours; it does not itself approve anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpApprovalBehaviour {
    RequireApproval,
    AutoApprove,
}

/// A single declared tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolDeclaration {
    pub namespace: String,
    pub name: String,
    pub input_schema: serde_json::Value,
    pub auth: McpAuth,
    pub approval: McpApprovalBehaviour,
}

impl McpToolDeclaration {
    /// The qualified name a caller invokes: `<namespace>/<name>`.
    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// A connector descriptor: names, schema, auth and approval behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConnectorDescriptor {
    pub id: String,
    pub transport: McpTransport,
    pub tools: Vec<McpToolDeclaration>,
}

/// A connector with activation state. New connectors are unactivated, so no
/// tool may be invoked until [`McpConnector::activate`] is called explicitly.
#[derive(Debug, Clone)]
pub struct McpConnector {
    descriptor: McpConnectorDescriptor,
    activation: Option<McpActivation>,
}

/// The record of an explicit activation. Binds the reviewed preview digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpActivation {
    pub preview_digest: Digest,
    pub activated_at: i64,
}

/// A prepared invocation: the declaration plus the caller's arguments. Holding
/// it is not permission; it must still pass the policy engine.
#[derive(Debug, Clone, PartialEq)]
pub struct McpInvocation {
    pub tool: McpToolDeclaration,
    pub arguments: serde_json::Value,
}

impl McpConnector {
    pub fn new(descriptor: McpConnectorDescriptor) -> Self {
        Self { descriptor, activation: None }
    }

    pub fn descriptor(&self) -> &McpConnectorDescriptor {
        &self.descriptor
    }

    pub fn is_activated(&self) -> bool {
        self.activation.is_some()
    }

    pub fn activation(&self) -> Option<&McpActivation> {
        self.activation.as_ref()
    }

    /// Explicit activation. Required before any invocation; a re-activation is
    /// allowed only with a fresh preview digest.
    pub fn activate(&mut self, preview_digest: Digest, activated_at: i64) -> Result<()> {
        if let Some(existing) = &self.activation
            && existing.preview_digest == preview_digest
        {
            return Err(CoreError::new("mcp_already_activated", "this connector is already activated for that preview"));
        }
        self.activation = Some(McpActivation { preview_digest, activated_at });
        Ok(())
    }

    /// Fail closed for an unknown namespace: only a declared namespace resolves.
    pub fn qualify_namespace(&self, namespace: &str) -> Result<()> {
        if namespace.is_empty() {
            return Err(CoreError::new("mcp_unknown_namespace", "an MCP namespace is required"));
        }
        if !self.descriptor.tools.iter().any(|tool| tool.namespace == namespace) {
            return Err(CoreError::new(
                "mcp_unknown_namespace",
                format!("connector `{}` declares no namespace `{namespace}`", self.descriptor.id),
            ));
        }
        Ok(())
    }

    /// Resolve a declared tool declaration by qualified name, failing closed for
    /// an unknown namespace or tool.
    pub fn declaration(&self, qualified: &str) -> Result<&McpToolDeclaration> {
        let (namespace, name) = split_qualified(qualified)?;
        self.qualify_namespace(namespace)?;
        self.descriptor
            .tools
            .iter()
            .find(|tool| tool.namespace == namespace && tool.name == name)
            .ok_or_else(|| CoreError::new("mcp_unknown_tool", format!("connector `{}` declares no tool `{qualified}`", self.descriptor.id)))
    }

    /// Prepare an invocation. Requires explicit activation first, then a
    /// declared tool. The returned invocation carries the declared auth and
    /// approval behaviour but no authority.
    pub fn invoke(&self, qualified: &str, arguments: serde_json::Value) -> Result<McpInvocation> {
        if self.activation.is_none() {
            return Err(CoreError::new("mcp_not_activated", "this MCP connector must be activated before any tool is invoked"));
        }
        let tool = self.declaration(qualified)?.clone();
        Ok(McpInvocation { tool, arguments })
    }
}

impl McpInvocation {
    /// Build the action a caller submits to the policy engine. The tool name is
    /// the action kind and its own scope, so the policy engine decides whether
    /// the task authority actually allows it. Preparation is not permission.
    pub fn action(&self, run: RunId, project: ProjectId, policy_version: u64, expected_state_digest: Digest) -> ActionSpec {
        let name = self.tool.qualified_name();
        ActionSpec {
            id: ActionId::new(),
            run_id: run,
            project_id: project,
            kind: name.clone(),
            canonical_args: self.arguments.clone(),
            scope: serde_json::json!({ "tools": [name], "paths": [] }),
            destination: None,
            policy_version,
            expected_state_digest,
        }
    }
}

fn split_qualified(qualified: &str) -> Result<(&str, &str)> {
    match qualified.split_once('/') {
        Some((namespace, name)) if !namespace.is_empty() && !name.is_empty() => Ok((namespace, name)),
        _ => Err(CoreError::new("mcp_unknown_namespace", "a qualified tool name must be `<namespace>/<tool>`")),
    }
}

//! Model Context Protocol (MCP) subsystem.
//!
//! This module re-exports the MCP client types used by the agent runtime
//! to discover and call tools from external MCP servers.
//!
//! ## Architecture
//!
//! - **Ports** (`crate::ports::mcp`): `McpClient`, `McpServerRegistry` traits
//! - **Adapters** (`crate::adapters::mcp`):
//!   - `HttpMcpClient` — JSON-RPC over HTTP POST
//!   - `StdioMcpClient` — JSON-RPC over subprocess stdin/stdout
//!   - `CachedMcpClient` — caching decorator for `tools/list`
//!   - `TimeoutMcpClient` — timeout decorator for all operations
//!   - `StaticMcpRegistry` — in-memory registry with runtime reload
//! - **API** (`crate::api::mcp_api`): exposes the gateway itself as an MCP server
//!
//! The MCP client is the *inbound* path (agent → external tools).
//! The MCP server API is the *outbound* path (external apps → gateway → agent).

pub use crate::ports::mcp::filter_tools_for_scope;
pub use crate::ports::mcp::{
    McpCallRequest, McpCallResult, McpClient, McpServerRegistry, McpToolDef,
};

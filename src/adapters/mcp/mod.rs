//! MCP transport adapters: HTTP (JSON-RPC POST) and stdio (subprocess).
//!
//! Also provides caching (`CachedMcpClient`) and timeout (`TimeoutMcpClient`)
//! decorators that wrap any [`McpClient`](crate::ports::mcp::McpClient) implementation.

pub mod cache;
pub mod http;
pub mod registry;
pub mod stdio;
pub mod timeout;

pub use cache::CachedMcpClient;
pub use http::HttpMcpClient;
pub use registry::{McpServerConfig, StaticMcpRegistry};
pub use stdio::StdioMcpClient;
pub use timeout::TimeoutMcpClient;

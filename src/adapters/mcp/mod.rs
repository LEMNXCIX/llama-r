//! MCP transport adapters: HTTP (JSON-RPC POST) and stdio (subprocess).
//!
//! Also provides caching (`CachedMcpClient`) and timeout (`TimeoutMcpClient`)
//! decorators that wrap any [`McpClient`](crate::ports::mcp::McpClient) implementation.

pub mod cache;
pub mod http;
pub mod namespaced;
pub mod registry;
pub mod stdio;
pub mod streaming;
pub mod timeout;

pub use cache::CachedMcpClient;
pub use http::HttpMcpClient;
pub use namespaced::NamespacedMcpClient;
pub use registry::{McpServerConfig, StaticMcpRegistry};
pub use stdio::StdioMcpClient;
pub use streaming::StreamingHttpMcpClient;
pub use timeout::TimeoutMcpClient;

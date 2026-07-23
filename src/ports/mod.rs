//! Hexagonal ports: traits the core depends on.
//!
//! Adapters (Ollama, MCP HTTP, LanceDB, Rig, SQLite) implement these traits.
//! Application services should depend on ports, never on concrete crates of
//! external apps.

pub mod engine;
pub mod history;
pub mod mcp;
pub mod rag;

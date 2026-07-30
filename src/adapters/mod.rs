//! Concrete adapters implementing [`crate::ports`].
//!
//! Keep external-system details here so the core stays free of app-specific code.

pub mod history;
pub mod mcp;
pub mod rag;
pub mod rig_engine;

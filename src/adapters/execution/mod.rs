//! SQLite implementation of the Fase 7 [`crate::ports::execution::ExecutionStore`].
//!
//! The store opens the **same** `history.db` the conversation-history adapter
//! uses and only adds tables (`CREATE TABLE IF NOT EXISTS`), so an older db
//! keeps opening unchanged. `rusqlite` is pulled in by the `history` feature,
//! so this adapter is gated on that feature exactly like the history adapter;
//! a build without it has no durable store and uses the no-op substitute.

/// The no-op substitute is always built: it is the `--no-default-features`
/// store and the degradation fallback when SQLite cannot be opened.
pub mod noop;

#[cfg(feature = "history")]
pub mod sqlite;

pub use noop::NoOpExecutionStore;

#[cfg(feature = "history")]
pub use sqlite::SqliteExecutionStore;

//! SQLite implementation of [`ConversationStore`].

#[cfg(feature = "history")]
pub mod sqlite;

#[cfg(feature = "history")]
pub use sqlite::SqliteConversationStore;

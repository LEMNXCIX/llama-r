//! SQLite implementation of [`ConversationStore`].

#[cfg(feature = "history")]
pub mod sqlite;

#[cfg(feature = "history")]
pub mod summarizer;

#[cfg(feature = "history")]
pub use sqlite::SqliteConversationStore;

#[cfg(feature = "history")]
pub use summarizer::{NoopSummarizer, OllamaSummarizer};

//! Router adapters: the decision model behind agent routing.
//!
//! d1 answers with probabilities instead of tokens, so the adapter that speaks
//! to it owns d1's wire format completely: [`llamacpp`] posts to llama-server's
//! `POST /v1/systemone`, [`openai_decisions`] posts to OpenRouter's hosted
//! `POST /api/alpha/decisions`, and [`opencode_decisions`] posts to OpenCode's
//! `POST /api/experimental/generate/decisions`. Nothing above this directory
//! knows those URLs or field names like `noul`.
//!
//! Keep it that way — a difference between the hosted API and the local server
//! is a change to the adapter (or the shared `decision_wire` codec) here, not to
//! [`crate::ports::router`].

mod decision_wire;
pub mod llamacpp;
pub mod openai_decisions;
pub mod opencode_decisions;

pub use llamacpp::LlamaCppRouter;
pub use openai_decisions::OpenRouterRouter;
pub use opencode_decisions::OpenCodeRouter;

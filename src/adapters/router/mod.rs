//! Router adapters: the decision model behind agent routing.
//!
//! d1 answers with probabilities instead of tokens, so the adapter that speaks
//! to it owns d1's wire format completely: [`llamacpp`] posts to llama-server's
//! `POST /v1/systemone`, and nothing above this directory knows that URL or
//! field names like `noul`.
//!
//! Keep it that way — a difference between the hosted API and the local server
//! is a change to one file here, not to [`crate::ports::router`].

pub mod llamacpp;

pub use llamacpp::LlamaCppRouter;
